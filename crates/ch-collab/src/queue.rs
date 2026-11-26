//! A simple merge queue (ch-dxk).
//!
//! A repository that turns the queue on merges pull requests only through it. A pull request is
//! queued per base branch and the queue works on one at a time, the oldest first. For that one:
//!
//! 1. If its base has moved on, the base is merged into its branch (the "update branch" a person
//!    can do by hand), so what is tested is the merged result. That is a push, so workflows run
//!    for it as for any push; for a pull request from a fork they still wait for a writer's
//!    approval of that exact commit, and the queue waits with them. Nothing here approves or runs
//!    anything on its own.
//! 2. When it is up to date, the merge gate decides, with its required checks, reviews and the rest.
//!    If the gate is clear it merges, as the person who queued it, and the next one starts.
//! 3. If a required check failed, it conflicts, its commits changed under the queue, or it waited
//!    too long, it leaves the queue and the pull request says why.
//!
//! Merging any other way is refused while the queue is on (see `merge_pull`), so a pull request
//! cannot skip it.

use crate::core::Core;
use crate::pulls::{PullRow, method_str, parse_method};
use crate::util::{as_user, writing_user};
use ch_core::collab::*;
use ch_core::model::*;
use ch_core::{Actor, Error, IssueId, Permission, RepoId, Result, UserId};
use chrono::{DateTime, Utc};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

/// How long one pull request may stay in the queue before it is taken out.
pub const MAX_WAIT_HOURS: i64 = 24;
/// The most pull requests one base branch queues.
pub const MAX_QUEUED: i64 = 50;

/// One lock per line (repository and base branch). The add request, the push worker, the check
/// events and the sweep all advance a line, and two of them at once would both update the same
/// branch: the second fails on a head that moved and would drop a good pull request.
type LineLock = Arc<tokio::sync::Mutex<()>>;
static LINES: LazyLock<std::sync::Mutex<HashMap<(i64, String), LineLock>>> = LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

fn line_lock(repo: RepoId, base_ref: &str) -> LineLock {
    let mut map = LINES.lock().unwrap_or_else(|p| p.into_inner());
    map.entry((repo.0, base_ref.to_string())).or_default().clone()
}

/// Forgets the lock of a line nobody holds or waits for, so the map stays small.
fn line_done(repo: RepoId, base_ref: &str) {
    let mut map = LINES.lock().unwrap_or_else(|p| p.into_inner());
    let key = (repo.0, base_ref.to_string());
    if map.get(&key).is_some_and(|l| Arc::strong_count(l) == 1) {
        map.remove(&key);
    }
}

#[derive(sqlx::FromRow)]
struct Entry {
    issue_id: i64,
    method: String,
    enqueued_by: Option<i64>,
    enqueued_at: DateTime<Utc>,
    head_sha: String,
}

impl Core {
    /// Whether the repository merges through the queue.
    pub async fn merge_queue_on(&self, repo: RepoId) -> Result<bool> {
        Ok(sqlx::query_scalar("SELECT merge_queue FROM repositories WHERE id = $1")
            .bind(repo.0)
            .fetch_optional(self.db())
            .await?
            .unwrap_or(false))
    }

    /// Turns the queue on or off for a repository. Turning it off lets everything merge the
    /// ordinary way again and forgets what was waiting.
    pub async fn set_merge_queue(&self, actor: &Actor, repo: RepoId, on: bool, client: &ClientInfo) -> Result<()> {
        self.need(actor, repo, Permission::Admin).await?;
        let mut tx = self.db().begin().await?;
        sqlx::query("UPDATE repositories SET merge_queue = $2 WHERE id = $1").bind(repo.0).bind(on).execute(&mut *tx).await?;
        if on {
            // What was armed for auto-merge goes through the queue now, or not at all.
            sqlx::query("UPDATE pull_requests SET auto_merge_method = NULL, auto_merge_by = NULL, auto_merge_head_sha = NULL WHERE repo_id = $1 AND auto_merge_method IS NOT NULL")
                .bind(repo.0)
                .execute(&mut *tx)
                .await?;
        }
        if !on {
            let waiting: Vec<i64> = sqlx::query_scalar("DELETE FROM merge_queue_entries WHERE repo_id = $1 RETURNING issue_id")
                .bind(repo.0)
                .fetch_all(&mut *tx)
                .await?;
            for id in waiting {
                Self::add_event(
                    &mut tx,
                    IssueId(id),
                    actor.user_id(),
                    "merge_queue_removed",
                    json!({ "reason": "the merge queue was turned off" }),
                )
                .await?;
            }
        }
        tx.commit().await?;
        let ev =
            ch_core::audit::AuditEvent::new(actor, client, ch_core::audit::action::MERGE_QUEUE_CHANGED, ch_core::audit::Outcome::Success)
                .target("repository", repo.0)
                .details(json!({ "on": on }));
        if let Err(e) = self.d.audit.record_standalone(ev).await {
            tracing::warn!(error = %e, "could not record a merge queue change");
        }
        Ok(())
    }

