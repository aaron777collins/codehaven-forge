//! `Accounts`: users, one-time links, passwords, suspension, SSH keys and access tokens.

use crate::core::*;
use crate::sshkeys::parse_public_key;
use crate::util::*;
use async_trait::async_trait;
use ch_core::audit::action;
use ch_core::error::{Error, FieldError, Result};
use ch_core::ids::*;
use ch_core::model::*;
use ch_core::services::Accounts;
use ch_core::{Actor, AuthVia, Scopes};
use ch_db::tokens;
use chrono::{DateTime, Duration, Utc};
use serde_json::json;
use std::sync::Arc;

pub struct AccountsService {
    pub(crate) core: Arc<Core>,
}

pub const DEFAULT_TOKEN_DAYS: u32 = 90;
pub const MAX_TOKEN_DAYS: u32 = 365;
const MAX_ACTIVE_TOKENS: i64 = 100;
const MAX_KEYS_PER_USER: i64 = 100;

impl AccountsService {
    pub fn new(core: Arc<Core>) -> Self {
        Self { core }
    }

    fn require_system_or_admin(actor: &Actor) -> Result<()> {
        require_site_admin(actor)
    }
}

/// The link path the web serves, `/password/<token>`.
pub fn link_path(token: &str) -> String {
    format!("/password/{token}")
}

/// How long an emailed reset link works.
const RESET_LINK_MINUTES: i64 = 60;

#[async_trait]
impl Accounts for AccountsService {
    async fn get_user(&self, id: UserId) -> Result<User> {
        Ok(load_user(&self.core.db, id).await?.into())
    }

    async fn get_by_username(&self, actor: &Actor, username: &str) -> Result<User> {
        if actor.is_anonymous() {
            return Err(Error::NotFound);
        }
        Ok(load_user_by_name(&self.core.db, username).await?.into())
    }

    async fn count_users(&self) -> Result<i64> {
        Ok(sqlx::query_scalar("SELECT count(*) FROM users WHERE kind = 'user'").fetch_one(&self.core.db).await?)
    }

    async fn create_user(&self, actor: &Actor, new: NewUser, client: &ClientInfo) -> Result<User> {
        if !actor.is_system() {
            return Err(Error::forbidden("Accounts are made by invite, or on the server with the admin command."));
        }
        validate_username(&new.username, "User")?;
        let email = normalize_email(&new.email);
        validate_email(&email)?;
        let mut tx = self.core.db.begin().await?;
        let id: i64 =
            sqlx::query_scalar("INSERT INTO users (username, display_name, is_admin, can_run_ci) VALUES ($1, $2, $3, $4) RETURNING id")
                .bind(&new.username)
                .bind(new.display_name.as_deref().unwrap_or(""))
                .bind(new.is_admin)
                .bind(new.can_run_ci)
                .fetch_one(&mut *tx)
                .await
                .map_err(|e| if is_unique_violation(&e, "") { exists("User", "username") } else { e.into() })?;
        sqlx::query("INSERT INTO user_emails (user_id, email, is_primary, verified_at) VALUES ($1, $2, true, now())")
            .bind(id)
            .bind(&email)
            .execute(&mut *tx)
            .await
            .map_err(|e| if is_unique_violation(&e, "") { exists("Email", "email") } else { e.into() })?;
        self.core
            .audit_tx(
                &mut tx,
                actor,
                client,
                action::USER_CREATED,
                Some(("user", id)),
                json!({"username": new.username, "admin": new.is_admin, "via": "cli"}),
            )
            .await?;
        if new.is_admin {
            self.core.audit_tx(&mut tx, actor, client, action::USER_ADMIN_GRANTED, Some(("user", id)), json!({"value": true})).await?;
        }
        tx.commit().await?;
        self.get_user(UserId(id)).await
    }

