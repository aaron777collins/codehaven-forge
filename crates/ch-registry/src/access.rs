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
        Actor::User { .. } | Actor::System => true,
        _ => false,
    }
}

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
        let allowed = match pkg.repo_id {
            Some(repo) => {
                let needed = if access == Access::Push { Permission::Write } else { Permission::Read };
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
    // only when the pusher can write to it.
    let linked: Option<(i64,)> = sqlx::query_as("SELECT id FROM repositories WHERE owner_id = $1 AND name = $2::citext")
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
        assert!(!scope_allows(&Actor::Anonymous, Access::Pull));
        assert!(scope_allows(&Actor::User { id: UserId(1), is_admin: false, has_2fa: false, via: ch_core::AuthVia::Basic }, Access::Push));
    }
}
