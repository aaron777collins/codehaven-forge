//! `Invites`: the only way an account other than the first comes to exist
//! (ARCHITECTURE.md section 7.4). There is no sign-up route and no setting to open one.
//!
//! - The link is `chi_` plus 32 base62 characters. Only its SHA-256 is stored.
//! - Accepting is one `UPDATE ... WHERE used_at IS NULL AND revoked_at IS NULL AND
//!   expires_at > now() RETURNING`, in the same transaction that creates the account, so two
//!   people racing one link get one account between them.
//! - Expired, used, revoked and made-up tokens are all `NotFound`.

use crate::accounts::exists;
use crate::core::*;
use crate::util::*;
use async_trait::async_trait;
use ch_core::Actor;
use ch_core::audit::{Outcome, action};
use ch_core::error::{Error, FieldError, Result};
use ch_core::events::DomainEvent;
use ch_core::ids::*;
use ch_core::model::*;
use ch_core::services::Invites;
use ch_db::tokens;
use chrono::{DateTime, Duration, Utc};
use serde_json::json;
use std::sync::Arc;

pub struct InvitesService {
    pub(crate) core: Arc<Core>,
}

#[derive(sqlx::FromRow)]
struct InviteRow {
    id: i64,
    created_by: Option<i64>,
    email: Option<String>,
    grants_admin: bool,
    grants_ci: bool,
    org_id: Option<i64>,
    team_id: Option<i64>,
    note: Option<String>,
    expires_at: DateTime<Utc>,
    used_at: Option<DateTime<Utc>>,
    used_by: Option<i64>,
    revoked_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
}

const INVITE_COLS: &str = "id, created_by, email::text AS email, grants_admin, grants_ci, org_id, team_id, note, expires_at, used_at, used_by, revoked_at, created_at";

impl From<InviteRow> for Invite {
    fn from(r: InviteRow) -> Invite {
        Invite {
            id: InviteId(r.id),
            created_by: r.created_by.map(UserId),
            email: r.email,
            grants_admin: r.grants_admin,
            grants_ci: r.grants_ci,
            org_id: r.org_id.map(OrgId),
            team_id: r.team_id.map(TeamId),
            note: r.note,
            expires_at: r.expires_at,
            used_at: r.used_at,
            used_by: r.used_by.map(UserId),
            revoked_at: r.revoked_at,
            created_at: r.created_at,
        }
    }
}

impl InvitesService {
    pub fn new(core: Arc<Core>) -> Self {
        Self { core }
    }

    /// Site admins and the CLI may invite anyone. Org owners may invite people into their own
    /// org when the site admin turned that on, and never grant admin or CI.
    async fn authorize_create(&self, actor: &Actor, req: &NewInvite) -> Result<()> {
        if actor.is_site_admin() {
            return Ok(());
        }
        if actor.is_admin_without_2fa() {
            return Err(Error::forbidden("Turn on two-factor sign-in before using the admin pages."));
        }
        let (Some(user), Some(org)) = (actor.user_id(), req.org_id) else {
            return Err(if actor.is_anonymous() { Error::unauthorized() } else { Error::forbidden("Only site admins can invite people.") });
        };
        if !self.core.config.invites.org_owners_can_invite {
            return Err(Error::forbidden("Only site admins can invite people."));
        }
        if !matches!(actor, Actor::User { .. }) {
            return Err(Error::forbidden("Only site admins can invite people."));
        }
        let role: Option<String> = sqlx::query_scalar("SELECT role FROM org_members WHERE org_id = $1 AND user_id = $2")
            .bind(org.0)
            .bind(user.0)
            .fetch_optional(&self.core.db)
            .await?;
        if role.as_deref() != Some("owner") {
            return Err(Error::forbidden("Only site admins can invite people."));
        }
        if req.grants_admin || req.grants_ci {
            return Err(Error::forbidden("Only site admins can grant admin or CI access."));
        }
        Ok(())
    }

