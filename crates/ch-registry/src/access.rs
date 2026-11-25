//! Who may pull or push which image (ARCHITECTURE.md section 10).
//!
//! An image is `owner/image`. A package row links it to its owner, and optionally to a
//! repository, in which case the repository's permissions decide: read to pull, write to
//! push. An image with no repository belongs to its owner (or, for an organization, to its
//! members to pull and its owners to push). A personal access token also needs the packages
//! scope, so a token made for something else cannot move images.

use crate::error::{RegError, RegResult};
use crate::names::split_name;
use ch_core::actor::Scopes;
use ch_core::model::ClientInfo;
use ch_core::services::OrgRole;
use ch_core::state::AppState;
use ch_core::{Actor, OrgId, Permission, RepoId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Pull,
    Push,
}

#[derive(Debug, Clone)]
pub struct Package {
    pub id: i64,
    pub owner_id: i64,
    pub repo_id: Option<i64>,
    pub owner_name: String,
    pub name: String,
    /// Its repository was deleted or moved to another owner: the owner alone may use it.
    pub orphaned: bool,
}

impl Package {
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner_name, self.name)
    }
}

#[derive(sqlx::FromRow)]
struct PackageRow {
    id: i64,
    owner_id: i64,
    repo_id: Option<i64>,
    owner_name: String,
    name: String,
    orphaned: bool,
}

fn scope_allows(actor: &Actor, access: Access) -> bool {
    match actor {
        Actor::Token { scopes, .. } => match access {
            Access::Pull => Scopes::has(scopes, "read:packages") || Scopes::has(scopes, "write:packages"),
            Access::Push => Scopes::has(scopes, "write:packages"),
        },
        // A workflow job: `packages: read` pulls, `packages: write` pushes, and only images
        // linked to its own repository (checked in `job_allows`).
        Actor::Job { perms, .. } => match access {
            Access::Pull => perms.packages >= Permission::Read,
            Access::Push => perms.packages >= Permission::Write,
        },
        Actor::User { .. } | Actor::System => true,
        // A signed-out pull, let in by the gate only for an image linked to a public repository
        // (ch-4pn.13). Never a push.
        Actor::Anonymous => access == Access::Pull,
        _ => false,
    }
}

/// What pulling a linked image needs on its repository: View, the same as reading its files.
/// So anyone who may read an internal or public repository may pull its images, and a
/// signed-out visitor may pull those of a public one (spec section 4, "Packages and registry").
const PULL_NEEDS: Permission = Permission::View;

async fn owner_kind(state: &AppState, owner: &str) -> RegResult<Option<(i64, String, String)>> {
    let row: Option<(i64, String, String)> = sqlx::query_as("SELECT id, kind, username::text FROM users WHERE username = $1::citext")
        .bind(owner)
        .fetch_optional(&state.db)
        .await?;
    Ok(row)
}

async fn find(state: &AppState, owner: &str, image: &str) -> RegResult<Option<Package>> {
    let row: Option<PackageRow> = sqlx::query_as(
        "SELECT p.id, p.owner_id, p.repo_id, u.username::text AS owner_name, p.name::text AS name, p.orphaned \
         FROM packages p JOIN users u ON u.id = p.owner_id \
         WHERE u.username = $1::citext AND p.kind = 'container' AND p.name = $2::citext",
    )
    .bind(owner)
    .bind(image)
    .fetch_optional(&state.db)
    .await?;
    Ok(row.map(|r| Package {
        id: r.id,
        owner_id: r.owner_id,
        repo_id: r.repo_id,
        owner_name: r.owner_name,
        name: r.name,
        orphaned: r.orphaned,
    }))
}

/// A job reaches only images linked to its own repository (ch-ctk.8). Its scope was checked by
/// `scope_allows`.
fn job_allows(actor: &Actor, pkg_repo: Option<i64>) -> Option<bool> {
    match actor {
        Actor::Job { repo_id, .. } => Some(pkg_repo == Some(repo_id.0)),
        _ => None,
    }
}

/// Can the actor act on an image its owner has, with no repository behind it? An orphaned image
/// (its repository was deleted or moved away) is the owner's alone: for an organization, its
/// owners, never every member (ch-ctk.12).
async fn owner_allows(
    state: &AppState,
    actor: &Actor,
    owner_id: i64,
    owner_is_org: bool,
    access: Access,
    orphaned: bool,
) -> RegResult<bool> {
    if actor.is_system() || actor.user_id().map(|u| u.0) == Some(owner_id) {
        return Ok(true);
    }
    if owner_is_org {
        let role = state.authz.org_role(actor, OrgId(owner_id)).await?;
        return Ok(match (access, role) {
            (_, Some(OrgRole::Owner)) => true,
            (Access::Pull, Some(_)) => !orphaned,
            _ => false,
        });
    }
    Ok(false)
}

