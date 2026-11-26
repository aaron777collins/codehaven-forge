//! `Authz`: the permission model of ARCHITECTURE.md section 7.2.
//!
//! `repo_permission` is the maximum of: site admin, owner of a personal repo, org owner,
//! direct collaborator, each team the user belongs to, the org's base permission for members,
//! and `Read` for any signed-in user when the repo is internal or public (epic ch-4pn). It is
//! then capped by the token's scopes, and a public repo stays readable to any token.
//!
//! `Anonymous` gets `View` on a public repository while the instance switch allows public
//! repositories, and `None` everywhere else. `View` sits below `Read`, so every check written
//! for `Read` still refuses a signed-out caller.
//!
//! The decision itself is the pure function `compute`, so a table test can cover every row
//! without a database.

use crate::core::Core;
use async_trait::async_trait;
use ch_core::error::{Error, Result};
use ch_core::ids::*;
use ch_core::model::Visibility;
use ch_core::services::{Authz, OrgRole};
use ch_core::{Actor, Permission};
use std::sync::Arc;

/// Everything `compute` needs to know about one user and one repository.
#[derive(Debug, Clone, Default)]
pub struct Facts {
    pub site_admin: bool,
    pub owns_personal_repo: bool,
    pub org_owner: bool,
    pub collaborator: Option<Permission>,
    pub team_permissions: Vec<Permission>,
    /// The org's base permission, when the user is a member of the repo's org.
    pub org_member_base: Option<Permission>,
    /// The repository is internal or public, so every signed-in account reads it.
    pub public: bool,
}

/// What the grant rows alone give, with nothing from the tier.
pub fn grant_of(f: &Facts) -> Permission {
    compute(&Facts { public: false, ..f.clone() })
}

pub fn compute(f: &Facts) -> Permission {
    let mut p = Permission::None;
    if f.site_admin || f.owns_personal_repo || f.org_owner {
        p = Permission::Admin;
    }
    if let Some(c) = f.collaborator {
        p = p.max(c);
    }
    for t in &f.team_permissions {
        p = p.max(*t);
    }
    if let Some(b) = f.org_member_base {
        p = p.max(b);
    }
    if f.public {
        p = p.max(Permission::Read);
    }
    p
}

/// The most a grant allows on one repository: Write when it holds write there, Read when it
/// holds read, nothing otherwise.
pub fn app_cap(grant: &ch_core::AppGrant, repo: RepoId) -> Permission {
    if grant.write.has(repo) {
        Permission::Write
    } else if grant.read.has(repo) {
        Permission::Read
    } else {
        Permission::None
    }
}

pub struct AuthzService {
    pub(crate) core: Arc<Core>,
}

impl AuthzService {
    pub fn new(core: Arc<Core>) -> Self {
        Self { core }
    }

    async fn user_permission(&self, user: UserId, site_admin: bool, repo: RepoId) -> Result<(Permission, bool)> {
        let (facts, archived, _) = self.user_facts(user, site_admin, repo).await?;
        Ok((facts.as_ref().map(compute).unwrap_or(Permission::None), archived))
    }

