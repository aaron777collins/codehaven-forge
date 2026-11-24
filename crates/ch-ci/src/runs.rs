//! Runs and their jobs: creating them from a workflow, moving jobs along as their `needs`
//! finish, concurrency groups, cancelling, re-running, and rolling job results up into the run.
//!
//! Everything that changes a run goes through [`advance_run`], which locks the run row, so two
//! runners finishing jobs at the same moment cannot both promote the same job.

use crate::ctx::Ctx;
use crate::discovery::repo_ci;
use crate::spec::{JobSpec, RunMeta, is_failure, join_payload, split_payload};
use ch_core::events::DomainEvent;
use ch_core::{Error, JobId, RepoId, Result, RunId, UserId};
use ch_workflow::expr::{self, Context, Status, Value};
use ch_workflow::{Workflow, plan};
use chrono::{DateTime, Utc};
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RunRow {
    pub id: i64,
    pub repo_id: i64,
    pub workflow_id: Option<i64>,
    pub run_number: i64,
    pub attempt: i32,
    pub event: String,
    pub head_sha: String,
    pub head_branch: Option<String>,
    pub git_ref: Option<String>,
    pub title: String,
    pub actor_id: Option<i64>,
    pub status: String,
    pub conclusion: Option<String>,
    pub concurrency_group: Option<String>,
    pub event_payload: Json,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
}

pub const RUN_COLS: &str = "id, repo_id, workflow_id, run_number, attempt, event, head_sha, head_branch, git_ref, title, actor_id, status, conclusion, \
                            concurrency_group, event_payload, created_at, started_at, completed_at";

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct JobRow {
    pub id: i64,
    pub run_id: i64,
    pub repo_id: i64,
    pub job_key: String,
    pub name: String,
    pub matrix: Option<Json>,
    pub runs_on: Vec<String>,
    pub needs: Vec<String>,
    pub status: String,
    pub conclusion: Option<String>,
    pub runner_id: Option<i64>,
    pub lease_expires_at: Option<DateTime<Utc>>,
    pub spec: Json,
    pub outputs: Json,
    pub log_path: Option<String>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
}

pub const JOB_COLS: &str = "id, run_id, repo_id, job_key, name, matrix, runs_on, needs, status, conclusion, runner_id, lease_expires_at, spec, outputs, \
                            log_path, created_at, started_at, completed_at";

impl JobRow {
    pub fn spec(&self) -> Result<JobSpec> {
        serde_json::from_value(self.spec.clone()).map_err(|e| Error::internal(anyhow::anyhow!("job {} has a damaged spec: {e}", self.id)))
    }
    fn outputs_map(&self) -> BTreeMap<String, String> {
        self.outputs
            .as_object()
            .map(|o| o.iter().map(|(k, v)| (k.clone(), v.as_str().map(String::from).unwrap_or_else(|| v.to_string()))).collect())
            .unwrap_or_default()
    }
}

/// What starts a run.
pub struct NewRun {
    pub repo_id: RepoId,
    pub workflow_id: Option<i64>,
    pub workflow_path: String,
    pub workflow_name: String,
    pub workflow: Workflow,
    pub event: String,
    pub git_ref: String,
    pub head_sha: String,
    pub head_branch: Option<String>,
    pub actor: Option<(UserId, String)>,
    pub title: String,
    /// Webhook-style payload, what `github.event` reads.
    pub event_payload: Json,
    pub inputs: Json,
    pub fork: bool,
    pub needs_approval: bool,
    /// Why it waits, shown on the run page. Set whenever `needs_approval` is.
    pub hold_reason: Option<String>,
    /// Who approved the commit, for a pull request from a fork that needed it.
    pub approved_by: Option<UserId>,
    pub trigger_id: Option<String>,
    pub base_ref: String,
    pub head_ref: String,
}

/// Side effects to run after the transaction commits.
#[derive(Default)]
pub struct Effects {
    pub events: Vec<DomainEvent>,
    pub finished_jobs: Vec<i64>,
    pub wake: bool,
    pub run_finished: Option<(i64, Option<String>)>,
}

impl Effects {
    /// Emits the queued events into the transaction. Called last, so the emit lock is held briefly.
    pub async fn emit(&mut self, ctx: &Ctx, tx: &mut sqlx::Transaction<'_, sqlx::Postgres>) -> Result<()> {
        for e in self.events.drain(..) {
            ctx.events.emit(tx, e).await?;
        }
        Ok(())
    }

    pub fn after_commit(&self, ctx: &Ctx) {
        for j in &self.finished_jobs {
            ctx.shared.hub.publish(*j, crate::shared::LogMsg::Done);
        }
        if self.wake {
            ctx.shared.hub.wake_runners();
        }
    }
}

// ---------------------------------------------------------------------------- contexts

/// The `github` context.
pub fn github_context(base_url: &str, run: &RunRow, meta: &RunMeta, event: &Json) -> Json {
    let git_ref = run.git_ref.clone().unwrap_or_default();
    let (ref_name, ref_type) = if let Some(b) = git_ref.strip_prefix("refs/heads/") {
        (b.to_string(), "branch")
    } else if let Some(t) = git_ref.strip_prefix("refs/tags/") {
        (t.to_string(), "tag")
    } else {
        (git_ref.clone(), "")
    };
    let owner = meta.repository.split('/').next().unwrap_or("");
    json!({
        "event_name": run.event,
        "event": event,
        "ref": git_ref,
        "ref_name": ref_name,
        "ref_type": ref_type,
        "sha": run.head_sha,
        "actor": meta.actor_login,
        "triggering_actor": meta.actor_login,
        "repository": meta.repository,
        "repository_owner": owner,
        "run_id": run.id.to_string(),
        "run_number": run.run_number.to_string(),
        "run_attempt": run.attempt.to_string(),
        "workflow": meta.workflow_name,
        "server_url": base_url,
        "api_url": format!("{base_url}/api/v3"),
        "base_ref": meta.base_ref,
        "head_ref": meta.head_ref,
        "job": "",
    })
}

/// Variables visible to a repository: the owner's, overridden by the repository's own. A run
/// from a fork sees none, the same as secrets: neither the owner's nor the repository's
/// variables reach code the repository's people did not write (ch-x12).
pub async fn load_vars(ctx: &Ctx, repo: i64, fork: bool) -> Result<BTreeMap<String, String>> {
    if fork {
        return Ok(BTreeMap::new());
    }
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT name, value, scope FROM variables
         WHERE (scope = 'repo' AND repo_id = $1) OR (scope = 'org' AND org_id = (SELECT owner_id FROM repositories WHERE id = $1))",
    )
    .bind(repo)
    .fetch_all(&ctx.db)
    .await?;
    let mut out = BTreeMap::new();
    for (n, v, scope) in rows.iter().filter(|r| r.2 == "org") {
        let _ = scope;
        out.insert(n.clone(), v.clone());
    }
    for (n, v, _) in rows.iter().filter(|r| r.2 == "repo") {
        out.insert(n.clone(), v.clone());
    }
    Ok(out)
}

