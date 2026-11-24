//! `Notifications`: the inbox, read and done state, watch levels and thread subscriptions.

use async_trait::async_trait;
use ch_core::error::{Error, Result};
use ch_core::ids::*;
use ch_core::model::Notification;
use ch_core::services::{Authz, Notifications};
use ch_core::{Actor, Permission};
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;

pub const LEVELS: &[&str] = &["all", "participating", "ignore"];

pub struct NotificationsService {
    pool: PgPool,
    authz: Arc<dyn Authz>,
}

impl NotificationsService {
    pub fn new(pool: PgPool, authz: Arc<dyn Authz>) -> Self {
        Self { pool, authz }
    }

    /// The account an inbox call is for. A token needs the `notifications` or `repo` scope.
    fn owner(actor: &Actor) -> Result<UserId> {
        match actor {
            Actor::User { id, .. } => Ok(*id),
            Actor::Token { user_id, scopes, .. } if scopes.has("notifications") || scopes.has("repo") => Ok(*user_id),
            Actor::Token { .. } => Err(Error::forbidden("This token needs the notifications scope")),
            // A connected app sees only the notifications of repositories it can read. The
            // filtering below judges each one as the app, never as the person.
            Actor::App { user_id, .. } => Ok(*user_id),
            Actor::Anonymous => Err(Error::unauthorized()),
            _ => Err(Error::forbidden("Not available to this kind of credential")),
        }
    }
}

#[async_trait]
impl Notifications for NotificationsService {
    async fn inbox(&self, actor: &Actor, unread_only: bool, page: u32, per_page: u32) -> Result<Vec<Notification>> {
        let user = Self::owner(actor)?;
        let per_page = if per_page == 0 { 30 } else { per_page.min(100) } as i64;
        let offset = (page.max(1) as i64 - 1) * per_page;
        #[allow(clippy::type_complexity)]
        let rows: Vec<(i64, i64, Option<i64>, String, String, i64, String, bool, DateTime<Utc>, Option<i64>, Option<String>)> =
            sqlx::query_as(
                "SELECT n.id, n.user_id, n.repo_id, n.reason, n.subject_type, n.subject_id, n.title, n.unread, n.updated_at, i.number,
                    CASE WHEN i.id IS NULL THEN NULL WHEN p.merged_at IS NOT NULL THEN 'merged' WHEN i.open THEN 'open' ELSE 'closed' END
             FROM notifications n
             LEFT JOIN issues i ON i.id = n.subject_id AND n.subject_type IN ('issue', 'pull')
             LEFT JOIN pull_requests p ON p.issue_id = i.id
             WHERE n.user_id = $1 AND NOT n.done AND ($2 = false OR n.unread) ORDER BY n.updated_at DESC, n.id DESC LIMIT $3 OFFSET $4",
            )
            .bind(user.0)
            .bind(unread_only)
            .bind(per_page)
            .bind(offset)
            .fetch_all(&self.pool)
            .await?;
        // Access can be taken away after a notification was written, and the title of a thread
        // someone can no longer open must not stay visible in their inbox.
        // Judged as the person themselves, not by a token's scopes: the inbox is theirs.
        // A connected app is judged as itself, so what it was not given stays out.
        let as_user = if actor.is_app() {
            actor.clone()
        } else {
            Actor::User { id: user, is_admin: false, has_2fa: false, via: ch_core::AuthVia::Session }
        };
        let mut readable: HashMap<i64, bool> = HashMap::new();
        let mut out = Vec::new();
        for (id, user_id, repo_id, reason, subject_type, subject_id, title, unread, updated_at, number, state) in rows {
            // A notification that belongs to no repository cannot be checked against a grant.
            if repo_id.is_none() && actor.is_app() {
                continue;
            }
            if let Some(r) = repo_id {
                let ok = match readable.get(&r) {
                    Some(v) => *v,
                    None => {
                        let v = self.authz.repo_permission(&as_user, RepoId(r)).await? >= Permission::View;
                        readable.insert(r, v);
                        v
                    }
                };
                if !ok {
                    continue;
                }
            }
            out.push(Notification {
                id: NotificationId(id),
                user_id: UserId(user_id),
                repo_id: repo_id.map(RepoId),
                reason,
                subject_type,
                subject_id,
                title,
                unread,
                updated_at,
                number,
                state,
            });
        }
        Ok(out)
    }