    /// The facts, whether the repo is archived, and whether a signed-out visitor could read it
    /// right now (public, with the instance switch on). `None` facts for a missing repo.
    ///
    /// A private fork carries its ancestors' code, so someone other than its owner who cannot
    /// see every private repository it was forked from gets nothing on it (ch-4pn.16): a grant on
    /// the fork alone does not open what the parent's owner closed. Its owner keeps it.
    async fn user_facts(&self, user: UserId, site_admin: bool, repo: RepoId) -> Result<(Option<Facts>, bool, bool)> {
        let (facts, archived, open_to_all) = self.own_facts(user, site_admin, repo).await?;
        let Some(f) = &facts else { return Ok((facts, archived, open_to_all)) };
        if f.site_admin || f.owns_personal_repo || f.org_owner {
            return Ok((facts, archived, open_to_all));
        }
        let ancestors: Vec<i64> = sqlx::query_scalar(
            "WITH RECURSIVE chain (id, parent, depth) AS (
                 SELECT r.id, r.fork_parent_id, 0 FROM repositories r
                  WHERE r.id = $1 AND r.fork_parent_id IS NOT NULL AND r.access_tier = 'private'
                 UNION ALL
                 SELECT p.id, p.fork_parent_id, c.depth + 1 FROM repositories p JOIN chain c ON p.id = c.parent WHERE c.depth < 64)
             SELECT c.id FROM chain c JOIN repositories a ON a.id = c.id WHERE c.depth > 0 AND a.access_tier = 'private'",
        )
        .bind(repo.0)
        .fetch_all(&self.core.db)
        .await?;
        for a in ancestors {
            let (af, _, _) = self.own_facts(user, site_admin, RepoId(a)).await?;
            if af.as_ref().map(compute).unwrap_or(Permission::None) < Permission::View {
                return Ok((Some(Facts::default()), archived, open_to_all));
            }
        }
        Ok((facts, archived, open_to_all))
    }

    async fn own_facts(&self, user: UserId, site_admin: bool, repo: RepoId) -> Result<(Option<Facts>, bool, bool)> {
        #[derive(sqlx::FromRow)]
        struct Row {
            owner_id: i64,
            owner_kind: String,
            access_tier: String,
            open_to_all: bool,
            archived: bool,
            org_base: String,
            org_role: Option<String>,
            collab: Option<String>,
            team_perms: Option<Vec<String>>,
            usable: bool,
            held: bool,
        }
        let row: Option<Row> = sqlx::query_as(
            "SELECT r.owner_id, o.kind AS owner_kind, r.access_tier, \
                    (r.access_tier = 'public' AND (SELECT s.allow_public_repos FROM visibility_settings s)) IS TRUE AS open_to_all, \
                    r.archived, o.org_base_permission AS org_base,
                    (SELECT m.role FROM org_members m WHERE m.org_id = r.owner_id AND m.user_id = $2) AS org_role,
                    (SELECT c.permission FROM repo_collaborators c WHERE c.repo_id = r.id AND c.user_id = $2) AS collab,
                    (SELECT array_agg(tr.permission) FROM team_repos tr JOIN team_members tm ON tm.team_id = tr.team_id WHERE tr.repo_id = r.id AND tm.user_id = $2) AS team_perms,
                    EXISTS (SELECT 1 FROM users me WHERE me.id = $2 AND me.suspended_at IS NULL) AS usable,
                    EXISTS (SELECT 1 FROM repo_access_holds h WHERE h.repo_id = r.id AND h.user_id = $2) AS held
             FROM repositories r JOIN users o ON o.id = r.owner_id WHERE r.id = $1",
        )
        .bind(repo.0)
        .bind(user.0)
        .fetch_optional(&self.core.db)
        .await?;
        let Some(r) = row else { return Ok((None, false, false)) };
        // A suspended account, or one that is gone, has no grant anywhere, whatever is still open
        // on its behalf: an SSH connection, an auto-merge it armed, a stream (ch-4he). Only what
        // anyone may read stays readable.
        if !r.usable {
            return Ok((Some(Facts::default()), r.archived, r.open_to_all));
        }
        let is_org = r.owner_kind == "org";
        let facts = Facts {
            site_admin,
            owns_personal_repo: !is_org && r.owner_id == user.0,
            org_owner: is_org && r.org_role.as_deref() == Some("owner"),
            collaborator: r.collab.as_deref().and_then(Permission::parse),
            team_permissions: r.team_perms.unwrap_or_default().iter().filter_map(|s| Permission::parse(s)).collect(),
            // A hold (ch-ze4) keeps this one member from the organization's base permission on this
            // repository: they could not see what it was forked from. Grants made to them by name
            // are not held back.
            org_member_base: if is_org && r.org_role.is_some() && !r.held { Permission::parse(&r.org_base) } else { None },
            // An unknown stored tier reads as private.
            public: matches!(Visibility::parse(&r.access_tier), Some(Visibility::Internal | Visibility::Public)),
        };
        Ok((Some(facts), r.archived, r.open_to_all))
    }

    /// `View` when a signed-out visitor may read the repository now: it is public and the
    /// instance allows public repositories. `None` for everything else, missing included.
    async fn anonymous_permission(&self, repo: RepoId) -> Result<(Permission, bool)> {
        let row: Option<(bool, bool)> = sqlx::query_as(
            "SELECT (r.access_tier = 'public' AND (SELECT s.allow_public_repos FROM visibility_settings s)) IS TRUE, r.archived \
             FROM repositories r WHERE r.id = $1",
        )
        .bind(repo.0)
        .fetch_optional(&self.core.db)
        .await?;
        Ok(match row {
            Some((true, archived)) => (Permission::View, archived),
            _ => (Permission::None, false),
        })
    }

    /// What the person can do on a repository as an app would see it: never site admin power.
    pub(crate) async fn user_permission_for_app(&self, user: UserId, repo: RepoId) -> Result<Permission> {
        Ok(self.user_permission(user, false, repo).await?.0)
    }

    /// The permission and whether the repo is archived.
    async fn resolve(&self, actor: &Actor, repo: RepoId) -> Result<(Permission, bool)> {
        match actor {
            Actor::Anonymous => self.anonymous_permission(repo).await,
            Actor::Runner { .. } => Ok((Permission::None, false)),
            Actor::System => Ok((Permission::Admin, false)),
            Actor::Job { repo_id, perms, .. } => {
                // A job works on its own repo and nothing else, public or not. It writes only when
                // its `permissions:` give it `contents: write`; every other scope is checked where
                // it is used (statuses, checks, issues, packages).
                let archived: Option<bool> = sqlx::query_scalar("SELECT archived FROM repositories WHERE id = $1")
                    .bind(repo.0)
                    .fetch_optional(&self.core.db)
                    .await?;
                let level = if perms.contents >= Permission::Write {
                    Permission::Write
                } else if perms.any() {
                    Permission::Read
                } else {
                    Permission::None
                };
                Ok(match archived {
                    Some(a) if *repo_id == repo => (level, a),
                    _ => (Permission::None, false),
                })
            }
            Actor::User { id, .. } => self.user_permission(*id, actor.is_site_admin(), repo).await,
            Actor::Token { user_id, scopes, .. } => {
                let (facts, archived, open_to_all) = self.user_facts(*user_id, false, repo).await?;
                let p = facts.as_ref().map(compute).unwrap_or(Permission::None).min(scopes.repo_cap());
                // What anyone can read, any token can read.
                let floor = if open_to_all { Permission::View } else { Permission::None };
                Ok((p.max(floor), archived))
            }
            Actor::App { user_id, grant, .. } => {
                // What the person can do, capped by what they gave this app on this repository.
                // Maintain and Admin are unreachable, and a repo the app was not given, public or
                // not, is simply missing.
                let cap = app_cap(grant, repo);
                if cap == Permission::None {
                    return Ok((Permission::None, false));
                }
                let (p, archived) = self.user_permission(*user_id, false, repo).await?;
                Ok((p.min(cap), archived))
            }
        }
    }
}