pub(crate) fn base_context<'a>(github: &Json, inputs: &Json, vars: &BTreeMap<String, String>) -> Context<'a> {
    Context::new()
        .with("github", Value::from_json(github))
        .with("inputs", Value::from_json(inputs))
        .with("vars", Value::from_json(&json!(vars)))
        .with("env", Value::from_json(&json!({})))
        .with("secrets", Value::from_json(&json!({})))
        .with("needs", Value::from_json(&json!({})))
        .with("runner", Value::from_json(&json!({})))
}

// ---------------------------------------------------------------------------- creating

pub struct Created {
    pub run_id: RunId,
    pub run_number: i64,
    pub already_existed: bool,
}

/// Starts a run: one `workflow_runs` row, one `workflow_jobs` row per job and matrix cell, and
/// a queued check run for each. All or nothing. Returns the existing run when the same event
/// already started this workflow.
pub async fn create_run(ctx: &Ctx, n: NewRun) -> Result<Created> {
    let info = repo_ci(ctx, n.repo_id).await?;
    if let Some(t) = &n.trigger_id
        && let Some((id, num)) = sqlx::query_as::<_, (i64, i64)>(
            "SELECT id, run_number FROM workflow_runs WHERE repo_id = $1 AND workflow_id IS NOT DISTINCT FROM $2
               AND event_payload->'meta'->>'trigger_id' = $3 AND event_payload->'meta'->>'workflow_path' = $4 LIMIT 1",
        )
        .bind(n.repo_id.0)
        .bind(n.workflow_id)
        .bind(t)
        .bind(&n.workflow_path)
        .fetch_optional(&ctx.db)
        .await?
    {
        return Ok(Created { run_id: RunId(id), run_number: num, already_existed: true });
    }

    let base = ctx.base_url();
    let meta = RunMeta {
        workflow_path: n.workflow_path.clone(),
        workflow_name: n.workflow_name.clone(),
        workflow_env: n.workflow.env.clone(),
        workflow_defaults: n.workflow.defaults.clone(),
        inputs: n.inputs.clone(),
        fork: n.fork,
        cancel_requested: false,
        trigger_id: n.trigger_id.clone(),
        base_ref: n.base_ref.clone(),
        head_ref: n.head_ref.clone(),
        actor_login: n.actor.as_ref().map(|a| a.1.clone()).unwrap_or_default(),
        repository: info.full_name(),
        approved_by: n.approved_by.map(|u| u.0),
        hold_reason: if n.needs_approval { Some(n.hold_reason.clone().unwrap_or_else(|| "Waiting for approval".into())) } else { None },
        cancel_in_progress: false,
        problem: None,
    };
    let vars = load_vars(ctx, n.repo_id.0, n.fork).await?;

    let mut tx = ctx.db.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)").bind(0x0c1_0000_0000_0000_i64 | n.repo_id.0).execute(&mut *tx).await?;
    let (run_number,): (i64,) = sqlx::query_as("SELECT COALESCE(MAX(run_number), 0) + 1 FROM workflow_runs WHERE repo_id = $1")
        .bind(n.repo_id.0)
        .fetch_one(&mut *tx)
        .await?;
    let status = if n.needs_approval { "waiting" } else { "queued" };
    let run_id: i64 = sqlx::query_scalar(
        "INSERT INTO workflow_runs (repo_id, workflow_id, run_number, attempt, event, head_sha, head_branch, git_ref, title, actor_id, status, event_payload)
         VALUES ($1, $2, $3, 1, $4, $5, $6, $7, $8, $9, $10, $11) RETURNING id",
    )
    .bind(n.repo_id.0)
    .bind(n.workflow_id)
    .bind(run_number)
    .bind(&n.event)
    .bind(&n.head_sha)
    .bind(&n.head_branch)
    .bind(&n.git_ref)
    .bind(n.title.chars().take(300).collect::<String>())
    .bind(n.actor.as_ref().map(|a| a.0.0))
    .bind(status)
    .bind(join_payload(&n.event_payload, &meta))
    .fetch_one(&mut *tx)
    .await?;
    let run = fetch_run_tx(&mut tx, run_id).await?;

    let github = github_context(&base, &run, &meta, &n.event_payload);
    let bctx = base_context(&github, &n.inputs, &vars);

    // The concurrency group is worked out now, so the run can be held or can cancel others.
    let (group, cancel_in_progress) = match &n.workflow.concurrency {
        Some(c) => {
            let g = expr::interpolate(&c.group, &bctx).map_err(|e| Error::invalid("Workflow", "concurrency.group", e.message))?;
            let cancel = expr::eval_typed(&c.cancel_in_progress, &bctx).map(|v| v.truthy() && v.to_display() != "false").unwrap_or(false);
            (Some(g).filter(|g| !g.trim().is_empty()), cancel)
        }
        None => (None, false),
    };
    if let Some(g) = &group {
        sqlx::query("UPDATE workflow_runs SET concurrency_group = $2 WHERE id = $1").bind(run_id).bind(g).execute(&mut *tx).await?;
        if n.needs_approval {
            // A held run cancels nothing and holds no group yet. What it will do is kept for the
            // moment someone approves it.
            sqlx::query("UPDATE workflow_runs SET event_payload = jsonb_set(event_payload, '{meta,cancel_in_progress}', to_jsonb($2::boolean)) WHERE id = $1")
                .bind(run_id)
                .bind(cancel_in_progress)
                .execute(&mut *tx)
                .await?;
        }
    }

    // `run-name:` titles the run, with the same contexts the jobs see.
    if let Some(rn) = n.workflow.run_name.as_deref().filter(|r| !r.trim().is_empty())
        && let Ok(t) = expr::interpolate(rn, &bctx)
        && !t.trim().is_empty()
    {
        sqlx::query("UPDATE workflow_runs SET title = $2 WHERE id = $1")
            .bind(run_id)
            .bind(t.trim().chars().take(300).collect::<String>())
            .execute(&mut *tx)
            .await?;
    }
    let planned = plan::plan(&n.workflow, &bctx).map_err(|d| {
        Error::Validation(d.iter().map(|d| ch_core::FieldError::custom("Workflow", &n.workflow_path, d.to_string())).collect())
    })?;
    let mut fx = Effects::default();
    for p in &planned {
        let spec = JobSpec {
            job: p.job.clone(),
            matrix: p.matrix.clone(),
            runs_on: p.runs_on.clone(),
            timeout_minutes: p.timeout_minutes,
            continue_on_error: p.continue_on_error,
            fail_fast: p.fail_fast,
            max_parallel: p.max_parallel,
            index: p.index,
            total: p.total,
            cancel: false,
            log_seq: 0,
            deferred_matrix: p.deferred_matrix.clone(),
            step_names: p.step_names.clone(),
        };
        let at = JobPlace {
            run_id,
            repo_id: n.repo_id.0,
            head_sha: &n.head_sha,
            workflow_name: &n.workflow_name,
            repository: &info.full_name(),
            base: &base,
        };
        insert_job(&mut tx, &at, &p.key, &p.name, p.matrix.as_ref(), &p.needs, &spec, &mut fx).await?;
    }
    fx.events.push(DomainEvent::WorkflowRunUpdated { repo_id: n.repo_id, run_id: RunId(run_id), status: status.into(), conclusion: None });
    fx.emit(ctx, &mut tx).await?;
    tx.commit().await?;

    // Concurrency first, so the new run is not held by a run it is about to cancel. A run that
    // still waits for approval applies it at approval, so an unapproved pull request cannot cancel
    // a job that is running.
    if let Some(g) = &group
        && !n.needs_approval
    {
        apply_concurrency(ctx, n.repo_id.0, g, run_id, cancel_in_progress).await?;
    }
    advance_run(ctx, run_id).await?;
    Ok(Created { run_id: RunId(run_id), run_number, already_existed: false })
}

