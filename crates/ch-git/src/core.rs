//! `GitCore`: the one concrete service behind `Repos`, `GitService` and `GitTransport`.
//!
//! It holds what every operation needs (database pool, audit log, event bus, the permission
//! service, the on-disk store) and the shared helpers they use. `build` inserts it into
//! `AppState.ext`, so the REST handlers and the hook server get the concrete type back
//! without a global.

use crate::command::{GitCommand, GitError, Output};
use crate::hooks::PushRegistry;
use crate::store::Store;
use ch_core::audit::{AuditEvent, Outcome};
use ch_core::model::{BranchProtection, ClientInfo, ContributionPolicy, Repo, Visibility};
use ch_core::services::{Audit, Authz, EventBus};
use ch_core::{Actor, Config, Error, Permission, RepoId, Result, UserId};
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use std::sync::Arc;
use tokio::sync::Semaphore;

/// Audit action names this crate records. They belong in `ch_core::audit::action` and move
/// there when the streams are integrated; kept here so this stream touches no contested file.
pub mod audit_action {
    pub const REPO_CREATED: &str = "repo.created";
    pub const REPO_RENAMED: &str = "repo.renamed";
    pub const REPO_FORKED: &str = "repo.forked";
    pub const REPO_ARCHIVED: &str = "repo.archived_changed";
    pub const REPO_SETTINGS: &str = "repo.settings_changed";
    pub const COLLABORATOR_ADDED: &str = "repo.collaborator_added";
    pub const COLLABORATOR_REMOVED: &str = "repo.collaborator_removed";
    pub const PUSH_REJECTED: &str = "repo.push_rejected";
}

pub const ZERO_SHA: &str = "0000000000000000000000000000000000000000";

pub struct GitCore {
    pub(crate) cfg: Arc<Config>,
    pub(crate) db: PgPool,
    pub(crate) audit: Arc<dyn Audit>,
    pub(crate) events: Arc<dyn EventBus>,
    pub(crate) authz: Arc<dyn Authz>,
    pub store: Store,
    pub(crate) pushes: PushRegistry,
    /// Where `codehaven hook` finds the server. `workers` binds it before the server accepts
    /// any request, so a push can never arrive before there is something to answer it.
    pub(crate) hook_socket: std::path::PathBuf,
    /// Caps how many git processes run at once, so a burst of page loads cannot fork-bomb
    /// the container.
    pub(crate) procs: Semaphore,
    /// Clones, fetches and pushes by signed-in callers at once. Signed-out ones have their own
    /// pools in the gate and never take these (ch-ulq).
    pub(crate) transfers: Arc<Semaphore>,
    pub(crate) pushed: crate::maintenance::Pushed,
    pub(crate) diff_cache: moka::future::Cache<(i64, String, String), Arc<ch_core::model::Diff>>,
    /// Running writes per repository, so a store rebuild can wait for them and keep new ones out.
    pub(crate) writes: crate::tier::WriteGate,
    /// This core, for work that must finish even if the request that started it is dropped.
    pub(crate) me: std::sync::Weak<GitCore>,
}

#[derive(sqlx::FromRow)]
pub(crate) struct RepoRow {
    pub id: i64,
    pub owner_id: i64,
    pub owner_name: String,
    pub name: String,
    pub description: String,
    pub access_tier: String,
    pub contribution_policy: String,
    pub default_branch: String,
    pub archived: bool,
    pub is_empty: bool,
    pub fork_parent_id: Option<i64>,
    pub topics: Vec<String>,
    pub pushed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

pub(crate) const REPO_SELECT: &str = "SELECT r.id, r.owner_id, u.username::text AS owner_name, r.name::text AS name, \
     r.description, r.access_tier, r.contribution_policy, r.default_branch, r.archived, r.is_empty, r.fork_parent_id, \
     r.topics, r.pushed_at, r.created_at \
     FROM repositories r JOIN users u ON u.id = r.owner_id";

impl RepoRow {
    pub(crate) fn into_repo(self) -> Repo {
        Repo {
            id: RepoId(self.id),
            owner_id: UserId(self.owner_id),
            owner_name: self.owner_name,
            name: self.name,
            description: self.description,
            // The column has a CHECK on these three. Anything else reads as private, never wider.
            visibility: Visibility::parse(&self.access_tier).unwrap_or(Visibility::Private),
            default_branch: self.default_branch,
            archived: self.archived,
            is_empty: self.is_empty,
            fork_parent_id: self.fork_parent_id.map(RepoId),
            topics: self.topics,
            pushed_at: self.pushed_at,
            created_at: self.created_at,
            contribution_policy: ContributionPolicy::parse(&self.contribution_policy).unwrap_or(ContributionPolicy::Grants),
        }
    }
}

impl GitCore {
    /// Runs a git command while holding one of the process permits.
    pub(crate) async fn exec(&self, cmd: GitCommand) -> std::result::Result<Output, GitError> {
        let _permit = self.procs.acquire().await.map_err(|_| GitError::Spawn(std::io::Error::other("shutting down")))?;
        cmd.run().await
    }

