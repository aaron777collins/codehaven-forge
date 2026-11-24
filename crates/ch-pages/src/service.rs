//! The `Pages` service: what the settings pages and the `/pages/open` hop call.

use crate::core::{Core, safe_inner_path};
use async_trait::async_trait;
use ch_core::audit::{AuditEvent, Outcome};
use ch_core::model::{ClientInfo, Visibility};
use ch_core::pages::*;
use ch_core::{Actor, AuthVia, Error, FieldError, Permission, RepoId, Result};
use std::sync::Arc;

pub struct PagesService {
    pub core: Arc<Core>,
}

/// What git's `check-ref-format --branch` allows, a little stricter (the same rule as the
/// workspace's branch names).
pub fn valid_branch(b: &str) -> bool {
    !b.is_empty()
        && b.len() <= 255
        && !b.starts_with('-')
        && !b.starts_with('/')
        && !b.ends_with('/')
        && !b.ends_with(".lock")
        && !b.ends_with('.')
        && !b.contains("..")
        && !b.contains("//")
        && !b.contains("@{")
        && b != "@"
        && !b.split('/').any(|c| c.starts_with('.'))
        && b.chars().all(|c| !c.is_control() && !matches!(c, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
}

fn invalid(field: &str, message: &str) -> Error {
    Error::Validation(vec![FieldError::custom("Pages", field, message)])
}

impl PagesService {
    fn on(&self) -> Result<()> {
        if self.core.host.is_some() { Ok(()) } else { Err(Error::NotFound) }
    }

    async fn need_admin(&self, actor: &Actor, repo: RepoId) -> Result<()> {
        self.on()?;
        self.core.authz.require(actor, repo, Permission::Admin).await
    }

    async fn repo_tier(&self, repo: RepoId) -> Result<Visibility> {
        let t: Option<String> =
            sqlx::query_scalar("SELECT access_tier FROM repositories WHERE id = $1").bind(repo.0).fetch_optional(&self.core.db).await?;
        t.as_deref().and_then(Visibility::parse).ok_or(Error::NotFound)
    }

    /// Queues a copy of the branch head and wakes the publisher.
    async fn queue(&self, actor: &Actor, repo: RepoId, branch: &str, folder: PagesFolder) -> Result<()> {
        let sha = self.core.git.resolve(repo, &format!("refs/heads/{branch}")).await.map_err(|e| match e {
            Error::NotFound => invalid("branch", "That branch does not exist."),
            e => e,
        })?;
        let tier = self.repo_tier(repo).await?;
        sqlx::query(
            "INSERT INTO pages_deployments (repo_id, commit_sha, branch, folder, tier, state, created_by) VALUES ($1, $2, $3, $4, $5, 'queued', $6)",
        )
        .bind(repo.0)
        .bind(&sha)
        .bind(branch)
        .bind(folder.as_str())
        .bind(tier.as_str())
        .bind(actor.user_id().map(|u| u.0))
        .execute(&self.core.db)
        .await?;
        self.core.wake.notify_one();
        Ok(())
    }

    async fn audit(&self, actor: &Actor, client: &ClientInfo, action: &str, repo: RepoId, details: serde_json::Value) {
        let ev = AuditEvent::new(actor, client, action, Outcome::Success).target("repo", repo.0).details(details);
        if let Err(e) = self.core.audit.record_standalone(ev).await {
            tracing::warn!(error = %e, action, "could not write a pages audit event");
        }
    }
}

#[async_trait]
impl Pages for PagesService {
    fn enabled(&self) -> bool {
        self.core.host.is_some()
    }

    async fn site(&self, actor: &Actor, repo: RepoId) -> Result<Option<PagesSite>> {
        self.need_admin(actor, repo).await?;
        #[derive(sqlx::FromRow)]
        struct S {
            branch: String,
            folder: String,
            audience: String,
            live_id: Option<i64>,
            owner: String,
            name: String,
            access_tier: String,
        }
        let s: Option<S> = sqlx::query_as(
            "SELECT s.branch, s.folder, s.audience, s.live_id, o.username::text AS owner, r.name::text AS name, r.access_tier \
             FROM pages_sites s JOIN repositories r ON r.id = s.repo_id JOIN users o ON o.id = r.owner_id WHERE s.repo_id = $1",
        )
        .bind(repo.0)
        .fetch_optional(&self.core.db)
        .await?;
        let Some(s) = s else { return Ok(None) };
        #[derive(sqlx::FromRow)]
        struct D {
            id: i64,
            commit_sha: String,
            branch: String,
            folder: String,
            tier: String,
            state: String,
            error: Option<String>,
            file_count: i64,
            total_bytes: i64,
            created_at: chrono::DateTime<chrono::Utc>,
            finished_at: Option<chrono::DateTime<chrono::Utc>>,
        }
        let rows: Vec<D> = sqlx::query_as(
            "SELECT id, commit_sha, branch, folder, tier, state, error, file_count, total_bytes, created_at, finished_at \
             FROM pages_deployments WHERE repo_id = $1 ORDER BY id DESC LIMIT 20",
        )
        .bind(repo.0)
        .fetch_all(&self.core.db)
        .await?;
        let tier = |t: &str| Visibility::parse(t).unwrap_or(Visibility::Private);
        let deployments: Vec<PagesDeployment> = rows
            .into_iter()
            .map(|d| PagesDeployment {
                live: Some(d.id) == s.live_id,
                id: d.id,
                commit_sha: d.commit_sha,
                branch: d.branch,
                folder: PagesFolder::parse(&d.folder).unwrap_or(PagesFolder::Root),
                tier: tier(&d.tier),
                state: DeployState::parse(&d.state).unwrap_or(DeployState::Failed),
                error: d.error,
                file_count: d.file_count,
                total_bytes: d.total_bytes,
                created_at: d.created_at,
                finished_at: d.finished_at,
            })
            .collect();
        let audience = tier(&s.audience);
        let serving_tier = deployments.iter().find(|d| d.live).map(|d| serving_tier(audience, d.tier, tier(&s.access_tier)));
        let full = self.core.full_mode().await;
        Ok(Some(PagesSite {
            repo_id: repo,
            branch: s.branch,
            folder: PagesFolder::parse(&s.folder).unwrap_or(PagesFolder::Root),
            audience,
            url: self.core.site_url(&s.owner, &s.name, full),
            serving_tier,
            deployments,
        }))
    }

    async fn configure(&self, actor: &Actor, repo: RepoId, input: PagesConfigInput, client: &ClientInfo) -> Result<PagesSite> {
        self.need_admin(actor, repo).await?;
        if !valid_branch(&input.branch) {
            return Err(invalid("branch", "That is not a branch name."));
        }
        let tier = self.repo_tier(repo).await?;
        if input.audience > tier {
            return Err(invalid("audience", "A site can never be seen by more people than its repository."));
        }
        let existed: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pages_sites WHERE repo_id = $1)")
            .bind(repo.0)
            .fetch_one(&self.core.db)
            .await?;
        // The branch must exist before anything is saved.
        self.core.git.resolve(repo, &format!("refs/heads/{}", input.branch)).await.map_err(|e| match e {
            Error::NotFound => invalid("branch", "That branch does not exist."),
            e => e,
        })?;
        sqlx::query(
            "INSERT INTO pages_sites (repo_id, branch, folder, audience, created_by) VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (repo_id) DO UPDATE SET branch = EXCLUDED.branch, folder = EXCLUDED.folder, audience = EXCLUDED.audience, updated_at = now()",
        )
        .bind(repo.0)
        .bind(&input.branch)
        .bind(input.folder.as_str())
        .bind(input.audience.as_str())
        .bind(actor.user_id().map(|u| u.0))
        .execute(&self.core.db)
        .await?;
        self.queue(actor, repo, &input.branch, input.folder).await?;
        let action = if existed { "pages.changed" } else { "pages.enabled" };
        self.audit(
            actor,
            client,
            action,
            repo,
            serde_json::json!({"branch": input.branch, "folder": input.folder.label(), "audience": input.audience.as_str()}),
        )
        .await;
        self.site(actor, repo).await?.ok_or(Error::NotFound)
    }

    async fn deploy(&self, actor: &Actor, repo: RepoId, client: &ClientInfo) -> Result<()> {
        self.need_admin(actor, repo).await?;
        let row: Option<(String, String)> =
            sqlx::query_as("SELECT branch, folder FROM pages_sites WHERE repo_id = $1").bind(repo.0).fetch_optional(&self.core.db).await?;
        let Some((branch, folder)) = row else { return Err(Error::NotFound) };
        self.queue(actor, repo, &branch, PagesFolder::parse(&folder).unwrap_or(PagesFolder::Root)).await?;
        self.audit(actor, client, "pages.deploy", repo, serde_json::json!({"branch": branch})).await;
        Ok(())
    }

    async fn rollback(&self, actor: &Actor, repo: RepoId, deployment: i64, client: &ClientInfo) -> Result<()> {
        self.need_admin(actor, repo).await?;
        let done = sqlx::query(
            "UPDATE pages_sites s SET live_id = d.id, updated_at = now() FROM pages_deployments d \
             WHERE s.repo_id = $1 AND d.id = $2 AND d.repo_id = s.repo_id AND d.state = 'ready'",
        )
        .bind(repo.0)
        .bind(deployment)
        .execute(&self.core.db)
        .await?;
        if done.rows_affected() == 0 {
            return Err(Error::NotFound);
        }
        self.audit(actor, client, "pages.rollback", repo, serde_json::json!({"deployment": deployment})).await;
        Ok(())
    }

    async fn disable(&self, actor: &Actor, repo: RepoId, client: &ClientInfo) -> Result<()> {
        self.need_admin(actor, repo).await?;
        // Every copy and file row goes with the site, and each file row releases its blob.
        let mut tx = self.core.db.begin().await?;
        sqlx::query("UPDATE pages_sites SET live_id = NULL WHERE repo_id = $1").bind(repo.0).execute(&mut *tx).await?;
        let gone = sqlx::query("DELETE FROM pages_sites WHERE repo_id = $1").bind(repo.0).execute(&mut *tx).await?;
        tx.commit().await?;
        if gone.rows_affected() > 0 {
            self.audit(actor, client, "pages.disabled", repo, serde_json::Value::Null).await;
        }
        Ok(())
    }

    async fn open(&self, actor: &Actor, owner: &str, project: &str, path: &str) -> Result<String> {
        self.on()?;
        // Only a person in a browser: never a token, an app or a job.
        let user = match actor {
            Actor::User { id, via: AuthVia::Session, .. } => *id,
            _ => return Err(Error::NotFound),
        };
        let site = self.core.find_site(owner, project).await?.ok_or(Error::NotFound)?;
        if !self.core.may_view(&site, Some(user)).await? {
            return Err(Error::NotFound);
        }
        let exp = chrono::Utc::now().timestamp() + crate::token::TTL_SECS;
        let token = crate::token::mint(&self.core.key, &crate::token::Grant { repo: site.repo, user, exp });
        let inner = safe_inner_path(path);
        let full = self.core.full_mode().await && valid_owner_label(&site.owner);
        Ok(if full {
            format!("{}/-/t/{token}/{}{inner}", self.core.owner_origin(&site.owner), site.name)
        } else {
            format!("{}/-/t/{token}/{}/{}{inner}", self.core.origin(), site.owner, site.name)
        })
    }

    async fn settings(&self) -> Result<PagesSettings> {
        Ok(PagesSettings { full_mode: self.core.full_mode().await })
    }

    async fn set_settings(&self, actor: &Actor, settings: PagesSettings, client: &ClientInfo) -> Result<()> {
        self.on()?;
        if !actor.is_site_admin() {
            return Err(Error::forbidden("Only a site admin with two-factor sign-in can change this."));
        }
        let mut tx = self.core.db.begin().await?;
        let before: bool = sqlx::query_scalar("SELECT full_mode FROM pages_settings FOR UPDATE").fetch_one(&mut *tx).await?;
        sqlx::query("UPDATE pages_settings SET full_mode = $1").bind(settings.full_mode).execute(&mut *tx).await?;
        if before != settings.full_mode {
            let ev = AuditEvent::new(actor, client, "pages.full_mode_changed", Outcome::Success)
                .details(serde_json::json!({"from": before, "to": settings.full_mode}));
            self.core.audit.record(&mut tx, ev).await?;
        }
        tx.commit().await?;
        self.core.forget_full_mode();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_names() {
        assert!(valid_branch("main"));
        assert!(valid_branch("gh-pages"));
        assert!(valid_branch("docs/site"));
        for bad in ["", "-x", "a..b", "a b", "a:b", "refs/../x", ".hidden", "a/.b", "x.lock", "@", "a@{1}", "a\u{7}"] {
            assert!(!valid_branch(bad), "{bad:?}");
        }
    }
}
