//! `TwoFactor`: TOTP set up, turned off, passkeys, and recovery codes (ARCHITECTURE.md section 7.1).
//!
//! An account has two-factor on when it has an authenticator app or at least one passkey
//! (`ch_has_second_factor`). Recovery codes belong to the account, not to one factor: they are made
//! when the first factor is turned on and removed when the last one goes. Taking the last factor away
//! needs the password and a current factor (a code from the app, or a recovery code).
//!
//! The secret is sealed with the master key and bound to the account, so a sealed value
//! copied to another row does not open. Recovery codes are stored as SHA-256 hashes and work
//! once. Turning two-factor on or off ends every session and issues a new cookie, because
//! privilege changed (section 7.6).

use crate::core::*;
use crate::credentials::verify_current_password;
use crate::lockout;
use crate::passkeys;
use crate::totp;
use async_trait::async_trait;
use ch_core::audit::{Outcome, action};
use ch_core::error::{Error, FieldError, Result};
use ch_core::identity::*;
use ch_core::ids::*;
use ch_core::model::ClientInfo;
use ch_core::services::TwoFactor;
use ch_core::{Actor, AuthVia};
use ch_db::tokens;
use chrono::{DateTime, Utc};
use serde_json::json;
use std::sync::Arc;
use webauthn_rs::prelude::{CredentialID, RegisterPublicKeyCredential};

pub struct TwoFactorService {
    pub(crate) core: Arc<Core>,
}

impl TwoFactorService {
    pub fn new(core: Arc<Core>) -> Self {
        Self { core }
    }

    fn user_of(actor: &Actor) -> Result<UserId> {
        match actor {
            Actor::User { id, via: AuthVia::Session, .. } => Ok(*id),
            Actor::Anonymous => Err(Error::unauthorized()),
            _ => Err(Error::forbidden("Sign in with a browser session to change two-factor settings.")),
        }
    }

    fn aad(user: UserId) -> Vec<u8> {
        format!("totp:{user}").into_bytes()
    }

    /// Whether the authenticator app is on, and how many passkeys are registered.
    async fn factors(&self, user: UserId) -> Result<(bool, i64)> {
        let totp: bool =
            sqlx::query_scalar("SELECT totp_enabled_at IS NOT NULL FROM users WHERE id = $1").bind(user.0).fetch_one(&self.core.db).await?;
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM webauthn_credentials WHERE user_id = $1")
            .bind(user.0)
            .fetch_one(&self.core.db)
            .await?;
        Ok((totp, n))
    }

