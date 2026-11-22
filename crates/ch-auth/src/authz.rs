//! `Authz`: the permission model of ARCHITECTURE.md section 7.2.
//!
//! `repo_permission` is the maximum of: site admin, owner of a personal repo, org owner,
//! direct collaborator, each team the user belongs to, the org's base permission for members,
//! and `Read` for any signed-in user when the repo is public. It is then capped by the
//! token's scopes. `Anonymous` gets `None` everywhere, public repo or not.
//!
//! The decision itself is the pure function `compute`, so a table test can cover every row
//! without a database.

use crate::core::Core;
use async_trait::async_trait;
use ch_core::error::{Error, Result};
use ch_core::ids::*;
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
    pub public: bool,
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
        #[derive(sqlx::FromRow)]
        struct Row {
            owner_id: i64,
            owner_kind: String,
            visibility: String,
            archived: bool,
            org_base: String,
            org_role: Option<String>,
            collab: Option<String>,
            team_perms: Option<Vec<String>>,
        }
        let row: Option<Row> = sqlx::query_as(
            "SELECT r.owner_id, o.kind AS owner_kind, r.visibility, r.archived, o.org_base_permission AS org_base,
                    (SELECT m.role FROM org_members m WHERE m.org_id = r.owner_id AND m.user_id = $2) AS org_role,
                    (SELECT c.permission FROM repo_collaborators c WHERE c.repo_id = r.id AND c.user_id = $2) AS collab,
                    (SELECT array_agg(tr.permission) FROM team_repos tr JOIN team_members tm ON tm.team_id = tr.team_id WHERE tr.repo_id = r.id AND tm.user_id = $2) AS team_perms
             FROM repositories r JOIN users o ON o.id = r.owner_id WHERE r.id = $1",
        )
        .bind(repo.0)
        .bind(user.0)
        .fetch_optional(&self.core.db)
        .await?;
        let Some(r) = row else { return Ok((Permission::None, false)) };
        let is_org = r.owner_kind == "org";
        let facts = Facts {
            site_admin,
            owns_personal_repo: !is_org && r.owner_id == user.0,
            org_owner: is_org && r.org_role.as_deref() == Some("owner"),
            collaborator: r.collab.as_deref().and_then(Permission::parse),
            team_permissions: r.team_perms.unwrap_or_default().iter().filter_map(|s| Permission::parse(s)).collect(),
            org_member_base: if is_org && r.org_role.is_some() { Permission::parse(&r.org_base) } else { None },
            public: r.visibility == "public",
        };
        Ok((compute(&facts), r.archived))
    }

    /// What the person can do on a repository as an app would see it: never site admin power.
    pub(crate) async fn user_permission_for_app(&self, user: UserId, repo: RepoId) -> Result<Permission> {
        Ok(self.user_permission(user, false, repo).await?.0)
    }

    /// The permission and whether the repo is archived.
    async fn resolve(&self, actor: &Actor, repo: RepoId) -> Result<(Permission, bool)> {
        match actor {
            Actor::Anonymous | Actor::Runner { .. } => Ok((Permission::None, false)),
            Actor::System => Ok((Permission::Admin, false)),
            Actor::Job { repo_id, perms, .. } => {
                // A job may read its own repo and nothing else, public or not.
                let archived: Option<bool> = sqlx::query_scalar("SELECT archived FROM repositories WHERE id = $1")
                    .bind(repo.0)
                    .fetch_optional(&self.core.db)
                    .await?;
                Ok(match archived {
                    Some(a) if *repo_id == repo && perms.contents_read => (Permission::Read, a),
                    _ => (Permission::None, false),
                })
            }
            Actor::User { id, .. } => self.user_permission(*id, actor.is_site_admin(), repo).await,
            Actor::Token { user_id, scopes, .. } => {
                let (p, archived) = self.user_permission(*user_id, false, repo).await?;
                Ok((p.min(scopes.repo_cap()), archived))
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

    /// `NotFound` when the actor cannot read the repo at all, `Forbidden` when they can see
    /// it but lack `needed`. A write to an archived repository says so. Admin level actions
    /// (such as unarchiving) stay possible.
    async fn require(&self, actor: &Actor, repo: RepoId, needed: Permission) -> Result<()> {
        let (have, archived) = self.resolve(actor, repo).await?;
        if have < Permission::Read {
            return Err(Error::NotFound);
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
}
