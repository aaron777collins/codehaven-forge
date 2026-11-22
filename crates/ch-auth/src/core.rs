//! State shared by every identity service: the pool, the audit log, the hasher, the master
//! key, the lockout table and the failed-auth limiter. Also row types and session helpers.

use crate::lockout::{self, Attempt, Locked, Lockout};
use crate::password::{HashParams, PasswordHasher};
use crate::ratelimit::Limiter;
use ch_core::audit::{AuditEvent, Outcome};
use ch_core::error::{Error, Result};
use ch_core::identity::SessionGrant;
use ch_core::ids::*;
use ch_core::model::{ClientInfo, User, UserKind};
use ch_core::services::{Audit, DbTx, EventBus};
use ch_core::{Actor, AuthVia, Config};
use ch_db::sealing::MasterKey;
use ch_db::tokens;
use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;
use std::sync::Arc;

pub const SESSION_IDLE_DAYS: i64 = 7;
pub const SESSION_ABSOLUTE_DAYS: i64 = 30;
/// A half signed-in session (password right, second factor pending) lives this long.
pub const PENDING_MINUTES: i64 = 10;

/// What the tests and the binary can change. The binary always uses `AuthOptions::default()`.
pub struct AuthOptions {
    pub hash_params: HashParams,
    /// `None` loads or creates `data/secrets/master.key`.
    pub master_key: Option<MasterKey>,
    /// Fetches OAuth client id documents. `None` uses the real https fetcher; tests inject one.
    pub client_fetcher: Option<Arc<dyn crate::apps::cimd::ClientDocFetcher>>,
}

impl Default for AuthOptions {
    fn default() -> Self {
        Self { hash_params: HashParams::PRODUCTION, master_key: None, client_fetcher: None }
    }
}

pub struct Core {
    pub db: PgPool,
    pub audit: Arc<dyn Audit>,
    pub events: Arc<dyn EventBus>,
    pub config: Arc<Config>,
    pub hasher: PasswordHasher,
    pub key: MasterKey,
    pub lockout: Lockout,
    /// Failed Basic and bearer attempts per address: 30 a minute.
    pub failed_auth: Limiter,
}

impl Core {
    pub fn new(db: PgPool, audit: Arc<dyn Audit>, events: Arc<dyn EventBus>, config: Arc<Config>, opts: AuthOptions) -> Result<Self> {
        let key = match opts.master_key {
            Some(k) => k,
            None => MasterKey::load_or_create(&config.master_key_path())?,
        };
        Ok(Self {
            lockout: Lockout::new(db.clone()),
            hasher: PasswordHasher::new(opts.hash_params)?,
            failed_auth: Limiter::new(30, 30),
            db,
            audit,
            events,
            config,
            key,
        })
    }

    /// Records an event that has no surrounding change, such as a failed sign-in. A failure
    /// to write the audit row is logged and never changes the outcome of the request.
    pub async fn audit_standalone(
        &self,
        actor: &Actor,
        client: &ClientInfo,
        action: &str,
        outcome: Outcome,
        target: Option<(&str, i64)>,
        details: serde_json::Value,
    ) {
        let mut ev = AuditEvent::new(actor, client, action, outcome).details(details);
        if let Some((kind, id)) = target {
            ev = ev.target(kind, id);
        }
        if let Err(e) = self.audit.record_standalone(ev).await {
            tracing::error!(error = %e, action, "audit write failed");
        }
    }

    /// Records in the caller's transaction.
    pub async fn audit_tx(
        &self,
        tx: &mut DbTx<'_>,
        actor: &Actor,
        client: &ClientInfo,
        action: &str,
        target: Option<(&str, i64)>,
        details: serde_json::Value,
    ) -> Result<()> {
        let mut ev = AuditEvent::new(actor, client, action, Outcome::Success).details(details);
        if let Some((kind, id)) = target {
            ev = ev.target(kind, id);
        }
        self.audit.record(tx, ev).await
    }

    pub fn external_url(&self) -> &str {
        self.config.server.external_url.trim_end_matches('/')
    }
}

// ---------------------------------------------------------------- lockout reservation