    /// Whether `code` is a current factor of the account: a code from the authenticator app that
    /// was not used yet, or an unused recovery code. A recovery code is not used up by this: the
    /// caller is about to remove the last factor, and with it every recovery code.
    async fn code_is_current(&self, user: UserId, code: &str) -> Result<bool> {
        let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
        if code.is_empty() || code.len() > 64 {
            return Ok(false);
        }
        if totp::looks_like_recovery_code(&code) {
            return Ok(sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM totp_recovery_codes WHERE user_id = $1 AND code_hash = $2 AND used_at IS NULL)",
            )
            .bind(user.0)
            .bind(tokens::hash_token(&totp::normalize_recovery_code(&code)))
            .fetch_one(&self.core.db)
            .await?);
        }
        let row: Option<(Option<Vec<u8>>, Option<i64>)> =
            sqlx::query_as("SELECT totp_secret_sealed, totp_last_step FROM users WHERE id = $1 AND totp_enabled_at IS NOT NULL")
                .bind(user.0)
                .fetch_optional(&self.core.db)
                .await?;
        let Some((Some(sealed), last_step)) = row else { return Ok(false) };
        let secret = self.core.key.open(&sealed, &Self::aad(user))?;
        let Some(step) = totp::verify(&secret, &code, Utc::now().timestamp() as u64, last_step) else { return Ok(false) };
        // The step is spent, as at sign-in, so the same code cannot be sent twice.
        Ok(sqlx::query("UPDATE users SET totp_last_step = $2 WHERE id = $1 AND (totp_last_step IS NULL OR totp_last_step < $2)")
            .bind(user.0)
            .bind(step)
            .execute(&self.core.db)
            .await?
            .rows_affected()
            == 1)
    }

    /// The current factor that taking the last factor away asks for, under the same attempt limits
    /// as a sign-in code.
    async fn require_current_factor(&self, user: UserId, code: &str, client: &ClientInfo) -> Result<()> {
        let acct_key = lockout::account_ip_key(user.0, client.ip);
        let ip_key = lockout::ip_key(client.ip);
        let res = reserve_login(&self.core, &acct_key, &ip_key).await?;
        if self.code_is_current(user, code).await? {
            self.core.lockout.release(&ip_key).await?;
            self.core.lockout.release(&acct_key).await?;
            return Ok(());
        }
        self.core
            .audit_standalone(
                &Actor::Anonymous,
                client,
                action::TWO_FACTOR_FAILED,
                Outcome::Failure,
                Some(("user", user.0)),
                json!({"via": "reauthentication"}),
            )
            .await;
        crate::authn::audit_new_locks(&self.core, &res.locks(), Some(user.0), client).await;
        Err(Error::Validation(vec![FieldError::custom(
            "TwoFactor",
            "code",
            "That is not a current code. Use a code from your authenticator app, or one of your recovery codes.",
        )]))
    }

    /// Ends what the last second factor protected: its recovery codes, every session, and the apps
    /// the person connected. Returns the new session to set.
    async fn last_factor_gone(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        user: UserId,
        why: &str,
        client: &ClientInfo,
    ) -> Result<SessionGrant> {
        sqlx::query("DELETE FROM totp_recovery_codes WHERE user_id = $1").bind(user.0).execute(&mut **tx).await?;
        delete_sessions_except(&mut **tx, user, None).await?;
        crate::apps::revoke_all_for_user(&mut *tx, user, why).await?;
        create_session(&mut **tx, user, false, client).await
    }

    async fn write_recovery_codes(&self, conn: &mut sqlx::PgConnection, user: UserId) -> Result<Vec<String>> {
        sqlx::query("DELETE FROM totp_recovery_codes WHERE user_id = $1").bind(user.0).execute(&mut *conn).await?;
        let codes: Vec<String> = (0..totp::RECOVERY_CODE_COUNT).map(|_| totp::new_recovery_code()).collect();
        for c in &codes {
            sqlx::query("INSERT INTO totp_recovery_codes (user_id, code_hash) VALUES ($1, $2)")
                .bind(user.0)
                .bind(tokens::hash_token(&totp::normalize_recovery_code(c)))
                .execute(&mut *conn)
                .await?;
        }
        Ok(codes)
    }
}

#[async_trait]
impl TwoFactor for TwoFactorService {
    async fn status(&self, actor: &Actor) -> Result<TwoFactorStatus> {
        let user = acting_user(actor, None)?;
        let totp_at: Option<DateTime<Utc>> = sqlx::query_scalar("SELECT totp_enabled_at FROM users WHERE id = $1")
            .bind(user.0)
            .fetch_optional(&self.core.db)
            .await?
            .flatten();
        let (passkeys, first_passkey): (i64, Option<DateTime<Utc>>) =
            sqlx::query_as("SELECT count(*), min(created_at) FROM webauthn_credentials WHERE user_id = $1")
                .bind(user.0)
                .fetch_one(&self.core.db)
                .await?;
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM totp_recovery_codes WHERE user_id = $1 AND used_at IS NULL")
            .bind(user.0)
            .fetch_one(&self.core.db)
            .await?;
        let enabled_at = [totp_at, first_passkey].into_iter().flatten().min();
        let enabled = enabled_at.is_some();
        Ok(TwoFactorStatus {
            enabled,
            totp: totp_at.is_some(),
            passkeys: passkeys as u32,
            enabled_at,
            recovery_codes_left: if enabled { left as u32 } else { 0 },
        })
    }

