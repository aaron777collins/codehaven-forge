//! `Orgs`: organizations, members, teams and what teams may do to repositories.
//!
//! An org is a row in `users` with `kind = 'org'`, so it shares the username namespace with
//! people. Orgs never sign in. Owners manage members and teams; site admins can do anything
//! an owner can.

use crate::authz::AuthzService;
use crate::core::*;
use crate::util::*;
use async_trait::async_trait;
use ch_core::audit::action;
use ch_core::error::{Error, FieldError, Result};
use ch_core::events::DomainEvent;
use ch_core::identity::*;
use ch_core::ids::*;
use ch_core::model::ClientInfo;
use ch_core::services::{Authz, OrgRole, Orgs};
use ch_core::{Actor, Permission};
use chrono::{DateTime, Utc};
use serde_json::json;
use std::sync::Arc;

pub struct OrgsService {
    pub(crate) core: Arc<Core>,
    authz: Arc<AuthzService>,
}

#[derive(sqlx::FromRow)]
struct OrgRow {
    id: i64,
    name: String,
    display_name: String,
    description: String,
    base: String,
    members: i64,
    created_at: DateTime<Utc>,
}

const ORG_SELECT: &str = "SELECT u.id, u.username::text AS name, u.display_name, u.description, u.org_base_permission AS base, \
    (SELECT count(*) FROM org_members m WHERE m.org_id = u.id) AS members, u.created_at FROM users u";

impl From<OrgRow> for Org {
    fn from(r: OrgRow) -> Org {
        Org {
            id: OrgId(r.id),
            name: r.name,
            display_name: r.display_name,
            description: r.description,
            base_permission: Permission::parse(&r.base).unwrap_or(Permission::Read),
            member_count: r.members,
            created_at: r.created_at,
        }
    }
}

#[derive(sqlx::FromRow)]
struct TeamRow {
    id: i64,
    org_id: i64,
    name: String,
    description: String,
    permission: String,
    members: i64,
    repos: i64,
    created_at: DateTime<Utc>,
}

const TEAM_SELECT: &str = "SELECT t.id, t.org_id, t.name::text AS name, t.description, t.permission, \
    (SELECT count(*) FROM team_members tm WHERE tm.team_id = t.id) AS members, \
    (SELECT count(*) FROM team_repos tr WHERE tr.team_id = t.id) AS repos, t.created_at FROM teams t";

impl From<TeamRow> for Team {
    fn from(r: TeamRow) -> Team {
        Team {
            id: TeamId(r.id),
            org_id: OrgId(r.org_id),
            name: r.name,
            description: r.description,
            permission: Permission::parse(&r.permission).unwrap_or(Permission::Read),
            member_count: r.members,
            repo_count: r.repos,
            created_at: r.created_at,
        }
    }
}

fn validate_team_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 100
        && name.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(Error::Validation(vec![FieldError::custom(
            "Team",
            "name",
            "Use letters, numbers, hyphens and underscores, starting with a letter or number.",
        )]))
    }
}

impl OrgsService {
    pub fn new(core: Arc<Core>, authz: Arc<AuthzService>) -> Self {
        Self { core, authz }
    }

    /// The caller's role in an org: `Some(Owner)` for a site admin.
    async fn role(&self, actor: &Actor, org: OrgId) -> Result<Option<OrgRole>> {
        self.authz.org_role(actor, org).await
    }

    async fn org_exists(&self, org: OrgId) -> Result<()> {
        let kind: Option<String> =
            sqlx::query_scalar("SELECT kind FROM users WHERE id = $1").bind(org.0).fetch_optional(&self.core.db).await?;
        if kind.as_deref() == Some("org") { Ok(()) } else { Err(Error::NotFound) }
    }