    async fn unread_count(&self, actor: &Actor) -> Result<i64> {
        let user = Self::owner(actor)?;
        if actor.is_app() {
            // Counted by repository, so the number never includes a thread the app cannot see.
            let rows: Vec<(i64, i64)> = sqlx::query_as("SELECT repo_id, count(*) FROM notifications WHERE user_id = $1 AND unread AND NOT done AND repo_id IS NOT NULL GROUP BY repo_id")
                .bind(user.0)
                .fetch_all(&self.pool)
                .await?;
            let mut total = 0;
            for (repo, n) in rows {
                if self.authz.repo_permission(actor, RepoId(repo)).await? >= Permission::View {
                    total += n;
                }
            }
            return Ok(total);
        }
        Ok(sqlx::query_scalar("SELECT count(*) FROM notifications WHERE user_id = $1 AND unread AND NOT done")
            .bind(user.0)
            .fetch_one(&self.pool)
            .await?)
    }

    async fn mark_read(&self, actor: &Actor, id: Option<NotificationId>) -> Result<()> {
        let user = Self::owner(actor)?;
        if let Actor::App { grant, .. } = actor {
            // One notification, for a repository the app may write to. Never "all of them".
            let Some(id) = id else { return Err(Error::forbidden("A connected app can mark one notification read at a time.")) };
            let repo: Option<Option<i64>> = sqlx::query_scalar("SELECT repo_id FROM notifications WHERE id = $1 AND user_id = $2")
                .bind(id.0)
                .bind(user.0)
                .fetch_optional(&self.pool)
                .await?;
            match repo {
                Some(Some(r)) if grant.write.has(RepoId(r)) => {}
                _ => return Err(Error::NotFound),
            }
        }
        match id {
            Some(id) => {
                let n = sqlx::query("UPDATE notifications SET unread = false, last_read_at = now() WHERE id = $1 AND user_id = $2")
                    .bind(id.0)
                    .bind(user.0)
                    .execute(&self.pool)
                    .await?
                    .rows_affected();
                if n == 0 {
                    return Err(Error::NotFound);
                }
            }
            None => {
                sqlx::query("UPDATE notifications SET unread = false, last_read_at = now() WHERE user_id = $1 AND unread")
                    .bind(user.0)
                    .execute(&self.pool)
                    .await?;
            }
        }
        Ok(())
    }

    async fn subscribe(&self, actor: &Actor, repo: RepoId, level: &str) -> Result<()> {
        let user = Self::owner(actor)?;
        if !LEVELS.contains(&level) {
            return Err(Error::invalid("Subscription", "level", "Use all, participating or ignore."));
        }
        // You can only watch what you can read.
        self.authz.require(actor, repo, Permission::View).await?;
        sqlx::query("INSERT INTO watches (user_id, repo_id, level) VALUES ($1, $2, $3) ON CONFLICT (user_id, repo_id) DO UPDATE SET level = EXCLUDED.level").bind(user.0).bind(repo.0).bind(level).execute(&self.pool).await?;
        Ok(())
    }

    async fn watch_level(&self, actor: &Actor, repo: RepoId) -> Result<String> {
        let user = Self::owner(actor)?;
        self.authz.require(actor, repo, Permission::View).await?;
        let level: Option<String> = sqlx::query_scalar("SELECT level FROM watches WHERE user_id = $1 AND repo_id = $2")
            .bind(user.0)
            .bind(repo.0)
            .fetch_optional(&self.pool)
            .await?;
        Ok(level.unwrap_or_else(|| "participating".into()))
    }

    async fn mark_done(&self, actor: &Actor, id: NotificationId) -> Result<()> {
        let user = Self::owner(actor)?;
        let n = sqlx::query("UPDATE notifications SET done = true, unread = false, last_read_at = COALESCE(last_read_at, now()) WHERE id = $1 AND user_id = $2").bind(id.0).bind(user.0).execute(&self.pool).await?.rows_affected();
        if n == 0 {
            return Err(Error::NotFound);
        }
        Ok(())
    }

    async fn set_thread_subscription(&self, actor: &Actor, subject_type: &str, subject_id: i64, subscribed: bool) -> Result<()> {
        let user = Self::owner(actor)?;
        if subject_type != "issue" && subject_type != "pull" {
            return Err(Error::invalid("Subscription", "subject_type", "Only issues and pull requests have thread subscriptions."));
        }
        let repo: Option<i64> =
            sqlx::query_scalar("SELECT repo_id FROM issues WHERE id = $1").bind(subject_id).fetch_optional(&self.pool).await?;
        let Some(repo) = repo else { return Err(Error::NotFound) };
        self.authz.require(actor, RepoId(repo), Permission::View).await?;
        sqlx::query(
            "INSERT INTO issue_subscriptions (issue_id, user_id, subscribed, reason) VALUES ($1, $2, $3, 'manual') ON CONFLICT (issue_id, user_id) DO UPDATE SET subscribed = EXCLUDED.subscribed, reason = 'manual'",
        )
        .bind(subject_id)
        .bind(user.0)
        .bind(subscribed)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