    async fn begin_setup(&self, actor: &Actor) -> Result<TotpSetup> {
        let user = Self::user_of(actor)?;
        let row = load_user(&self.core.db, user).await?;
        if row.totp_enabled_at.is_some() {
            return Err(Error::conflict("Two-factor sign-in is already on. Turn it off first to set it up again."));
        }
        let secret = totp::new_secret();
        let sealed = self.core.key.seal(&secret, &Self::aad(user))?;
        sqlx::query("UPDATE users SET totp_secret_sealed = $2, totp_last_step = NULL WHERE id = $1 AND totp_enabled_at IS NULL")
            .bind(user.0)
            .bind(sealed)
            .execute(&self.core.db)
            .await?;
        Ok(TotpSetup {
            secret: totp::base32_encode(&secret),
            otpauth_uri: totp::otpauth_uri(&self.core.config.server.name, &row.username, &secret),
        })
    }

    async fn confirm_setup(&self, actor: &Actor, current_cookie: &str, code: &str, client: &ClientInfo) -> Result<TwoFactorEnabled> {
        let user = Self::user_of(actor)?;
        let _ = current_cookie;
        let (sealed, enabled): (Option<Vec<u8>>, bool) =
            sqlx::query_as("SELECT totp_secret_sealed, totp_enabled_at IS NOT NULL FROM users WHERE id = $1")
                .bind(user.0)
                .fetch_one(&self.core.db)
                .await?;
        if enabled {
            return Err(Error::conflict("Two-factor sign-in is already on."));
        }
        let Some(sealed) = sealed else {
            return Err(Error::conflict("Start the setup first to get a secret."));
        };
        let secret = self.core.key.open(&sealed, &Self::aad(user))?;
        let Some(step) = totp::verify(&secret, code, Utc::now().timestamp() as u64, None) else {
            return Err(Error::Validation(vec![FieldError::custom(
                "TwoFactor",
                "code",
                "That code is not right. Check the time on your phone and try the next code.",
            )]));
        };
        let mut tx = self.core.db.begin().await?;
        sqlx::query("UPDATE users SET totp_enabled_at = now(), totp_last_step = $2, updated_at = now() WHERE id = $1")
            .bind(user.0)
            .bind(step)
            .execute(&mut *tx)
            .await?;
        let codes = self.write_recovery_codes(&mut tx, user).await?;
        delete_sessions_except(&mut *tx, user, None).await?;
        let grant = create_session(&mut *tx, user, false, client).await?;
        self.core.audit_tx(&mut tx, actor, client, action::TWO_FACTOR_ON, Some(("user", user.0)), json!({})).await?;
        tx.commit().await?;
        Ok(TwoFactorEnabled { recovery_codes: codes, grant })
    }

    async fn disable(&self, actor: &Actor, current_cookie: &str, password: &str, code: &str, client: &ClientInfo) -> Result<SessionGrant> {
        let user = Self::user_of(actor)?;
        let _ = current_cookie;
        verify_current_password(&self.core, user, password, client).await?;
        let (totp_on, passkeys) = self.factors(user).await?;
        if !totp_on {
            return Err(Error::conflict("The authenticator app is not on."));
        }
        // Without a passkey this is the last second factor: a current one is asked for too.
        if passkeys == 0 {
            self.require_current_factor(user, code, client).await?;
        }
        let mut tx = self.core.db.begin().await?;
        sqlx::query(
            "UPDATE users SET totp_secret_sealed = NULL, totp_enabled_at = NULL, totp_last_step = NULL, updated_at = now() WHERE id = $1",
        )
        .bind(user.0)
        .execute(&mut *tx)
        .await?;
        let grant = if passkeys == 0 {
            self.last_factor_gone(&mut tx, user, "two-factor turned off", client).await?
        } else {
            // Two-factor stays on with the passkeys, so the recovery codes and the connected apps
            // stay. The sessions end all the same, as for any change of how sign-in is proved.
            delete_sessions_except(&mut *tx, user, None).await?;
            create_session(&mut *tx, user, false, client).await?
        };
        let details = if passkeys == 0 { json!({}) } else { json!({"authenticator_app_only": true}) };
        self.core.audit_tx(&mut tx, actor, client, action::TWO_FACTOR_OFF, Some(("user", user.0)), details).await?;
        tx.commit().await?;
        Ok(grant)
    }