    /// Owner or site admin. Others who can see the org get `Forbidden`, everyone else `NotFound`.
    /// A personal access token also needs the admin:org scope (ch-84u).
    async fn require_owner(&self, actor: &Actor, org: OrgId) -> Result<()> {
        self.org_exists(org).await?;
        require_admin_org_scope(actor)?;
        match self.role(actor, org).await? {
            Some(OrgRole::Owner) => Ok(()),
            Some(OrgRole::Member) => Err(Error::forbidden("Only organization owners can do that.")),
            None if actor.is_anonymous() => Err(Error::unauthorized()),
            None => Err(Error::NotFound),
        }
    }

    async fn require_member(&self, actor: &Actor, org: OrgId) -> Result<()> {
        self.org_exists(org).await?;
        match self.role(actor, org).await? {
            Some(_) => Ok(()),
            None if actor.is_anonymous() => Err(Error::unauthorized()),
            None => Err(Error::NotFound),
        }
    }

    async fn team_org(&self, team: TeamId) -> Result<OrgId> {
        let org: Option<i64> =
            sqlx::query_scalar("SELECT org_id FROM teams WHERE id = $1").bind(team.0).fetch_optional(&self.core.db).await?;
        org.map(OrgId).ok_or(Error::NotFound)
    }

    async fn owner_count(&self, tx: &mut sqlx::PgConnection, org: OrgId) -> Result<i64> {
        Ok(sqlx::query_scalar("SELECT count(*) FROM org_members WHERE org_id = $1 AND role = 'owner'")
            .bind(org.0)
            .fetch_one(&mut *tx)
            .await?)
    }
}

/// Managing an org with a personal access token needs admin:org, whatever the person's role.
/// Other kinds of caller are unaffected: a session is the person, and an app or a job has no org
/// role at all, so `require_owner` refuses it anyway.
fn require_admin_org_scope(actor: &Actor) -> Result<()> {
    match actor {
        Actor::Token { scopes, .. } if !token_has_scope(scopes, "admin:org") => {
            Err(Error::forbidden("This token needs the admin:org scope"))
        }
        _ => Ok(()),
    }
}

fn role_of(s: &str) -> OrgRole {
    if s == "owner" { OrgRole::Owner } else { OrgRole::Member }
}