    /// Puts a pull request in the queue for its base branch.
    pub async fn queue_add(&self, actor: &Actor, repo: RepoId, number: i64, method: MergeMethod, client: &ClientInfo) -> Result<()> {
        let user = writing_user(actor, repo)?;
        self.need(actor, repo, Permission::Write).await?;
        let _ = client;
        if !self.merge_queue_on(repo).await? {
            return Err(Error::invalid("PullRequest", "queue", "This repository does not use a merge queue"));
        }
        let (issue, pull) = self.pull_pair(repo, number).await?;
        if !issue.open || pull.merged_at.is_some() {
            return Err(Error::invalid("PullRequest", "state", "Only an open pull request can join the queue"));
        }
        if pull.draft {
            return Err(Error::invalid("PullRequest", "draft", "A draft pull request cannot join the queue"));
        }
        let allowed = {
            let r = self.repo(repo).await?;
            match method {
                MergeMethod::Merge => r.allow_merge_commit,
                MergeMethod::Squash => r.allow_squash_merge,
                MergeMethod::Rebase => r.allow_rebase_merge,
            }
        };
        if !allowed {
            return Err(Error::invalid("PullRequest", "method", "That merge method is turned off for this repository"));
        }
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM merge_queue_entries WHERE repo_id = $1 AND base_ref = $2")
            .bind(repo.0)
            .bind(&pull.base_ref)
            .fetch_one(self.db())
            .await?;
        if n >= MAX_QUEUED {
            return Err(Error::invalid("PullRequest", "queue", "The queue is full. Try again when some of it has merged"));
        }
        let mut tx = self.db().begin().await?;
        let added = sqlx::query(
            "INSERT INTO merge_queue_entries (issue_id, repo_id, base_ref, method, enqueued_by, head_sha, note)
             VALUES ($1, $2, $3, $4, $5, $6, 'Waiting for its turn') ON CONFLICT DO NOTHING",
        )
        .bind(issue.id.0)
        .bind(repo.0)
        .bind(&pull.base_ref)
        .bind(method_str(method))
        .bind(user.0)
        .bind(&pull.head_sha)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if added == 0 {
            return Err(Error::conflict("This pull request is already in the queue"));
        }
        Self::add_event(&mut tx, issue.id, Some(user), "merge_queue_added", json!({ "method": method_str(method) })).await?;
        tx.commit().await?;
        self.queue_advance(repo, &pull.base_ref).await?;
        Ok(())
    }