    async fn regenerate_recovery_codes(&self, actor: &Actor, password: &str, client: &ClientInfo) -> Result<Vec<String>> {
        let user = Self::user_of(actor)?;
        verify_current_password(&self.core, user, password, client).await?;
        let (totp_on, passkeys) = self.factors(user).await?;
        if !totp_on && passkeys == 0 {
            return Err(Error::conflict("Turn on two-factor sign-in first."));
        }
        let mut tx = self.core.db.begin().await?;
        let codes = self.write_recovery_codes(&mut tx, user).await?;
        self.core
            .audit_tx(&mut tx, actor, client, action::TWO_FACTOR_ON, Some(("user", user.0)), json!({"recovery_codes_regenerated": true}))
            .await?;
        tx.commit().await?;
        Ok(codes)
    }

    async fn passkeys_available(&self) -> bool {
        self.core.rp.available()
    }

    async fn passkeys(&self, actor: &Actor) -> Result<Vec<PasskeyInfo>> {
        let user = acting_user(actor, None)?;
        let rows: Vec<(i64, String, DateTime<Utc>, Option<DateTime<Utc>>)> =
            sqlx::query_as("SELECT id, name, created_at, last_used_at FROM webauthn_credentials WHERE user_id = $1 ORDER BY id")
                .bind(user.0)
                .fetch_all(&self.core.db)
                .await?;
        Ok(rows.into_iter().map(|(id, name, created_at, last_used_at)| PasskeyInfo { id, name, created_at, last_used_at }).collect())
    }

    async fn begin_passkey(&self, actor: &Actor, client: &ClientInfo) -> Result<String> {
        let _ = client;
        let user = Self::user_of(actor)?;
        let wa = self.core.rp.get()?;
        let row = load_user(&self.core.db, user).await?;
        let existing = passkeys::load(&self.core.db, user).await?;
        if existing.len() as i64 >= passkeys::MAX_PASSKEYS {
            return Err(Error::conflict(format!("At most {} passkeys can be registered. Remove one first.", passkeys::MAX_PASSKEYS)));
        }
        // A passkey the account already holds is refused by the authenticator itself.
        let exclude: Vec<CredentialID> = existing.iter().map(|(_, p)| p.cred_id().clone()).collect();
        let name = if row.display_name.is_empty() { row.username.clone() } else { row.display_name.clone() };
        let (options, state) = wa
            .start_passkey_registration(passkeys::user_handle(user), &row.username, &name, Some(exclude))
            .map_err(|e| Error::internal(anyhow::anyhow!("passkey registration could not start: {e}")))?;
        passkeys::put_state(&self.core.db, &passkeys::register_key(user), &state).await?;
        serde_json::to_string(&options).map_err(|e| Error::internal(anyhow::anyhow!("passkey options: {e}")))
    }

