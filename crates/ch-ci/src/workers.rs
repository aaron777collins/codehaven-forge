//! Background loops: the event consumer, the scheduler, the lease reaper and the janitor.

use crate::ctx::Ctx;
use crate::discovery::repo_ci;
use crate::logs::append_system_line;
use crate::runs::{self, JobRow, advance_all_active, fetch_run};
use crate::triggers::{self, Trigger};
use ch_core::model::CiAvailability;
use ch_core::state::{AppState, Worker};
use ch_core::{RepoId, Result};
use ch_workflow::trigger::EventInfo;
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub fn all(state: &AppState) -> Vec<Worker> {
    let ctx = Ctx::from_state(state);
    let (a, b, c, d) = (ctx.clone(), ctx.clone(), ctx.clone(), ctx);
    vec![
        Worker::new("ci-triggers", move |t| triggers_loop(a, t)),
        Worker::new("ci-scheduler", move |t| scheduler_loop(b, t)),
        Worker::new("ci-reaper", move |t| reaper_loop(c, t)),
        Worker::new("ci-janitor", move |t| janitor_loop(d, t)),
    ]
}

async fn nap(token: &CancellationToken, d: Duration) -> bool {
    tokio::select! {
        _ = token.cancelled() => false,
        _ = tokio::time::sleep(d) => true,
    }
}

async fn triggers_loop(ctx: Ctx, token: CancellationToken) {
    let mut failures = BTreeMap::new();
    loop {
        let n = match triggers::process_batch(&ctx, &mut failures).await {
            Ok(n) => n,
            Err(e) => {
                tracing::error!(error = %e, "ci-triggers failed");
                0
            }
        };
        // A full batch means more is waiting, so go again at once.
        if n < 50 && !nap(&token, Duration::from_secs(1)).await {
            return;
        }
        if token.is_cancelled() {
            return;
        }
    }
}

/// Fires every schedule that is due. Each fires at most once per minute: its next time is
/// worked out from now, so a server that was down does not fire every missed minute.
pub async fn fire_due_schedules(ctx: &Ctx) -> Result<usize> {
    let now = ctx.now();
    let mut tx = ctx.db.begin().await?;
    let due: Vec<(i64, i64, i64, String, chrono::DateTime<chrono::Utc>)> =
        sqlx::query_as("SELECT id, workflow_id, repo_id, cron, next_run_at FROM schedules WHERE next_run_at <= $1 ORDER BY next_run_at FOR UPDATE SKIP LOCKED LIMIT 50")
            .bind(now)
            .fetch_all(&mut *tx)
            .await?;
    for (id, _, _, cron, _) in &due {
        match ch_workflow::schedule::next_after(cron, now) {
            Some(next) => {
                sqlx::query("UPDATE schedules SET last_run_at = $2, next_run_at = $3 WHERE id = $1")
                    .bind(id)
                    .bind(now)
                    .bind(next)
                    .execute(&mut *tx)
                    .await?;
            }
            None => {
                sqlx::query("DELETE FROM schedules WHERE id = $1").bind(id).execute(&mut *tx).await?;
            }
        }
    }
    tx.commit().await?;
    let mut fired = 0;
    for (id, workflow_id, repo, _cron, scheduled) in due {
        match fire_schedule(ctx, id, workflow_id, RepoId(repo), scheduled).await {
            Ok(n) => fired += n,
            Err(e) => tracing::warn!(schedule = id, error = %e, "scheduled run failed to start"),
        }
    }
    Ok(fired)
}