#[async_trait]
impl Orgs for OrgsService {
    async fn create(&self, actor: &Actor, name: &str, description: &str, client: &ClientInfo) -> Result<Org> {
        let creator = acting_user(actor, Some("admin:org"))?;
        validate_username(name, "Organization")?;
        if description.chars().count() > 500 {
            return Err(Error::Validation(vec![FieldError::custom("Organization", "description", "Use at most 500 characters.")]));
        }
        let mut tx = self.core.db.begin().await?;
        let id: i64 = // An org may run workflows when the person who made it may. A site admin can change it later.
        sqlx::query_scalar(
            "INSERT INTO users (kind, username, description, can_run_ci)
             VALUES ('org', $1, $2, COALESCE((SELECT can_run_ci FROM users WHERE id = $3), false)) RETURNING id",
        )
            .bind(name)
            .bind(description.trim())
            .bind(creator.0)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| if is_unique_violation(&e, "") { crate::accounts::exists("Organization", "name") } else { e.into() })?;
        sqlx::query("INSERT INTO org_members (org_id, user_id, role) VALUES ($1, $2, 'owner')")
            .bind(id)
            .bind(creator.0)
            .execute(&mut *tx)
            .await?;
        self.core.audit_tx(&mut tx, actor, client, action::ORG_CREATED, Some(("org", id)), json!({"name": name})).await?;
        self.core
            .events
            .emit(&mut tx, DomainEvent::MemberAdded { repo_id: None, org_id: Some(OrgId(id)), user_id: creator, actor: Some(creator) })
            .await?;
        tx.commit().await?;
        self.get(actor, name).await
    }

    async fn get(&self, actor: &Actor, name: &str) -> Result<Org> {
        if actor.is_anonymous() {
            return Err(Error::NotFound);
        }
        let row: OrgRow = sqlx::query_as(&format!("{ORG_SELECT} WHERE u.username = $1::citext AND u.kind = 'org'"))
            .bind(name)
            .fetch_optional(&self.core.db)
            .await?
            .ok_or(Error::NotFound)?;
        Ok(row.into())
    }

    async fn list_for_user(&self, actor: &Actor, user: UserId) -> Result<Vec<Org>> {
        require_self_or_admin(actor, user)?;
        let rows: Vec<OrgRow> = sqlx::query_as(&format!(
            "{ORG_SELECT} JOIN org_members om ON om.org_id = u.id WHERE om.user_id = $1 AND u.kind = 'org' ORDER BY u.username"
        ))
        .bind(user.0)
        .fetch_all(&self.core.db)
        .await?;
        Ok(rows.into_iter().map(Org::from).collect())
    }

    async fn update(
        &self,
        actor: &Actor,
        org: OrgId,
        display_name: Option<String>,
        description: Option<String>,
        base_permission: Option<Permission>,
        client: &ClientInfo,
    ) -> Result<Org> {
        self.require_owner(actor, org).await?;
        if display_name.as_ref().is_some_and(|d| d.chars().count() > 100) || description.as_ref().is_some_and(|d| d.chars().count() > 500) {
            return Err(Error::Validation(vec![FieldError::custom("Organization", "description", "That is too long.")]));
        }
        let mut tx = self.core.db.begin().await?;
        sqlx::query("UPDATE users SET display_name = COALESCE($2, display_name), description = COALESCE($3, description), org_base_permission = COALESCE($4, org_base_permission), updated_at = now() WHERE id = $1")
            .bind(org.0)
            .bind(display_name.as_deref().map(str::trim))
            .bind(description.as_deref().map(str::trim))
            .bind(base_permission.map(|p| p.as_str()))
            .execute(&mut *tx)
            .await?;
        self.core
            .audit_tx(
                &mut tx,
                actor,
                client,
                action::ORG_UPDATED,
                Some(("org", org.0)),
                json!({"base_permission": base_permission.map(|p| p.as_str())}),
            )
            .await?;
        tx.commit().await?;
        let name: String =
            sqlx::query_scalar("SELECT username::text FROM users WHERE id = $1").bind(org.0).fetch_one(&self.core.db).await?;
        self.get(actor, &name).await
    }

    async fn delete(&self, actor: &Actor, org: OrgId, client: &ClientInfo) -> Result<()> {
        self.require_owner(actor, org).await?;
        let repos: i64 =
            sqlx::query_scalar("SELECT count(*) FROM repositories WHERE owner_id = $1").bind(org.0).fetch_one(&self.core.db).await?;
        if repos > 0 {
            return Err(Error::conflict("Delete or transfer the organization's repositories first."));
        }
        let mut tx = self.core.db.begin().await?;
        let name: String = sqlx::query_scalar("DELETE FROM users WHERE id = $1 AND kind = 'org' RETURNING username::text")
            .bind(org.0)
            .fetch_one(&mut *tx)
            .await?;
        self.core.audit_tx(&mut tx, actor, client, action::ORG_DELETED, Some(("org", org.0)), json!({"name": name})).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn members(&self, actor: &Actor, org: OrgId) -> Result<Vec<OrgMember>> {
        self.require_member(actor, org).await?;
        let rows: Vec<(i64, String, String, String)> = sqlx::query_as(
            "SELECT u.id, u.username::text, u.display_name, m.role FROM org_members m JOIN users u ON u.id = m.user_id WHERE m.org_id = $1 ORDER BY m.role DESC, u.username",
        )
        .bind(org.0)
        .fetch_all(&self.core.db)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(id, username, display_name, role)| OrgMember { user_id: UserId(id), username, display_name, role: role_of(&role) })
            .collect())
    }

    async fn set_member(&self, actor: &Actor, org: OrgId, user: UserId, role: OrgRole, client: &ClientInfo) -> Result<()> {
        self.require_owner(actor, org).await?;
        let mut tx = self.core.db.begin().await?;
        let target: Option<(String, bool)> =
            sqlx::query_as("SELECT kind, suspended_at IS NOT NULL FROM users WHERE id = $1").bind(user.0).fetch_optional(&mut *tx).await?;
        match target {
            Some((kind, false)) if kind == "user" => {}
            _ => {
                return Err(Error::Validation(vec![FieldError::custom(
                    "Membership",
                    "user",
                    "That account does not exist or is suspended.",
                )]));
            }
        }
        let role_s = if role == OrgRole::Owner { "owner" } else { "member" };
        let existing: Option<String> = sqlx::query_scalar("SELECT role FROM org_members WHERE org_id = $1 AND user_id = $2 FOR UPDATE")
            .bind(org.0)
            .bind(user.0)
            .fetch_optional(&mut *tx)
            .await?;
        if existing.as_deref() == Some("owner") && role == OrgRole::Member && self.owner_count(&mut tx, org).await? <= 1 {
            return Err(Error::conflict("An organization needs at least one owner."));
        }
        sqlx::query("INSERT INTO org_members (org_id, user_id, role) VALUES ($1, $2, $3) ON CONFLICT (org_id, user_id) DO UPDATE SET role = EXCLUDED.role").bind(org.0).bind(user.0).bind(role_s).execute(&mut *tx).await?;
        self.core
            .audit_tx(&mut tx, actor, client, action::ORG_MEMBER_SET, Some(("org", org.0)), json!({"user_id": user.0, "role": role_s}))
            .await?;
        if existing.is_none() {
            self.core
                .events
                .emit(&mut tx, DomainEvent::MemberAdded { repo_id: None, org_id: Some(org), user_id: user, actor: actor.user_id() })
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn remove_member(&self, actor: &Actor, org: OrgId, user: UserId, client: &ClientInfo) -> Result<()> {
        self.org_exists(org).await?;
        if actor.user_id() != Some(user) {
            self.require_owner(actor, org).await?;
        } else {
            require_admin_org_scope(actor)?;
            self.require_member(actor, org).await?;
        }
        let mut tx = self.core.db.begin().await?;
        let role: Option<String> = sqlx::query_scalar("SELECT role FROM org_members WHERE org_id = $1 AND user_id = $2 FOR UPDATE")
            .bind(org.0)
            .bind(user.0)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(role) = role else { return Err(Error::NotFound) };
        if role == "owner" && self.owner_count(&mut tx, org).await? <= 1 {
            return Err(Error::conflict("The last owner cannot leave. Make someone else an owner first."));
        }
        sqlx::query("DELETE FROM team_members WHERE user_id = $1 AND team_id IN (SELECT id FROM teams WHERE org_id = $2)")
            .bind(user.0)
            .bind(org.0)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM org_members WHERE org_id = $1 AND user_id = $2").bind(org.0).bind(user.0).execute(&mut *tx).await?;
        self.core.audit_tx(&mut tx, actor, client, action::ORG_MEMBER_REMOVED, Some(("org", org.0)), json!({"user_id": user.0})).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn teams(&self, actor: &Actor, org: OrgId) -> Result<Vec<Team>> {
        self.require_member(actor, org).await?;
        let rows: Vec<TeamRow> =
            sqlx::query_as(&format!("{TEAM_SELECT} WHERE t.org_id = $1 ORDER BY t.name")).bind(org.0).fetch_all(&self.core.db).await?;
        Ok(rows.into_iter().map(Team::from).collect())
    }

    async fn get_team(&self, actor: &Actor, org: OrgId, name: &str) -> Result<Team> {
        self.require_member(actor, org).await?;
        let row: TeamRow = sqlx::query_as(&format!("{TEAM_SELECT} WHERE t.org_id = $1 AND t.name = $2::citext"))
            .bind(org.0)
            .bind(name)
            .fetch_optional(&self.core.db)
            .await?
            .ok_or(Error::NotFound)?;
        Ok(row.into())
    }

    async fn create_team(
        &self,
        actor: &Actor,
        org: OrgId,
        name: &str,
        description: &str,
        permission: Permission,
        client: &ClientInfo,
    ) -> Result<Team> {
        self.require_owner(actor, org).await?;
        validate_team_name(name)?;
        if permission == Permission::None {
            return Err(Error::Validation(vec![FieldError::custom("Team", "permission", "Use read, triage, write, maintain or admin.")]));
        }
        let mut tx = self.core.db.begin().await?;
        let id: i64 = sqlx::query_scalar("INSERT INTO teams (org_id, name, description, permission) VALUES ($1, $2, $3, $4) RETURNING id")
            .bind(org.0)
            .bind(name)
            .bind(clip(description.trim(), 500))
            .bind(permission.as_str())
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| if is_unique_violation(&e, "") { crate::accounts::exists("Team", "name") } else { e.into() })?;
        self.core
            .audit_tx(
                &mut tx,
                actor,
                client,
                action::TEAM_CREATED,
                Some(("team", id)),
                json!({"org_id": org.0, "name": name, "permission": permission.as_str()}),
            )
            .await?;
        tx.commit().await?;
        self.get_team(actor, org, name).await
    }

    async fn set_team_permission(&self, actor: &Actor, team: TeamId, permission: Permission, client: &ClientInfo) -> Result<()> {
        let org = self.team_org(team).await?;
        self.require_owner(actor, org).await?;
        if permission == Permission::None {
            return Err(Error::Validation(vec![FieldError::custom("Team", "permission", "Use read, triage, write, maintain or admin.")]));
        }
        let mut tx = self.core.db.begin().await?;
        sqlx::query("UPDATE teams SET permission = $2 WHERE id = $1").bind(team.0).bind(permission.as_str()).execute(&mut *tx).await?;
        self.core
            .audit_tx(
                &mut tx,
                actor,
                client,
                action::TEAM_UPDATED,
                Some(("team", team.0)),
                json!({"org_id": org.0, "permission": permission.as_str()}),
            )
            .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn delete_team(&self, actor: &Actor, team: TeamId, client: &ClientInfo) -> Result<()> {
        let org = self.team_org(team).await?;
        self.require_owner(actor, org).await?;
        let mut tx = self.core.db.begin().await?;
        sqlx::query("DELETE FROM teams WHERE id = $1").bind(team.0).execute(&mut *tx).await?;
        self.core.audit_tx(&mut tx, actor, client, action::TEAM_DELETED, Some(("team", team.0)), json!({"org_id": org.0})).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn team_members(&self, actor: &Actor, team: TeamId) -> Result<Vec<OrgMember>> {
        let org = self.team_org(team).await?;
        self.require_member(actor, org).await?;
        let rows: Vec<(i64, String, String, Option<String>)> = sqlx::query_as(
            "SELECT u.id, u.username::text, u.display_name, (SELECT m.role FROM org_members m WHERE m.org_id = $2 AND m.user_id = u.id)
             FROM team_members tm JOIN users u ON u.id = tm.user_id WHERE tm.team_id = $1 ORDER BY u.username",
        )
        .bind(team.0)
        .bind(org.0)
        .fetch_all(&self.core.db)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(id, username, display_name, role)| OrgMember {
                user_id: UserId(id),
                username,
                display_name,
                role: role_of(role.as_deref().unwrap_or("member")),
            })
            .collect())
    }

    async fn add_team_member(&self, actor: &Actor, team: TeamId, user: UserId, client: &ClientInfo) -> Result<()> {
        let org = self.team_org(team).await?;
        self.require_owner(actor, org).await?;
        let mut tx = self.core.db.begin().await?;
        let is_member: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM org_members WHERE org_id = $1 AND user_id = $2)")
            .bind(org.0)
            .bind(user.0)
            .fetch_one(&mut *tx)
            .await?;
        if !is_member {
            return Err(Error::conflict("Add them to the organization first."));
        }
        sqlx::query("INSERT INTO team_members (team_id, user_id) VALUES ($1, $2) ON CONFLICT DO NOTHING")
            .bind(team.0)
            .bind(user.0)
            .execute(&mut *tx)
            .await?;
        self.core.audit_tx(&mut tx, actor, client, action::TEAM_MEMBER_ADDED, Some(("team", team.0)), json!({"user_id": user.0})).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn remove_team_member(&self, actor: &Actor, team: TeamId, user: UserId, client: &ClientInfo) -> Result<()> {
        let org = self.team_org(team).await?;
        self.require_owner(actor, org).await?;
        let mut tx = self.core.db.begin().await?;
        let n = sqlx::query("DELETE FROM team_members WHERE team_id = $1 AND user_id = $2")
            .bind(team.0)
            .bind(user.0)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if n == 0 {
            return Err(Error::NotFound);
        }
        self.core.audit_tx(&mut tx, actor, client, action::TEAM_MEMBER_REMOVED, Some(("team", team.0)), json!({"user_id": user.0})).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn team_repos(&self, actor: &Actor, team: TeamId) -> Result<Vec<TeamRepo>> {
        let org = self.team_org(team).await?;
        self.require_member(actor, org).await?;
        let rows: Vec<(i64, String)> = sqlx::query_as("SELECT repo_id, permission FROM team_repos WHERE team_id = $1 ORDER BY repo_id")
            .bind(team.0)
            .fetch_all(&self.core.db)
            .await?;
        Ok(rows
            .into_iter()
            .map(|(r, p)| TeamRepo { repo_id: RepoId(r), permission: Permission::parse(&p).unwrap_or(Permission::Read) })
            .collect())
    }

    async fn set_team_repo(&self, actor: &Actor, team: TeamId, repo: RepoId, permission: Permission, client: &ClientInfo) -> Result<()> {
        let org = self.team_org(team).await?;
        if permission == Permission::None {
            return Err(Error::Validation(vec![FieldError::custom("Team", "permission", "Use remove to take access away.")]));
        }
        // Admin on the repository, which an org owner has on every repo of the org. This changes a
        // team, so a token also needs admin:org.
        require_admin_org_scope(actor)?;
        self.authz.require(actor, repo, Permission::Admin).await?;
        let owner: Option<i64> =
            sqlx::query_scalar("SELECT owner_id FROM repositories WHERE id = $1").bind(repo.0).fetch_optional(&self.core.db).await?;
        if owner != Some(org.0) {
            return Err(Error::Validation(vec![FieldError::custom(
                "Team",
                "repository",
                "A team can only be given repositories of its own organization.",
            )]));
        }
        let mut tx = self.core.db.begin().await?;
        sqlx::query("INSERT INTO team_repos (team_id, repo_id, permission) VALUES ($1, $2, $3) ON CONFLICT (team_id, repo_id) DO UPDATE SET permission = EXCLUDED.permission")
            .bind(team.0)
            .bind(repo.0)
            .bind(permission.as_str())
            .execute(&mut *tx)
            .await?;
        self.core
            .audit_tx(
                &mut tx,
                actor,
                client,
                action::TEAM_REPO_SET,
                Some(("team", team.0)),
                json!({"repo_id": repo.0, "permission": permission.as_str()}),
            )
            .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn remove_team_repo(&self, actor: &Actor, team: TeamId, repo: RepoId, client: &ClientInfo) -> Result<()> {
        self.team_org(team).await?;
        require_admin_org_scope(actor)?;
        self.authz.require(actor, repo, Permission::Admin).await?;
        let mut tx = self.core.db.begin().await?;
        let n = sqlx::query("DELETE FROM team_repos WHERE team_id = $1 AND repo_id = $2")
            .bind(team.0)
            .bind(repo.0)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if n == 0 {
            return Err(Error::NotFound);
        }
        self.core.audit_tx(&mut tx, actor, client, action::TEAM_REPO_REMOVED, Some(("team", team.0)), json!({"repo_id": repo.0})).await?;
        tx.commit().await?;
        Ok(())
    }
}