#[async_trait]
impl Authz for AuthzService {
    async fn repo_permission(&self, actor: &Actor, repo: RepoId) -> Result<Permission> {
        Ok(self.resolve(actor, repo).await?.0)
    }

    async fn has_grant(&self, actor: &Actor, repo: RepoId) -> Result<bool> {
        let (user, site_admin) = match actor {
            Actor::System => return Ok(true),
            Actor::User { id, .. } => (*id, actor.is_site_admin()),
            Actor::Token { user_id, .. } | Actor::App { user_id, .. } => (*user_id, false),
            // A job contributes (issues, comments, pull requests) to its own repository only
            // when its token has `issues: write` or `pull-requests: write`.
            Actor::Job { repo_id, perms, .. } => return Ok(*repo_id == repo && perms.contribute()),
            _ => return Ok(false),
        };
        let (facts, _, _) = self.user_facts(user, site_admin, repo).await?;
        Ok(facts.as_ref().map(grant_of).unwrap_or(Permission::None) >= Permission::Read)
    }

    async fn org_role(&self, actor: &Actor, org: OrgId) -> Result<Option<OrgRole>> {
        if actor.is_site_admin() {
            return Ok(Some(OrgRole::Owner));
        }
        let Some(user) = actor.user_id() else { return Ok(None) };
        let role: Option<String> = sqlx::query_scalar("SELECT role FROM org_members WHERE org_id = $1 AND user_id = $2")
            .bind(org.0)
            .bind(user.0)
            .fetch_optional(&self.core.db)
            .await?;
        Ok(match role.as_deref() {
            Some("owner") => Some(OrgRole::Owner),
            Some(_) => Some(OrgRole::Member),
            None => None,
        })
    }