async fn fire_schedule(
    ctx: &Ctx,
    schedule: i64,
    workflow_id: i64,
    repo: RepoId,
    scheduled: chrono::DateTime<chrono::Utc>,
) -> Result<usize> {
    let info = repo_ci(ctx, repo).await?;
    if info.availability() != CiAvailability::Enabled {
        return Ok(0);
    }
    let path: Option<String> = sqlx::query_scalar("SELECT path FROM workflows WHERE id = $1 AND state = 'active'")
        .bind(workflow_id)
        .fetch_optional(&ctx.db)
        .await?;
    let Some(path) = path else { return Ok(0) };
    // Schedules only ever come from the default branch.
    let sha = match ctx.source.resolve(repo, &info.default_branch).await {
        Ok(s) => s,
        Err(ch_core::Error::NotFound) => return Ok(0),
        Err(e) => return Err(e),
    };
    let git_ref = format!("refs/heads/{}", info.default_branch);
    let t = Trigger {
        repo,
        sha,
        git_ref: git_ref.clone(),
        info: EventInfo::simple("schedule"),
        actor: None,
        title: String::new(),
        payload: json!({ "schedule": scheduled.format("%Y-%m-%dT%H:%M:00Z").to_string(), "repository": { "full_name": info.full_name() } }),
        inputs: Json::Null,
        fork: false,
        needs_approval: false,
        hold_reason: None,
        approved_by: None,
        base_ref: String::new(),
        head_ref: String::new(),
        // One run per schedule per minute, even if two servers fire it.
        trigger_id: format!("sched:{schedule}:{}", scheduled.format("%Y%m%d%H%M")),
        head_branch: Some(info.default_branch.clone()),
        sync_schedules: false,
    };
    Ok(triggers::fire(ctx, &t, Some(&path)).await?.len())
}

async fn scheduler_loop(ctx: Ctx, token: CancellationToken) {
    let mut sweeps = 0u32;
    loop {
        if let Err(e) = fire_due_schedules(&ctx).await {
            tracing::error!(error = %e, "scheduler failed");
        }
        sweeps += 1;
        // Every two minutes, advance runs that a crash may have left half-way.
        if sweeps.is_multiple_of(8)
            && let Err(e) = advance_all_active(&ctx).await
        {
            tracing::error!(error = %e, "run sweep failed");
        }
        if !nap(&token, Duration::from_secs(15)).await {
            return;
        }
    }
}

/// Fails jobs whose runner went quiet, and jobs that ran far past their own timeout. Returns how
/// many it failed.
pub async fn reap_jobs(ctx: &Ctx) -> Result<usize> {
    let mut n = 0;
    let lost: Vec<JobRow> = sqlx::query_as::<_, JobRow>(&format!(
        "SELECT {} FROM workflow_jobs WHERE status = 'in_progress' AND lease_expires_at < now()",
        runs::JOB_COLS
    ))
    .fetch_all(&ctx.db)
    .await?;
    for j in lost {
        let run = fetch_run(ctx, j.run_id).await?;
        append_system_line(ctx, &run, &j, "The runner stopped responding.").await;
        if runs::finish_job(ctx, j.id, "failure", &BTreeMap::new()).await? {
            n += 1;
        }
    }
    // The runner enforces each job's timeout. This is the backstop for a runner that does not.
    let slow: Vec<JobRow> = sqlx::query_as::<_, JobRow>(&format!(
        "SELECT {} FROM workflow_jobs WHERE status = 'in_progress' AND started_at < now() - make_interval(mins => COALESCE((spec->>'timeout_minutes')::int, 360) + 5)",
        runs::JOB_COLS
    ))
    .fetch_all(&ctx.db)
    .await?;
    for j in slow {
        let run = fetch_run(ctx, j.run_id).await?;
        append_system_line(ctx, &run, &j, "The job ran past its time limit and was stopped.").await;
        if runs::finish_job(ctx, j.id, "timed_out", &BTreeMap::new()).await? {
            n += 1;
        }
    }
    n += reap_queued(ctx).await?;
    Ok(n)
}

