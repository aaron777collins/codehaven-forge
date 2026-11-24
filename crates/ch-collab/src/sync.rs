//! Event consumers: `pr-sync` keeps pull requests in step with the branches, and
//! `collab-defaults` gives new repositories their starting labels.
//!
//! Both read the outbox through `EventBus::poll` and `ack`. A handler is safe to run twice for
//! the same event: it compares what is stored with what the branches say now.

use crate::core::{Core, IssueRow};
use crate::pulls::{PullRow, full_branch};
use ch_core::events::{DomainEvent, StoredEvent};
use ch_core::state::Worker;
use ch_core::text;
use ch_core::{IssueId, PullId, RepoId, Result, UserId};
use futures::future::BoxFuture;
use serde_json::json;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const ZERO_SHA: &str = "0000000000000000000000000000000000000000";
const BATCH: i64 = 50;
const MAX_ATTEMPTS: u32 = 5;

type Handler = for<'a> fn(&'a Core, &'a StoredEvent) -> BoxFuture<'a, Result<()>>;

pub fn workers(core: Core) -> Vec<Worker> {
    let a = core.clone();
    let b = core;
    vec![
        Worker::new("collab-pr-sync", move |t| consume(a, t, "pr-sync", |c, e| Box::pin(pr_sync(c, e)))),
        Worker::new("collab-defaults", move |t| consume(b, t, "collab-defaults", |c, e| Box::pin(defaults(c, e)))),
    ]
}

async fn consume(core: Core, token: CancellationToken, name: &'static str, handler: Handler) {
    loop {
        match drain(&core, name, handler, &token).await {
            Ok(_) => {}
            Err(e) => tracing::warn!(consumer = name, error = %e, "consumer could not read the outbox"),
        }
        tokio::select! {
            _ = token.cancelled() => return,
            _ = tokio::time::sleep(Duration::from_millis(750)) => {}
        }
    }
}

/// Handles every event after the cursor. Returns how many it handled.
pub async fn drain(core: &Core, name: &str, handler: Handler, token: &CancellationToken) -> Result<usize> {
    let mut handled = 0;
    loop {
        let batch = core.d.events.poll(name, BATCH).await?;
        if batch.is_empty() {
            return Ok(handled);
        }
        for ev in &batch {
            if token.is_cancelled() {
                return Ok(handled);
            }
            let mut attempt = 0;
            loop {
                attempt += 1;
                match handler(core, ev).await {
                    Ok(()) => break,
                    Err(e) if attempt < MAX_ATTEMPTS => {
                        tracing::warn!(consumer = name, event = ev.id, attempt, error = %e, "event failed, retrying");
                        tokio::select! {
                            _ = token.cancelled() => return Ok(handled),
                            _ = tokio::time::sleep(Duration::from_millis(200 * attempt as u64)) => {}
                        }
                    }
                    Err(e) => {
                        // A poison event must not hold up everything behind it.
                        tracing::error!(consumer = name, event = ev.id, error = %e, "event skipped after repeated failures");
                        break;
                    }
                }
            }
            core.d.events.ack(name, ev.id).await?;
            handled += 1;
        }
    }
}

/// For tests and the bootstrap path: one pass of `pr-sync` over everything new.
pub async fn run_pr_sync_once(core: &Core) -> Result<usize> {
    drain(core, "pr-sync", |c, e| Box::pin(pr_sync(c, e)), &CancellationToken::new()).await
}

pub async fn run_defaults_once(core: &Core) -> Result<usize> {
    drain(core, "collab-defaults", |c, e| Box::pin(defaults(c, e)), &CancellationToken::new()).await
}

async fn defaults(core: &Core, ev: &StoredEvent) -> Result<()> {
    if let DomainEvent::RepoCreated { repo_id, .. } = &ev.event {
        core.seed_default_labels(*repo_id).await?;
    }
    Ok(())
}

async fn pr_sync(core: &Core, ev: &StoredEvent) -> Result<()> {
    match &ev.event {
        DomainEvent::Push { repo_id, git_ref, before, after, pusher, app, server_built, .. } => {
            let Some(branch) = git_ref.strip_prefix("refs/heads/") else { return Ok(()) };
            if after == ZERO_SHA {
                return Ok(());
            }
            // A commit the server wrote on top of the branch holds what the branch held.
            let built_on = server_built.then(|| before.clone());
            head_moved(core, *repo_id, branch, after, *pusher, *app, built_on).await?;
            base_moved(core, *repo_id, branch, before, after, *pusher, app.is_some()).await?;
            keywords_from_push(core, *repo_id, branch, before, after, *pusher, app.is_some()).await?;
        }
        DomainEvent::CheckRunUpdated { repo_id, sha, .. } | DomainEvent::StatusCreated { repo_id, sha, .. } => {
            auto_merge_for_sha(core, *repo_id, sha).await?;
        }
        DomainEvent::ReviewSubmitted { pull_id, .. } => {
            core.try_auto_merge(IssueId(pull_id.0)).await.map(|_| ())?;
        }
        _ => {}
    }
    Ok(())
}

