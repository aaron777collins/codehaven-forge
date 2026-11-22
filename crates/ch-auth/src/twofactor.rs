//! `TwoFactor`: TOTP set up, turned off, and recovery codes (ARCHITECTURE.md section 7.1).
//!
//! The secret is sealed with the master key and bound to the account, so a sealed value
//! copied to another row does not open. Recovery codes are stored as SHA-256 hashes and work
//! once. Turning two-factor on or off ends every session and issues a new cookie, because
//! privilege changed (section 7.6).

use crate::core::*;
use crate::credentials::verify_current_password;
use crate::totp;
use async_trait::async_trait;
use ch_core::audit::action;
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
        let enabled_at: Option<DateTime<Utc>> = sqlx::query_scalar("SELECT totp_enabled_at FROM users WHERE id = $1")
            .bind(user.0)
            .fetch_optional(&self.core.db)
            .await?
            .flatten();
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM totp_recovery_codes WHERE user_id = $1 AND used_at IS NULL")
            .bind(user.0)
            .fetch_one(&self.core.db)
            .await?;
        Ok(TwoFactorStatus {
            enabled: enabled_at.is_some(),
            enabled_at,
            recovery_codes_left: if enabled_at.is_some() { left as u32 } else { 0 },
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

    async fn disable(&self, actor: &Actor, current_cookie: &str, password: &str, client: &ClientInfo) -> Result<SessionGrant> {
        let user = Self::user_of(actor)?;
        let _ = current_cookie;
        verify_current_password(&self.core, user, password, client).await?;
        let mut tx = self.core.db.begin().await?;
        sqlx::query(
            "UPDATE users SET totp_secret_sealed = NULL, totp_enabled_at = NULL, totp_last_step = NULL, updated_at = now() WHERE id = $1",
        )
        .bind(user.0)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM totp_recovery_codes WHERE user_id = $1").bind(user.0).execute(&mut *tx).await?;
        delete_sessions_except(&mut *tx, user, None).await?;
        crate::apps::revoke_all_for_user(&mut tx, user, "two-factor turned off").await?;
        let grant = create_session(&mut *tx, user, false, client).await?;
        self.core.audit_tx(&mut tx, actor, client, action::TWO_FACTOR_OFF, Some(("user", user.0)), json!({})).await?;
        tx.commit().await?;
        Ok(grant)
    }

    async fn regenerate_recovery_codes(&self, actor: &Actor, password: &str, client: &ClientInfo) -> Result<Vec<String>> {
        let user = Self::user_of(actor)?;
        verify_current_password(&self.core, user, password, client).await?;
        let enabled: bool =
            sqlx::query_scalar("SELECT totp_enabled_at IS NOT NULL FROM users WHERE id = $1").bind(user.0).fetch_one(&self.core.db).await?;
        if !enabled {
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
}
