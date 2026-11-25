//! `Authn`: turns the credential in a request into an `Actor` (ARCHITECTURE.md section 7.1).
//!
//! - Session cookie: opens pages and the API. Half signed-in sessions are `Anonymous`.
//! - Basic with a password: accounts without two-factor only.
//! - Basic or Bearer with a personal access token (`chp_...`), a job token or a runner
//!   credential: found by hash. A token of a suspended account stops working at once.
//! - SSH fingerprints: user keys and deploy keys.
//!
//! Every failure looks the same to the caller. Failed Basic and bearer attempts count
//! against a per-address limit, and failed passwords against the account lockout. Both
//! budgets are reserved before the password hash runs, so simultaneous guesses get the
//! same budget as one at a time.

use crate::core::*;
use crate::lockout::{self, Locked};
use crate::util::generic_credentials_error;
use async_trait::async_trait;
use ch_core::audit::{Outcome, action};
use ch_core::error::{Error, Result};
use ch_core::ids::*;
use ch_core::live::Credential;
use ch_core::model::{ClientInfo, SshIdentity};
use ch_core::services::Authn;
use ch_core::{Actor, AuthVia, Scopes};
use ch_db::tokens;
use chrono::{DateTime, Utc};
use serde_json::json;
use std::sync::Arc;

pub struct AuthnService {
    pub(crate) core: Arc<Core>,
}

impl AuthnService {
    pub fn new(core: Arc<Core>) -> Self {
        Self { core }
    }

    fn check_address_budget(&self, client: &ClientInfo) -> Result<()> {
        match self.core.failed_auth.is_limited(&client.ip.to_string()) {
            Some(retry_after_secs) => Err(Error::RateLimited { retry_after_secs }),
            None => Ok(()),
        }
    }

    fn count_failed_secret(&self, client: &ClientInfo) {
        let _ = self.core.failed_auth.hit(&client.ip.to_string());
    }

    async fn token_actor(&self, secret: &str) -> Result<Option<Actor>> {
        #[derive(sqlx::FromRow)]
        struct Row {
            id: i64,
            user_id: i64,
            scopes: Vec<String>,
        }
        let row: Option<Row> = sqlx::query_as(
            "SELECT t.id, t.user_id, t.scopes
             FROM personal_access_tokens t JOIN users u ON u.id = t.user_id
             WHERE t.token_hash = $1 AND t.revoked_at IS NULL AND t.expires_at > now()
               AND u.suspended_at IS NULL AND u.kind = 'user'",
        )
        .bind(tokens::hash_token(secret))
        .fetch_optional(&self.core.db)
        .await?;
        let Some(row) = row else { return Ok(None) };
        sqlx::query("UPDATE personal_access_tokens SET last_used_at = now() WHERE id = $1 AND (last_used_at IS NULL OR last_used_at < now() - interval '1 minute')")
            .bind(row.id)
            .execute(&self.core.db)
            .await?;
        Ok(Some(Actor::Token { user_id: UserId(row.user_id), token_id: TokenId(row.id), scopes: Scopes::new(row.scopes) }))
    }

    /// A job token. The lookup is ch_db::ci_auth::job_from_token, the one the CI stream uses
    /// too, so both read the fork flag where ch-ci's join_payload writes it (`meta.fork`). A
    /// fork pull request's job reads contents and writes nothing (ARCHITECTURE.md section 7.2,
    /// ch-x12). The token only works while its job is in progress.
    async fn job_actor(&self, secret: &str) -> Result<Option<Actor>> {
        Ok(ch_db::ci_auth::job_from_token(&self.core.db, secret).await?.map(|(job_id, repo_id, perms)| Actor::Job {
            job_id,
            repo_id,
            perms,
        }))
    }

    async fn runner_actor(&self, secret: &str) -> Result<Option<Actor>> {
        let id: Option<i64> = sqlx::query_scalar("SELECT id FROM runners WHERE credential_hash = $1 AND revoked_at IS NULL")
            .bind(tokens::hash_token(secret))
            .fetch_optional(&self.core.db)
            .await?;
        if let Some(id) = id {
            sqlx::query("UPDATE runners SET last_seen_at = now() WHERE id = $1 AND (last_seen_at IS NULL OR last_seen_at < now() - interval '10 seconds')")
                .bind(id)
                .execute(&self.core.db)
                .await?;
        }
        Ok(id.map(|id| Actor::Runner { runner_id: RunnerId(id) }))
    }