async fn head_moved(
    core: &Core,
    repo: RepoId,
    branch: &str,
    after: &str,
    pusher: Option<UserId>,
    app: Option<ch_core::AppOrigin>,
    built_on: Option<String>,
) -> Result<()> {
    for (issue, pull) in core.open_pulls_by_head(repo, branch).await? {
        // What was last announced, not `head_sha`: a page view or a merge refreshes the pull
        // request too, and must not swallow this push's `synchronize` (ch-hyd).
        let synced: Option<String> =
            sqlx::query_scalar("SELECT synced_head_sha FROM pull_requests WHERE issue_id = $1").bind(issue.id).fetch_one(core.db()).await?;
        let old_head = synced.clone().unwrap_or_else(|| pull.head_sha.clone());
        core.refresh_pull(&issue, &pull).await;
        let fresh = core.pull_row(issue.id).await?;
        if fresh.head_sha == old_head {
            continue;
        }
        // The pusher and app on this push describe its own commit only. When the branch moved
        // on before this event was handled, it holds a later commit someone else may have
        // pushed, and that push's own event will find nothing new. Nobody is named for the
        // head then, so CI waits for a person instead of running it as this pusher's (ch-x12).
        let origin_known = fresh.head_sha == after;
        let (pusher, app) = if origin_known { (pusher, app) } else { (None, None) };
        // When the server built the commit for the pusher, they vouch for their change only, not
        // for the commit under it. CI counts them only if that commit was itself let run.
        let built_on = if origin_known { built_on.clone() } else { None };
        let mut tx = core.db().begin().await?;
        // Claimed in the transaction, so two pushes handled at once announce one head once.
        let claimed =
            sqlx::query("UPDATE pull_requests SET synced_head_sha = $2 WHERE issue_id = $1 AND synced_head_sha IS NOT DISTINCT FROM $3")
                .bind(issue.id)
                .bind(&fresh.head_sha)
                .bind(&synced)
                .execute(&mut *tx)
                .await?
                .rows_affected();
        if claimed == 0 {
            continue;
        }
        Core::add_event(&mut tx, issue.id, pusher, "head_ref_updated", json!({ "before": old_head, "after": fresh.head_sha })).await?;
        if pull.auto_merge_method.is_some() {
            // New code from a connected app, from nobody that can be named, or from someone
            // without write access on the base (a fork's owner, say) is not what the person
            // approved for auto-merge. It is cleared before anything can try to merge, and the
            // timeline says so. A writer's push keeps it armed, for the new head (ch-m2y).
            let writer = match pusher {
                Some(u) if origin_known && app.is_none() => {
                    core.permission(&crate::util::as_user(u), issue.repo_id).await? >= ch_core::Permission::Write
                }
                _ => false,
            };
            if writer {
                sqlx::query("UPDATE pull_requests SET auto_merge_head_sha = $2 WHERE issue_id = $1")
                    .bind(issue.id)
                    .bind(&fresh.head_sha)
                    .execute(&mut *tx)
                    .await?;
            } else {
                sqlx::query("UPDATE pull_requests SET auto_merge_method = NULL, auto_merge_by = NULL, auto_merge_head_sha = NULL WHERE issue_id = $1")
                    .bind(issue.id)
                    .execute(&mut *tx)
                    .await?;
                let reason = if !origin_known {
                    "it is not known who pushed to this branch"
                } else if app.is_some() {
                    "a connected app pushed to this branch"
                } else {
                    "someone without write access pushed to this branch"
                };
                Core::add_event(&mut tx, issue.id, pusher, "auto_merge_disabled", json!({ "reason": reason })).await?;
            }
        }
        Core::touch(&mut tx, issue.id).await?;
        core.d
            .events
            .emit(
                &mut tx,
                DomainEvent::PrSynchronized {
                    repo_id: issue.repo_id,
                    pull_id: PullId(issue.id.0),
                    number: issue.number,
                    head_sha: fresh.head_sha.clone(),
                    app,
                    pusher,
                    origin_known,
                    built_on,
                },
            )
            .await?;
        tx.commit().await?;
        core.publish_pull_ref(&issue, &fresh).await;
        if let Err(e) = core.adjust_comments_for_new_head(&issue, &fresh, &old_head).await {
            tracing::warn!(error = %e, "could not move review comments to the new head");
        }
        core.request_code_owners(&issue, &fresh).await;
    }
    Ok(())
}

async fn base_moved(core: &Core, repo: RepoId, branch: &str, _before: &str, after: &str, pusher: Option<UserId>, app: bool) -> Result<()> {
    for (issue, pull) in core.open_pulls_by_base(repo, branch).await? {
        if merged_by_push(core, &issue, &pull, after).await? {
            mark_merged_by_push(core, &issue, &pull, after, pusher, app).await?;
            continue;
        }
        core.refresh_pull(&issue, &pull).await;
        // A moved base can clear or raise the "branch is behind" rule.
        if pull.auto_merge_method.is_some() {
            let _ = core.try_auto_merge(issue.id).await;
        }
    }
    Ok(())
}