    /// Like `exec` but the exit code is the caller's business.
    pub(crate) async fn exec_any(&self, cmd: GitCommand) -> std::result::Result<Output, GitError> {
        let _permit = self.procs.acquire().await.map_err(|_| GitError::Spawn(std::io::Error::other("shutting down")))?;
        cmd.run_any().await
    }

    /// A git command in the repository. The repository must exist on disk.
    pub(crate) fn git(&self, repo: RepoId) -> GitCommand {
        self.store.cmd(repo)
    }

    /// The repository row, with no permission check. Internal use and after a check.
    pub(crate) async fn repo_row(&self, id: RepoId) -> Result<Repo> {
        let row: Option<RepoRow> = sqlx::query_as(&format!("{REPO_SELECT} WHERE r.id = $1")).bind(id.0).fetch_optional(&self.db).await?;
        row.map(RepoRow::into_repo).ok_or(Error::NotFound)
    }

    /// Finds `owner/name`, following a rename or transfer redirect. No permission check.
    pub(crate) async fn repo_by_name_unchecked(&self, owner: &str, name: &str) -> Result<Repo> {
        let name = name.strip_suffix(".git").unwrap_or(name);
        let row: Option<RepoRow> = sqlx::query_as(&format!("{REPO_SELECT} WHERE u.username = $1::citext AND r.name = $2::citext"))
            .bind(owner)
            .bind(name)
            .fetch_optional(&self.db)
            .await?;
        if let Some(r) = row {
            return Ok(r.into_repo());
        }
        let row: Option<RepoRow> = sqlx::query_as(&format!(
            "{REPO_SELECT} WHERE r.id = (SELECT repo_id FROM repo_redirects WHERE old_owner = $1::citext AND old_name = $2::citext)"
        ))
        .bind(owner)
        .bind(name)
        .fetch_optional(&self.db)
        .await?;
        row.map(RepoRow::into_repo).ok_or(Error::NotFound)
    }

    /// Checks the actor has `needed` on the repository. A write on an archived repository
    /// fails with a plain explanation, for everyone including admins.
    pub(crate) async fn require(&self, actor: &Actor, repo: &Repo, needed: Permission) -> Result<()> {
        if actor.is_system() {
            return Ok(());
        }
        self.authz.require(actor, repo.id, needed).await?;
        if repo.archived && needed >= Permission::Write {
            return Err(Error::forbidden("This repository is archived and read-only."));
        }
        Ok(())
    }

    pub(crate) async fn protections_of(&self, repo: RepoId) -> Result<Vec<BranchProtection>> {
        #[derive(sqlx::FromRow)]
        struct P {
            pattern: String,
            required_approvals: i32,
            dismiss_stale_reviews: bool,
            require_code_owner_review: bool,
            required_checks: Vec<String>,
            strict: bool,
            require_resolved_threads: bool,
            block_force_push: bool,
            block_deletion: bool,
            allow_direct_push: bool,
        }
        let rows: Vec<P> = sqlx::query_as(
            "SELECT pattern, required_approvals, dismiss_stale_reviews, require_code_owner_review, required_checks, strict, \
             require_resolved_threads, block_force_push, block_deletion, allow_direct_push \
             FROM branch_protections WHERE repo_id = $1 ORDER BY pattern",
        )
        .bind(repo.0)
        .fetch_all(&self.db)
        .await?;
        Ok(rows
            .into_iter()
            .map(|p| BranchProtection {
                pattern: p.pattern,
                required_approvals: p.required_approvals.max(0) as u32,
                dismiss_stale_reviews: p.dismiss_stale_reviews,
                require_code_owner_review: p.require_code_owner_review,
                required_checks: p.required_checks,
                strict: p.strict,
                require_resolved_threads: p.require_resolved_threads,
                block_force_push: p.block_force_push,
                block_deletion: p.block_deletion,
                allow_direct_push: p.allow_direct_push,
            })
            .collect())
    }

    /// Records an audit event outside any other change, for refusals worth remembering.
    pub(crate) async fn audit_standalone(
        &self,
        actor: &Actor,
        client: &ClientInfo,
        action: &str,
        outcome: Outcome,
        repo: RepoId,
        detail: serde_json::Value,
    ) {
        let ev = AuditEvent::new(actor, client, action, outcome).target("repository", repo.0).details(detail);
        if let Err(e) = self.audit.record_standalone(ev).await {
            tracing::warn!(error = %e, "could not record audit event");
        }
    }

    /// The identity to put on commits the server makes for a user (the README at creation,
    /// web edits, merges): their display name and their noreply address. Account emails
    /// never go into history.
    pub(crate) async fn commit_identity(&self, user: UserId) -> Result<(String, String)> {
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT u.username::text, u.display_name \
             FROM users u WHERE u.id = $1",
        )
        .bind(user.0)
        .fetch_optional(&self.db)
        .await?;
        let (username, display) = row.ok_or(Error::NotFound)?;
        let name = if display.trim().is_empty() { username.clone() } else { display };
        Ok((name, ch_core::noreply::noreply_email(user.0, &username, &self.cfg.server.external_url)))
    }
}