/// One password attempt reserved against the account and the address budgets, before the
/// password is checked.
pub struct Reservation {
    pub acct_key: String,
    pub ip_key: String,
    /// The lock this attempt itself created for the account key, if it was the threshold-th.
    pub acct_locked_now: Option<Locked>,
    /// The lock this attempt itself created for the address key, if it was the threshold-th.
    pub ip_locked_now: Option<Locked>,
}

impl Reservation {
    /// The locks this attempt created, for the audit trail.
    pub fn locks(&self) -> Vec<(&str, Locked)> {
        let mut locks = Vec::new();
        if let Some(l) = self.acct_locked_now {
            locks.push((self.acct_key.as_str(), l));
        }
        if let Some(l) = self.ip_locked_now {
            locks.push((self.ip_key.as_str(), l));
        }
        locks
    }
}

/// Reserves one sign-in attempt for the address key and then the account key, atomically per
/// key, BEFORE the password is checked. However many guesses run at once, at most
/// `ACCOUNT_THRESHOLD` per account and `ADDRESS_THRESHOLD` per address ever reach the
/// password hasher. `Err(RateLimited)` when either key is locked. The address goes first, so
/// an attacker on a locked address is refused without touching the account key and cannot
/// keep burning the victim account's budget. When the account key is the locked one, the
/// address reservation is given back, so a locked account does not eat the address budget of
/// the people behind the same address.
pub async fn reserve_login(core: &Core, acct_key: &str, ip_key: &str) -> Result<Reservation> {
    let ip_locked_now = match core.lockout.begin_attempt(ip_key, lockout::ADDRESS_THRESHOLD).await? {
        Attempt::Allowed { locked_now } => locked_now,
        Attempt::Locked(l) => return Err(Error::RateLimited { retry_after_secs: l.retry_after_secs }),
    };
    let acct_locked_now = match core.lockout.begin_attempt(acct_key, lockout::ACCOUNT_THRESHOLD).await? {
        Attempt::Allowed { locked_now } => locked_now,
        Attempt::Locked(l) => {
            // The address reservation is given back: a locked account must not eat the address
            // budget of the people behind it.
            let _ = core.lockout.release(ip_key).await;
            return Err(Error::RateLimited { retry_after_secs: l.retry_after_secs });
        }
    };
    Ok(Reservation { acct_key: acct_key.into(), ip_key: ip_key.into(), acct_locked_now, ip_locked_now })
}

pub const USER_COLS: &str = "u.id, u.kind, u.username::text AS username, u.display_name, u.is_admin, u.can_run_ci, u.suspended_at, \
     u.totp_enabled_at, (u.password_hash IS NOT NULL) AS has_password, u.invited_by, u.created_at, \
     (SELECT e.email::text FROM user_emails e WHERE e.user_id = u.id ORDER BY e.is_primary DESC, e.id LIMIT 1) AS email";

#[derive(sqlx::FromRow)]
pub struct UserRow {
    pub id: i64,
    pub kind: String,
    pub username: String,
    pub display_name: String,
    pub is_admin: bool,
    pub can_run_ci: bool,
    pub suspended_at: Option<DateTime<Utc>>,
    pub totp_enabled_at: Option<DateTime<Utc>>,
    pub has_password: bool,
    pub invited_by: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub email: Option<String>,
}

impl From<UserRow> for User {
    fn from(r: UserRow) -> User {
        User {
            id: UserId(r.id),
            kind: UserKind::parse(&r.kind).unwrap_or(UserKind::User),
            username: r.username,
            display_name: r.display_name,
            email: r.email,
            is_admin: r.is_admin,
            can_run_ci: r.can_run_ci,
            suspended: r.suspended_at.is_some(),
            has_2fa: r.totp_enabled_at.is_some(),
            has_password: r.has_password,
            invited_by: r.invited_by.map(UserId),
            created_at: r.created_at,
        }
    }
}

pub async fn load_user<'e, E: sqlx::PgExecutor<'e>>(ex: E, id: UserId) -> Result<UserRow> {
    sqlx::query_as::<_, UserRow>(&format!("SELECT {USER_COLS} FROM users u WHERE u.id = $1"))
        .bind(id.0)
        .fetch_optional(ex)
        .await?
        .ok_or(Error::NotFound)
}

pub async fn load_user_by_name<'e, E: sqlx::PgExecutor<'e>>(ex: E, name: &str) -> Result<UserRow> {
    sqlx::query_as::<_, UserRow>(&format!("SELECT {USER_COLS} FROM users u WHERE u.username = $1::citext"))
        .bind(name)
        .fetch_optional(ex)
        .await?
        .ok_or(Error::NotFound)
}