    async fn pending_exists(&self, token: &str) -> Result<bool> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM invites WHERE token_hash = $1 AND used_at IS NULL AND revoked_at IS NULL AND expires_at > now())",
        )
        .bind(tokens::hash_token(token))
        .fetch_one(&self.core.db)
        .await?)
    }
}

/// True when the text has the shape of an invite token. Anything else is not even looked up.
pub fn token_shape_ok(token: &str) -> bool {
    token.len() == 36 && token.starts_with("chi_") && token[4..].bytes().all(|b| b.is_ascii_alphanumeric())
}

#[async_trait]
impl Invites for InvitesService {
    async fn create(&self, actor: &Actor, req: NewInvite, client: &ClientInfo) -> Result<CreatedInvite> {
        self.authorize_create(actor, &req).await?;
        let cfg = &self.core.config.invites;
        let hours = req.expires_in_hours.unwrap_or(cfg.default_expiry_hours);
        if hours < cfg.min_expiry_hours || hours > cfg.max_expiry_hours {
            return Err(Error::Validation(vec![FieldError::custom(
                "Invite",
                "expires_in_hours",
                format!("An invite can last from {} hour(s) to {} hours.", cfg.min_expiry_hours, cfg.max_expiry_hours),
            )]));
        }
        let email = match req.email.as_deref().map(str::trim).filter(|e| !e.is_empty()) {
            Some(e) => {
                validate_email(e)?;
                Some(e.to_string())
            }
            None => None,
        };
        let note = req.note.as_deref().map(str::trim).filter(|n| !n.is_empty()).map(|n| clip(n, 500));
        if let Some(org) = req.org_id {
            let kind: Option<String> =
                sqlx::query_scalar("SELECT kind FROM users WHERE id = $1").bind(org.0).fetch_optional(&self.core.db).await?;
            if kind.as_deref() != Some("org") {
                return Err(Error::Validation(vec![FieldError::custom("Invite", "org", "That organization does not exist.")]));
            }
        }
        if let Some(team) = req.team_id {
            let ok: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM teams WHERE id = $1 AND org_id IS NOT DISTINCT FROM $2)")
                .bind(team.0)
                .bind(req.org_id.map(|o| o.0))
                .fetch_one(&self.core.db)
                .await?;
            if !ok {
                return Err(Error::Validation(vec![FieldError::custom("Invite", "team", "That team is not in that organization.")]));
            }
        }
        let token = tokens::new_token("chi_", 32);
        let expires_at = Utc::now() + Duration::hours(hours as i64);
        let via = if actor.is_system() { "cli" } else { "web" };
        let mut tx = self.core.db.begin().await?;
        let row: InviteRow = sqlx::query_as(&format!(
            "INSERT INTO invites (token_hash, created_by, created_via, email, grants_admin, grants_ci, org_id, team_id, note, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) RETURNING {INVITE_COLS}"
        ))
        .bind(tokens::hash_token(&token))
        .bind(actor.user_id().map(|u| u.0))
        .bind(via)
        .bind(&email)
        .bind(req.grants_admin)
        .bind(req.grants_ci)
        .bind(req.org_id.map(|o| o.0))
        .bind(req.team_id.map(|t| t.0))
        .bind(&note)
        .bind(expires_at)
        .fetch_one(&mut *tx)
        .await?;
        self.core
            .audit_tx(
                &mut tx,
                actor,
                client,
                action::INVITE_CREATED,
                Some(("invite", row.id)),
                json!({"grants_admin": req.grants_admin, "grants_ci": req.grants_ci, "email": email, "org_id": req.org_id, "expires_at": expires_at}),
            )
            .await?;
        tx.commit().await?;
        let url = format!("{}/invite/{token}", self.core.external_url());
        Ok(CreatedInvite { invite: row.into(), token, url })
    }

    async fn list(&self, actor: &Actor) -> Result<Vec<Invite>> {
        let rows: Vec<InviteRow> = if actor.is_site_admin() {
            sqlx::query_as(&format!("SELECT {INVITE_COLS} FROM invites ORDER BY created_at DESC, id DESC LIMIT 500"))
                .fetch_all(&self.core.db)
                .await?
        } else if let (Actor::User { id, .. }, true) = (actor, self.core.config.invites.org_owners_can_invite) {
            sqlx::query_as(&format!("SELECT {INVITE_COLS} FROM invites WHERE created_by = $1 ORDER BY created_at DESC, id DESC LIMIT 500"))
                .bind(id.0)
                .fetch_all(&self.core.db)
                .await?
        } else {
            return require_site_admin(actor).map(|_| Vec::new());
        };
        Ok(rows.into_iter().map(Invite::from).collect())
    }

    async fn revoke(&self, actor: &Actor, id: InviteId, client: &ClientInfo) -> Result<()> {
        let created_by: Option<Option<i64>> =
            sqlx::query_scalar("SELECT created_by FROM invites WHERE id = $1").bind(id.0).fetch_optional(&self.core.db).await?;
        let Some(created_by) = created_by else { return Err(Error::NotFound) };
        if !actor.is_site_admin()
            && !(created_by.is_some() && created_by == actor.user_id().map(|u| u.0) && self.core.config.invites.org_owners_can_invite)
        {
            return require_site_admin(actor);
        }
        let mut tx = self.core.db.begin().await?;
        let n = sqlx::query("UPDATE invites SET revoked_at = now() WHERE id = $1 AND used_at IS NULL AND revoked_at IS NULL")
            .bind(id.0)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if n == 0 {
            return Err(Error::conflict("That invite was already used or revoked."));
        }
        self.core.audit_tx(&mut tx, actor, client, action::INVITE_REVOKED, Some(("invite", id.0)), json!({})).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn peek(&self, token: &str) -> Result<InvitePeek> {
        if !token_shape_ok(token) {
            return Err(Error::NotFound);
        }
        let row: Option<(Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT u.username::text, i.email::text FROM invites i LEFT JOIN users u ON u.id = i.created_by
             WHERE i.token_hash = $1 AND i.used_at IS NULL AND i.revoked_at IS NULL AND i.expires_at > now()",
        )
        .bind(tokens::hash_token(token))
        .fetch_optional(&self.core.db)
        .await?;
        let (invited_by, email) = row.ok_or(Error::NotFound)?;
        Ok(InvitePeek { invited_by, email })
    }

    async fn accept(&self, token: &str, account: NewAccount, client: &ClientInfo) -> Result<User> {
        let refuse = |why: &'static str| async move {
            self.core
                .audit_standalone(&Actor::Anonymous, client, action::INVITE_REFUSED, Outcome::Failure, None, json!({"why": why}))
                .await;
            Error::NotFound
        };
        if !token_shape_ok(token) {
            return Err(refuse("malformed").await);
        }
        if !self.pending_exists(token).await? {
            return Err(refuse("not_pending").await);
        }
        validate_username(&account.username, "User")?;
        let email = normalize_email(&account.email);
        validate_email(&email)?;
        let display_name = account.display_name.as_deref().map(str::trim).map(|d| clip(d, 100)).unwrap_or_default();
        crate::password::check_policy(&account.username, &email, &account.password)?;
        // Hash before the claim: it is slow, and the claim should be quick. A failure
        // after this point rolls the claim back, so a typo never burns the invite.
        let hash = self.core.hasher.hash(&account.password).await?;

        let mut tx = self.core.db.begin().await?;
        let claimed: Option<InviteRow> = sqlx::query_as(&format!(
            "UPDATE invites SET used_at = now() WHERE token_hash = $1 AND used_at IS NULL AND revoked_at IS NULL AND expires_at > now() RETURNING {INVITE_COLS}"
        ))
        .bind(tokens::hash_token(token))
        .fetch_optional(&mut *tx)
        .await?;
        let Some(inv) = claimed else {
            drop(tx);
            return Err(refuse("lost_race_or_expired").await);
        };
        if let Some(want) = &inv.email
            && !want.eq_ignore_ascii_case(&email)
        {
            // Dropping the transaction gives the invite back.
            return Err(Error::Validation(vec![FieldError::custom("User", "email", "This invite is for a different email address.")]));
        }
        let user_id: i64 = sqlx::query_scalar("INSERT INTO users (username, display_name, password_hash, is_admin, can_run_ci, invited_by) VALUES ($1, $2, $3, $4, $5, $6) RETURNING id")
            .bind(&account.username)
            .bind(&display_name)
            .bind(&hash)
            .bind(inv.grants_admin)
            .bind(inv.grants_ci)
            .bind(inv.created_by)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| if is_unique_violation(&e, "") { exists("User", "username") } else { e.into() })?;
        sqlx::query("INSERT INTO user_emails (user_id, email, is_primary, verified_at) VALUES ($1, $2, true, CASE WHEN $3 THEN now() END)")
            .bind(user_id)
            .bind(&email)
            .bind(inv.email.is_some())
            .execute(&mut *tx)
            .await
            .map_err(|e| if is_unique_violation(&e, "") { exists("Email", "email") } else { e.into() })?;
        sqlx::query("UPDATE invites SET used_by = $2 WHERE id = $1").bind(inv.id).bind(user_id).execute(&mut *tx).await?;
        if let Some(org) = inv.org_id {
            sqlx::query("INSERT INTO org_members (org_id, user_id, role) VALUES ($1, $2, 'member') ON CONFLICT DO NOTHING")
                .bind(org)
                .bind(user_id)
                .execute(&mut *tx)
                .await?;
            if let Some(team) = inv.team_id {
                sqlx::query("INSERT INTO team_members (team_id, user_id) VALUES ($1, $2) ON CONFLICT DO NOTHING")
                    .bind(team)
                    .bind(user_id)
                    .execute(&mut *tx)
                    .await?;
            }
            self.core
                .events
                .emit(
                    &mut tx,
                    DomainEvent::MemberAdded {
                        repo_id: None,
                        org_id: Some(OrgId(org)),
                        user_id: UserId(user_id),
                        actor: inv.created_by.map(UserId),
                    },
                )
                .await?;
        }
        let who = Actor::User { id: UserId(user_id), is_admin: inv.grants_admin, has_2fa: false, via: ch_core::AuthVia::Session };
        self.core
            .audit_tx(
                &mut tx,
                &who,
                client,
                action::INVITE_ACCEPTED,
                Some(("invite", inv.id)),
                json!({"user_id": user_id, "username": account.username}),
            )
            .await?;
        self.core
            .audit_tx(
                &mut tx,
                &who,
                client,
                action::USER_CREATED,
                Some(("user", user_id)),
                json!({"username": account.username, "via": "invite", "invited_by": inv.created_by}),
            )
            .await?;
        if inv.grants_admin {
            self.core
                .audit_tx(
                    &mut tx,
                    &who,
                    client,
                    action::USER_ADMIN_GRANTED,
                    Some(("user", user_id)),
                    json!({"value": true, "via": "invite"}),
                )
                .await?;
        }
        tx.commit().await?;
        Ok(load_user(&self.core.db, UserId(user_id)).await?.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_shape() {
        assert!(token_shape_ok(&format!("chi_{}", "a".repeat(32))));
        assert!(!token_shape_ok("chi_short"));
        assert!(!token_shape_ok(&format!("chp_{}", "a".repeat(32))));
        assert!(!token_shape_ok(&format!("chi_{}!", "a".repeat(31))));
        assert!(!token_shape_ok(""));
    }
}
