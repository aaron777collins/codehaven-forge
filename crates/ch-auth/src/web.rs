//! What the web pages call for identity: `ch_core::web_contract::{Sessions, People}`.
//!
//! The pages want one call per form. The identity traits (`Credentials`, `TwoFactor`,
//! `Directory`, `Orgs`) are the real contract, and carry the rules: lockout, the same refusal
//! for every bad sign-in, new cookies after a change of privilege. These two structs translate
//! and add nothing, apart from the plain reads the people pages need.

use crate::core::{Core, USER_COLS, UserRow, load_user};
use async_trait::async_trait;
use ch_core::audit::{AuditEvent, Outcome, StoredAudit};
use ch_core::identity::{Profile, SessionGrant, SignInStep};
use ch_core::model::{ClientInfo, User};
use ch_core::services::{Accounts, Credentials, Directory, Orgs, TwoFactor};
use ch_core::web_contract::{
    LinkPeek, Member, NewSession, PasskeyOn, People, SessionInfo, Sessions, SignInOutcome, SiteStats, Team, TwoFactorOn, TwoFactorSetup,
    UpdateProfile,
};
use ch_core::{Actor, AuditId, Error, OrgId, Result, SessionId, TeamId, UserId};
use std::net::IpAddr;
use std::sync::Arc;

fn new_session(g: SessionGrant) -> NewSession {
    NewSession { cookie_value: g.cookie, max_age_secs: g.max_age_secs.max(0) as u64 }
}

pub struct SessionsSvc {
    pub core: Arc<Core>,
    pub credentials: Arc<dyn Credentials>,
    pub twofactor: Arc<dyn TwoFactor>,
    pub directory: Arc<dyn Directory>,
    pub accounts: Arc<dyn Accounts>,
}

#[async_trait]
impl Sessions for SessionsSvc {
    async fn sign_in(&self, username: &str, password: &str, client: &ClientInfo) -> Result<SignInOutcome> {
        match self.credentials.sign_in(username, password, client).await {
            Ok(s) => Ok(match s.step {
                SignInStep::Complete => SignInOutcome::Session(new_session(s.grant)),
                SignInStep::TwoFactorRequired => SignInOutcome::TwoFactorRequired { pending: s.grant.cookie },
            }),
            // Every bad sign-in looks the same. A lockout is the one thing that says more.
            Err(Error::Unauthorized(_)) => Ok(SignInOutcome::Rejected),
            Err(e) => Err(e),
        }
    }