/// Jobs that wait in the queue for a runner that will not come (ch-7rq). A job whose labels no
/// registered runner has fails after `no_runner_minutes`, since adding a runner is the only fix.
/// Any other job fails as timed out after `queue_timeout_minutes`.
async fn reap_queued(ctx: &Ctx) -> Result<usize> {
    let mut n = 0;
    let unmatched: Vec<JobRow> = sqlx::query_as::<_, JobRow>(&format!(
        "SELECT {} FROM workflow_jobs j
         WHERE j.status = 'queued' AND COALESCE(j.queued_at, j.created_at) < now() - make_interval(mins => $1)
           AND NOT EXISTS (SELECT 1 FROM runners r WHERE r.revoked_at IS NULL
                           AND (r.repo_id IS NULL OR r.repo_id = j.repo_id) AND j.runs_on <@ r.labels)",
        runs::JOB_COLS
    ))
    .bind(ctx.config.ci.no_runner_minutes.clamp(1, 7 * 24 * 60) as i32)
    .fetch_all(&ctx.db)
    .await?;
    for j in unmatched {
        let run = fetch_run(ctx, j.run_id).await?;
        let offered = runs::runner_labels(ctx, j.repo_id).await?;
        append_system_line(ctx, &run, &j, &runs::no_runner_line(&j.runs_on, &offered)).await;
        if runs::finish_job(ctx, j.id, "failure", &BTreeMap::new()).await? {
            n += 1;
        }
    }
    let stale: Vec<JobRow> = sqlx::query_as::<_, JobRow>(&format!(
        "SELECT {} FROM workflow_jobs WHERE status = 'queued' AND COALESCE(queued_at, created_at) < now() - make_interval(mins => $1)",
        runs::JOB_COLS
    ))
    .bind(ctx.config.ci.queue_timeout_minutes.clamp(1, 35 * 24 * 60) as i32)
    .fetch_all(&ctx.db)
    .await?;
    for j in stale {
        let run = fetch_run(ctx, j.run_id).await?;
        let hours = ctx.config.ci.queue_timeout_minutes.div_ceil(60).max(1);
        let line = format!("No runner picked up this job within {hours} hour{}, so it was stopped.", if hours == 1 { "" } else { "s" });
        append_system_line(ctx, &run, &j, &line).await;
        if runs::finish_job(ctx, j.id, "timed_out", &BTreeMap::new()).await? {
            n += 1;
        }
    }
    Ok(n)
}

async fn reaper_loop(ctx: Ctx, token: CancellationToken) {
    loop {
        if let Err(e) = reap_jobs(&ctx).await {
            tracing::error!(error = %e, "lease reaper failed");
        }
        // Jobs whose wait timer at an environment has run out start now (ch-bj5).
        if let Err(e) = crate::environments::release_due(&ctx).await {
            tracing::error!(error = %e, "releasing jobs at environments failed");
        }
        if !nap(&token, Duration::from_secs(15)).await {
            return;
        }
    }
}

/// Expired artifacts, old caches, old logs, unused registration tokens.
pub async fn janitor_pass(ctx: &Ctx) -> Result<()> {
    let a = crate::store::expire_artifacts(ctx).await?;
    let c = crate::store::evict_caches(ctx).await?;
    let l = crate::logs::purge_old(ctx, ctx.config.ci.log_retention_days).await?;
    sqlx::query("DELETE FROM runner_registration_tokens WHERE expires_at < now() - interval '7 days'").execute(&ctx.db).await?;
    let u = crate::toolkit::drop_abandoned_uploads(ctx).await?;
    if a + c + l + u > 0 {
        tracing::info!(artifacts = a, caches = c, logs = l, unfinished_uploads = u, "cleaned up CI storage");
    }
    Ok(())
}

async fn janitor_loop(ctx: Ctx, token: CancellationToken) {
    loop {
        if !nap(&token, Duration::from_secs(60)).await {
            return;
        }
        if let Err(e) = janitor_pass(&ctx).await {
            tracing::error!(error = %e, "CI cleanup failed");
        }
        if !nap(&token, Duration::from_secs(59 * 60)).await {
            return;
        }
    }
}
