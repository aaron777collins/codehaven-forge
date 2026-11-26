//! `Credentials`: the interactive sign-in flows (ARCHITECTURE.md sections 7.1 and 7.6).
//!
//! Password sign-in with lockout, the second factor, sign-out, the sessions list, password
//! change, and the one-time set-password and reset links.

use crate::authn::audit_new_locks;
use crate::core::*;
use crate::lockout::{self};
use crate::passkeys;
use crate::totp;
use crate::util::*;
use async_trait::async_trait;
use ch_core::audit::{Outcome, action};
use ch_core::error::{Error, Result};
use ch_core::identity::*;
use ch_core::ids::*;
use ch_core::model::*;
use ch_core::services::Credentials;
use ch_core::{Actor, AuthVia};
use ch_db::tokens;
use chrono::{DateTime, Utc};
use serde_json::json;
use std::sync::Arc;

pub struct CredentialsService {
    pub(crate) core: Arc<Core>,
}

/// Wrong codes one half-signed-in session may try before it is ended.
const PENDING_CODE_ATTEMPTS: i32 = 5;

/// One second-factor attempt in flight: whose it is and which budgets it has been counted against.
struct SecondFactorAttempt {
    session_id: i64,
    user_id: i64,
    acct_key: String,
    ip_key: String,
    wide_key: String,
    res: Reservation,
}

#[derive(sqlx::FromRow)]
struct LoginRow {
    id: i64,
    password_hash: Option<String>,
    suspended: bool,
    has_2fa: bool,
    last_login_at: Option<DateTime<Utc>>,
}

impl CredentialsService {
    pub fn new(core: Arc<Core>) -> Self {
        Self { core }
    }

    async fn find_login(&self, login: &str) -> Result<Option<LoginRow>> {
        Ok(sqlx::query_as(
            "SELECT u.id, u.password_hash, u.suspended_at IS NOT NULL AS suspended, ch_has_second_factor(u.id) AS has_2fa, u.last_login_at
             FROM users u
             WHERE u.kind = 'user' AND (u.username = $1::citext OR EXISTS (
                 SELECT 1 FROM user_emails e WHERE e.user_id = u.id AND e.email = $1::citext AND (e.is_primary OR e.verified_at IS NOT NULL)))
             LIMIT 1",
        )
        .bind(login)
        .fetch_optional(&self.core.db)
        .await?)
    }

    async fn lockout_notice(&self, user: i64, since: Option<DateTime<Utc>>) -> Result<Option<SecurityNotice>> {
        let row: Option<(DateTime<Utc>, serde_json::Value)> = sqlx::query_as(
            "SELECT at, details FROM audit_events WHERE action = 'auth.lockout' AND target_type = 'user' AND target_id = $1 AND at > COALESCE($2, 'epoch'::timestamptz) ORDER BY id DESC LIMIT 1",
        )
        .bind(user.to_string())
        .bind(since)
        .fetch_optional(&self.core.db)
        .await?;
        Ok(row.map(|(at, d)| SecurityNotice {
            locked_at: at,
            source_ips: d
                .get("source_ips")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                .unwrap_or_default(),
        }))
    }

    async fn failed(&self, res: &Reservation, client: &ClientInfo, user: Option<i64>, act: &str, via: &str) {
        self.core.audit_standalone(&Actor::Anonymous, client, act, Outcome::Failure, user.map(|u| ("user", u)), json!({"via": via})).await;
        audit_new_locks(&self.core, &res.locks(), user, client).await;
    }