    /// Password check for Basic auth. The attempt is reserved before the hash runs, so
    /// concurrent guesses share one budget.
    async fn basic_password(&self, username: &str, password: &str, client: &ClientInfo) -> Result<Actor> {
        let row: Option<(i64, Option<String>, bool, bool)> = sqlx::query_as(
            "SELECT id, password_hash, suspended_at IS NOT NULL, totp_enabled_at IS NOT NULL FROM users WHERE username = $1::citext AND kind = 'user'",
        )
        .bind(username)
        .fetch_optional(&self.core.db)
        .await?;
        let acct_key = match &row {
            Some((id, ..)) => lockout::account_ip_key(*id, client.ip),
            None => lockout::name_ip_key(username, client.ip),
        };
        let ip_key = lockout::ip_key(client.ip);
        let res = reserve_login(&self.core, &acct_key, &ip_key).await?;
        let (usable_hash, suspended, has_2fa) = match &row {
            Some((_, hash, susp, tfa)) => (if *susp { None } else { hash.as_deref() }, *susp, *tfa),
            None => (None, false, false),
        };
        let ok = self.core.hasher.verify(usable_hash, password).await;
        if !ok {
            if !suspended {
                self.fail_password(&res.locks(), row.as_ref().map(|r| r.0), client).await;
            }
            self.count_failed_secret(client);
            return Err(generic_credentials_error());
        }
        let (id, hash, _, _) = row.expect("a verified password has an account");
        if has_2fa {
            // Right password, but this account needs a token for git and the API. Both
            // reservations are given back: a right password that is refused is not a guess,
            // and a git client that tries this on every request must not fill the address
            // budget.
            self.core.lockout.release(&ip_key).await?;
            self.core.lockout.release(&acct_key).await?;
            return Err(Error::Unauthorized("This account uses two-factor sign-in. Use a personal access token as the password.".into()));
        }
        self.core.lockout.record_success(&acct_key).await?;
        // A git client that signs in with a password on every request must not fill the
        // address budget.
        self.core.lockout.release(&ip_key).await?;
        if let Some(stored) = hash
            && self.core.hasher.needs_rehash(&stored)
            && let Ok(new_hash) = self.core.hasher.hash(password).await
        {
            sqlx::query("UPDATE users SET password_hash = $2 WHERE id = $1").bind(id).bind(new_hash).execute(&self.core.db).await?;
        }
        let user = load_user(&self.core.db, UserId(id)).await?;
        Ok(actor_for(&user, AuthVia::Basic))
    }

    pub(crate) async fn fail_password(&self, locks: &[(&str, Locked)], user_id: Option<i64>, client: &ClientInfo) {
        self.core
            .audit_standalone(
                &Actor::Anonymous,
                client,
                action::SIGN_IN_FAILED,
                Outcome::Failure,
                user_id.map(|id| ("user", id)),
                json!({"via": "basic"}),
            )
            .await;
        audit_new_locks(&self.core, locks, user_id, client).await;
    }
}

/// Audits the lockout event for each lock the current attempt created (the `locked_now`
/// locks of the reservation). Called only on failure.
pub(crate) async fn audit_new_locks(core: &Core, locks: &[(&str, Locked)], user_id: Option<i64>, client: &ClientInfo) {
    for (key, locked) in locks {
        let ips = recent_failure_ips(core, user_id, client).await;
        core.audit_standalone(
            &Actor::Anonymous,
            client,
            action::LOCKOUT,
            Outcome::Failure,
            user_id.map(|id| ("user", id)),
            json!({"key_kind": key.split(':').next().unwrap_or("?"), "until": locked.until, "source_ips": ips}),
        )
        .await;
    }
}

/// Distinct addresses of recent failed sign-ins for the account (or this one address when the
/// account does not exist), so the owner can see where the guessing came from.
async fn recent_failure_ips(core: &Core, user_id: Option<i64>, client: &ClientInfo) -> Vec<String> {
    let Some(id) = user_id else { return vec![client.ip.to_string()] };
    let rows: std::result::Result<Vec<String>, _> = sqlx::query_scalar(
        "SELECT DISTINCT host(client_ip) FROM audit_events
         WHERE target_type = 'user' AND target_id = $1 AND action IN ('auth.sign_in_failed', 'auth.two_factor_failed')
           AND at > now() - interval '30 minutes' AND client_ip IS NOT NULL LIMIT 20",
    )
    .bind(id.to_string())
    .fetch_all(&core.db)
    .await;
    rows.unwrap_or_else(|_| vec![client.ip.to_string()])
}