    /// Takes a pull request out of the queue. Anyone who can write may.
    pub async fn queue_remove(&self, actor: &Actor, repo: RepoId, number: i64, client: &ClientInfo) -> Result<()> {
        let user = writing_user(actor, repo)?;
        self.need(actor, repo, Permission::Write).await?;
        let _ = client;
        let (issue, _) = self.pull_pair(repo, number).await?;
        let mut tx = self.db().begin().await?;
        let gone =
            sqlx::query("DELETE FROM merge_queue_entries WHERE issue_id = $1").bind(issue.id.0).execute(&mut *tx).await?.rows_affected();
        if gone == 0 {
            return Err(Error::NotFound);
        }
        Self::add_event(&mut tx, issue.id, Some(user), "merge_queue_removed", json!({ "reason": "taken out by a person" })).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Where the pull request stands in its queue, if it is in one.
    pub async fn queue_state(&self, actor: &Actor, repo: RepoId, number: i64) -> Result<Option<QueueView>> {
        self.need_read(actor, repo).await?;
        let (issue, pull) = self.pull_pair(repo, number).await?;
        let row: Option<(String, String, DateTime<Utc>)> =
            sqlx::query_as("SELECT note, method, enqueued_at FROM merge_queue_entries WHERE issue_id = $1")
                .bind(issue.id.0)
                .fetch_optional(self.db())
                .await?;
        let Some((note, method, at)) = row else { return Ok(None) };
        let ahead: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM merge_queue_entries WHERE repo_id = $1 AND base_ref = $2 AND (enqueued_at, issue_id) < ($3, $4)",
        )
        .bind(repo.0)
        .bind(&pull.base_ref)
        .bind(at)
        .bind(issue.id.0)
        .fetch_one(self.db())
        .await?;
        let total: i64 = sqlx::query_scalar("SELECT count(*) FROM merge_queue_entries WHERE repo_id = $1 AND base_ref = $2")
            .bind(repo.0)
            .bind(&pull.base_ref)
            .fetch_one(self.db())
            .await?;
        Ok(Some(QueueView {
            position: ahead as u32 + 1,
            total: total as u32,
            note,
            method: parse_method(&method).unwrap_or(MergeMethod::Merge),
        }))
    }