    /// The start of every second-factor attempt, whatever the factor: the half signed-in session,
    /// and the attempt reserved against the account and address budgets. The code stage also has
    /// an account-wide budget, across every address. Only someone who already holds the password
    /// and a pending session gets here, so an outsider cannot use it to lock anyone out, and a
    /// password thief cannot walk the code space by changing address (ch-rx2).
    async fn begin_second_factor(&self, pending_cookie: &str, client: &ClientInfo) -> Result<SecondFactorAttempt> {
        let hash = hash_cookie(pending_cookie);
        let sess: Option<(i64, i64)> =
            sqlx::query_as("SELECT id, user_id FROM sessions WHERE token_hash = $1 AND pending_2fa AND expires_at > now()")
                .bind(&hash)
                .fetch_optional(&self.core.db)
                .await?;
        let Some((session_id, user_id)) = sess else {
            return Err(Error::Unauthorized("Your sign-in timed out. Please sign in again.".into()));
        };
        let acct_key = lockout::account_ip_key(user_id, client.ip);
        let ip_key = lockout::ip_key(client.ip);
        let res = reserve_login(&self.core, &acct_key, &ip_key).await?;
        let wide_key = lockout::account_key(user_id);
        if let lockout::Attempt::Locked(l) = self.core.lockout.begin_attempt(&wide_key, lockout::ACCOUNT_THRESHOLD).await? {
            let _ = self.core.lockout.release(&ip_key).await;
            return Err(Error::RateLimited { retry_after_secs: l.retry_after_secs });
        }
        Ok(SecondFactorAttempt { session_id, user_id, acct_key, ip_key, wide_key, res })
    }

    /// A wrong second factor. The pending sign-in gets a few wrong tries, then it is gone: the
    /// person has to start again with the password, and the account count keeps growing meanwhile.
    async fn reject_second_factor(&self, at: &SecondFactorAttempt, client: &ClientInfo) -> Error {
        self.failed(&at.res, client, Some(at.user_id), action::TWO_FACTOR_FAILED, "two_factor").await;
        if let Ok(Some(_)) = self.core.lockout.record_failure(&format!("pending:{}", at.session_id), PENDING_CODE_ATTEMPTS).await {
            let _ = sqlx::query("DELETE FROM sessions WHERE id = $1").bind(at.session_id).execute(&self.core.db).await;
        }
        two_factor_error()
    }

    /// The right second factor: the budgets are cleared, the half session is swapped for a whole one.
    async fn accept_second_factor(&self, at: &SecondFactorAttempt, client: &ClientInfo, method: &str) -> Result<SignIn> {
        let user_id = at.user_id;
        let last_login_at: Option<DateTime<Utc>> =
            sqlx::query_scalar("SELECT last_login_at FROM users WHERE id = $1").bind(user_id).fetch_one(&self.core.db).await?;
        self.core.lockout.record_success(&at.acct_key).await?;
        self.core.lockout.record_success(&at.wide_key).await?;
        self.core.lockout.release(&at.ip_key).await?;
        let mut tx = self.core.db.begin().await?;
        sqlx::query("DELETE FROM sessions WHERE id = $1").bind(at.session_id).execute(&mut *tx).await?;
        let grant = create_session(&mut *tx, UserId(user_id), false, client).await?;
        sqlx::query("UPDATE users SET last_login_at = now() WHERE id = $1").bind(user_id).execute(&mut *tx).await?;
        let actor = Actor::User { id: UserId(user_id), is_admin: false, has_2fa: true, via: AuthVia::Session };
        self.core
            .audit_tx(&mut tx, &actor, client, action::SIGN_IN, Some(("user", user_id)), json!({"stage": "two_factor", "method": method}))
            .await?;
        if method == "recovery_code" {
            self.core.audit_tx(&mut tx, &actor, client, action::RECOVERY_CODE_USED, Some(("user", user_id)), json!({})).await?;
        }
        if method == "passkey" {
            self.core.audit_tx(&mut tx, &actor, client, action::PASSKEY_USED, Some(("user", user_id)), json!({})).await?;
        }
        tx.commit().await?;
        let notice = self.lockout_notice(user_id, last_login_at).await?;
        let user: User = load_user(&self.core.db, UserId(user_id)).await?.into();
        Ok(SignIn { user, grant, step: SignInStep::Complete, notice })
    }