    async fn request_password_reset(&self, login: &str, client: &ClientInfo) -> Result<Option<(String, OneTimeLink)>> {
        let login = login.trim();
        if login.is_empty() || login.len() > 254 {
            return Ok(None);
        }
        // A person, active, with a primary email: found by username or by that email.
        let row: Option<(i64, String)> = sqlx::query_as(
            "SELECT u.id, e.email::text FROM users u JOIN user_emails e ON e.user_id = u.id AND e.is_primary
             WHERE u.kind = 'user' AND u.suspended_at IS NULL AND (u.username = $1::citext OR e.email = $1::citext)
             LIMIT 1",
        )
        .bind(login)
        .fetch_optional(&self.core.db)
        .await?;
        let Some((user, email)) = row else { return Ok(None) };
        let token = tokens::new_token("cho_", 32);
        let expires_at = Utc::now() + Duration::minutes(RESET_LINK_MINUTES);
        let mut tx = self.core.db.begin().await?;
        // Serialise requests for one account so two at once cannot both pass the limit.
        sqlx::query("SELECT id FROM users WHERE id = $1 FOR UPDATE").bind(user).execute(&mut *tx).await?;
        let recent: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM one_time_links WHERE user_id = $1 AND purpose = 'reset_password' AND created_at > now() - interval '10 minutes')",
        )
        .bind(user)
        .fetch_one(&mut *tx)
        .await?;
        if recent {
            return Ok(None);
        }
        // One live emailed link per account. No session ends here: the request can come from
        // anyone, and redeeming a link is what signs the account out everywhere. So a request
        // replaces only links asked for the same way, never one an admin issued (ch-ev5).
        sqlx::query(
            "UPDATE one_time_links SET used_at = now() WHERE user_id = $1 AND purpose = 'reset_password' AND used_at IS NULL AND self_requested",
        )
        .bind(user)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO one_time_links (token_hash, user_id, purpose, expires_at, self_requested) VALUES ($1, $2, 'reset_password', $3, true)",
        )
            .bind(tokens::hash_token(&token))
            .bind(user)
            .bind(expires_at)
            .execute(&mut *tx)
            .await?;
        self.core
            .audit_tx(
                &mut tx,
                &Actor::Anonymous,
                client,
                action::PASSWORD_RESET,
                Some(("user", user)),
                json!({"stage": "link_emailed", "expires_at": expires_at}),
            )
            .await?;
        tx.commit().await?;
        Ok(Some((email, OneTimeLink { path: link_path(&token), token, expires_at })))
    }

    async fn issue_link(
        &self,
        actor: &Actor,
        user: UserId,
        purpose: LinkPurpose,
        ttl_minutes: u32,
        client: &ClientInfo,
    ) -> Result<OneTimeLink> {
        Self::require_system_or_admin(actor)?;
        let target = load_user(&self.core.db, user).await?;
        if target.kind != "user" {
            return Err(Error::NotFound);
        }
        let ttl = ttl_minutes.clamp(1, 7 * 24 * 60);
        let token = tokens::new_token("cho_", 32);
        let expires_at = Utc::now() + Duration::minutes(ttl as i64);
        let mut tx = self.core.db.begin().await?;
        // One live link per account and purpose: a new one replaces the old.
        sqlx::query("UPDATE one_time_links SET used_at = now() WHERE user_id = $1 AND purpose = $2 AND used_at IS NULL")
            .bind(user.0)
            .bind(purpose.as_str())
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO one_time_links (token_hash, user_id, purpose, expires_at) VALUES ($1, $2, $3, $4)")
            .bind(tokens::hash_token(&token))
            .bind(user.0)
            .bind(purpose.as_str())
            .bind(expires_at)
            .execute(&mut *tx)
            .await?;
        // A reset means the password may be known to someone else, so it signs the account out
        // everywhere now, in this transaction, not when the link is used. A first password link
        // (`SetPassword`) leaves sessions alone.
        let mut signed_out = 0;
        let mut tokens_revoked = 0;
        if purpose == LinkPurpose::ResetPassword {
            signed_out = delete_sessions_except(&mut *tx, user, None).await?;
            crate::apps::revoke_all_for_user(&mut tx, user, "password reset").await?;
            // A token made by whoever knew the old password would outlive the reset (ch-p9j).
            tokens_revoked = sqlx::query("UPDATE personal_access_tokens SET revoked_at = now() WHERE user_id = $1 AND revoked_at IS NULL")
                .bind(user.0)
                .execute(&mut *tx)
                .await?
                .rows_affected();
        }
        let act = if purpose == LinkPurpose::SetPassword { action::PASSWORD_SET } else { action::PASSWORD_RESET };
        self.core
            .audit_tx(
                &mut tx,
                actor,
                client,
                act,
                Some(("user", user.0)),
                json!({"stage": "link_issued", "expires_at": expires_at, "sessions_ended": signed_out, "tokens_revoked": tokens_revoked}),
            )
            .await?;
        tx.commit().await?;
        Ok(OneTimeLink { path: link_path(&token), token, expires_at })
    }

    async fn set_password(&self, actor: &Actor, user: UserId, password: &str, client: &ClientInfo) -> Result<()> {
        if !actor.is_system() {
            return Err(Error::forbidden("Only the server's admin command can set a password directly."));
        }
        let target = load_user(&self.core.db, user).await?;
        check_policy_for(&target, password)?;
        let hash = self.core.hasher.hash(password).await?;
        let mut tx = self.core.db.begin().await?;
        sqlx::query("UPDATE users SET password_hash = $2, updated_at = now() WHERE id = $1")
            .bind(user.0)
            .bind(hash)
            .execute(&mut *tx)
            .await?;
        delete_sessions_except(&mut *tx, user, None).await?;
        crate::apps::revoke_all_for_user(&mut tx, user, "password set").await?;
        self.core.audit_tx(&mut tx, actor, client, action::PASSWORD_SET, Some(("user", user.0)), json!({"stage": "set_directly"})).await?;
        tx.commit().await?;
        self.core.lockout.clear_all_addresses(&[lockout::account_key(user.0)]).await?;
        Ok(())
    }

    async fn sign_out_everywhere(&self, actor: &Actor, user: UserId, client: &ClientInfo) -> Result<()> {
        require_self_or_admin(actor, user)?;
        let mut tx = self.core.db.begin().await?;
        let n = delete_sessions_except(&mut *tx, user, None).await?;
        crate::apps::revoke_all_for_user(&mut tx, user, "signed out everywhere").await?;
        self.core
            .audit_tx(&mut tx, actor, client, action::SESSION_REVOKED, Some(("user", user.0)), json!({"all": true, "count": n}))
            .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn unlock(&self, actor: &Actor, user: UserId, client: &ClientInfo) -> Result<()> {
        Self::require_system_or_admin(actor)?;
        let target = load_user(&self.core.db, user).await?;
        self.core.lockout.clear_all_addresses(&[lockout::account_key(user.0), lockout::name_key(&target.username)]).await?;
        self.core
            .audit_standalone(actor, client, action::UNLOCK, ch_core::audit::Outcome::Success, Some(("user", user.0)), json!({}))
            .await;
        Ok(())
    }

    async fn suspend(&self, actor: &Actor, user: UserId, client: &ClientInfo) -> Result<()> {
        Self::require_system_or_admin(actor)?;
        if actor.user_id() == Some(user) {
            return Err(Error::conflict("You cannot suspend your own account."));
        }
        load_user(&self.core.db, user).await?;
        let mut tx = self.core.db.begin().await?;
        sqlx::query("UPDATE users SET suspended_at = COALESCE(suspended_at, now()), updated_at = now() WHERE id = $1")
            .bind(user.0)
            .execute(&mut *tx)
            .await?;
        // Ends sessions now. Tokens and keys are refused by every lookup while suspended.
        delete_sessions_except(&mut *tx, user, None).await?;
        crate::apps::revoke_all_for_user(&mut tx, user, "account suspended").await?;
        self.core.audit_tx(&mut tx, actor, client, action::USER_SUSPENDED, Some(("user", user.0)), json!({"suspended": true})).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn set_can_run_ci(&self, actor: &Actor, user: UserId, value: bool, client: &ClientInfo) -> Result<()> {
        Self::require_system_or_admin(actor)?;
        let mut tx = self.core.db.begin().await?;
        let n = sqlx::query("UPDATE users SET can_run_ci = $2, updated_at = now() WHERE id = $1")
            .bind(user.0)
            .bind(value)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if n == 0 {
            return Err(Error::NotFound);
        }
        self.core.audit_tx(&mut tx, actor, client, action::USER_CI_CHANGED, Some(("user", user.0)), json!({"value": value})).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn list_ssh_keys(&self, actor: &Actor, user: UserId) -> Result<Vec<SshKey>> {
        if actor.is_anonymous() {
            return Err(Error::unauthorized());
        }
        actor.deny_app("list SSH keys")?;
        // Titles and last use are the account's own business: the person or a site admin (ch-uph).
        require_self_or_admin(actor, user)?;
        let rows: Vec<(i64, i64, String, String, String, DateTime<Utc>, Option<DateTime<Utc>>)> = sqlx::query_as(
            "SELECT id, user_id, title, key_type, fingerprint, created_at, last_used_at FROM ssh_keys WHERE user_id = $1 ORDER BY id",
        )
        .bind(user.0)
        .fetch_all(&self.core.db)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(id, user_id, title, key_type, fingerprint, created_at, last_used_at)| SshKey {
                id: SshKeyId(id),
                user_id: UserId(user_id),
                title,
                key_type,
                fingerprint,
                created_at,
                last_used_at,
            })
            .collect())
    }

    async fn add_ssh_key(&self, actor: &Actor, user: UserId, title: &str, public_key: &str, client: &ClientInfo) -> Result<SshKey> {
        if actor.user_id() != Some(user) {
            return Err(if actor.is_anonymous() {
                Error::unauthorized()
            } else {
                Error::forbidden("You can only add keys to your own account.")
            });
        }
        // A key signs in to Git over SSH as the whole account, with no scope and no expiry, so a
        // token may add one only when it already holds that much: `user` and `repo` (ch-fwm), and
        // `workflow`, since a push over SSH can change workflow files (ch-s7r).
        if let Actor::Token { scopes, .. } = actor
            && !(scopes.has("user") && scopes.has("repo") && scopes.has("workflow"))
        {
            return Err(Error::forbidden(
                "An SSH key acts as your whole account in Git, so this token needs the user, repo and workflow scopes",
            ));
        }
        let title = title.trim();
        if title.is_empty() || title.chars().count() > 100 {
            return Err(Error::Validation(vec![FieldError::custom("PublicKey", "title", "Give the key a title of up to 100 characters.")]));
        }
        let parsed = parse_public_key(public_key)?;
        let have: i64 =
            sqlx::query_scalar("SELECT count(*) FROM ssh_keys WHERE user_id = $1").bind(user.0).fetch_one(&self.core.db).await?;
        if have >= MAX_KEYS_PER_USER {
            return Err(Error::conflict("That is the most keys one account can have."));
        }
        let mut tx = self.core.db.begin().await?;
        let (id, created_at): (i64, DateTime<Utc>) = sqlx::query_as(
            "INSERT INTO ssh_keys (user_id, title, public_key, key_type, fingerprint) VALUES ($1, $2, $3, $4, $5) RETURNING id, created_at",
        )
        .bind(user.0)
        .bind(title)
        .bind(&parsed.normalized)
        .bind(&parsed.key_type)
        .bind(&parsed.fingerprint)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| {
            if is_unique_violation(&e, "") {
                Error::Validation(vec![FieldError::custom("PublicKey", "key", "That key is already in use on this server.")])
            } else {
                e.into()
            }
        })?;
        self.core
            .audit_tx(
                &mut tx,
                actor,
                client,
                action::SSH_KEY_ADDED,
                Some(("ssh_key", id)),
                json!({"fingerprint": parsed.fingerprint, "title": title}),
            )
            .await?;
        tx.commit().await?;
        Ok(SshKey {
            id: SshKeyId(id),
            user_id: user,
            title: title.into(),
            key_type: parsed.key_type,
            fingerprint: parsed.fingerprint,
            created_at,
            last_used_at: None,
        })
    }

    async fn delete_ssh_key(&self, actor: &Actor, id: SshKeyId, client: &ClientInfo) -> Result<()> {
        let owner: Option<(i64, String)> =
            sqlx::query_as("SELECT user_id, fingerprint FROM ssh_keys WHERE id = $1").bind(id.0).fetch_optional(&self.core.db).await?;
        let Some((owner, fingerprint)) = owner else { return Err(Error::NotFound) };
        require_self_or_admin(actor, UserId(owner))?;
        let mut tx = self.core.db.begin().await?;
        sqlx::query("DELETE FROM ssh_keys WHERE id = $1").bind(id.0).execute(&mut *tx).await?;
        self.core
            .audit_tx(&mut tx, actor, client, action::SSH_KEY_REMOVED, Some(("ssh_key", id.0)), json!({"fingerprint": fingerprint}))
            .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn list_tokens(&self, actor: &Actor, user: UserId) -> Result<Vec<AccessToken>> {
        require_self_or_admin(actor, user)?;
        // The names and scopes of a person's tokens are account data: a token needs `user` (ch-p9j).
        crate::core::need_user_scope(actor)?;
        #[allow(clippy::type_complexity)]
        let rows: Vec<(i64, i64, String, String, Vec<String>, DateTime<Utc>, Option<DateTime<Utc>>, DateTime<Utc>)> = sqlx::query_as(
            "SELECT id, user_id, name, prefix, scopes, expires_at, last_used_at, created_at FROM personal_access_tokens WHERE user_id = $1 AND revoked_at IS NULL ORDER BY id DESC",
        )
        .bind(user.0)
        .fetch_all(&self.core.db)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(id, user_id, name, prefix, scopes, expires_at, last_used_at, created_at)| AccessToken {
                id: TokenId(id),
                user_id: UserId(user_id),
                name,
                prefix,
                scopes,
                expires_at,
                last_used_at,
                created_at,
            })
            .collect())
    }

    async fn create_token(
        &self,
        actor: &Actor,
        user: UserId,
        name: &str,
        scopes: Vec<String>,
        expires_in_days: u32,
        client: &ClientInfo,
    ) -> Result<(AccessToken, String)> {
        // A token never mints tokens. Only a person signed in with a session or a password.
        match actor {
            Actor::User { id, via: AuthVia::Session | AuthVia::Basic, .. } if *id == user => {}
            Actor::Anonymous => return Err(Error::unauthorized()),
            _ => return Err(Error::forbidden("Sign in as yourself to create a token. A token cannot create tokens.")),
        }
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 100 {
            return Err(Error::Validation(vec![FieldError::custom("Token", "name", "Give the token a name of up to 100 characters.")]));
        }
        if scopes.is_empty() {
            return Err(Error::Validation(vec![FieldError::custom("Token", "scopes", "Pick at least one scope.")]));
        }
        if let Some(bad) = scopes.iter().find(|s| !Scopes::is_known(s)) {
            return Err(Error::Validation(vec![FieldError::custom("Token", "scopes", format!("Unknown scope: {bad}"))]));
        }
        let days = if expires_in_days == 0 { DEFAULT_TOKEN_DAYS } else { expires_in_days };
        if days > MAX_TOKEN_DAYS {
            return Err(Error::Validation(vec![FieldError::custom("Token", "expires_in_days", "A token can last one year at most.")]));
        }
        let active: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM personal_access_tokens WHERE user_id = $1 AND revoked_at IS NULL AND expires_at > now()",
        )
        .bind(user.0)
        .fetch_one(&self.core.db)
        .await?;
        if active >= MAX_ACTIVE_TOKENS {
            return Err(Error::conflict("Delete an old token before making another."));
        }
        let mut scopes = scopes;
        scopes.sort();
        scopes.dedup();
        let secret = tokens::new_token("chp_", 40);
        let prefix: String = secret.chars().take(8).collect();
        let expires_at = Utc::now() + Duration::days(days as i64);
        let mut tx = self.core.db.begin().await?;
        let (id, created_at): (i64, DateTime<Utc>) = sqlx::query_as("INSERT INTO personal_access_tokens (user_id, name, token_hash, prefix, scopes, expires_at) VALUES ($1, $2, $3, $4, $5, $6) RETURNING id, created_at")
            .bind(user.0)
            .bind(name)
            .bind(tokens::hash_token(&secret))
            .bind(&prefix)
            .bind(&scopes)
            .bind(expires_at)
            .fetch_one(&mut *tx)
            .await?;
        self.core
            .audit_tx(
                &mut tx,
                actor,
                client,
                action::TOKEN_CREATED,
                Some(("token", id)),
                json!({"name": name, "scopes": scopes, "expires_at": expires_at}),
            )
            .await?;
        tx.commit().await?;
        Ok((
            AccessToken { id: TokenId(id), user_id: user, name: name.into(), prefix, scopes, expires_at, last_used_at: None, created_at },
            secret,
        ))
    }

    async fn delete_token(&self, actor: &Actor, id: TokenId, client: &ClientInfo) -> Result<()> {
        let owner: Option<i64> = sqlx::query_scalar("SELECT user_id FROM personal_access_tokens WHERE id = $1 AND revoked_at IS NULL")
            .bind(id.0)
            .fetch_optional(&self.core.db)
            .await?;
        let Some(owner) = owner else { return Err(Error::NotFound) };
        require_self_or_admin(actor, UserId(owner))?;
        if let Actor::Token { token_id, .. } = actor
            && *token_id != id
        {
            return Err(Error::forbidden("A token can only delete itself."));
        }
        let mut tx = self.core.db.begin().await?;
        sqlx::query("UPDATE personal_access_tokens SET revoked_at = now() WHERE id = $1").bind(id.0).execute(&mut *tx).await?;
        self.core.audit_tx(&mut tx, actor, client, action::TOKEN_DELETED, Some(("token", id.0)), json!({})).await?;
        tx.commit().await?;
        Ok(())
    }
}

use crate::lockout;

pub(crate) fn exists(resource: &str, field: &str) -> Error {
    Error::Validation(vec![FieldError::new(resource, field, "already_exists")])
}

/// The password policy for a loaded account.
pub(crate) fn check_policy_for(u: &UserRow, password: &str) -> Result<()> {
    crate::password::check_policy(&u.username, u.email.as_deref().unwrap_or(""), password)
}