    async fn finish_passkey(
        &self,
        actor: &Actor,
        current_cookie: &str,
        name: &str,
        credential: &str,
        client: &ClientInfo,
    ) -> Result<PasskeyAdded> {
        let _ = current_cookie;
        let user = Self::user_of(actor)?;
        let wa = self.core.rp.get()?;
        let name = passkeys::clean_name(name)?;
        let refused = || Error::invalid("Passkey", "credential", "That passkey could not be added. Try again.");
        let Some(state) =
            passkeys::take_state::<webauthn_rs::prelude::PasskeyRegistration>(&self.core.db, &passkeys::register_key(user)).await?
        else {
            return Err(Error::invalid("Passkey", "credential", "That took too long. Start again."));
        };
        let response: RegisterPublicKeyCredential = serde_json::from_str(credential).map_err(|_| refused())?;
        let passkey = match wa.finish_passkey_registration(&response, &state) {
            Ok(p) => p,
            Err(e) => {
                tracing::info!(error = %e, "a passkey registration did not verify");
                return Err(refused());
            }
        };
        let (totp_on, held) = self.factors(user).await?;
        if held >= passkeys::MAX_PASSKEYS {
            return Err(Error::conflict(format!("At most {} passkeys can be registered. Remove one first.", passkeys::MAX_PASSKEYS)));
        }
        let first = !totp_on && held == 0;
        let json = serde_json::to_value(&passkey).map_err(|e| Error::internal(anyhow::anyhow!("passkey: {e}")))?;
        let mut tx = self.core.db.begin().await?;
        let inserted: std::result::Result<(i64, DateTime<Utc>), sqlx::Error> = sqlx::query_as(
            "INSERT INTO webauthn_credentials (user_id, credential_id, name, passkey) VALUES ($1, $2, $3, $4) RETURNING id, created_at",
        )
        .bind(user.0)
        .bind(passkey.cred_id().as_ref() as &[u8])
        .bind(&name)
        .bind(json)
        .fetch_one(&mut *tx)
        .await;
        let (id, created_at) = match inserted {
            Ok(r) => r,
            Err(sqlx::Error::Database(d)) if d.is_unique_violation() => return Err(Error::conflict("That passkey is already registered.")),
            Err(e) => return Err(e.into()),
        };
        let (codes, grant) = if first {
            // Two-factor was off and is on now: recovery codes are made and every session ends,
            // as when the authenticator app is turned on.
            let codes = self.write_recovery_codes(&mut tx, user).await?;
            delete_sessions_except(&mut *tx, user, None).await?;
            let grant = create_session(&mut *tx, user, false, client).await?;
            self.core.audit_tx(&mut tx, actor, client, action::TWO_FACTOR_ON, Some(("user", user.0)), json!({"via": "passkey"})).await?;
            (codes, Some(grant))
        } else {
            (Vec::new(), None)
        };
        self.core.audit_tx(&mut tx, actor, client, action::PASSKEY_ADDED, Some(("user", user.0)), json!({"name": name})).await?;
        tx.commit().await?;
        Ok(PasskeyAdded { passkey: PasskeyInfo { id, name, created_at, last_used_at: None }, recovery_codes: codes, grant })
    }

    async fn remove_passkey(
        &self,
        actor: &Actor,
        current_cookie: &str,
        id: i64,
        password: &str,
        code: &str,
        client: &ClientInfo,
    ) -> Result<Option<SessionGrant>> {
        let _ = current_cookie;
        let user = Self::user_of(actor)?;
        verify_current_password(&self.core, user, password, client).await?;
        let name: String = sqlx::query_scalar("SELECT name FROM webauthn_credentials WHERE id = $1 AND user_id = $2")
            .bind(id)
            .bind(user.0)
            .fetch_optional(&self.core.db)
            .await?
            .ok_or(Error::NotFound)?;
        let (totp_on, held) = self.factors(user).await?;
        let last = !totp_on && held <= 1;
        if last {
            self.require_current_factor(user, code, client).await?;
        }
        let mut tx = self.core.db.begin().await?;
        sqlx::query("DELETE FROM webauthn_credentials WHERE id = $1 AND user_id = $2").bind(id).bind(user.0).execute(&mut *tx).await?;
        self.core.audit_tx(&mut tx, actor, client, action::PASSKEY_REMOVED, Some(("user", user.0)), json!({"name": name})).await?;
        let grant = if last {
            let grant = self.last_factor_gone(&mut tx, user, "two-factor turned off", client).await?;
            self.core.audit_tx(&mut tx, actor, client, action::TWO_FACTOR_OFF, Some(("user", user.0)), json!({"via": "passkey"})).await?;
            Some(grant)
        } else {
            None
        };
        tx.commit().await?;
        Ok(grant)
    }
}