/// What the page calls step `i` of a job: the name worked out at planning, else as written.
fn step_name(spec: &JobSpec, i: usize) -> String {
    spec.step_names.get(i).cloned().unwrap_or_else(|| spec.job.steps[i].display_name())
}

/// Where a new job goes: the run, and what its check run and link need to know.
struct JobPlace<'a> {
    run_id: i64,
    repo_id: i64,
    head_sha: &'a str,
    workflow_name: &'a str,
    repository: &'a str,
    base: &'a str,
}

/// Inserts one job as `waiting`, with its steps and its queued check run. Returns the job id.
#[allow(clippy::too_many_arguments)]
async fn insert_job(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    at: &JobPlace<'_>,
    key: &str,
    name: &str,
    matrix: Option<&BTreeMap<String, Json>>,
    needs: &[String],
    spec: &JobSpec,
    fx: &mut Effects,
) -> Result<i64> {
    let labels: Vec<String> = spec.runs_on.iter().map(|l| l.trim().to_lowercase()).collect();
    let job_id: i64 = sqlx::query_scalar(
        "INSERT INTO workflow_jobs (run_id, repo_id, job_key, name, matrix, runs_on, needs, status, spec)
         VALUES ($1, $2, $3, $4, $5, $6, $7, 'waiting', $8) RETURNING id",
    )
    .bind(at.run_id)
    .bind(at.repo_id)
    .bind(key)
    .bind(name)
    .bind(matrix.map(|m| json!(m)))
    .bind(&labels)
    .bind(needs)
    .bind(serde_json::to_value(spec).map_err(Error::internal)?)
    .fetch_one(&mut **tx)
    .await?;
    for (i, s) in spec.job.steps.iter().enumerate() {
        sqlx::query("INSERT INTO workflow_steps (job_id, number, name) VALUES ($1, $2, $3)")
            .bind(job_id)
            .bind(s.number as i32)
            .bind(step_name(spec, i).chars().take(200).collect::<String>())
            .execute(&mut **tx)
            .await?;
    }
    let cname = check_name(at.workflow_name, name);
    let details = format!("{}/{}/actions/runs/{}/jobs/{job_id}", at.base, at.repository, at.run_id);
    sqlx::query("INSERT INTO check_runs (repo_id, head_sha, name, status, details_url, job_id) VALUES ($1, $2, $3, 'queued', $4, $5)")
        .bind(at.repo_id)
        .bind(at.head_sha)
        .bind(&cname)
        .bind(details)
        .bind(job_id)
        .execute(&mut **tx)
        .await?;
    fx.events.push(DomainEvent::CheckRunUpdated {
        repo_id: RepoId(at.repo_id),
        sha: at.head_sha.to_string(),
        name: cname,
        status: "queued".into(),
        conclusion: None,
    });
    Ok(job_id)
}

/// The label sets of the runners that may take jobs from `repo`: not removed, and serving every
/// repository or this one.
pub async fn runner_labels(ctx: &Ctx, repo: i64) -> Result<Vec<Vec<String>>> {
    Ok(sqlx::query_scalar("SELECT labels FROM runners WHERE revoked_at IS NULL AND (repo_id IS NULL OR repo_id = $1) ORDER BY id")
        .bind(repo)
        .fetch_all(&ctx.db)
        .await?)
}

/// Whether some runner has every label a job asks for. The same rule the job queue uses.
pub fn some_runner_fits(runs_on: &[String], offered: &[Vec<String>]) -> bool {
    offered.iter().any(|labels| runs_on.iter().all(|l| labels.contains(l)))
}

/// What a job that no runner can take says, on its page and in its log.
pub fn no_runner_line(runs_on: &[String], offered: &[Vec<String>]) -> String {
    let mut all: Vec<&str> = offered.iter().flatten().map(String::as_str).collect();
    all.sort_unstable();
    all.dedup();
    let asks = runs_on.join(", ");
    if all.is_empty() {
        format!("No runner has the labels this job asks for: {asks}. No runner is registered for this repository.")
    } else {
        format!("No runner has the labels this job asks for: {asks}. The runners here offer: {}.", all.join(", "))
    }
}

/// `<workflow> / <job>`, the name a check run and a required-status rule use.
pub fn check_name(workflow: &str, job: &str) -> String {
    format!("{workflow} / {job}")
}

async fn fetch_run_tx(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, id: i64) -> Result<RunRow> {
    Ok(sqlx::query_as::<_, RunRow>(&format!("SELECT {RUN_COLS} FROM workflow_runs WHERE id = $1")).bind(id).fetch_one(&mut **tx).await?)
}

pub async fn fetch_run(ctx: &Ctx, id: i64) -> Result<RunRow> {
    sqlx::query_as::<_, RunRow>(&format!("SELECT {RUN_COLS} FROM workflow_runs WHERE id = $1"))
        .bind(id)
        .fetch_optional(&ctx.db)
        .await?
        .ok_or(Error::NotFound)
}

pub async fn fetch_jobs(ctx: &Ctx, run: i64) -> Result<Vec<JobRow>> {
    Ok(sqlx::query_as::<_, JobRow>(&format!("SELECT {JOB_COLS} FROM workflow_jobs WHERE run_id = $1 ORDER BY id"))
        .bind(run)
        .fetch_all(&ctx.db)
        .await?)
}