    async fn complete_two_factor(&self, pending: &str, code: &str, client: &ClientInfo) -> Result<Option<NewSession>> {
        match self.credentials.complete_two_factor(pending, code, client).await {
            Ok(s) => Ok(Some(new_session(s.grant))),
            Err(Error::Unauthorized(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn pending_factors(&self, pending: &str) -> Result<Option<ch_core::identity::PendingFactors>> {
        match self.credentials.pending_factors(pending).await {
            Ok(f) => Ok(Some(f)),
            Err(Error::Unauthorized(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn begin_passkey_sign_in(&self, pending: &str, client: &ClientInfo) -> Result<String> {
        self.credentials.begin_passkey_sign_in(pending, client).await
    }

    async fn complete_passkey_sign_in(&self, pending: &str, credential: &str, client: &ClientInfo) -> Result<Option<NewSession>> {
        match self.credentials.complete_passkey_sign_in(pending, credential, client).await {
            Ok(s) => Ok(Some(new_session(s.grant))),
            Err(Error::Unauthorized(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn sign_out(&self, cookie_value: &str, client: &ClientInfo) -> Result<()> {
        self.credentials.sign_out(cookie_value, client).await
    }

    async fn sessions(&self, actor: &Actor, current_cookie: &str) -> Result<Vec<SessionInfo>> {
        Ok(self
            .credentials
            .sessions(actor, current_cookie)
            .await?
            .into_iter()
            .map(|s| SessionInfo {
                id: s.id,
                ip: s.ip.unwrap_or_default(),
                user_agent: s.user_agent,
                created_at: s.created_at,
                last_seen_at: s.last_seen_at,
                current: s.current,
            })
            .collect())
    }

    async fn revoke_session(&self, actor: &Actor, id: SessionId, client: &ClientInfo) -> Result<()> {
        self.credentials.revoke_session(actor, id, client).await
    }

    async fn security_log(&self, actor: &Actor, limit: i64) -> Result<Vec<StoredAudit>> {
        let records = self.credentials.sign_in_history(actor, limit.clamp(1, 200) as u32).await?;
        let (kind, id) = match actor {
            Actor::User { id, .. } => ("user", Some(id.0)),
            Actor::Token { user_id, .. } => ("token", Some(user_id.0)),
            _ => ("anonymous", None),
        };
        Ok(records
            .into_iter()
            .enumerate()
            .map(|(i, r)| StoredAudit {
                id: AuditId(i as i64),
                at: r.at,
                event: AuditEvent {
                    actor_kind: kind.into(),
                    actor_id: id,
                    actor_label: actor.audit_label(),
                    client_ip: r.ip.and_then(|ip| ip.parse::<IpAddr>().ok()),
                    user_agent: r.user_agent,
                    action: r.action,
                    target_type: Some("user".into()),
                    target_id: id.map(|i| i.to_string()),
                    outcome: if r.success { Outcome::Success } else { Outcome::Failure },
                    details: serde_json::json!({}),
                },
            })
            .collect())
    }

    async fn begin_two_factor_setup(&self, actor: &Actor, _client: &ClientInfo) -> Result<TwoFactorSetup> {
        let s = self.twofactor.begin_setup(actor).await?;
        Ok(TwoFactorSetup { secret: s.secret, otpauth_url: s.otpauth_uri })
    }

    async fn confirm_two_factor_setup(&self, actor: &Actor, current_cookie: &str, code: &str, client: &ClientInfo) -> Result<TwoFactorOn> {
        let on = self.twofactor.confirm_setup(actor, current_cookie, code, client).await?;
        Ok(TwoFactorOn { recovery_codes: on.recovery_codes, session: new_session(on.grant) })
    }

    async fn disable_two_factor(
        &self,
        actor: &Actor,
        current_cookie: &str,
        password: &str,
        code: &str,
        client: &ClientInfo,
    ) -> Result<NewSession> {
        Ok(new_session(self.twofactor.disable(actor, current_cookie, password, code, client).await?))
    }

    async fn regenerate_recovery_codes(&self, actor: &Actor, password: &str, client: &ClientInfo) -> Result<Vec<String>> {
        self.twofactor.regenerate_recovery_codes(actor, password, client).await
    }

    async fn two_factor_status(&self, actor: &Actor) -> Result<ch_core::identity::TwoFactorStatus> {
        self.twofactor.status(actor).await
    }

    async fn passkeys_available(&self) -> bool {
        self.twofactor.passkeys_available().await
    }

    async fn passkeys(&self, actor: &Actor) -> Result<Vec<ch_core::identity::PasskeyInfo>> {
        self.twofactor.passkeys(actor).await
    }

    async fn begin_passkey_registration(&self, actor: &Actor, client: &ClientInfo) -> Result<String> {
        self.twofactor.begin_passkey(actor, client).await
    }

    async fn finish_passkey_registration(
        &self,
        actor: &Actor,
        current_cookie: &str,
        name: &str,
        credential: &str,
        client: &ClientInfo,
    ) -> Result<PasskeyOn> {
        let added = self.twofactor.finish_passkey(actor, current_cookie, name, credential, client).await?;
        Ok(PasskeyOn { passkey: added.passkey, recovery_codes: added.recovery_codes, session: added.grant.map(new_session) })
    }

    async fn remove_passkey(
        &self,
        actor: &Actor,
        current_cookie: &str,
        id: i64,
        password: &str,
        code: &str,
        client: &ClientInfo,
    ) -> Result<Option<NewSession>> {
        Ok(self.twofactor.remove_passkey(actor, current_cookie, id, password, code, client).await?.map(new_session))
    }

    async fn peek_link(&self, token: &str) -> Result<LinkPeek> {
        let p = self.credentials.peek_link(token).await?;
        Ok(LinkPeek { username: p.username, purpose: p.purpose })
    }

    async fn redeem_link(&self, token: &str, new_password: &str, client: &ClientInfo) -> Result<()> {
        self.credentials.redeem_link(token, new_password, client).await.map(|_| ())
    }

    async fn change_password(
        &self,
        actor: &Actor,
        current_cookie: &str,
        current: &str,
        new: &str,
        client: &ClientInfo,
    ) -> Result<NewSession> {
        Ok(new_session(self.credentials.change_password(actor, current_cookie, current, new, client).await?))
    }

    async fn update_profile(&self, actor: &Actor, update: UpdateProfile, client: &ClientInfo) -> Result<User> {
        let id = actor.user_id().ok_or_else(Error::unauthorized)?;
        if let Some(email) = update.email.as_deref().map(str::trim).filter(|e| !e.is_empty()) {
            let me = self.accounts.get_user(id).await?;
            if me.email.as_deref().map(|e| !e.eq_ignore_ascii_case(email)).unwrap_or(true) {
                // A new primary address has to be verified by mail, and this server has none.
                return Err(Error::invalid(
                    "User",
                    "email",
                    "This server cannot send mail yet, so it cannot verify a new address. Ask an admin to change it.",
                ));
            }
        }
        self.directory.update_profile(actor, id, Profile { display_name: update.display_name, description: None }, client).await
    }
}

pub struct PeopleSvc {
    pub core: Arc<Core>,
    pub orgs: Arc<dyn Orgs>,
}

impl PeopleSvc {
    fn signed_in(actor: &Actor) -> Result<()> {
        if actor.is_anonymous() { Err(Error::unauthorized()) } else { Ok(()) }
    }

    async fn as_user(&self, id: UserId) -> Result<User> {
        Ok(load_user(&self.core.db, id).await?.into())
    }
}

#[async_trait]
impl People for PeopleSvc {
    async fn list_users(&self, actor: &Actor, query: Option<&str>, limit: u32) -> Result<Vec<User>> {
        Self::signed_in(actor)?;
        let like = query
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .map(|q| format!("%{}%", q.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")));
        // Suspended accounts are for site admins only.
        let hide_suspended = !actor.is_site_admin();
        let rows: Vec<UserRow> = sqlx::query_as(&format!(
            "SELECT {USER_COLS} FROM users u WHERE u.kind = 'user' AND ($2 = false OR u.suspended_at IS NULL)
               AND ($1::text IS NULL OR u.username::text ILIKE $1 OR u.display_name ILIKE $1)
             ORDER BY u.username LIMIT $3"
        ))
        .bind(like)
        .bind(hide_suspended)
        .bind(i64::from(limit.clamp(1, 200)))
        .fetch_all(&self.core.db)
        .await?;
        Ok(rows.into_iter().map(User::from).collect())
    }

    async fn orgs_of(&self, actor: &Actor, user: UserId) -> Result<Vec<User>> {
        let mut out = Vec::new();
        for org in self.orgs.list_for_user(actor, user).await? {
            out.push(self.as_user(UserId(org.id.0)).await?);
        }
        Ok(out)
    }

    async fn org_members(&self, actor: &Actor, org: OrgId) -> Result<Vec<Member>> {
        let mut out = Vec::new();
        for m in self.orgs.members(actor, org).await? {
            out.push(Member { user: self.as_user(m.user_id).await?, role: m.role });
        }
        Ok(out)
    }

    async fn teams(&self, actor: &Actor, org: OrgId) -> Result<Vec<Team>> {
        let teams = self.orgs.teams(actor, org).await?;
        let mut out = Vec::with_capacity(teams.len());
        for t in teams {
            out.push(ch_core::web_contract::Team {
                id: t.id,
                org_id: t.org_id,
                name: t.name,
                description: t.description,
                permission: t.permission,
                members: t.member_count,
            });
        }
        Ok(out)
    }

    async fn team_members(&self, actor: &Actor, team: TeamId) -> Result<Vec<User>> {
        let mut out = Vec::new();
        for m in self.orgs.team_members(actor, team).await? {
            out.push(self.as_user(m.user_id).await?);
        }
        Ok(out)
    }

    async fn create_org(&self, actor: &Actor, name: &str, display_name: &str, client: &ClientInfo) -> Result<User> {
        let org = self.orgs.create(actor, name, "", client).await?;
        if !display_name.trim().is_empty() {
            self.orgs.update(actor, org.id, Some(display_name.trim().to_string()), None, None, client).await?;
        }
        self.as_user(UserId(org.id.0)).await
    }

    async fn site_stats(&self, actor: &Actor) -> Result<SiteStats> {
        crate::core::require_site_admin(actor)?;
        let db = &self.core.db;
        let users: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE kind = 'user'").fetch_one(db).await?;
        let orgs: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE kind = 'org'").fetch_one(db).await?;
        let repos: i64 = sqlx::query_scalar("SELECT count(*) FROM repositories").fetch_one(db).await?;
        let runners_online: i64 =
            sqlx::query_scalar("SELECT count(*) FROM runners WHERE revoked_at IS NULL AND last_seen_at > now() - interval '90 seconds'")
                .fetch_one(db)
                .await?;
        let queued_jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM workflow_jobs WHERE status = 'queued'").fetch_one(db).await?;
        let (total, free) = match rustix::fs::statvfs(&self.core.config.storage.data_dir) {
            Ok(st) => (st.f_blocks.saturating_mul(st.f_frsize), st.f_bavail.saturating_mul(st.f_frsize)),
            Err(_) => (0, 0),
        };
        Ok(SiteStats {
            users,
            orgs,
            repos,
            runners_online,
            queued_jobs,
            data_dir_total_bytes: total,
            data_dir_free_bytes: free,
            database_ok: true,
            version: env!("CARGO_PKG_VERSION").to_string(),
        })
    }
}