    /// Drops an entry and says why on the pull request.
    async fn queue_drop(&self, issue: IssueId, why: &str) -> Result<()> {
        let mut tx = self.db().begin().await?;
        if sqlx::query("DELETE FROM merge_queue_entries WHERE issue_id = $1").bind(issue.0).execute(&mut *tx).await?.rows_affected() > 0 {
            Self::add_event(&mut tx, issue, None, "merge_queue_removed", json!({ "reason": why })).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn queue_note(&self, issue: IssueId, note: &str) -> Result<()> {
        sqlx::query("UPDATE merge_queue_entries SET note = $2 WHERE issue_id = $1 AND note <> $2")
            .bind(issue.0)
            .bind(note)
            .execute(self.db())
            .await?;
        Ok(())
    }

    /// Whether a required check of the pull request's head has failed (not merely not finished).
    async fn required_check_failed(&self, pull: &PullRow) -> Result<bool> {
        let Some(rules) = self.rules_for_pull(pull).await? else { return Ok(false) };
        if rules.required_checks.is_empty() {
            return Ok(false);
        }
        let checks = self.d.checks();
        let bad = |c: &str| matches!(c, "failure" | "cancelled" | "timed_out" | "error" | "action_required");
        let runs = checks.check_runs_for_sha(pull.repo_id, &pull.head_sha).await.unwrap_or_default();
        let statuses = checks.statuses_for_sha(pull.repo_id, &pull.head_sha).await.unwrap_or_default();
        Ok(rules.required_checks.iter().any(|name| {
            runs.iter().any(|r| &r.name == name && r.status == "completed" && r.conclusion.as_deref().is_some_and(bad))
                || statuses.iter().any(|s| &s.context == name && bad(&s.state))
        }))
    }

    /// Works on the head of the line for one base branch until it has to wait. Returns how many
    /// pull requests it merged.
    ///
    /// One call per line at a time: a second caller waits for the first and then reads the line
    /// as the first left it.
    pub async fn queue_advance(&self, repo: RepoId, base_ref: &str) -> Result<u32> {
        let lock = line_lock(repo, base_ref);
        let out = {
            let _held = lock.lock().await;
            self.queue_advance_locked(repo, base_ref).await
        };
        drop(lock);
        line_done(repo, base_ref);
        out
    }

    async fn queue_advance_locked(&self, repo: RepoId, base_ref: &str) -> Result<u32> {
        let mut merged = 0;
        // Bounded: each pass merges, drops or waits.
        for _ in 0..(MAX_QUEUED + 1) {
            let head: Option<Entry> = sqlx::query_as(
                "SELECT issue_id, method, enqueued_by, enqueued_at, head_sha FROM merge_queue_entries
                  WHERE repo_id = $1 AND base_ref = $2 ORDER BY enqueued_at, issue_id LIMIT 1",
            )
            .bind(repo.0)
            .bind(base_ref)
            .fetch_optional(self.db())
            .await?;
            let Some(e) = head else { break };
            let id = IssueId(e.issue_id);
            let issue = self.issue_row_by_id(id).await?;
            let pull = self.pull_row(id).await?;
            if !issue.open || pull.merged_at.is_some() {
                self.queue_drop(id, "the pull request is no longer open").await?;
                continue;
            }
            if !self.merge_queue_on(repo).await? {
                self.queue_drop(id, "the repository no longer uses a merge queue").await?;
                continue;
            }
            if Utc::now() - e.enqueued_at > chrono::Duration::hours(MAX_WAIT_HOURS) {
                self.queue_drop(id, "it waited longer than a day").await?;
                continue;
            }
            let Some(by) = e.enqueued_by.map(UserId) else {
                self.queue_drop(id, "the person who queued it is gone").await?;
                continue;
            };
            if self.permission(&as_user(by), repo).await? < Permission::Write {
                self.queue_drop(id, "the person who queued it can no longer write here").await?;
                continue;
            }
            // Commits pushed by anyone but the queue are not what was queued.
            if pull.head_sha != e.head_sha {
                self.queue_drop(id, "new commits were pushed, so it needs queueing again").await?;
                continue;
            }
            self.refresh_pull(&issue, &pull).await;
            let pull = self.pull_row(id).await?;
            let state = pull.mergeable();
            if state == MergeableState::Conflict {
                self.queue_drop(id, "it conflicts with its base branch").await?;
                continue;
            }
            // The test merge never says "behind", so ask git directly: the checks must run on the
            // base's tip plus this branch, which means the head has to contain that tip.
            let behind = if state == MergeableState::Behind {
                true
            } else {
                match self.compare_pull(&pull, &pull.base_sha, &pull.head_sha).await {
                    Ok(c) => c.merge_base != pull.base_sha,
                    Err(_) => {
                        self.queue_note(id, "Waiting: could not compare the branch with its base yet").await?;
                        break;
                    }
                }
            };
            if behind {
                // Bring it up to date; the checks then run on the merged result.
                match self.pull_update_branch(&as_user(by), repo, issue.number, &ClientInfo::local()).await {
                    Ok(v) => {
                        sqlx::query("UPDATE merge_queue_entries SET head_sha = $2, note = 'Updated with its base branch, waiting for checks' WHERE issue_id = $1")
                            .bind(id.0)
                            .bind(&v.pull.head_sha)
                            .execute(self.db())
                            .await?;
                    }
                    Err(err) => {
                        self.queue_drop(id, &format!("it could not be brought up to date: {err}")).await?;
                        continue;
                    }
                }
                break;
            }
            let method = parse_method(&e.method).unwrap_or(MergeMethod::Merge);
            let report = self.gate_for(&issue, &pull, Some(method)).await?;
            if report.mergeable {
                let opts = MergeOptions { method: Some(method), expected_head_sha: Some(e.head_sha.clone()), ..Default::default() };
                match self.merge_pull(&Actor::System, by, id, opts, &ClientInfo::local()).await {
                    Ok(()) => {
                        // The merge is on the timeline already; the entry just goes.
                        sqlx::query("DELETE FROM merge_queue_entries WHERE issue_id = $1").bind(id.0).execute(self.db()).await?;
                        merged += 1;
                        continue;
                    }
                    Err(Error::MergeBlocked(_)) | Err(Error::Conflict(_)) => {
                        self.queue_note(id, "Waiting: the merge was refused for now").await?;
                        break;
                    }
                    Err(err) => return Err(err),
                }
            }
            if self.required_check_failed(&pull).await? {
                self.queue_drop(id, "a required check failed").await?;
                continue;
            }
            let why = report.reasons.first().cloned().unwrap_or_else(|| "waiting for checks".into());
            self.queue_note(id, &format!("Waiting: {why}")).await?;
            break;
        }
        Ok(merged)
    }

    /// Advances every queue that has something in it. A safety net beside the events.
    pub async fn queue_sweep(&self) -> Result<()> {
        let lines: Vec<(i64, String)> =
            sqlx::query_as("SELECT DISTINCT repo_id, base_ref FROM merge_queue_entries").fetch_all(self.db()).await?;
        for (repo, base) in lines {
            if let Err(e) = self.queue_advance(RepoId(repo), &base).await {
                tracing::warn!(error = %e, "merge queue could not advance");
            }
        }
        Ok(())
    }
}