    async fn finish_sign_in(&self, row: &LoginRow, pending: bool, client: &ClientInfo, notice: Option<SecurityNotice>) -> Result<SignIn> {
        let grant = create_session(&self.core.db, UserId(row.id), pending, client).await?;
        if !pending {
            sqlx::query("UPDATE users SET last_login_at = now() WHERE id = $1").bind(row.id).execute(&self.core.db).await?;
        }
        let user: User = load_user(&self.core.db, UserId(row.id)).await?.into();
        let step = if pending { SignInStep::TwoFactorRequired } else { SignInStep::Complete };
        Ok(SignIn { user, grant, step, notice })
    }
}

/// Checks the account's current password for a sensitive change, with the same lockout as
/// sign-in. Shared by password change and the two-factor settings. The attempt is reserved
/// before the hash runs, like sign-in.
pub(crate) async fn verify_current_password(core: &Core, user: UserId, password: &str, client: &ClientInfo) -> Result<()> {
    let acct_key = lockout::account_ip_key(user.0, client.ip);
    let ip_key = lockout::ip_key(client.ip);
    let res = reserve_login(core, &acct_key, &ip_key).await?;
    let hash: Option<String> =
        sqlx::query_scalar("SELECT password_hash FROM users WHERE id = $1").bind(user.0).fetch_optional(&core.db).await?.flatten();
    if core.hasher.verify(hash.as_deref(), password).await {
        core.lockout.release(&ip_key).await?;
        core.lockout.release(&acct_key).await?;
        return Ok(());
    }
    core.audit_standalone(
        &Actor::Anonymous,
        client,
        action::SIGN_IN_FAILED,
        Outcome::Failure,
        Some(("user", user.0)),
        json!({"via": "reauthentication"}),
    )
    .await;
    audit_new_locks(core, &res.locks(), Some(user.0), client).await;
    Err(Error::Unauthorized("That password is not right.".into()))
}

fn session_actor(actor: &Actor) -> Result<UserId> {
    match actor {
        Actor::User { id, via: AuthVia::Session, .. } => Ok(*id),
        Actor::Anonymous => Err(Error::unauthorized()),
        _ => Err(Error::forbidden("Sign in with a browser session to do this.")),
    }
}

fn two_factor_error() -> Error {
    Error::Unauthorized("That code is not right.".into())
}

pub fn link_token_shape_ok(token: &str) -> bool {
    token.len() == 36 && token.starts_with("cho_") && token[4..].bytes().all(|b| b.is_ascii_alphanumeric())
}

#[async_trait]
impl Credentials for CredentialsService {
    async fn sign_in(&self, login: &str, password: &str, client: &ClientInfo) -> Result<SignIn> {
        let login = login.trim();
        if login.is_empty() || login.len() > 254 || password.is_empty() || password.len() > 1024 {
            return Err(generic_credentials_error());
        }
        let ip_key = lockout::ip_key(client.ip);
        let row = self.find_login(login).await?;
        let acct_key = match &row {
            Some(r) => lockout::account_ip_key(r.id, client.ip),
            None => lockout::name_ip_key(login, client.ip),
        };
        // Reserve the attempt before the slow hash runs: a burst of simultaneous guesses must
        // not all slip past the check before any failure is counted.
        let res = reserve_login(&self.core, &acct_key, &ip_key).await?;
        let usable = row.as_ref().filter(|r| !r.suspended).and_then(|r| r.password_hash.as_deref());
        let ok = self.core.hasher.verify(usable, password).await;
        let known = row.as_ref().filter(|r| !r.suspended).map(|r| r.id);
        let Some(row) = row.filter(|_| ok) else {
            self.failed(&res, client, known, action::SIGN_IN_FAILED, "password").await;
            return Err(generic_credentials_error());
        };
        // With two-factor on, the right password is only half of the proof. Wiping the failure
        // count here would let someone who has the password guess codes forever, so the count is
        // cleared once the code is right (complete_two_factor).
        if !row.has_2fa {
            self.core.lockout.record_success(&acct_key).await?;
        }
        self.core.lockout.release(&ip_key).await?;
        if let Some(stored) = &row.password_hash
            && self.core.hasher.needs_rehash(stored)
            && let Ok(new_hash) = self.core.hasher.hash(password).await
        {
            sqlx::query("UPDATE users SET password_hash = $2 WHERE id = $1").bind(row.id).bind(new_hash).execute(&self.core.db).await?;
        }
        let pending = row.has_2fa;
        let notice = if pending { None } else { self.lockout_notice(row.id, row.last_login_at).await? };
        self.core
            .audit_standalone(
                &Actor::User { id: UserId(row.id), is_admin: false, has_2fa: row.has_2fa, via: AuthVia::Session },
                client,
                action::SIGN_IN,
                Outcome::Success,
                Some(("user", row.id)),
                json!({"stage": if pending { "password" } else { "complete" }}),
            )
            .await;
        self.finish_sign_in(&row, pending, client, notice).await
    }