/// Applies a concurrency group to a new run. With `cancel-in-progress` every other active run
/// in the group is cancelled. Without it, only a run that has not started yet is replaced, so
/// at most one run is waiting behind the running one.
async fn apply_concurrency(ctx: &Ctx, repo: i64, group: &str, new_run: i64, cancel_in_progress: bool) -> Result<()> {
    let older: Vec<i64> = if cancel_in_progress {
        sqlx::query_scalar("SELECT id FROM workflow_runs WHERE repo_id = $1 AND concurrency_group = $2 AND id < $3 AND status IN ('queued', 'in_progress') ORDER BY id")
            .bind(repo)
            .bind(group)
            .bind(new_run)
            .fetch_all(&ctx.db)
            .await?
    } else {
        sqlx::query_scalar(
            "SELECT r.id FROM workflow_runs r WHERE r.repo_id = $1 AND r.concurrency_group = $2 AND r.id < $3 AND r.status = 'queued'
               AND NOT EXISTS (SELECT 1 FROM workflow_jobs j WHERE j.run_id = r.id AND j.status IN ('in_progress', 'completed'))
             ORDER BY r.id",
        )
        .bind(repo)
        .bind(group)
        .bind(new_run)
        .fetch_all(&ctx.db)
        .await?
    };
    for id in older {
        cancel_run_inner(ctx, id).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------- advancing

/// How a finished job counts for jobs that need it.
fn needs_result(spec: &JobSpec, conclusion: Option<&str>) -> &'static str {
    match conclusion {
        Some("success") => "success",
        Some("skipped") => "skipped",
        Some("cancelled") => "cancelled",
        Some("failure" | "timed_out") if spec.continue_on_error => "success",
        _ => "failure",
    }
}

/// One needed job as the jobs after it see it: its result and its outputs, over every matrix cell.
pub(crate) fn aggregate(jobs: &[JobRow], key: &str) -> (&'static str, BTreeMap<String, String>) {
    let cells: Vec<&JobRow> = jobs.iter().filter(|j| j.job_key == key).collect();
    let mut results = Vec::new();
    let mut outputs = BTreeMap::new();
    for c in &cells {
        let r = c.spec().map(|s| needs_result(&s, c.conclusion.as_deref())).unwrap_or("failure");
        results.push(r);
        for (k, v) in c.outputs_map() {
            if !v.is_empty() || !outputs.contains_key(&k) {
                outputs.insert(k, v);
            }
        }
    }
    let r = if results.contains(&"failure") {
        "failure"
    } else if results.contains(&"cancelled") {
        "cancelled"
    } else if !results.is_empty() && results.iter().all(|r| *r == "skipped") {
        "skipped"
    } else {
        "success"
    };
    (r, outputs)
}

async fn group_held(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, run: &RunRow) -> Result<bool> {
    let Some(g) = &run.concurrency_group else { return Ok(false) };
    let held: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM workflow_runs WHERE repo_id = $1 AND concurrency_group = $2 AND id < $3 AND status IN ('queued', 'in_progress'))",
    )
    .bind(run.repo_id)
    .bind(g)
    .bind(run.id)
    .fetch_one(&mut **tx)
    .await?;
    Ok(held)
}

/// Moves one job to a new state and mirrors it on its check run.
async fn transition(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run: &RunRow,
    job: &mut JobRow,
    status: &str,
    conclusion: Option<&str>,
    fx: &mut Effects,
) -> Result<()> {
    let spec = job.spec()?;
    sqlx::query(
        "UPDATE workflow_jobs SET status = $2, conclusion = $3,
                started_at = CASE WHEN $2 = 'in_progress' AND started_at IS NULL THEN now() ELSE started_at END,
                completed_at = CASE WHEN $2 = 'completed' THEN now() ELSE completed_at END,
                lease_expires_at = CASE WHEN $2 = 'completed' THEN NULL ELSE lease_expires_at END
         WHERE id = $1",
    )
    .bind(job.id)
    .bind(status)
    .bind(conclusion)
    .execute(&mut **tx)
    .await?;
    if status == "completed" {
        // A step still waiting when the job ends did not run.
        sqlx::query("UPDATE workflow_steps SET status = 'completed', conclusion = 'skipped' WHERE job_id = $1 AND status = 'queued'")
            .bind(job.id)
            .execute(&mut **tx)
            .await?;
        sqlx::query("UPDATE workflow_steps SET status = 'completed', conclusion = $2, completed_at = now() WHERE job_id = $1 AND status = 'in_progress'")
            .bind(job.id)
            .bind(match conclusion {
                Some("cancelled") => "cancelled",
                Some("success") => "success",
                _ => "failure",
            })
            .execute(&mut **tx)
            .await?;
    }
    // A job allowed to fail reads as neutral, so it does not turn the commit red.
    let check_conclusion = match conclusion {
        Some("failure" | "timed_out") if spec.continue_on_error => Some("neutral"),
        other => other,
    };
    let check: Option<(String, String)> = sqlx::query_as(
        "UPDATE check_runs SET status = $2, conclusion = $3,
                started_at = CASE WHEN $2 = 'in_progress' AND started_at IS NULL THEN now() ELSE started_at END,
                completed_at = CASE WHEN $2 = 'completed' THEN now() ELSE completed_at END
         WHERE job_id = $1 RETURNING head_sha, name",
    )
    .bind(job.id)
    .bind(status)
    .bind(check_conclusion)
    .fetch_optional(&mut **tx)
    .await?;
    let repo = RepoId(run.repo_id);
    fx.events.push(DomainEvent::WorkflowJobUpdated {
        repo_id: repo,
        run_id: RunId(run.id),
        job_id: JobId(job.id),
        status: status.into(),
        conclusion: conclusion.map(String::from),
    });
    if let Some((sha, name)) = check {
        fx.events.push(DomainEvent::CheckRunUpdated {
            repo_id: repo,
            sha,
            name,
            status: status.into(),
            conclusion: check_conclusion.map(String::from),
        });
    }
    if status == "completed" {
        fx.finished_jobs.push(job.id);
    }
    if status == "queued" {
        fx.wake = true;
    }
    job.status = status.into();
    job.conclusion = conclusion.map(String::from);
    Ok(())
}

async fn flag_cancel(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, job: &mut JobRow) -> Result<()> {
    sqlx::query("UPDATE workflow_jobs SET spec = jsonb_set(spec, '{cancel}', 'true') WHERE id = $1")
        .bind(job.id)
        .execute(&mut **tx)
        .await?;
    if let Some(o) = job.spec.as_object_mut() {
        o.insert("cancel".into(), Json::Bool(true));
    }
    Ok(())
}

