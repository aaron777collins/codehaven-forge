//! `Directory`: the admin user list, admin flag, emails and profile.

use crate::accounts::exists;
use crate::core::*;
use crate::util::*;
use async_trait::async_trait;
use ch_core::Actor;
use ch_core::audit::action;
use ch_core::error::{Error, FieldError, Result};
use ch_core::identity::*;
use ch_core::ids::*;
use ch_core::model::*;
use ch_core::services::Directory;
use serde_json::json;
use std::sync::Arc;

pub struct DirectoryService {
    pub(crate) core: Arc<Core>,
}

const MAX_EMAILS: i64 = 10;

impl DirectoryService {
    pub fn new(core: Arc<Core>) -> Self {
        Self { core }
    }

    fn own(actor: &Actor, user: UserId) -> Result<()> {
        match actor {
            Actor::User { id, .. } if *id == user => Ok(()),
            Actor::Token { user_id, scopes, .. } if *user_id == user && (scopes.has("user") || scopes.has("repo")) => Ok(()),
            Actor::Anonymous => Err(Error::unauthorized()),
            _ => Err(Error::forbidden("You can only change your own account.")),
        }
    }
}

#[async_trait]
impl Directory for DirectoryService {
    async fn list_users(&self, actor: &Actor, filter: UserListFilter) -> Result<Vec<User>> {
        require_site_admin(actor)?;
        let per_page = if filter.per_page == 0 { 30 } else { filter.per_page.min(100) } as i64;
        let page = filter.page.max(1) as i64;
        let like = filter
            .query
            .as_deref()
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .map(|q| format!("%{}%", q.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")));
        let rows: Vec<UserRow> = sqlx::query_as(&format!(
            "SELECT {USER_COLS} FROM users u WHERE u.kind = 'user' AND ($1::text IS NULL OR u.username::text ILIKE $1 OR u.display_name ILIKE $1
                 OR EXISTS (SELECT 1 FROM user_emails e WHERE e.user_id = u.id AND e.email::text ILIKE $1))
             ORDER BY u.id LIMIT $2 OFFSET $3"
        ))
        .bind(like)
        .bind(per_page)
        .bind((page - 1) * per_page)
        .fetch_all(&self.core.db)
        .await?;
        Ok(rows.into_iter().map(User::from).collect())
    }

    async fn set_admin(&self, actor: &Actor, user: UserId, value: bool, client: &ClientInfo) -> Result<()> {
        require_site_admin(actor)?;
        let mut tx = self.core.db.begin().await?;
        // Lock every admin row so two admins removing each other cannot both succeed.
        let admins: Vec<(i64,)> =
            sqlx::query_as("SELECT id FROM users WHERE is_admin AND suspended_at IS NULL AND kind = 'user' FOR UPDATE")
                .fetch_all(&mut *tx)
                .await?;
        let target: Option<(bool, String)> = sqlx::query_as("SELECT suspended_at IS NOT NULL, kind FROM users WHERE id = $1 FOR UPDATE")
            .bind(user.0)
            .fetch_optional(&mut *tx)
            .await?;
        match target {
            Some((suspended, kind)) if kind == "user" => {
                if value && suspended {
                    return Err(Error::conflict("Unsuspend the account before making it an admin."));
                }
            }
            _ => return Err(Error::NotFound),
        }
        if !value && admins.iter().all(|(id,)| *id == user.0) {
            return Err(Error::conflict("That is the last admin. Make someone else an admin first."));
        }
        sqlx::query("UPDATE users SET is_admin = $2, updated_at = now() WHERE id = $1").bind(user.0).bind(value).execute(&mut *tx).await?;
        // Privilege changed, so the target signs in again.
        delete_sessions_except(&mut *tx, user, None).await?;
        self.core.audit_tx(&mut tx, actor, client, action::USER_ADMIN_GRANTED, Some(("user", user.0)), json!({"value": value})).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn unsuspend(&self, actor: &Actor, user: UserId, client: &ClientInfo) -> Result<()> {
        require_site_admin(actor)?;
        let mut tx = self.core.db.begin().await?;
        let n = sqlx::query("UPDATE users SET suspended_at = NULL, updated_at = now() WHERE id = $1 AND kind = 'user'")
            .bind(user.0)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if n == 0 {
            return Err(Error::NotFound);
        }
        self.core.audit_tx(&mut tx, actor, client, action::USER_UNSUSPENDED, Some(("user", user.0)), json!({})).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn emails(&self, actor: &Actor, user: UserId) -> Result<Vec<EmailAddress>> {
        require_self_or_admin(actor, user)?;
        let rows: Vec<(String, bool, bool)> = sqlx::query_as(
            "SELECT email::text, is_primary, verified_at IS NOT NULL FROM user_emails WHERE user_id = $1 ORDER BY is_primary DESC, id",
        )
        .bind(user.0)
        .fetch_all(&self.core.db)
        .await?;
        Ok(rows.into_iter().map(|(email, primary, verified)| EmailAddress { email, primary, verified }).collect())
    }

    async fn add_email(&self, actor: &Actor, user: UserId, email: &str, client: &ClientInfo) -> Result<EmailAddress> {
        Self::own(actor, user)?;
        let email = normalize_email(email);
        validate_email(&email)?;
        let have: i64 =
            sqlx::query_scalar("SELECT count(*) FROM user_emails WHERE user_id = $1").bind(user.0).fetch_one(&self.core.db).await?;
        if have >= MAX_EMAILS {
            return Err(Error::conflict("That is the most email addresses one account can have."));
        }
        let mut tx = self.core.db.begin().await?;
        sqlx::query("INSERT INTO user_emails (user_id, email, is_primary) VALUES ($1, $2, false)")
            .bind(user.0)
            .bind(&email)
            .execute(&mut *tx)
            .await
            .map_err(|e| if is_unique_violation(&e, "") { exists("Email", "email") } else { e.into() })?;
        self.core.audit_tx(&mut tx, actor, client, action::EMAIL_ADDED, Some(("user", user.0)), json!({"email": email})).await?;
        tx.commit().await?;
        Ok(EmailAddress { email, primary: false, verified: false })
    }

    async fn remove_email(&self, actor: &Actor, user: UserId, email: &str, client: &ClientInfo) -> Result<()> {
        Self::own(actor, user)?;
        let mut tx = self.core.db.begin().await?;
        let row: Option<(i64, bool)> = sqlx::query_as("SELECT id, is_primary FROM user_emails WHERE user_id = $1 AND email = $2::citext")
            .bind(user.0)
            .bind(email)
            .fetch_optional(&mut *tx)
            .await?;
        let Some((id, primary)) = row else { return Err(Error::NotFound) };
        if primary {
            return Err(Error::conflict("That is your primary address. Make another one primary first."));
        }
        sqlx::query("DELETE FROM user_emails WHERE id = $1").bind(id).execute(&mut *tx).await?;
        self.core.audit_tx(&mut tx, actor, client, action::EMAIL_REMOVED, Some(("user", user.0)), json!({"email": email})).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn set_primary_email(&self, actor: &Actor, user: UserId, email: &str, client: &ClientInfo) -> Result<()> {
        Self::own(actor, user)?;
        let mut tx = self.core.db.begin().await?;
        let row: Option<(i64, bool)> =
            sqlx::query_as("SELECT id, verified_at IS NOT NULL FROM user_emails WHERE user_id = $1 AND email = $2::citext FOR UPDATE")
                .bind(user.0)
                .bind(email)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((id, verified)) = row else { return Err(Error::NotFound) };
        if !verified {
            return Err(Error::conflict("Only a verified address can be primary. This server cannot send mail yet, so ask an admin."));
        }
        sqlx::query("UPDATE user_emails SET is_primary = false WHERE user_id = $1 AND is_primary").bind(user.0).execute(&mut *tx).await?;
        sqlx::query("UPDATE user_emails SET is_primary = true WHERE id = $1").bind(id).execute(&mut *tx).await?;
        self.core
            .audit_tx(&mut tx, actor, client, action::EMAIL_ADDED, Some(("user", user.0)), json!({"email": email, "primary": true}))
            .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn update_profile(&self, actor: &Actor, user: UserId, profile: Profile, _client: &ClientInfo) -> Result<User> {
        Self::own(actor, user)?;
        if let Some(n) = &profile.display_name
            && n.chars().count() > 100
        {
            return Err(Error::Validation(vec![FieldError::custom("User", "name", "Use at most 100 characters.")]));
        }
        if let Some(d) = &profile.description
            && d.chars().count() > 500
        {
            return Err(Error::Validation(vec![FieldError::custom("User", "bio", "Use at most 500 characters.")]));
        }
        sqlx::query("UPDATE users SET display_name = COALESCE($2, display_name), description = COALESCE($3, description), updated_at = now() WHERE id = $1")
            .bind(user.0)
            .bind(profile.display_name.as_deref().map(str::trim))
            .bind(profile.description.as_deref().map(str::trim))
            .execute(&self.core.db)
            .await?;
        Ok(load_user(&self.core.db, user).await?.into())
    }
}