    async fn complete_two_factor(&self, pending_cookie: &str, code: &str, client: &ClientInfo) -> Result<SignIn> {
        if pending_cookie.len() != 64 || code.len() > 64 {
            return Err(two_factor_error());
        }
        let at = self.begin_second_factor(pending_cookie, client).await?;
        let (sealed, last_step, suspended): (Option<Vec<u8>>, Option<i64>, bool) = sqlx::query_as(
            "SELECT totp_secret_sealed, totp_last_step, suspended_at IS NOT NULL FROM users WHERE id = $1 AND ch_has_second_factor(id)",
        )
        .bind(at.user_id)
        .fetch_optional(&self.core.db)
        .await?
        .ok_or_else(two_factor_error)?;
        if suspended {
            return Err(two_factor_error());
        }
        let mut used_recovery = false;
        let good = if totp::looks_like_recovery_code(code) {
            let n = sqlx::query("UPDATE totp_recovery_codes SET used_at = now() WHERE user_id = $1 AND code_hash = $2 AND used_at IS NULL")
                .bind(at.user_id)
                .bind(tokens::hash_token(&totp::normalize_recovery_code(code)))
                .execute(&self.core.db)
                .await?
                .rows_affected();
            used_recovery = n == 1;
            used_recovery
        } else {
            // An account with only a passkey has no secret, so a code that is not a recovery code
            // is simply wrong.
            let secret = self.core.key.open(&sealed.ok_or_else(two_factor_error)?, format!("totp:{}", at.user_id).as_bytes())?;
            match totp::verify(&secret, code, Utc::now().timestamp() as u64, last_step) {
                // Two requests with the same code race here. Only one moves the step forward.
                Some(step) => {
                    sqlx::query("UPDATE users SET totp_last_step = $2 WHERE id = $1 AND (totp_last_step IS NULL OR totp_last_step < $2)")
                        .bind(at.user_id)
                        .bind(step)
                        .execute(&self.core.db)
                        .await?
                        .rows_affected()
                        == 1
                }
                None => false,
            }
        };
        if !good {
            return Err(self.reject_second_factor(&at, client).await);
        }
        self.accept_second_factor(&at, client, if used_recovery { "recovery_code" } else { "totp" }).await
    }

    async fn pending_factors(&self, pending_cookie: &str) -> Result<PendingFactors> {
        if pending_cookie.len() != 64 {
            return Err(Error::Unauthorized("Your sign-in timed out. Please sign in again.".into()));
        }
        let user: Option<i64> =
            sqlx::query_scalar("SELECT user_id FROM sessions WHERE token_hash = $1 AND pending_2fa AND expires_at > now()")
                .bind(hash_cookie(pending_cookie))
                .fetch_optional(&self.core.db)
                .await?;
        let Some(user) = user else { return Err(Error::Unauthorized("Your sign-in timed out. Please sign in again.".into())) };
        let (totp, passkeys): (bool, i64) = sqlx::query_as(
            "SELECT totp_enabled_at IS NOT NULL, (SELECT count(*) FROM webauthn_credentials WHERE user_id = users.id) FROM users WHERE id = $1",
        )
        .bind(user)
        .fetch_one(&self.core.db)
        .await?;
        Ok(PendingFactors { totp, passkeys: passkeys > 0 && self.core.rp.available() })
    }