/// The person behind a token. A packages scope gives package access on the repositories the
/// person may use, with no `repo` scope needed, and nothing more: this is only ever asked
/// about a registry operation (ch-ctk.12).
fn as_person(actor: &Actor) -> Actor {
    match actor {
        Actor::Token { user_id, .. } => Actor::User { id: *user_id, is_admin: false, has_2fa: false, via: ch_core::AuthVia::Basic },
        other => other.clone(),
    }
}

/// Does the credential carry any package access at all? The catalog asks this once before it
/// reads a single row.
pub fn may_list(actor: &Actor) -> bool {
    scope_allows(actor, Access::Pull)
}

/// Answers already worked out during one listing, so many images of one repository or one
/// owner cost one permission lookup, not one each. Only ever lives for one request.
#[derive(Default)]
pub struct PullMemo {
    repo: std::collections::HashMap<i64, bool>,
    owner: std::collections::HashMap<(i64, bool), bool>,
}

/// May the actor pull an image that exists? The same rule `authorize` applies to a found image,
/// asked without side effects and answering "no" where `authorize` would answer 403 or 404.
/// The catalog uses it to list only what the caller could pull. The answer for a repository
/// (or for an owner, when the image has no repository) is the same for every image behind it,
/// so `memo` keeps it for the rest of the listing; the rule itself is not restated anywhere.
pub async fn can_pull(state: &AppState, actor: &Actor, pkg: &Package, owner_is_org: bool, memo: &mut PullMemo) -> RegResult<bool> {
    if !scope_allows(actor, Access::Pull) {
        return Ok(false);
    }
    if let Some(ok) = job_allows(actor, pkg.repo_id) {
        return Ok(ok);
    }
    match pkg.repo_id {
        Some(repo) => {
            if let Some(&known) = memo.repo.get(&repo) {
                return Ok(known);
            }
            let allowed = match state.authz.require(&as_person(actor), RepoId(repo), PULL_NEEDS).await {
                Ok(()) => true,
                Err(ch_core::Error::NotFound | ch_core::Error::Forbidden(_)) => false,
                Err(e) => return Err(e.into()),
            };
            memo.repo.insert(repo, allowed);
            Ok(allowed)
        }
        None => {
            if let Some(&known) = memo.owner.get(&(pkg.owner_id, pkg.orphaned)) {
                return Ok(known);
            }
            let allowed = owner_allows(state, actor, pkg.owner_id, owner_is_org, Access::Pull, pkg.orphaned).await?;
            memo.owner.insert((pkg.owner_id, pkg.orphaned), allowed);
            Ok(allowed)
        }
    }
}