/// The signed-in account behind an actor: a session or basic user, or a token. Tokens need
/// the scope when `scope` is given.
pub fn acting_user(actor: &Actor, scope: Option<&str>) -> Result<UserId> {
    match actor {
        Actor::User { id, .. } => Ok(*id),
        Actor::Token { user_id, scopes, .. } => match scope {
            Some(s) if !token_has_scope(scopes, s) => Err(Error::forbidden(format!("This token needs the {s} scope"))),
            _ => Ok(*user_id),
        },
        Actor::Anonymous => Err(Error::unauthorized()),
        _ => Err(Error::forbidden("Not available to this kind of credential")),
    }
}

/// `admin:org` includes `read:org`.
pub fn token_has_scope(scopes: &ch_core::Scopes, scope: &str) -> bool {
    scopes.has(scope) || (scope == "read:org" && scopes.has("admin:org"))
}

/// Self, or a site admin acting on anyone, or the CLI.
pub fn require_self_or_admin(actor: &Actor, target: UserId) -> Result<()> {
    if actor.is_site_admin() || actor.user_id() == Some(target) {
        return Ok(());
    }
    Err(if actor.is_anonymous() { Error::unauthorized() } else { Error::NotFound })
}

/// Site admin power, with the helpful refusal for an admin who has not set up two-factor.
pub fn require_site_admin(actor: &Actor) -> Result<()> {
    if actor.is_site_admin() {
        Ok(())
    } else if actor.is_admin_without_2fa() {
        Err(Error::forbidden("Turn on two-factor sign-in before using the admin pages."))
    } else if actor.is_anonymous() {
        Err(Error::unauthorized())
    } else {
        Err(Error::forbidden("Site admins only."))
    }
}

pub fn client_ip_inet(client: &ClientInfo) -> ipnetwork::IpNetwork {
    ipnetwork::IpNetwork::from(client.ip)
}

// ---------------------------------------------------------------- sessions

pub fn new_session_cookie() -> String {
    tokens::random_hex_256()
}

pub fn hash_cookie(cookie: &str) -> Vec<u8> {
    tokens::hash_token(cookie)
}

/// Creates a session row and returns the cookie to set.
pub async fn create_session<'e, E: sqlx::PgExecutor<'e>>(ex: E, user: UserId, pending: bool, client: &ClientInfo) -> Result<SessionGrant> {
    let cookie = new_session_cookie();
    let ttl = if pending { Duration::minutes(PENDING_MINUTES) } else { Duration::days(SESSION_IDLE_DAYS) };
    let expires_at = Utc::now() + ttl;
    let mut csrf = [0u8; 32];
    rand::Rng::fill(&mut rand::rng(), &mut csrf);
    sqlx::query("INSERT INTO sessions (token_hash, user_id, csrf_secret, pending_2fa, expires_at, ip, user_agent) VALUES ($1, $2, $3, $4, $5, $6, $7)")
        .bind(hash_cookie(&cookie))
        .bind(user.0)
        .bind(csrf.to_vec())
        .bind(pending)
        .bind(expires_at)
        .bind(client_ip_inet(client))
        .bind(client.user_agent.as_deref().map(|s| crate::util::clip(s, 300)))
        .execute(ex)
        .await?;
    Ok(SessionGrant { cookie, expires_at, max_age_secs: ttl.num_seconds() })
}

/// Ends every session of the account except the one with `keep_hash`.
pub async fn delete_sessions_except<'e, E: sqlx::PgExecutor<'e>>(ex: E, user: UserId, keep_hash: Option<&[u8]>) -> Result<u64> {
    let r = sqlx::query("DELETE FROM sessions WHERE user_id = $1 AND ($2::bytea IS NULL OR token_hash <> $2)")
        .bind(user.0)
        .bind(keep_hash)
        .execute(ex)
        .await?;
    Ok(r.rows_affected())
}

/// A signed-in actor from a user row, for a given way in.
pub fn actor_for(row: &UserRow, via: AuthVia) -> Actor {
    Actor::User { id: UserId(row.id), is_admin: row.is_admin, has_2fa: row.totp_enabled_at.is_some(), via }
}