#[allow(clippy::wrong_self_convention)]
#[async_trait]
impl Authn for AuthnService {
    async fn from_session(&self, cookie_value: &str, client: &ClientInfo) -> Result<Actor> {
        if cookie_value.len() != 64 || !cookie_value.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(Actor::Anonymous);
        }
        #[derive(sqlx::FromRow)]
        struct Row {
            id: i64,
            pending_2fa: bool,
            last_seen_at: DateTime<Utc>,
            user_id: i64,
        }
        let row: Option<Row> = sqlx::query_as(
            "SELECT s.id, s.pending_2fa, s.last_seen_at, s.user_id FROM sessions s JOIN users u ON u.id = s.user_id
             WHERE s.token_hash = $1 AND s.expires_at > now() AND s.created_at + interval '30 days' > now()
               AND u.suspended_at IS NULL AND u.kind = 'user'",
        )
        .bind(hash_cookie(cookie_value))
        .fetch_optional(&self.core.db)
        .await?;
        let Some(row) = row else { return Ok(Actor::Anonymous) };
        if row.pending_2fa {
            return Ok(Actor::Anonymous);
        }
        if (Utc::now() - row.last_seen_at).num_seconds() > 60 {
            sqlx::query(
                "UPDATE sessions SET last_seen_at = now(), expires_at = LEAST(now() + interval '7 days', created_at + interval '30 days'), ip = $2, user_agent = $3 WHERE id = $1",
            )
            .bind(row.id)
            .bind(client_ip_inet(client))
            .bind(client.user_agent.as_deref().map(|s| crate::util::clip(s, 300)))
            .execute(&self.core.db)
            .await?;
        }
        let user = load_user(&self.core.db, UserId(row.user_id)).await?;
        Ok(actor_for(&user, AuthVia::Session))
    }

    async fn from_basic(&self, username: &str, secret: &str, client: &ClientInfo) -> Result<Actor> {
        self.check_address_budget(client)?;
        if secret.starts_with("chp_") {
            return match self.token_actor(secret).await? {
                Some(a) => Ok(a),
                None => {
                    self.count_failed_secret(client);
                    Err(Error::unauthorized())
                }
            };
        }
        // `git clone` inside a job sends its job token as the password.
        if let Some(a) = self.job_actor(secret).await? {
            return Ok(a);
        }
        self.basic_password(username, secret, client).await
    }

    async fn from_bearer(&self, token: &str, client: &ClientInfo) -> Result<Actor> {
        // An app token opens `/mcp` only (`from_app_bearer`). Here it is simply not a credential.
        if token.starts_with(ch_core::apps::ACCESS_PREFIX)
            || token.starts_with(ch_core::apps::REFRESH_PREFIX)
            || token.starts_with(ch_core::apps::CODE_PREFIX)
        {
            return Err(Error::unauthorized());
        }
        self.check_address_budget(client)?;
        let found = if token.starts_with("chp_") {
            self.token_actor(token).await?
        } else if token.starts_with("eyJ") {
            // The runtime token an action gets for artifacts and caches (ch-ctk.6): its own job's
            // credential in the shape the Actions toolkit expects.
            ch_db::ci_auth::job_from_runtime_token(&self.core.db, token).await?.map(|(job_id, repo_id, perms)| Actor::Job {
                job_id,
                repo_id,
                perms,
            })
        } else if let Some(a) = self.job_actor(token).await? {
            Some(a)
        } else {
            self.runner_actor(token).await?
        };
        match found {
            Some(a) => Ok(a),
            None => {
                self.count_failed_secret(client);
                Err(Error::unauthorized())
            }
        }
    }

    async fn from_app_bearer(&self, token: &str, _client: &ClientInfo) -> Result<Actor> {
        // Not counted against the address's failed-auth budget: the gate rate limits rejected
        // `/mcp` requests on their own, and a full failure bucket must never block a valid token.
        match crate::apps::app_actor(&self.core, token).await? {
            Some(a) => Ok(a),
            None => Err(Error::unauthorized()),
        }
    }

    async fn from_ssh_key(&self, fingerprint: &str) -> Result<Option<SshIdentity>> {
        let user: Option<i64> = sqlx::query_scalar(
            "SELECT k.user_id FROM ssh_keys k JOIN users u ON u.id = k.user_id WHERE k.fingerprint = $1 AND u.suspended_at IS NULL AND u.kind = 'user'",
        )
        .bind(fingerprint)
        .fetch_optional(&self.core.db)
        .await?;
        if let Some(uid) = user {
            sqlx::query("UPDATE ssh_keys SET last_used_at = now() WHERE fingerprint = $1 AND (last_used_at IS NULL OR last_used_at < now() - interval '1 minute')")
                .bind(fingerprint)
                .execute(&self.core.db)
                .await?;
            return Ok(Some(SshIdentity::User(UserId(uid))));
        }
        let deploy: Option<(i64, bool)> = sqlx::query_as("SELECT repo_id, read_only FROM deploy_keys WHERE fingerprint = $1")
            .bind(fingerprint)
            .fetch_optional(&self.core.db)
            .await?;
        let Some((repo_id, read_only)) = deploy else { return Ok(None) };
        // A key on a locked fork opens nothing: the fork's owner lost access to a repository it
        // was forked from, and a key must not outlast that (ch-4pn.16).
        let authz = crate::authz::AuthzService::new(self.core.clone());
        if ch_db::fork_lock::is_locked(&self.core.db, &authz, RepoId(repo_id)).await? {
            return Ok(None);
        }
        Ok(Some(SshIdentity::Deploy { repo_id: RepoId(repo_id), read_only }))
    }

    async fn recheck(&self, credential: &Credential) -> Result<Option<Actor>> {
        // Reads only: no last-seen, expiry or last-used update and no failure count, so a
        // stream that asks every second neither keeps a session alive nor fills a budget.
        let db = &self.core.db;
        let user_id: Option<i64> = match credential {
            Credential::Session { hash } => {
                sqlx::query_scalar(
                    "SELECT s.user_id FROM sessions s JOIN users u ON u.id = s.user_id
                     WHERE s.token_hash = $1 AND NOT s.pending_2fa AND s.expires_at > now()
                       AND s.created_at + interval '30 days' > now() AND u.suspended_at IS NULL AND u.kind = 'user'",
                )
                .bind(hash.as_slice())
                .fetch_optional(db)
                .await?
            }
            Credential::Basic { user_id, fingerprint } => {
                let row: Option<(Option<String>, bool)> = sqlx::query_as(
                    "SELECT password_hash, totp_enabled_at IS NOT NULL FROM users WHERE id = $1 AND suspended_at IS NULL AND kind = 'user'",
                )
                .bind(user_id.0)
                .fetch_optional(db)
                .await?;
                match row {
                    Some((Some(h), false)) if password_fingerprint_of(&h) == *fingerprint => Some(user_id.0),
                    _ => None,
                }
            }
            Credential::Token { token_id } => {
                let row: Option<(i64, Vec<String>)> = sqlx::query_as(
                    "SELECT t.user_id, t.scopes FROM personal_access_tokens t JOIN users u ON u.id = t.user_id
                     WHERE t.id = $1 AND t.revoked_at IS NULL AND t.expires_at > now() AND u.suspended_at IS NULL AND u.kind = 'user'",
                )
                .bind(token_id.0)
                .fetch_optional(db)
                .await?;
                return Ok(row.map(|(user_id, scopes)| Actor::Token {
                    user_id: UserId(user_id),
                    token_id: *token_id,
                    scopes: Scopes::new(scopes),
                }));
            }
            Credential::Job { job_id } => {
                return Ok(ch_db::ci_auth::job_by_id(db, *job_id).await?.map(|(repo_id, perms)| Actor::Job {
                    job_id: *job_id,
                    repo_id,
                    perms,
                }));
            }
            Credential::Runner { runner_id } => {
                let live: Option<i64> = sqlx::query_scalar("SELECT id FROM runners WHERE id = $1 AND revoked_at IS NULL")
                    .bind(runner_id.0)
                    .fetch_optional(db)
                    .await?;
                return Ok(live.map(|_| Actor::Runner { runner_id: *runner_id }));
            }
            // An app opens `/mcp` only, which has no stream.
            Credential::App { .. } => return Ok(None),
        };
        let Some(id) = user_id else { return Ok(None) };
        let via = if matches!(credential, Credential::Basic { .. }) { AuthVia::Basic } else { AuthVia::Session };
        match load_user(db, UserId(id)).await {
            Ok(user) => Ok(Some(actor_for(&user, via))),
            Err(Error::NotFound) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn password_fingerprint(&self, user: UserId) -> Result<Option<Vec<u8>>> {
        let hash: Option<Option<String>> =
            sqlx::query_scalar("SELECT password_hash FROM users WHERE id = $1").bind(user.0).fetch_optional(&self.core.db).await?;
        Ok(hash.flatten().map(|h| password_fingerprint_of(&h)))
    }
}

/// A fingerprint of a stored password hash. Any password change gives a new salt and so a new
/// fingerprint, which ends a Basic credential's live check.
fn password_fingerprint_of(stored_hash: &str) -> Vec<u8> {
    tokens::sha256(stored_hash.as_bytes()).to_vec()
}