/// Finds the image and checks the access. `create` lets a push make the image on first use.
/// An image the caller may not read looks the same as one that does not exist.
pub async fn authorize(
    state: &AppState,
    actor: &Actor,
    full_name: &str,
    access: Access,
    create: bool,
    _client: &ClientInfo,
) -> RegResult<Package> {
    let (owner, image) = split_name(full_name).ok_or_else(|| RegError::name_invalid("use owner/image with a lowercase image name"))?;
    if !scope_allows(actor, access) {
        return Err(if access == Access::Push {
            RegError::denied("this token needs the write:packages scope")
        } else {
            RegError::denied("this token needs the read:packages scope")
        });
    }
    let Some((owner_id, kind, owner_name)) = owner_kind(state, owner).await? else { return Err(RegError::name_unknown()) };
    let is_org = kind == "org";

    let person = as_person(actor);
    if let Some(pkg) = find(state, owner, image).await? {
        if let Some(ok) = job_allows(actor, pkg.repo_id) {
            return if ok { Ok(pkg) } else { Err(RegError::name_unknown()) };
        }
        let allowed = match pkg.repo_id {
            Some(repo) => {
                let needed = if access == Access::Push { Permission::Write } else { PULL_NEEDS };
                match state.authz.require(&person, RepoId(repo), needed).await {
                    Ok(()) => true,
                    Err(ch_core::Error::NotFound) => return Err(RegError::name_unknown()),
                    Err(ch_core::Error::Forbidden(m)) => return Err(RegError::denied(m)),
                    Err(e) => return Err(e.into()),
                }
            }
            None => owner_allows(state, actor, owner_id, is_org, access, pkg.orphaned).await?,
        };
        if allowed {
            return Ok(pkg);
        }
        // Someone who may push but not pull does not exist; someone who may not read cannot
        // tell the image is there.
        return Err(if access == Access::Push && owner_allows(state, actor, owner_id, is_org, Access::Pull, pkg.orphaned).await? {
            RegError::denied("you may pull this image but not push to it")
        } else {
            RegError::name_unknown()
        });
    }

    if !(create && access == Access::Push) {
        return Err(RegError::name_unknown());
    }
    // A job creates only an image named like its own repository, linked to it, and never for a
    // public repository, where the first push would publish it (ch-4pn.13).
    if let Actor::Job { repo_id, .. } = actor {
        let own: Option<(i64, String, String)> = sqlx::query_as("SELECT owner_id, name::text, access_tier FROM repositories WHERE id = $1")
            .bind(repo_id.0)
            .fetch_optional(&state.db)
            .await?;
        return match own {
            Some((o, name, tier)) if o == owner_id && name.eq_ignore_ascii_case(image.split('/').next().unwrap_or(image)) => {
                if tier == "public" {
                    return Err(RegError::denied("a workflow cannot create an image for a public repository; push it once yourself"));
                }
                let inserted: (i64, Option<i64>) = sqlx::query_as(
                    "INSERT INTO packages (owner_id, repo_id, kind, name) VALUES ($1, $2, 'container', $3) \
                     ON CONFLICT (owner_id, kind, name) DO UPDATE SET name = packages.name RETURNING id, repo_id",
                )
                .bind(owner_id)
                .bind(repo_id.0)
                .bind(image)
                .fetch_one(&state.db)
                .await?;
                // Someone else made it first: it is theirs, and this job is held to the usual rule.
                if inserted.1 != Some(repo_id.0) {
                    return Err(RegError::name_unknown());
                }
                Ok(Package { id: inserted.0, owner_id, repo_id: Some(repo_id.0), owner_name, name: image.to_string(), orphaned: false })
            }
            _ => Err(RegError::name_unknown()),
        };
    }
    if !owner_allows(state, actor, owner_id, is_org, Access::Push, false).await? {
        // Only someone who could read that account's images learns that pushing is refused.
        // Anyone else gets the answer an unreadable image gets (ch-x0b).
        return Err(if owner_allows(state, actor, owner_id, is_org, Access::Pull, false).await? {
            RegError::denied("you cannot push images for that account")
        } else {
            RegError::name_unknown()
        });
    }
    // A repository with the same name as the image is linked, so its permissions apply, but
    // only when the pusher can write to it, and never when it is public: a first push must not
    // publish an image by itself (ch-4pn.13). Such an image starts as its owner's alone.
    let linked: Option<(i64,)> =
        sqlx::query_as("SELECT id FROM repositories WHERE owner_id = $1 AND name = $2::citext AND access_tier <> 'public'")
            .bind(owner_id)
            .bind(image)
            .fetch_optional(&state.db)
            .await?;
    let repo_id = match linked {
        Some((id,)) if state.authz.require(&person, RepoId(id), Permission::Write).await.is_ok() => Some(id),
        _ => None,
    };
    let inserted: (i64,) = sqlx::query_as(
        "INSERT INTO packages (owner_id, repo_id, kind, name) VALUES ($1, $2, 'container', $3) \
         ON CONFLICT (owner_id, kind, name) DO UPDATE SET name = packages.name RETURNING id",
    )
    .bind(owner_id)
    .bind(repo_id)
    .bind(image)
    .fetch_one(&state.db)
    .await?;
    Ok(Package { id: inserted.0, owner_id, repo_id, owner_name, name: image.to_string(), orphaned: false })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ch_core::UserId;

    #[test]
    fn tokens_need_the_packages_scope() {
        let tok = |s: &[&str]| Actor::Token { user_id: UserId(1), token_id: ch_core::TokenId(1), scopes: Scopes::new(s.iter().copied()) };
        assert!(scope_allows(&tok(&["read:packages"]), Access::Pull));
        assert!(!scope_allows(&tok(&["read:packages"]), Access::Push));
        assert!(scope_allows(&tok(&["write:packages"]), Access::Push));
        assert!(scope_allows(&tok(&["write:packages"]), Access::Pull));
        assert!(!scope_allows(&tok(&["repo"]), Access::Pull), "repo access is not package access");
        assert!(!scope_allows(&tok(&[]), Access::Pull));
        assert!(scope_allows(&Actor::Anonymous, Access::Pull), "the gate decides which images");
        assert!(!scope_allows(&Actor::Anonymous, Access::Push));
        assert!(scope_allows(&Actor::User { id: UserId(1), is_admin: false, has_2fa: false, via: ch_core::AuthVia::Basic }, Access::Push));
    }
}