    async fn begin_passkey_sign_in(&self, pending_cookie: &str, client: &ClientInfo) -> Result<String> {
        let _ = client;
        let wa = self.core.rp.get()?;
        if pending_cookie.len() != 64 {
            return Err(two_factor_error());
        }
        let sess: Option<(i64, i64)> =
            sqlx::query_as("SELECT s.id, s.user_id FROM sessions s JOIN users u ON u.id = s.user_id WHERE s.token_hash = $1 AND s.pending_2fa AND s.expires_at > now() AND u.suspended_at IS NULL")
                .bind(hash_cookie(pending_cookie))
                .fetch_optional(&self.core.db)
                .await?;
        let Some((session_id, user_id)) = sess else {
            return Err(Error::Unauthorized("Your sign-in timed out. Please sign in again.".into()));
        };
        let keys: Vec<_> = passkeys::load(&self.core.db, UserId(user_id)).await?.into_iter().map(|(_, p)| p).collect();
        if keys.is_empty() {
            return Err(Error::conflict("This account has no passkey."));
        }
        let (options, state) =
            wa.start_passkey_authentication(&keys).map_err(|e| Error::internal(anyhow::anyhow!("passkey sign-in could not start: {e}")))?;
        passkeys::put_state(&self.core.db, &passkeys::sign_in_key(session_id), &state).await?;
        serde_json::to_string(&options).map_err(|e| Error::internal(anyhow::anyhow!("passkey options: {e}")))
    }

    async fn complete_passkey_sign_in(&self, pending_cookie: &str, credential: &str, client: &ClientInfo) -> Result<SignIn> {
        if pending_cookie.len() != 64 || credential.len() > 16 * 1024 {
            return Err(two_factor_error());
        }
        let wa = self.core.rp.get()?;
        let at = self.begin_second_factor(pending_cookie, client).await?;
        let user = UserId(at.user_id);
        // The challenge is read once: an answer cannot be replayed against it, and a wrong answer
        // needs a new challenge (the browser asks for one again).
        let state =
            passkeys::take_state::<webauthn_rs::prelude::PasskeyAuthentication>(&self.core.db, &passkeys::sign_in_key(at.session_id))
                .await?;
        let response: Option<webauthn_rs::prelude::PublicKeyCredential> = serde_json::from_str(credential).ok();
        let verified = match (state, response) {
            (Some(state), Some(response)) => match wa.finish_passkey_authentication(&response, &state) {
                Ok(r) => Some(r),
                Err(e) => {
                    tracing::info!(error = %e, "a passkey sign-in did not verify");
                    None
                }
            },
            _ => None,
        };
        let suspended: bool =
            sqlx::query_scalar("SELECT suspended_at IS NOT NULL FROM users WHERE id = $1").bind(user.0).fetch_one(&self.core.db).await?;
        let Some(result) = verified.filter(|_| !suspended) else {
            return Err(self.reject_second_factor(&at, client).await);
        };
        // Keep the counter and flags the authenticator reported, and when it was used.
        let mut keys = passkeys::load(&self.core.db, user).await?;
        let mut known = false;
        for (id, key) in &mut keys {
            if key.cred_id() != result.cred_id() {
                continue;
            }
            known = true;
            let changed = key.update_credential(&result).unwrap_or(false);
            let json = if changed { serde_json::to_value(&*key).ok() } else { None };
            sqlx::query(
                "UPDATE webauthn_credentials SET last_used_at = now(), sign_count = $2, passkey = COALESCE($3, passkey) WHERE id = $1",
            )
            .bind(*id)
            .bind(i64::from(result.counter()))
            .bind(json)
            .execute(&self.core.db)
            .await?;
        }
        if !known {
            return Err(self.reject_second_factor(&at, client).await);
        }
        self.accept_second_factor(&at, client, "passkey").await
    }