    /// `NotFound` when the actor cannot see the repo at all, `Forbidden` when they can see
    /// it but lack `needed`. A write to an archived repository says so. Admin level actions
    /// (such as unarchiving) stay possible.
    async fn require(&self, actor: &Actor, repo: RepoId, needed: Permission) -> Result<()> {
        let (have, archived) = self.resolve(actor, repo).await?;
        if have < Permission::View {
            return Err(Error::NotFound);
        }
        // A signed-out visitor only ever reads. Anything more is the gate's rejection, never a
        // message that says the repository exists.
        if actor.is_anonymous() && needed > Permission::View {
            return Err(Error::unauthorized());
        }
        if have < needed {
            return Err(Error::forbidden(match needed {
                Permission::Write => "You need write access to this repository.",
                Permission::Admin => "You need admin access to this repository.",
                _ => "You do not have permission to do that.",
            }));
        }
        if archived && needed > Permission::Read && needed < Permission::Admin {
            return Err(Error::forbidden("This repository is archived, so it is read only."));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Permission::*;

    #[test]
    fn an_app_is_capped_by_its_grant() {
        use ch_core::{AppGrant, RepoSet};
        let only = |ids: &[i64]| RepoSet::Only(ids.iter().map(|i| RepoId(*i)).collect());
        let g = AppGrant { read: only(&[1, 2]), write: only(&[2]), workflows: RepoSet::none() };
        assert_eq!(app_cap(&g, RepoId(1)), Read);
        assert_eq!(app_cap(&g, RepoId(2)), Write);
        assert_eq!(app_cap(&g, RepoId(3)), None, "unselected equals missing");
        let all = AppGrant { read: RepoSet::All, write: RepoSet::All, workflows: RepoSet::All };
        assert_eq!(app_cap(&all, RepoId(99)), Write, "never Maintain or Admin");
    }

    fn f() -> Facts {
        Facts::default()
    }

    #[test]
    fn table_of_every_row_in_7_2() {
        let cases: Vec<(&str, Facts, Permission)> = vec![
            ("nothing", f(), None),
            ("site admin", Facts { site_admin: true, ..f() }, Admin),
            ("owner of a personal repo", Facts { owns_personal_repo: true, ..f() }, Admin),
            ("org owner", Facts { org_owner: true, ..f() }, Admin),
            ("collaborator read", Facts { collaborator: Some(Read), ..f() }, Read),
            ("collaborator write", Facts { collaborator: Some(Write), ..f() }, Write),
            ("team triage", Facts { team_permissions: vec![Triage], ..f() }, Triage),
            ("two teams take the maximum", Facts { team_permissions: vec![Read, Maintain, Triage], ..f() }, Maintain),
            ("org base permission for members", Facts { org_member_base: Some(Read), ..f() }, Read),
            ("org base none", Facts { org_member_base: Some(None), ..f() }, None),
            ("org base write", Facts { org_member_base: Some(Write), ..f() }, Write),
            ("public repo, signed in", Facts { public: true, ..f() }, Read),
            ("collaborator beats public", Facts { public: true, collaborator: Some(Write), ..f() }, Write),
            ("team beats collaborator", Facts { collaborator: Some(Read), team_permissions: vec![Admin], ..f() }, Admin),
            ("base permission beats public", Facts { public: true, org_member_base: Some(Triage), ..f() }, Triage),
            (
                "a lower grant never lowers a higher one",
                Facts { collaborator: Some(Admin), org_member_base: Some(Read), public: true, ..f() },
                Admin,
            ),
        ];
        for (name, facts, want) in cases {
            assert_eq!(compute(&facts), want, "{name}");
        }
    }

    #[test]
    fn the_tier_is_never_a_grant() {
        // What the contribution setting asks: does access come from a grant row?
        assert_eq!(grant_of(&Facts { public: true, ..f() }), None, "reading an internal or public repo is not a grant");
        assert_eq!(grant_of(&Facts { public: true, collaborator: Some(Read), ..f() }), Read);
        assert_eq!(grant_of(&Facts { public: true, org_member_base: Some(None), ..f() }), None);
        assert_eq!(grant_of(&Facts { owns_personal_repo: true, public: true, ..f() }), Admin);
        // The tier still gives every signed-in account Read, so nothing changes for them.
        assert_eq!(compute(&Facts { public: true, ..f() }), Read);
        // View exists only for signed-out reading and is below every grant.
        assert!(View < Read && None < View);
        assert_eq!(Permission::parse("view"), Option::None, "View is never stored or parsed");
    }
}