/// Moves the run forward as far as it can go: cancels what must stop, promotes jobs whose
/// needs are done, skips jobs whose `if:` is false, and rolls the jobs up into the run.
/// Safe to call at any time and any number of times.
pub async fn advance_run(ctx: &Ctx, run_id: i64) -> Result<()> {
    let mut tx = ctx.db.begin().await?;
    let Some(run) = sqlx::query_as::<_, RunRow>(&format!("SELECT {RUN_COLS} FROM workflow_runs WHERE id = $1 FOR UPDATE"))
        .bind(run_id)
        .fetch_optional(&mut *tx)
        .await?
    else {
        return Ok(());
    };
    if run.status == "completed" {
        return Ok(());
    }
    let (event, meta) = split_payload(&run.event_payload);
    let mut jobs: Vec<JobRow> =
        sqlx::query_as::<_, JobRow>(&format!("SELECT {JOB_COLS} FROM workflow_jobs WHERE run_id = $1 ORDER BY id FOR UPDATE"))
            .bind(run_id)
            .fetch_all(&mut *tx)
            .await?;
    let mut fx = Effects::default();

    // 1. A cancelled run: whatever has not started never will, and what has is told to stop.
    if meta.cancel_requested {
        for job in jobs.iter_mut() {
            match job.status.as_str() {
                "waiting" | "queued" => transition(&mut tx, &run, job, "completed", Some("cancelled"), &mut fx).await?,
                "in_progress" => flag_cancel(&mut tx, job).await?,
                _ => {}
            }
        }
    }

    // 2. Fail-fast: when a matrix cell fails, its siblings stop.
    let keys: Vec<String> = {
        let mut k: Vec<String> = jobs.iter().map(|j| j.job_key.clone()).collect();
        k.dedup();
        k
    };
    for key in &keys {
        let failed = jobs.iter().any(|j| {
            j.job_key == *key
                && j.status == "completed"
                && is_failure(j.conclusion.as_deref())
                && !j.spec().map(|s| s.continue_on_error).unwrap_or(false)
        });
        let fail_fast =
            jobs.iter().find(|j| j.job_key == *key).and_then(|j| j.spec().ok()).map(|s| s.fail_fast && s.total > 1).unwrap_or(false);
        if failed && fail_fast {
            for job in jobs.iter_mut().filter(|j| j.job_key == *key) {
                match job.status.as_str() {
                    "waiting" | "queued" => transition(&mut tx, &run, job, "completed", Some("cancelled"), &mut fx).await?,
                    "in_progress" if !job.spec().map(|s| s.cancel).unwrap_or(false) => flag_cancel(&mut tx, job).await?,
                    _ => {}
                }
            }
        }
    }

    // 3. Promote or skip waiting jobs until nothing changes.
    let held = run.status == "waiting" || group_held(&mut tx, &run).await?;
    if !held && !meta.cancel_requested {
        let vars = load_vars_tx(&mut tx, run.repo_id, meta.fork).await?;
        let github = github_context(&ctx.base_url(), &run, &meta, &event);
        loop {
            let mut progressed = false;
            let mut extra: Vec<JobRow> = Vec::new();
            // An index, not an iterator: each job is judged against the others, and expanding a matrix adds rows.
            #[allow(clippy::needless_range_loop)]
            for i in 0..jobs.len() {
                if jobs[i].status != "waiting" {
                    continue;
                }
                let needs = jobs[i].needs.clone();
                let ready = needs.iter().all(|k| jobs.iter().filter(|j| j.job_key == *k).all(|j| j.status == "completed"));
                if !ready {
                    continue;
                }
                let spec = jobs[i].spec()?;
                let mut needs_ctx = serde_json::Map::new();
                let mut status = Status::Success;
                for k in &needs {
                    let (r, outputs) = aggregate(&jobs, k);
                    match r {
                        "cancelled" => status = Status::Cancelled,
                        "failure" | "skipped" if status != Status::Cancelled => status = Status::Failure,
                        _ => {}
                    }
                    needs_ctx.insert(k.clone(), json!({ "result": r, "outputs": outputs }));
                }
                let mut cx = base_context(&github, &meta.inputs, &vars).with_status(status);
                cx.set("needs", Value::from_json(&Json::Object(needs_ctx)));
                cx.set("matrix", Value::from_json(&json!(spec.matrix.clone().unwrap_or_default())));
                let verdict = expr::should_run(spec.job.condition.as_deref(), &cx);
                let mut j = jobs[i].clone();
                match verdict {
                    Ok(true) if spec.deferred_matrix.is_some() => {
                        // The matrix was waiting on `needs`. Now it can be expanded: this job
                        // becomes the first cell and the others are added after it.
                        let m = spec.deferred_matrix.clone().unwrap_or_default();
                        let cells = plan::expand_deferred(&m, &cx);
                        match cells {
                            Ok(cells) if cells.is_empty() => {
                                transition(&mut tx, &run, &mut j, "completed", Some("skipped"), &mut fx).await?;
                            }
                            Ok(cells) => {
                                let at = JobPlace {
                                    run_id: run.id,
                                    repo_id: run.repo_id,
                                    head_sha: &run.head_sha,
                                    workflow_name: &meta.workflow_name,
                                    repository: &meta.repository,
                                    base: &ctx.base_url(),
                                };
                                let total = cells.len();
                                let mut planned = Vec::new();
                                let mut bad = None;
                                for (idx, cell) in cells.into_iter().enumerate() {
                                    match plan::plan_cell(&spec.job, Some(cell), idx, total, spec.fail_fast, spec.max_parallel, None, &cx) {
                                        Ok(p) => planned.push(p),
                                        Err(d) => {
                                            bad = Some(d.iter().map(|x| x.message.clone()).collect::<Vec<_>>().join("; "));
                                            break;
                                        }
                                    }
                                }
                                if let Some(msg) = bad {
                                    crate::logs::append_system_line(ctx, &run, &j, &format!("The matrix could not be expanded: {msg}"))
                                        .await;
                                    transition(&mut tx, &run, &mut j, "completed", Some("failure"), &mut fx).await?;
                                } else {
                                    for (idx, p) in planned.iter().enumerate() {
                                        let ns = JobSpec {
                                            job: p.job.clone(),
                                            matrix: p.matrix.clone(),
                                            runs_on: p.runs_on.clone(),
                                            timeout_minutes: p.timeout_minutes,
                                            continue_on_error: p.continue_on_error,
                                            fail_fast: p.fail_fast,
                                            max_parallel: p.max_parallel,
                                            index: p.index,
                                            total: p.total,
                                            cancel: false,
                                            log_seq: 0,
                                            deferred_matrix: None,
                                            step_names: p.step_names.clone(),
                                        };
                                        if idx == 0 {
                                            let labels: Vec<String> = ns.runs_on.iter().map(|l| l.trim().to_lowercase()).collect();
                                            sqlx::query(
                                                "UPDATE workflow_jobs SET name = $2, matrix = $3, runs_on = $4, spec = $5 WHERE id = $1",
                                            )
                                            .bind(j.id)
                                            .bind(&p.name)
                                            .bind(p.matrix.as_ref().map(|m| json!(m)))
                                            .bind(&labels)
                                            .bind(serde_json::to_value(&ns).map_err(Error::internal)?)
                                            .execute(&mut *tx)
                                            .await?;
                                            for (i, st) in ns.job.steps.iter().enumerate() {
                                                sqlx::query("UPDATE workflow_steps SET name = $3 WHERE job_id = $1 AND number = $2")
                                                    .bind(j.id)
                                                    .bind(st.number as i32)
                                                    .bind(step_name(&ns, i).chars().take(200).collect::<String>())
                                                    .execute(&mut *tx)
                                                    .await?;
                                            }
                                            sqlx::query("UPDATE check_runs SET name = $2 WHERE job_id = $1")
                                                .bind(j.id)
                                                .bind(check_name(&meta.workflow_name, &p.name))
                                                .execute(&mut *tx)
                                                .await?;
                                            j.name = p.name.clone();
                                            j.matrix = p.matrix.as_ref().map(|m| json!(m));
                                            j.runs_on = labels;
                                            j.spec = serde_json::to_value(&ns).map_err(Error::internal)?;
                                        } else {
                                            let id = insert_job(&mut tx, &at, &p.key, &p.name, p.matrix.as_ref(), &p.needs, &ns, &mut fx)
                                                .await?;
                                            let row =
                                                sqlx::query_as::<_, JobRow>(&format!("SELECT {JOB_COLS} FROM workflow_jobs WHERE id = $1"))
                                                    .bind(id)
                                                    .fetch_one(&mut *tx)
                                                    .await?;
                                            extra.push(row);
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                crate::logs::append_system_line(ctx, &run, &j, &format!("The matrix could not be expanded: {e}")).await;
                                transition(&mut tx, &run, &mut j, "completed", Some("failure"), &mut fx).await?;
                            }
                        }
                    }
                    Ok(true) => {
                        if let Some(mp) = spec.max_parallel {
                            let active = jobs
                                .iter()
                                .filter(|o| o.job_key == j.job_key && matches!(o.status.as_str(), "queued" | "in_progress"))
                                .count();
                            if active >= mp as usize {
                                continue;
                            }
                        }
                        transition(&mut tx, &run, &mut j, "queued", None, &mut fx).await?;
                    }
                    Ok(false) => {
                        transition(&mut tx, &run, &mut j, "completed", Some("skipped"), &mut fx).await?;
                    }
                    Err(e) => {
                        crate::logs::append_system_line(ctx, &run, &j, &format!("The job's `if:` could not be evaluated: {}", e.message))
                            .await;
                        transition(&mut tx, &run, &mut j, "completed", Some("failure"), &mut fx).await?;
                    }
                }
                jobs[i] = j;
                progressed = true;
            }
            jobs.append(&mut extra);
            if !progressed {
                break;
            }
        }
    }

    // 4. Roll the jobs up into the run.
    let all_done = !jobs.is_empty() && jobs.iter().all(|j| j.status == "completed");
    let new_status;
    let mut new_conclusion = None;
    if all_done {
        new_status = "completed";
        let failed = jobs.iter().any(|j| is_failure(j.conclusion.as_deref()) && !j.spec().map(|s| s.continue_on_error).unwrap_or(false));
        new_conclusion = Some(if meta.cancel_requested {
            "cancelled"
        } else if failed {
            "failure"
        } else if jobs.iter().any(|j| j.conclusion.as_deref() == Some("cancelled")) {
            "cancelled"
        } else if jobs.iter().all(|j| j.conclusion.as_deref() == Some("skipped")) {
            "skipped"
        } else {
            "success"
        });
    } else if jobs.iter().any(|j| matches!(j.status.as_str(), "in_progress" | "completed")) {
        new_status = "in_progress";
    } else {
        new_status = run.status.as_str();
    }
    if new_status != run.status || new_conclusion != run.conclusion.as_deref() {
        sqlx::query(
            "UPDATE workflow_runs SET status = $2, conclusion = $3,
                    started_at = CASE WHEN $2 IN ('in_progress', 'completed') AND started_at IS NULL THEN now() ELSE started_at END,
                    completed_at = CASE WHEN $2 = 'completed' THEN now() ELSE completed_at END
             WHERE id = $1",
        )
        .bind(run.id)
        .bind(new_status)
        .bind(new_conclusion)
        .execute(&mut *tx)
        .await?;
        fx.events.push(DomainEvent::WorkflowRunUpdated {
            repo_id: RepoId(run.repo_id),
            run_id: RunId(run.id),
            status: new_status.into(),
            conclusion: new_conclusion.map(String::from),
        });
        if new_status == "completed" {
            fx.run_finished = Some((run.repo_id, run.concurrency_group.clone()));
        }
    }
    fx.emit(ctx, &mut tx).await?;
    tx.commit().await?;
    fx.after_commit(ctx);
    // The next run in the group may have been waiting for this one.
    if let Some((repo, Some(group))) = fx.run_finished.take() {
        Box::pin(advance_group(ctx, repo, &group)).await?;
    }
    Ok(())
}

async fn load_vars_tx(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, repo: i64, fork: bool) -> Result<BTreeMap<String, String>> {
    if fork {
        return Ok(BTreeMap::new());
    }
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT name, value, scope FROM variables
         WHERE (scope = 'repo' AND repo_id = $1) OR (scope = 'org' AND org_id = (SELECT owner_id FROM repositories WHERE id = $1))",
    )
    .bind(repo)
    .fetch_all(&mut **tx)
    .await?;
    let mut out = BTreeMap::new();
    for (n, v, _) in rows.iter().filter(|r| r.2 == "org") {
        out.insert(n.clone(), v.clone());
    }
    for (n, v, _) in rows.iter().filter(|r| r.2 == "repo") {
        out.insert(n.clone(), v.clone());
    }
    Ok(out)
}

/// Advances every active run of a concurrency group, oldest first.
pub async fn advance_group(ctx: &Ctx, repo: i64, group: &str) -> Result<()> {
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM workflow_runs WHERE repo_id = $1 AND concurrency_group = $2 AND status IN ('queued', 'in_progress') ORDER BY id",
    )
    .bind(repo)
    .bind(group)
    .fetch_all(&ctx.db)
    .await?;
    for id in ids {
        advance_run(ctx, id).await?;
    }
    Ok(())
}

/// Advances every run that is not finished. The sweeper calls this so a missed wake-up (a crash
/// between two steps) heals itself.
pub async fn advance_all_active(ctx: &Ctx) -> Result<u64> {
    let ids: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM workflow_runs WHERE status IN ('queued', 'in_progress') ORDER BY id").fetch_all(&ctx.db).await?;
    let n = ids.len() as u64;
    for id in ids {
        if let Err(e) = advance_run(ctx, id).await {
            tracing::warn!(run = id, error = %e, "could not advance run");
        }
    }
    Ok(n)
}

// ---------------------------------------------------------------------------- cancel, re-run, approve

async fn cancel_run_inner(ctx: &Ctx, run_id: i64) -> Result<()> {
    sqlx::query(
        "UPDATE workflow_runs SET event_payload = jsonb_set(event_payload, '{meta,cancel_requested}', 'true')
         WHERE id = $1 AND status <> 'completed'",
    )
    .bind(run_id)
    .execute(&ctx.db)
    .await?;
    advance_run(ctx, run_id).await
}

/// Cancels a run: jobs not started never start, running ones stop at their next heartbeat.
pub async fn cancel_run(ctx: &Ctx, run_id: i64) -> Result<()> {
    let run = fetch_run(ctx, run_id).await?;
    if run.status == "completed" {
        return Err(Error::conflict("Cannot cancel a workflow run that is completed."));
    }
    cancel_run_inner(ctx, run_id).await
}

/// Starts a new attempt of a finished run from the jobs as they were planned. With
/// `failed_only`, jobs that succeeded (or were skipped) are copied across as they are. `by` is
/// recorded as whoever started the attempt; the original run's actor is kept when there is none.
pub async fn rerun(ctx: &Ctx, run_id: i64, failed_only: bool, by: Option<UserId>) -> Result<RunId> {
    let old = fetch_run(ctx, run_id).await?;
    if old.status != "completed" {
        return Err(Error::conflict("This workflow run is still in progress."));
    }
    let info = repo_ci(ctx, RepoId(old.repo_id)).await?;
    if info.availability() != ch_core::model::CiAvailability::Enabled {
        return Err(Error::forbidden("Workflows are not enabled for this repository."));
    }
    // A saved run keeps its ref and commit, so starting it again needs the same check as dispatch.
    if old.event == "workflow_dispatch" {
        let full = old.git_ref.as_deref().unwrap_or_default();
        if crate::source::branch_or_tag(ctx.source.as_ref(), RepoId(old.repo_id), full).await?.is_none() {
            return Err(Error::invalid("Dispatch", "ref", crate::source::CHOOSE_REF));
        }
    }
    let (event, mut meta) = split_payload(&old.event_payload);
    if meta.problem.is_some() {
        return Err(Error::conflict("This run never started because the workflow file has a problem. Fix the file and push again."));
    }
    // A run that waited for approval and was cancelled before anyone gave it never ran with a
    // person's say-so. Starting it again must not skip that (ch-x12).
    if meta.hold_reason.is_some() && meta.approved_by.is_none() {
        return Err(Error::conflict("This run was never approved, so it cannot be re-run. Push again or approve a new run."));
    }
    let old_jobs = fetch_jobs(ctx, old.id).await?;
    meta.cancel_requested = false;
    let base = ctx.base_url();
    let mut tx = ctx.db.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)").bind(0x0c1_0000_0000_0000_i64 | old.repo_id).execute(&mut *tx).await?;
    let (attempt,): (i32,) =
        sqlx::query_as("SELECT COALESCE(MAX(attempt), 0) + 1 FROM workflow_runs WHERE repo_id = $1 AND run_number = $2")
            .bind(old.repo_id)
            .bind(old.run_number)
            .fetch_one(&mut *tx)
            .await?;
    let new_id: i64 = sqlx::query_scalar(
        "INSERT INTO workflow_runs (repo_id, workflow_id, run_number, attempt, event, head_sha, head_branch, git_ref, title, actor_id, status, concurrency_group, event_payload, recorded_tier)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 'queued', $11, $12, (SELECT recorded_tier FROM workflow_runs WHERE id = $13)) RETURNING id",
    )
    // A re-run keeps the tier the run was recorded at: what it shows below Read must not widen
    // because the repository did (ch-4pn.12).
    .bind(old.repo_id)
    .bind(old.workflow_id)
    .bind(old.run_number)
    .bind(attempt)
    .bind(&old.event)
    .bind(&old.head_sha)
    .bind(&old.head_branch)
    .bind(&old.git_ref)
    .bind(&old.title)
    .bind(by.map(|u| u.0).or(old.actor_id))
    .bind(&old.concurrency_group)
    .bind(join_payload(&event, &meta))
    .bind(old.id)
    .fetch_one(&mut *tx)
    .await?;
    let mut fx = Effects::default();
    for j in &old_jobs {
        let mut spec = j.spec()?;
        let keep = failed_only && j.status == "completed" && matches!(j.conclusion.as_deref(), Some("success" | "skipped"));
        spec.cancel = false;
        spec.log_seq = 0;
        let (status, conclusion, outputs) =
            if keep { ("completed", j.conclusion.clone(), j.outputs.clone()) } else { ("waiting", None, json!({})) };
        let job_id: i64 = sqlx::query_scalar(
            "INSERT INTO workflow_jobs (run_id, repo_id, job_key, name, matrix, runs_on, needs, status, conclusion, spec, outputs, started_at, completed_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) RETURNING id",
        )
        .bind(new_id)
        .bind(old.repo_id)
        .bind(&j.job_key)
        .bind(&j.name)
        .bind(&j.matrix)
        .bind(&j.runs_on)
        .bind(&j.needs)
        .bind(status)
        .bind(&conclusion)
        .bind(serde_json::to_value(&spec).map_err(Error::internal)?)
        .bind(outputs)
        .bind(if keep { j.started_at } else { None })
        .bind(if keep { j.completed_at } else { None })
        .fetch_one(&mut *tx)
        .await?;
        for (i, s) in spec.job.steps.iter().enumerate() {
            let (sst, sconc) = if keep { ("completed", Some("success")) } else { ("queued", None) };
            sqlx::query("INSERT INTO workflow_steps (job_id, number, name, status, conclusion) VALUES ($1, $2, $3, $4, $5)")
                .bind(job_id)
                .bind(s.number as i32)
                .bind(step_name(&spec, i).chars().take(200).collect::<String>())
                .bind(sst)
                .bind(sconc)
                .execute(&mut *tx)
                .await?;
        }
        let name = check_name(&meta.workflow_name, &j.name);
        let details = format!("{base}/{}/actions/runs/{new_id}/jobs/{job_id}", info.full_name());
        let check_conc = if keep { conclusion.as_deref() } else { None };
        sqlx::query(
            "INSERT INTO check_runs (repo_id, head_sha, name, status, conclusion, details_url, job_id) VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(old.repo_id)
        .bind(&old.head_sha)
        .bind(&name)
        .bind(if keep { "completed" } else { "queued" })
        .bind(check_conc)
        .bind(details)
        .bind(job_id)
        .execute(&mut *tx)
        .await?;
        fx.events.push(DomainEvent::CheckRunUpdated {
            repo_id: RepoId(old.repo_id),
            sha: old.head_sha.clone(),
            name,
            status: if keep { "completed".into() } else { "queued".into() },
            conclusion: check_conc.map(String::from),
        });
    }
    fx.events.push(DomainEvent::WorkflowRunUpdated {
        repo_id: RepoId(old.repo_id),
        run_id: RunId(new_id),
        status: "queued".into(),
        conclusion: None,
    });
    fx.emit(ctx, &mut tx).await?;
    tx.commit().await?;
    advance_run(ctx, new_id).await?;
    Ok(RunId(new_id))
}

/// A maintainer lets a run from a fork go ahead.
pub async fn approve_run(ctx: &Ctx, run_id: i64, approver: UserId) -> Result<()> {
    let mut tx = ctx.db.begin().await?;
    let row: Option<(String, bool)> = sqlx::query_as(
        "SELECT status, COALESCE((event_payload->'meta'->>'fork')::boolean, false) FROM workflow_runs WHERE id = $1 FOR UPDATE",
    )
    .bind(run_id)
    .fetch_optional(&mut *tx)
    .await?;
    match row {
        None => return Err(Error::NotFound),
        // A fork's runs are approved by commit, before they exist (ch-x12). One waiting from
        // before that was cancelled when the change went in.
        Some((_, true)) => return Err(Error::conflict("Runs from a fork are approved on the pull request, one commit at a time.")),
        Some((s, false)) if s == "waiting" => {}
        Some(_) => return Err(Error::conflict("This workflow run does not need approval.")),
    }
    sqlx::query("UPDATE workflow_runs SET status = 'queued', event_payload = jsonb_set(event_payload, '{meta,approved_by}', to_jsonb($2::bigint)) WHERE id = $1")
        .bind(run_id)
        .bind(approver.0)
        .execute(&mut *tx)
        .await?;
    // A person vouched for this commit of a pull request, so a commit the server later builds on
    // it (Update branch, a suggestion, a web edit) need not wait again (ch-3xn).
    sqlx::query(
        "INSERT INTO pull_cleared_commits (pull_id, head_sha)
         SELECT pr.issue_id, r.head_sha FROM workflow_runs r
           JOIN issues i ON i.repo_id = r.repo_id AND i.number = (r.event_payload->'event'->'pull_request'->>'number')::bigint
           JOIN pull_requests pr ON pr.issue_id = i.id
          WHERE r.id = $1 AND r.event = 'pull_request'
         ON CONFLICT DO NOTHING",
    )
    .bind(run_id)
    .execute(&mut *tx)
    .await?;
    let held: (i64, Option<String>, bool) = sqlx::query_as(
        "SELECT repo_id, concurrency_group, COALESCE((event_payload->'meta'->>'cancel_in_progress')::boolean, false) FROM workflow_runs WHERE id = $1",
    )
    .bind(run_id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    // Only now does the run take its place in its concurrency group.
    if let (repo, Some(g), cancel) = held {
        apply_concurrency(ctx, repo, &g, run_id, cancel).await?;
    }
    advance_run(ctx, run_id).await
}

// ---------------------------------------------------------------------------- jobs finishing

/// Records that a job ended and moves its run along. `outputs` are the job's evaluated outputs.
/// Returns false when the job had already finished.
pub async fn finish_job(ctx: &Ctx, job_id: i64, conclusion: &str, outputs: &BTreeMap<String, String>) -> Result<bool> {
    let mut tx = ctx.db.begin().await?;
    let Some(run_id): Option<i64> =
        sqlx::query_scalar("SELECT run_id FROM workflow_jobs WHERE id = $1").bind(job_id).fetch_optional(&mut *tx).await?
    else {
        return Err(Error::NotFound);
    };
    let run = sqlx::query_as::<_, RunRow>(&format!("SELECT {RUN_COLS} FROM workflow_runs WHERE id = $1 FOR UPDATE"))
        .bind(run_id)
        .fetch_one(&mut *tx)
        .await?;
    let mut job = sqlx::query_as::<_, JobRow>(&format!("SELECT {JOB_COLS} FROM workflow_jobs WHERE id = $1 FOR UPDATE"))
        .bind(job_id)
        .fetch_one(&mut *tx)
        .await?;
    if job.status == "completed" {
        return Ok(false);
    }
    let mut fx = Effects::default();
    sqlx::query("UPDATE workflow_jobs SET outputs = $2 WHERE id = $1").bind(job_id).bind(json!(outputs)).execute(&mut *tx).await?;
    transition(&mut tx, &run, &mut job, "completed", Some(conclusion), &mut fx).await?;
    fx.emit(ctx, &mut tx).await?;
    tx.commit().await?;
    fx.after_commit(ctx);
    advance_run(ctx, run_id).await?;
    Ok(true)
}

/// Marks a queued job as taken by a runner, in the caller's transaction. The caller has
/// already set `status`, `runner_id` and the lease with its claim statement.
pub async fn note_job_started(ctx: &Ctx, tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, job: &JobRow, fx: &mut Effects) -> Result<()> {
    let _ = ctx;
    let check: Option<(String, String)> = sqlx::query_as(
        "UPDATE check_runs SET status = 'in_progress', started_at = COALESCE(started_at, now()) WHERE job_id = $1 RETURNING head_sha, name",
    )
    .bind(job.id)
    .fetch_optional(&mut **tx)
    .await?;
    let repo = RepoId(job.repo_id);
    fx.events.push(DomainEvent::WorkflowJobUpdated {
        repo_id: repo,
        run_id: RunId(job.run_id),
        job_id: JobId(job.id),
        status: "in_progress".into(),
        conclusion: None,
    });
    if let Some((sha, name)) = check {
        fx.events.push(DomainEvent::CheckRunUpdated { repo_id: repo, sha, name, status: "in_progress".into(), conclusion: None });
    }
    sqlx::query(
        "UPDATE workflow_runs SET status = 'in_progress', started_at = COALESCE(started_at, now())
         WHERE id = $1 AND status IN ('queued')",
    )
    .bind(job.run_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(coe: bool) -> JobSpec {
        JobSpec {
            job: serde_json::from_value(json!({
                "key": "a", "line": 1, "name": null, "runs_on": ["x"], "container": null, "strategy": null, "needs": [], "condition": null,
                "timeout_minutes": null, "continue_on_error": null, "env": {}, "outputs": {}, "defaults": {"shell": null, "working_directory": null},
                "concurrency": null, "steps": []
            }))
            .unwrap(),
            matrix: None,
            runs_on: vec!["x".into()],
            timeout_minutes: 10,
            continue_on_error: coe,
            fail_fast: true,
            max_parallel: None,
            index: 0,
            total: 1,
            cancel: false,
            log_seq: 0,
            deferred_matrix: None,
            step_names: vec![],
        }
    }

    #[test]
    fn a_failed_job_allowed_to_fail_counts_as_success_for_needs() {
        assert_eq!(needs_result(&spec(true), Some("failure")), "success");
        assert_eq!(needs_result(&spec(false), Some("failure")), "failure");
        assert_eq!(needs_result(&spec(false), Some("timed_out")), "failure");
        assert_eq!(needs_result(&spec(false), Some("cancelled")), "cancelled");
        assert_eq!(needs_result(&spec(false), Some("skipped")), "skipped");
        assert_eq!(needs_result(&spec(false), None), "failure");
    }

    #[test]
    fn check_names_have_the_workflow_first() {
        assert_eq!(check_name("ci", "test (a, 1)"), "ci / test (a, 1)");
    }

    #[test]
    fn github_context_has_what_scripts_expect() {
        let run = RunRow {
            id: 5,
            repo_id: 1,
            workflow_id: None,
            run_number: 3,
            attempt: 2,
            event: "push".into(),
            head_sha: "a".repeat(40),
            head_branch: Some("main".into()),
            git_ref: Some("refs/heads/release/1".into()),
            title: String::new(),
            actor_id: None,
            status: "queued".into(),
            conclusion: None,
            concurrency_group: None,
            event_payload: Json::Null,
            created_at: Utc::now(),
            started_at: None,
            completed_at: None,
        };
        let meta =
            RunMeta { repository: "alice/demo".into(), actor_login: "alice".into(), workflow_name: "ci".into(), ..Default::default() };
        let g = github_context("https://x.example", &run, &meta, &json!({"k": 1}));
        assert_eq!(g["ref_name"], "release/1");
        assert_eq!(g["ref_type"], "branch");
        assert_eq!(g["repository_owner"], "alice");
        assert_eq!(g["run_attempt"], "2");
        assert_eq!(g["api_url"], "https://x.example/api/v3");
        assert_eq!(g["event"]["k"], 1);
    }
}