    async fn sign_out(&self, cookie: &str, client: &ClientInfo) -> Result<()> {
        if cookie.len() != 64 {
            return Ok(());
        }
        let user: Option<i64> = sqlx::query_scalar("DELETE FROM sessions WHERE token_hash = $1 RETURNING user_id")
            .bind(hash_cookie(cookie))
            .fetch_optional(&self.core.db)
            .await?;
        if let Some(u) = user {
            let actor = Actor::User { id: UserId(u), is_admin: false, has_2fa: false, via: AuthVia::Session };
            self.core.audit_standalone(&actor, client, action::SIGN_OUT, Outcome::Success, Some(("user", u)), json!({})).await;
        }
        Ok(())
    }

    async fn sessions(&self, actor: &Actor, current_cookie: &str) -> Result<Vec<SessionInfo>> {
        let user = session_actor(actor)?;
        let current = hash_cookie(current_cookie);
        #[allow(clippy::type_complexity)]
        let rows: Vec<(i64, Option<String>, Option<String>, DateTime<Utc>, DateTime<Utc>, Vec<u8>)> = sqlx::query_as(
            "SELECT id, host(ip), user_agent, created_at, last_seen_at, token_hash FROM sessions WHERE user_id = $1 AND NOT pending_2fa AND expires_at > now() ORDER BY last_seen_at DESC",
        )
        .bind(user.0)
        .fetch_all(&self.core.db)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(id, ip, user_agent, created_at, last_seen_at, h)| SessionInfo {
                id: SessionId(id),
                ip,
                user_agent,
                created_at,
                last_seen_at,
                current: h == current,
            })
            .collect())
    }

    async fn revoke_session(&self, actor: &Actor, id: SessionId, client: &ClientInfo) -> Result<()> {
        let user = session_actor(actor)?;
        let mut tx = self.core.db.begin().await?;
        let n = sqlx::query("DELETE FROM sessions WHERE id = $1 AND user_id = $2")
            .bind(id.0)
            .bind(user.0)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if n == 0 {
            return Err(Error::NotFound);
        }
        self.core.audit_tx(&mut tx, actor, client, action::SESSION_REVOKED, Some(("session", id.0)), json!({})).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn sign_out_others(&self, actor: &Actor, current_cookie: &str, client: &ClientInfo) -> Result<u64> {
        let user = session_actor(actor)?;
        let mut tx = self.core.db.begin().await?;
        let n = delete_sessions_except(&mut *tx, user, Some(&hash_cookie(current_cookie))).await?;
        self.core
            .audit_tx(&mut tx, actor, client, action::SESSION_REVOKED, Some(("user", user.0)), json!({"others": true, "count": n}))
            .await?;
        tx.commit().await?;
        Ok(n)
    }

    async fn sign_in_history(&self, actor: &Actor, limit: u32) -> Result<Vec<SignInRecord>> {
        let user = acting_user(actor, None)?;
        let rows: Vec<(DateTime<Utc>, Option<String>, Option<String>, String, String)> = sqlx::query_as(
            "SELECT at, host(client_ip), user_agent, action, outcome FROM audit_events
             WHERE target_type = 'user' AND target_id = $1 AND action IN ('auth.sign_in', 'auth.sign_in_failed', 'auth.two_factor_failed', 'auth.lockout', 'auth.recovery_code_used')
             ORDER BY id DESC LIMIT $2",
        )
        .bind(user.0.to_string())
        .bind(limit.clamp(1, 200) as i64)
        .fetch_all(&self.core.db)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(at, ip, user_agent, action, outcome)| SignInRecord { at, ip, user_agent, action, success: outcome == "success" })
            .collect())
    }

    async fn change_password(
        &self,
        actor: &Actor,
        current_cookie: &str,
        current_password: &str,
        new_password: &str,
        client: &ClientInfo,
    ) -> Result<SessionGrant> {
        let user = session_actor(actor)?;
        verify_current_password(&self.core, user, current_password, client).await?;
        let row = load_user(&self.core.db, user).await?;
        crate::accounts::check_policy_for(&row, new_password)?;
        let hash = self.core.hasher.hash(new_password).await?;
        let _ = current_cookie;
        let mut tx = self.core.db.begin().await?;
        sqlx::query("UPDATE users SET password_hash = $2, updated_at = now() WHERE id = $1")
            .bind(user.0)
            .bind(hash)
            .execute(&mut *tx)
            .await?;
        // A change of privilege: every session ends, including this one, and a new id is issued.
        delete_sessions_except(&mut *tx, user, None).await?;
        crate::apps::revoke_all_for_user(&mut tx, user, "password changed").await?;
        let grant = create_session(&mut *tx, user, false, client).await?;
        self.core.audit_tx(&mut tx, actor, client, action::PASSWORD_CHANGED, Some(("user", user.0)), json!({})).await?;
        tx.commit().await?;
        Ok(grant)
    }

    async fn peek_link(&self, token: &str) -> Result<LinkPeek> {
        if !link_token_shape_ok(token) {
            return Err(Error::NotFound);
        }
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT u.username::text, l.purpose FROM one_time_links l JOIN users u ON u.id = l.user_id
             WHERE l.token_hash = $1 AND l.used_at IS NULL AND l.expires_at > now() AND u.suspended_at IS NULL AND u.kind = 'user'",
        )
        .bind(tokens::hash_token(token))
        .fetch_optional(&self.core.db)
        .await?;
        let (username, purpose) = row.ok_or(Error::NotFound)?;
        let purpose = if purpose == "reset_password" { LinkPurpose::ResetPassword } else { LinkPurpose::SetPassword };
        Ok(LinkPeek { username, purpose })
    }

    async fn redeem_link(&self, token: &str, new_password: &str, client: &ClientInfo) -> Result<User> {
        if !link_token_shape_ok(token) {
            return Err(Error::NotFound);
        }
        let token_hash = tokens::hash_token(token);
        let user_id: Option<i64> = sqlx::query_scalar(
            "SELECT l.user_id FROM one_time_links l JOIN users u ON u.id = l.user_id
             WHERE l.token_hash = $1 AND l.used_at IS NULL AND l.expires_at > now() AND u.suspended_at IS NULL AND u.kind = 'user'",
        )
        .bind(&token_hash)
        .fetch_optional(&self.core.db)
        .await?;
        let Some(user_id) = user_id else { return Err(Error::NotFound) };
        let row = load_user(&self.core.db, UserId(user_id)).await?;
        crate::accounts::check_policy_for(&row, new_password)?;
        let hash = self.core.hasher.hash(new_password).await?;
        let mut tx = self.core.db.begin().await?;
        let claimed: Option<(i64, String)> =
            sqlx::query_as("UPDATE one_time_links SET used_at = now() WHERE token_hash = $1 AND used_at IS NULL AND expires_at > now() RETURNING user_id, purpose").bind(&token_hash).fetch_optional(&mut *tx).await?;
        let Some((uid, purpose)) = claimed else { return Err(Error::NotFound) };
        // The password is set now, so every other open link for the account ends with it.
        sqlx::query("UPDATE one_time_links SET used_at = now() WHERE user_id = $1 AND used_at IS NULL").bind(uid).execute(&mut *tx).await?;
        sqlx::query("UPDATE users SET password_hash = $2, updated_at = now() WHERE id = $1")
            .bind(uid)
            .bind(&hash)
            .execute(&mut *tx)
            .await?;
        delete_sessions_except(&mut *tx, UserId(uid), None).await?;
        crate::apps::revoke_all_for_user(&mut tx, UserId(uid), "password reset").await?;
        let act = if purpose == "reset_password" { action::PASSWORD_RESET } else { action::PASSWORD_SET };
        let who = Actor::User { id: UserId(uid), is_admin: row.is_admin, has_2fa: row.second_factor, via: AuthVia::Session };
        self.core.audit_tx(&mut tx, &who, client, act, Some(("user", uid)), json!({"stage": "redeemed"})).await?;
        tx.commit().await?;
        self.core.lockout.clear_all_addresses(&[lockout::account_key(uid), lockout::name_key(&row.username)]).await?;
        Ok(load_user(&self.core.db, UserId(uid)).await?.into())
    }

    fn check_password(&self, username: &str, email: &str, password: &str) -> Result<()> {
        crate::password::check_policy(username, email, password)
    }
}