/// True when every commit of the pull request is already on the base now.
async fn merged_by_push(core: &Core, issue: &IssueRow, pull: &PullRow, base_tip: &str) -> Result<bool> {
    // A deleted fork's head stays where it was. The base's branch of that name is not it.
    let head = match pull.head_repo() {
        Some(head_repo) => core.d.git.resolve(head_repo, &full_branch(&pull.head_ref)).await.unwrap_or_else(|_| pull.head_sha.clone()),
        None => pull.head_sha.clone(),
    };
    if head == pull.base_sha {
        return Ok(false);
    }
    // A head that never left the base (no commits of its own) is not "merged".
    match core.compare_pull(pull, base_tip, &head).await {
        Ok(c) => Ok(c.ahead_by == 0 && c.commits.is_empty() && c.merge_base == head && issue.open),
        Err(_) => Ok(false),
    }
}

async fn mark_merged_by_push(
    core: &Core,
    issue: &IssueRow,
    pull: &PullRow,
    base_tip: &str,
    pusher: Option<UserId>,
    app: bool,
) -> Result<()> {
    let who = pusher.unwrap_or(issue.author_id);
    let mut tx = core.db().begin().await?;
    let still_open: Option<bool> =
        sqlx::query_scalar("SELECT open FROM issues WHERE id = $1 FOR UPDATE").bind(issue.id).fetch_optional(&mut *tx).await?;
    if still_open != Some(true) {
        return Ok(());
    }
    sqlx::query("UPDATE issues SET open = false, state_reason = NULL, closed_at = now(), closed_by = $2, updated_at = now() WHERE id = $1")
        .bind(issue.id)
        .bind(who)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE pull_requests SET merged_at = now(), merged_by = $2, merge_commit_sha = $3, auto_merge_method = NULL, auto_merge_by = NULL, mergeable_state = 'clean' WHERE issue_id = $1")
        .bind(issue.id)
        .bind(who)
        .bind(base_tip)
        .execute(&mut *tx)
        .await?;
    Core::add_event(&mut tx, issue.id, pusher, "merged", json!({ "sha": base_tip, "method": null, "base": pull.base_ref, "via": "push" }))
        .await?;
    let pid = PullId(issue.id.0);
    core.d
        .events
        .emit(&mut tx, DomainEvent::PrClosed { repo_id: issue.repo_id, pull_id: pid, number: issue.number, merged: true, actor: pusher })
        .await?;
    core.d
        .events
        .emit(
            &mut tx,
            DomainEvent::PrMerged {
                repo_id: issue.repo_id,
                pull_id: pid,
                number: issue.number,
                merge_sha: base_tip.to_string(),
                actor: who,
            },
        )
        .await?;
    tx.commit().await?;
    let repo = core.repo(issue.repo_id).await?;
    if pull.base_ref == repo.default_branch {
        let texts = vec![issue.title.clone(), issue.body.clone()];
        core.close_by_keywords(&repo, who, &texts, json!({ "pull": issue.number }), app).await;
    }
    Ok(())
}

/// `Fixes #12` in a commit pushed to the default branch closes issue 12.
async fn keywords_from_push(
    core: &Core,
    repo_id: RepoId,
    branch: &str,
    before: &str,
    after: &str,
    pusher: Option<UserId>,
    app: bool,
) -> Result<()> {
    let repo = core.repo(repo_id).await?;
    if branch != repo.default_branch || before == ZERO_SHA {
        return Ok(());
    }
    let commits = match core.d.git.compare(repo_id, before, after).await {
        Ok(c) => c.commits,
        Err(_) => return Ok(()),
    };
    let texts: Vec<String> = commits.into_iter().map(|c| c.message).filter(|m| !text::refs(m).closes.is_empty()).collect();
    if texts.is_empty() {
        return Ok(());
    }
    let closer = pusher.unwrap_or(repo.owner_id);
    core.close_by_keywords(&repo, closer, &texts, json!({ "commit": after }), app).await;
    Ok(())
}

async fn auto_merge_for_sha(core: &Core, repo: RepoId, sha: &str) -> Result<()> {
    let ids: Vec<IssueId> = sqlx::query_scalar(
        "SELECT p.issue_id FROM pull_requests p JOIN issues i ON i.id = p.issue_id
         WHERE p.repo_id = $1 AND p.head_sha = $2 AND i.open AND p.auto_merge_method IS NOT NULL",
    )
    .bind(repo)
    .bind(sha)
    .fetch_all(core.db())
    .await?;
    for id in ids {
        if let Err(e) = core.try_auto_merge(id).await {
            tracing::warn!(error = %e, "auto-merge attempt failed");
        }
    }
    Ok(())
}
