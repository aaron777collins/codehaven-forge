//! REST v3 for statuses, checks and Actions, shaped like GitHub's so existing tools work.
//!
//! Every handler gets the caller from the gate (`CurrentActor`), finds the repository through
//! `Repos` (a repository the caller cannot read is a 404, the same as one that does not exist),
//! asks `Authz` for what the call needs, and then does the work through the same functions the
//! service uses.

use crate::ctx::Ctx;
use crate::runs::{self, JobRow, RunRow};
use crate::secrets::{self, Scope};
use crate::service::{runner_summaries, steps_for};
use crate::store;
use axum::Json;
use axum::Router;
use axum::extract::{Extension, Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use ch_api_types::ci as dto;
use ch_api_types::common::SimpleUser;
use ch_core::access::CurrentActor;
use ch_core::audit::{AuditEvent, Outcome, action};
use ch_core::model::{ClientInfo, LogRange, Repo};
use ch_core::state::AppState;
use ch_core::{Actor, Error, JobId, Permission, RepoId, Result, RunId, UserId};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::json;

pub fn routes() -> Router<AppState> {
    Router::new()
        // statuses and checks
        .route("/repos/{owner}/{repo}/statuses/{sha}", post(create_status))
        .route("/repos/{owner}/{repo}/commits/{reference}/statuses", get(list_statuses))
        .route("/repos/{owner}/{repo}/commits/{reference}/status", get(combined_status))
        .route("/repos/{owner}/{repo}/commits/{reference}/check-runs", get(list_check_runs))
        .route("/repos/{owner}/{repo}/check-runs/{id}", get(get_check_run))
        // workflows
        .route("/repos/{owner}/{repo}/actions/workflows", get(list_workflows))
        .route("/repos/{owner}/{repo}/actions/workflows/{workflow}", get(get_workflow))
        .route("/repos/{owner}/{repo}/actions/workflows/{workflow}/enable", put(enable_workflow))
        .route("/repos/{owner}/{repo}/actions/workflows/{workflow}/disable", put(disable_workflow))
        .route("/repos/{owner}/{repo}/actions/workflows/{workflow}/dispatches", post(dispatch_workflow))
        .route("/repos/{owner}/{repo}/actions/workflows/{workflow}/runs", get(list_workflow_runs))
        // runs and jobs
        .route("/repos/{owner}/{repo}/actions/runs", get(list_runs))
        .route("/repos/{owner}/{repo}/actions/runs/{id}", get(get_run))
        .route("/repos/{owner}/{repo}/actions/runs/{id}/cancel", post(cancel_run))
        .route("/repos/{owner}/{repo}/actions/runs/{id}/rerun", post(rerun_run))
        .route("/repos/{owner}/{repo}/actions/runs/{id}/rerun-failed-jobs", post(rerun_failed))
        .route("/repos/{owner}/{repo}/actions/runs/{id}/approve", post(approve_run))
        // Codehaven's own: a pull request from a fork runs nothing until its commit is approved.
        .route("/repos/{owner}/{repo}/actions/pulls/{number}/approval", get(fork_approval).post(approve_fork_commit))
        .route("/repos/{owner}/{repo}/actions/runs/{id}/jobs", get(list_run_jobs))
        .route("/repos/{owner}/{repo}/actions/runs/{id}/logs", get(run_logs))
        .route("/repos/{owner}/{repo}/actions/runs/{id}/artifacts", get(list_run_artifacts))
        .route("/repos/{owner}/{repo}/actions/jobs/{id}", get(get_job))
        .route("/repos/{owner}/{repo}/actions/jobs/{id}/logs", get(job_logs))
        .route("/repos/{owner}/{repo}/actions/jobs/{id}/logs/stream", get(crate::live::stream_logs))
        // artifacts and caches
        .route("/repos/{owner}/{repo}/actions/artifacts", get(list_artifacts))
        .route("/repos/{owner}/{repo}/actions/artifacts/{id}", get(get_artifact).delete(delete_artifact))
        .route("/repos/{owner}/{repo}/actions/artifacts/{id}/zip", get(artifact_zip))
        .route("/repos/{owner}/{repo}/actions/caches", get(list_caches))
        .route("/repos/{owner}/{repo}/actions/caches/{id}", delete(delete_cache))
        // secrets and variables
        .route("/repos/{owner}/{repo}/actions/secrets", get(repo_secrets))
        .route("/repos/{owner}/{repo}/actions/secrets/{name}", get(repo_secret).put(put_repo_secret).delete(delete_repo_secret))
        .route("/repos/{owner}/{repo}/actions/variables", get(repo_variables).post(create_repo_variable))
        .route(
            "/repos/{owner}/{repo}/actions/variables/{name}",
            get(repo_variable).patch(update_repo_variable).delete(delete_repo_variable),
        )
        .route("/orgs/{org}/actions/secrets", get(org_secrets))
        .route("/orgs/{org}/actions/secrets/{name}", put(put_org_secret).delete(delete_org_secret))
        .route("/orgs/{org}/actions/variables", get(org_variables).post(create_org_variable))
        .route("/orgs/{org}/actions/variables/{name}", axum::routing::patch(update_org_variable).delete(delete_org_variable))
        // runners
        .route("/repos/{owner}/{repo}/actions/runners", get(repo_runners))
        .route("/repos/{owner}/{repo}/actions/runners/registration-token", post(repo_runner_token))
        .route("/admin/actions/runners", get(admin_runners))
        .route("/admin/actions/runners/registration-token", post(admin_runner_token))
        .route("/admin/actions/runners/{id}", delete(admin_remove_runner))
}

// ---------------------------------------------------------------------------- helpers

fn client(c: Option<Extension<ClientInfo>>) -> ClientInfo {
    c.map(|e| e.0).unwrap_or_else(ClientInfo::local)
}

async fn repo_for(state: &AppState, actor: &Actor, owner: &str, name: &str, need: Permission) -> Result<Repo> {
    let repo = state.repos.get_by_name(actor, owner, name).await?;
    state.authz.require(actor, repo.id, need).await?;
    Ok(repo)
}

#[derive(Deserialize, Default)]
pub struct Page {
    page: Option<u32>,
    per_page: Option<u32>,
}

impl Page {
    fn limit(&self) -> i64 {
        i64::from(self.per_page.unwrap_or(30).clamp(1, 100))
    }
    fn offset(&self) -> i64 {
        i64::from(self.page.unwrap_or(1).max(1) - 1) * self.limit()
    }
}

struct Urls {
    base: String,
    full: String,
}

impl Urls {
    fn new(ctx: &Ctx, repo: &Repo) -> Self {
        Self { base: ctx.base_url(), full: repo.full_name() }
    }
    fn api(&self) -> String {
        format!("{}/api/v3/repos/{}", self.base, self.full)
    }
    fn web(&self) -> String {
        format!("{}/{}", self.base, self.full)
    }
    /// The repository's short name: the part of `full` after the last slash.
    fn name(&self) -> &str {
        self.full.rsplit_once('/').map_or(self.full.as_str(), |(_, name)| name)
    }
}

async fn simple_user(ctx: &Ctx, id: Option<i64>) -> Result<Option<SimpleUser>> {
    let Some(id) = id else { return Ok(None) };
    let row: Option<(String, String, bool)> =
        sqlx::query_as("SELECT username::text, kind, is_admin FROM users WHERE id = $1").bind(id).fetch_optional(&ctx.db).await?;
    let base = ctx.base_url();
    Ok(row.map(|(login, kind, admin)| SimpleUser {
        avatar_url: ch_api_types::avatar_url(&base, &login),
        url: format!("{base}/api/v3/users/{login}"),
        html_url: format!("{base}/{login}"),
        kind: match kind.as_str() {
            "org" => "Organization",
            "bot" => "Bot",
            _ => "User",
        }
        .into(),
        site_admin: admin,
        login,
        id,
    }))
}

// ---------------------------------------------------------------------------- statuses

type StatusRow = (i64, String, String, Option<String>, Option<String>, Option<i64>, DateTime<Utc>, String);

fn status_dto(urls: &Urls, r: StatusRow, creator: Option<SimpleUser>) -> dto::CommitStatus {
    let (id, state, context, description, target_url, _creator, created_at, sha) = r;
    dto::CommitStatus {
        id,
        state,
        context,
        description,
        target_url,
        created_at,
        updated_at: created_at,
        creator,
        url: format!("{}/statuses/{sha}", urls.api()),
    }
}

const STATUS_COLS: &str = "id, state, context, description, target_url, creator_id, created_at, sha";

async fn resolve_ref(ctx: &Ctx, repo: RepoId, r: &str) -> String {
    ctx.source.resolve(repo, r).await.unwrap_or_else(|_| r.to_string())
}

async fn create_status(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name, sha)): Path<(String, String, String)>,
    Json(body): Json<dto::CreateStatus>,
) -> Result<Response> {
    let repo = state.repos.get_by_name(&actor, &owner, &name).await?;
    let ctx = Ctx::from_state(&state);
    let made = state
        .checks
        .create_status(
            &actor,
            repo.id,
            &sha,
            &body.state,
            body.context.as_deref().unwrap_or("default"),
            body.description.as_deref(),
            body.target_url.as_deref(),
        )
        .await?;
    let row: StatusRow =
        sqlx::query_as(&format!("SELECT {STATUS_COLS} FROM commit_statuses WHERE id = $1")).bind(made.id).fetch_one(&ctx.db).await?;
    let creator = simple_user(&ctx, row.5).await?;
    Ok((StatusCode::CREATED, Json(status_dto(&Urls::new(&ctx, &repo), row, creator))).into_response())
}

async fn list_statuses(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name, reference)): Path<(String, String, String)>,
    Query(p): Query<Page>,
) -> Result<Json<Vec<dto::CommitStatus>>> {
    let repo = repo_for(&state, &actor, &owner, &name, Permission::Read).await?;
    let ctx = Ctx::from_state(&state);
    let sha = resolve_ref(&ctx, repo.id, &reference).await;
    let rows: Vec<StatusRow> = sqlx::query_as(&format!(
        "SELECT {STATUS_COLS} FROM commit_statuses WHERE repo_id = $1 AND sha = $2 ORDER BY created_at DESC, id DESC LIMIT $3 OFFSET $4"
    ))
    .bind(repo.id.0)
    .bind(&sha)
    .bind(p.limit())
    .bind(p.offset())
    .fetch_all(&ctx.db)
    .await?;
    let urls = Urls::new(&ctx, &repo);
    let mut out = Vec::new();
    for r in rows {
        let creator = simple_user(&ctx, r.5).await?;
        out.push(status_dto(&urls, r, creator));
    }
    Ok(Json(out))
}

/// The combined state counts check runs and statuses together, so a commit with a failing
/// workflow job shows as failing. The list is the latest status of each context.
async fn combined_status(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name, reference)): Path<(String, String, String)>,
) -> Result<Json<dto::CombinedStatus>> {
    let repo = repo_for(&state, &actor, &owner, &name, Permission::Read).await?;
    let ctx = Ctx::from_state(&state);
    let sha = resolve_ref(&ctx, repo.id, &reference).await;
    let combined = state.checks.combined(repo.id, &sha).await?;
    let rows: Vec<StatusRow> = sqlx::query_as(&format!(
        "SELECT DISTINCT ON (context) {STATUS_COLS} FROM commit_statuses WHERE repo_id = $1 AND sha = $2 ORDER BY context, created_at DESC, id DESC"
    ))
    .bind(repo.id.0)
    .bind(&sha)
    .fetch_all(&ctx.db)
    .await?;
    let urls = Urls::new(&ctx, &repo);
    let mut statuses = Vec::new();
    for r in rows {
        let creator = simple_user(&ctx, r.5).await?;
        statuses.push(status_dto(&urls, r, creator));
    }
    Ok(Json(dto::CombinedStatus {
        state: match combined {
            ch_core::model::CombinedState::Pending => "pending",
            ch_core::model::CombinedState::Success => "success",
            ch_core::model::CombinedState::Failure => "failure",
        }
        .into(),
        sha: sha.clone(),
        total_count: statuses.len() as u32,
        statuses,
        repository: None,
        commit_url: format!("{}/commits/{sha}", urls.api()),
        url: format!("{}/commits/{sha}/status", urls.api()),
    }))
}

type CheckRow = (
    i64,
    String,
    String,
    String,
    Option<String>,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
    Option<String>,
    Option<String>,
    serde_json::Value,
);

const CHECK_COLS: &str = "id, name, head_sha, status, conclusion, started_at, completed_at, details_url, external_id, output";

fn check_dto(urls: &Urls, r: CheckRow) -> dto::CheckRun {
    let (id, name, head_sha, status, conclusion, started_at, completed_at, details_url, external_id, output) = r;
    let out = |k: &str| output.get(k).and_then(|v| v.as_str()).map(String::from);
    dto::CheckRun {
        id,
        name,
        head_sha,
        status,
        conclusion,
        started_at,
        completed_at,
        html_url: details_url.clone().unwrap_or_else(|| format!("{}/actions", urls.web())),
        details_url,
        external_id,
        output: dto::CheckRunOutput { title: out("title"), summary: out("summary"), text: out("text") },
        url: format!("{}/check-runs/{id}", urls.api()),
    }
}

#[derive(Deserialize, Default)]
struct CheckQuery {
    check_name: Option<String>,
    status: Option<String>,
    page: Option<u32>,
    per_page: Option<u32>,
}

impl CheckQuery {
    fn page(&self) -> Page {
        Page { page: self.page, per_page: self.per_page }
    }
}

async fn list_check_runs(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name, reference)): Path<(String, String, String)>,
    Query(q): Query<CheckQuery>,
) -> Result<Json<dto::CheckRunsResponse>> {
    let repo = repo_for(&state, &actor, &owner, &name, Permission::Read).await?;
    let ctx = Ctx::from_state(&state);
    let sha = resolve_ref(&ctx, repo.id, &reference).await;
    // Latest attempt of each check name, like the combined state.
    let rows: Vec<CheckRow> = sqlx::query_as(&format!(
        "SELECT {CHECK_COLS} FROM (SELECT DISTINCT ON (name) {CHECK_COLS} FROM check_runs WHERE repo_id = $1 AND head_sha = $2 ORDER BY name, id DESC) c
         WHERE ($3::text IS NULL OR name = $3) AND ($4::text IS NULL OR status = $4) ORDER BY name LIMIT $5 OFFSET $6"
    ))
    .bind(repo.id.0)
    .bind(&sha)
    .bind(&q.check_name)
    .bind(&q.status)
    .bind(q.page().limit())
    .bind(q.page().offset())
    .fetch_all(&ctx.db)
    .await?;
    let total: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM (SELECT DISTINCT ON (name) name, status FROM check_runs WHERE repo_id = $1 AND head_sha = $2 ORDER BY name, id DESC) c
         WHERE ($3::text IS NULL OR name = $3) AND ($4::text IS NULL OR status = $4)",
    )
    .bind(repo.id.0)
    .bind(&sha)
    .bind(&q.check_name)
    .bind(&q.status)
    .fetch_one(&ctx.db)
    .await?;
    let urls = Urls::new(&ctx, &repo);
    Ok(Json(dto::CheckRunsResponse { total_count: total as u32, check_runs: rows.into_iter().map(|r| check_dto(&urls, r)).collect() }))
}

async fn get_check_run(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> Result<Json<dto::CheckRun>> {
    let repo = repo_for(&state, &actor, &owner, &name, Permission::Read).await?;
    let ctx = Ctx::from_state(&state);
    let row: Option<CheckRow> = sqlx::query_as(&format!("SELECT {CHECK_COLS} FROM check_runs WHERE id = $1 AND repo_id = $2"))
        .bind(id)
        .bind(repo.id.0)
        .fetch_optional(&ctx.db)
        .await?;
    Ok(Json(check_dto(&Urls::new(&ctx, &repo), row.ok_or(Error::NotFound)?)))
}

// ---------------------------------------------------------------------------- workflows

fn workflow_dto(urls: &Urls, id: i64, name: String, path: String, state: String, created: DateTime<Utc>) -> dto::Workflow {
    dto::Workflow {
        id,
        name,
        html_url: format!("{}/blob/HEAD/{path}", urls.web()),
        // Served by the Actions pages for signed-in viewers (ch-c1d), by file name as on GitHub.
        badge_url: format!("{}/actions/workflows/{}/badge.svg", urls.web(), path.rsplit('/').next().unwrap_or(&path)),
        url: format!("{}/actions/workflows/{id}", urls.api()),
        path,
        state,
        created_at: created,
        updated_at: created,
    }
}

type WfRow = (i64, String, String, String, DateTime<Utc>);

async fn workflow_row(ctx: &Ctx, repo: RepoId, which: &str) -> Result<WfRow> {
    let id: Option<i64> = which.parse().ok();
    let row: Option<WfRow> = sqlx::query_as(
        "SELECT id, name, path, state, created_at FROM workflows WHERE repo_id = $1 AND (id = $2 OR path = $3 OR path LIKE '%/' || $3) ORDER BY (path LIKE '.codehaven/%') DESC LIMIT 1",
    )
    .bind(repo.0)
    .bind(id)
    .bind(which)
    .fetch_optional(&ctx.db)
    .await?;
    row.ok_or(Error::NotFound)
}

async fn list_workflows(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name)): Path<(String, String)>,
    Query(p): Query<Page>,
) -> Result<Json<dto::WorkflowsResponse>> {
    let repo = repo_for(&state, &actor, &owner, &name, Permission::Read).await?;
    let ctx = Ctx::from_state(&state);
    // Listing reads the default branch, which also refreshes the table.
    state.ci.workflows(&actor, repo.id, None).await?;
    let rows: Vec<WfRow> =
        sqlx::query_as("SELECT id, name, path, state, created_at FROM workflows WHERE repo_id = $1 ORDER BY path LIMIT $2 OFFSET $3")
            .bind(repo.id.0)
            .bind(p.limit())
            .bind(p.offset())
            .fetch_all(&ctx.db)
            .await?;
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM workflows WHERE repo_id = $1").bind(repo.id.0).fetch_one(&ctx.db).await?;
    let urls = Urls::new(&ctx, &repo);
    Ok(Json(dto::WorkflowsResponse {
        total_count: total as u32,
        workflows: rows.into_iter().map(|(id, n, p, s, c)| workflow_dto(&urls, id, n, p, s, c)).collect(),
    }))
}

async fn get_workflow(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((owner, name, which)): Path<(String, String, String)>,
) -> Result<Json<dto::Workflow>> {
    let repo = repo_for(&state, &actor, &owner, &name, Permission::Read).await?;
    let ctx = Ctx::from_state(&state);
    let (id, n, p, s, c) = workflow_row(&ctx, repo.id, &which).await?;
    Ok(Json(workflow_dto(&Urls::new(&ctx, &repo), id, n, p, s, c)))
}

async fn set_workflow_state(state: AppState, actor: Actor, owner: String, name: String, which: String, new: &str) -> Result<StatusCode> {
    let repo = repo_for(&state, &actor, &owner, &name, Permission::Write).await?;
    let ctx = Ctx::from_state(&state);
    let (id, ..) = workflow_row(&ctx, repo.id, &which).await?;
    sqlx::query("UPDATE workflows SET state = $2 WHERE id = $1").bind(id).bind(new).execute(&ctx.db).await?;
    if new == "disabled" {
        sqlx::query("DELETE FROM schedules WHERE workflow_id = $1").bind(id).execute(&ctx.db).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn enable_workflow(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((o, n, w)): Path<(String, String, String)>,
) -> Result<StatusCode> {
    set_workflow_state(state, actor, o, n, w, "active").await
}
async fn disable_workflow(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((o, n, w)): Path<(String, String, String)>,
) -> Result<StatusCode> {
    set_workflow_state(state, actor, o, n, w, "disabled").await
}

async fn dispatch_workflow(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    c: Option<Extension<ClientInfo>>,
    Path((owner, name, which)): Path<(String, String, String)>,
    Json(body): Json<dto::DispatchRequest>,
) -> Result<StatusCode> {
    let repo = repo_for(&state, &actor, &owner, &name, Permission::Write).await?;
    let inputs = body.inputs.map(serde_json::Value::Object).unwrap_or(serde_json::Value::Null);
    state.ci.dispatch(&actor, repo.id, &which, &body.git_ref, inputs, &client(c)).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------- runs

async fn run_dto(ctx: &Ctx, urls: &Urls, r: &RunRow) -> Result<dto::WorkflowRun> {
    let (event, meta) = crate::spec::split_payload(&r.event_payload);
    let actor = simple_user(ctx, r.actor_id).await?;
    let id = r.id;
    // The commit the run built. A commit that cannot be read leaves it null.
    let head_commit = ctx.source.commit(RepoId(r.repo_id), &r.head_sha).await.ok().flatten().map(|c| dto::RunHeadCommit {
        id: c.id,
        message: c.message,
        author: Some(dto::RunCommitPerson { name: c.author_name, email: c.author_email }),
        committer: None,
    });
    let pull_requests = run_pull_requests(ctx, urls, r, &event, meta.fork).await?;
    Ok(dto::WorkflowRun {
        id,
        name: Some(meta.workflow_name.clone()).filter(|n| !n.is_empty()),
        run_number: r.run_number as u32,
        run_attempt: r.attempt as u32,
        event: r.event.clone(),
        status: Some(r.status.clone()),
        conclusion: r.conclusion.clone(),
        workflow_id: r.workflow_id.unwrap_or(0),
        head_branch: r.head_branch.clone(),
        head_sha: r.head_sha.clone(),
        head_commit,
        pull_requests,
        path: meta.workflow_path.clone(),
        display_title: r.title.clone(),
        triggering_actor: actor.clone(),
        actor,
        created_at: r.created_at,
        updated_at: r.completed_at.or(r.started_at).unwrap_or(r.created_at),
        run_started_at: r.started_at,
        html_url: format!("{}/actions/runs/{id}", urls.web()),
        url: format!("{}/actions/runs/{id}", urls.api()),
        jobs_url: format!("{}/actions/runs/{id}/jobs", urls.api()),
        logs_url: format!("{}/actions/runs/{id}/logs", urls.api()),
        cancel_url: format!("{}/actions/runs/{id}/cancel", urls.api()),
        rerun_url: format!("{}/actions/runs/{id}/rerun", urls.api()),
        repository: None,
    })
}

/// The pull request a `pull_request` run was for, as GitHub lists it. A fork's is left out, as on
/// GitHub, and so is one that no longer exists.
async fn run_pull_requests(ctx: &Ctx, urls: &Urls, r: &RunRow, event: &serde_json::Value, fork: bool) -> Result<Vec<dto::RunPullRequest>> {
    if r.event != "pull_request" || fork {
        return Ok(vec![]);
    }
    let pr = &event["pull_request"];
    let Some(number) = pr["number"].as_i64() else { return Ok(vec![]) };
    let row: Option<(i64,)> = sqlx::query_as("SELECT id FROM issues WHERE repo_id = $1 AND number = $2 AND is_pull")
        .bind(r.repo_id)
        .bind(number)
        .fetch_optional(&ctx.db)
        .await?;
    let Some((pull_id,)) = row else { return Ok(vec![]) };
    let side = |key: &str| dto::RunPullRef {
        git_ref: pr[key]["ref"].as_str().unwrap_or_default().to_string(),
        sha: pr[key]["sha"].as_str().unwrap_or_default().to_string(),
        repo: dto::RunPullRepo { id: r.repo_id, url: urls.api(), name: urls.name().to_string() },
    };
    Ok(vec![dto::RunPullRequest {
        id: pull_id,
        number,
        url: format!("{}/pulls/{number}", urls.api()),
        head: side("head"),
        base: side("base"),
    }])
}

#[derive(Deserialize, Default)]
struct RunQuery {
    status: Option<String>,
    branch: Option<String>,
    event: Option<String>,
    head_sha: Option<String>,
    page: Option<u32>,
    per_page: Option<u32>,
}

impl RunQuery {
    fn page(&self) -> Page {
        Page { page: self.page, per_page: self.per_page }
    }
}

async fn runs_page(
    state: &AppState,
    actor: &Actor,
    owner: &str,
    name: &str,
    workflow: Option<&str>,
    q: RunQuery,
) -> Result<Json<dto::WorkflowRunsResponse>> {
    let repo = repo_for(state, actor, owner, name, Permission::Read).await?;
    let ctx = Ctx::from_state(state);
    let wf_id: Option<i64> = match workflow {
        Some(w) => Some(workflow_row(&ctx, repo.id, w).await?.0),
        None => None,
    };
    // `status` filters on either the status or the conclusion, as GitHub's does.
    let filter = "repo_id = $1 AND ($2::bigint IS NULL OR workflow_id = $2) AND ($3::text IS NULL OR status = $3 OR conclusion = $3)
                  AND ($4::text IS NULL OR head_branch = $4) AND ($5::text IS NULL OR event = $5) AND ($6::text IS NULL OR head_sha = $6)";
    let rows = sqlx::query_as::<_, RunRow>(&format!(
        "SELECT {} FROM workflow_runs WHERE {filter} ORDER BY id DESC LIMIT $7 OFFSET $8",
        runs::RUN_COLS
    ))
    .bind(repo.id.0)
    .bind(wf_id)
    .bind(&q.status)
    .bind(&q.branch)
    .bind(&q.event)
    .bind(&q.head_sha)
    .bind(q.page().limit())
    .bind(q.page().offset())
    .fetch_all(&ctx.db)
    .await?;
    let total: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM workflow_runs WHERE {filter}"))
        .bind(repo.id.0)
        .bind(wf_id)
        .bind(&q.status)
        .bind(&q.branch)
        .bind(&q.event)
        .bind(&q.head_sha)
        .fetch_one(&ctx.db)
        .await?;
    let urls = Urls::new(&ctx, &repo);
    let mut out = Vec::new();
    for r in &rows {
        out.push(run_dto(&ctx, &urls, r).await?);
    }
    Ok(Json(dto::WorkflowRunsResponse { total_count: total as u32, workflow_runs: out }))
}

async fn list_runs(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((o, n)): Path<(String, String)>,
    Query(q): Query<RunQuery>,
) -> Result<Json<dto::WorkflowRunsResponse>> {
    runs_page(&state, &actor, &o, &n, None, q).await
}

async fn list_workflow_runs(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((o, n, w)): Path<(String, String, String)>,
    Query(q): Query<RunQuery>,
) -> Result<Json<dto::WorkflowRunsResponse>> {
    runs_page(&state, &actor, &o, &n, Some(&w), q).await
}

async fn run_in(state: &AppState, repo: &Repo, id: i64) -> Result<RunRow> {
    let ctx = Ctx::from_state(state);
    let r = runs::fetch_run(&ctx, id).await?;
    if r.repo_id != repo.id.0 {
        return Err(Error::NotFound);
    }
    Ok(r)
}

async fn get_run(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((o, n, id)): Path<(String, String, i64)>,
) -> Result<Json<dto::WorkflowRun>> {
    let repo = repo_for(&state, &actor, &o, &n, Permission::Read).await?;
    let ctx = Ctx::from_state(&state);
    let r = run_in(&state, &repo, id).await?;
    Ok(Json(run_dto(&ctx, &Urls::new(&ctx, &repo), &r).await?))
}

async fn cancel_run(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    c: Option<Extension<ClientInfo>>,
    Path((o, n, id)): Path<(String, String, i64)>,
) -> Result<StatusCode> {
    let repo = repo_for(&state, &actor, &o, &n, Permission::Write).await?;
    state.ci.cancel(&actor, repo.id, RunId(id), &client(c)).await?;
    Ok(StatusCode::ACCEPTED)
}

async fn rerun_run(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    c: Option<Extension<ClientInfo>>,
    Path((o, n, id)): Path<(String, String, i64)>,
) -> Result<StatusCode> {
    let repo = repo_for(&state, &actor, &o, &n, Permission::Write).await?;
    state.ci.rerun(&actor, repo.id, RunId(id), false, &client(c)).await?;
    Ok(StatusCode::CREATED)
}

async fn rerun_failed(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    c: Option<Extension<ClientInfo>>,
    Path((o, n, id)): Path<(String, String, i64)>,
) -> Result<StatusCode> {
    let repo = repo_for(&state, &actor, &o, &n, Permission::Write).await?;
    state.ci.rerun(&actor, repo.id, RunId(id), true, &client(c)).await?;
    Ok(StatusCode::CREATED)
}

async fn approve_run(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    c: Option<Extension<ClientInfo>>,
    Path((o, n, id)): Path<(String, String, i64)>,
) -> Result<StatusCode> {
    let repo = repo_for(&state, &actor, &o, &n, Permission::Write).await?;
    state.ci.approve_run(&actor, repo.id, RunId(id), &client(c)).await?;
    Ok(StatusCode::CREATED)
}

#[derive(serde::Deserialize)]
struct ApproveCommit {
    head_sha: String,
}

/// `{"pending": true, "head_sha": ...}` while the pull request's latest commit waits for approval.
async fn fork_approval(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((o, n, number)): Path<(String, String, i64)>,
) -> Result<Json<serde_json::Value>> {
    let repo = repo_for(&state, &actor, &o, &n, Permission::Read).await?;
    let sha = state.ci.pending_fork_approval(&actor, repo.id, number).await?;
    Ok(Json(serde_json::json!({ "pending": sha.is_some(), "head_sha": sha })))
}

/// Approves the pull request's latest commit, named in `head_sha`, and starts its runs.
async fn approve_fork_commit(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    c: Option<Extension<ClientInfo>>,
    Path((o, n, number)): Path<(String, String, i64)>,
    Json(body): Json<ApproveCommit>,
) -> Result<(StatusCode, Json<serde_json::Value>)> {
    let repo = repo_for(&state, &actor, &o, &n, Permission::Write).await?;
    let runs = state.ci.approve_fork_commit(&actor, repo.id, number, &body.head_sha, &client(c)).await?;
    Ok((StatusCode::CREATED, Json(serde_json::json!({ "run_ids": runs.iter().map(|r| r.0).collect::<Vec<_>>() }))))
}

async fn job_dto(ctx: &Ctx, urls: &Urls, j: &JobRow, attempt: u32) -> Result<dto::WorkflowJob> {
    let steps = steps_for(ctx, j.id).await?;
    let runner_name: Option<String> = match j.runner_id {
        Some(r) => sqlx::query_scalar("SELECT name FROM runners WHERE id = $1").bind(r).fetch_optional(&ctx.db).await?,
        None => None,
    };
    Ok(dto::WorkflowJob {
        id: j.id,
        run_id: j.run_id,
        run_attempt: attempt,
        name: j.name.clone(),
        status: j.status.clone(),
        conclusion: j.conclusion.clone(),
        started_at: j.started_at,
        completed_at: j.completed_at,
        steps: steps
            .into_iter()
            .map(|s| dto::WorkflowStep {
                name: s.name,
                status: s.status,
                conclusion: s.conclusion,
                number: s.number as u32,
                started_at: s.started_at,
                completed_at: s.completed_at,
            })
            .collect(),
        labels: j.runs_on.clone(),
        runner_id: j.runner_id,
        runner_name,
        html_url: format!("{}/actions/runs/{}/jobs/{}", urls.web(), j.run_id, j.id),
        url: format!("{}/actions/jobs/{}", urls.api(), j.id),
        check_run_url: format!("{}/check-runs/{}", urls.api(), check_id(ctx, j.id).await.unwrap_or(0)),
    })
}

async fn check_id(ctx: &Ctx, job: i64) -> Result<i64> {
    Ok(sqlx::query_scalar("SELECT id FROM check_runs WHERE job_id = $1").bind(job).fetch_optional(&ctx.db).await?.unwrap_or(0))
}

async fn list_run_jobs(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((o, n, id)): Path<(String, String, i64)>,
) -> Result<Json<dto::WorkflowJobsResponse>> {
    let repo = repo_for(&state, &actor, &o, &n, Permission::Read).await?;
    let ctx = Ctx::from_state(&state);
    let run = run_in(&state, &repo, id).await?;
    let urls = Urls::new(&ctx, &repo);
    let jobs = runs::fetch_jobs(&ctx, id).await?;
    let mut out = Vec::new();
    for j in &jobs {
        out.push(job_dto(&ctx, &urls, j, run.attempt as u32).await?);
    }
    Ok(Json(dto::WorkflowJobsResponse { total_count: out.len() as u32, jobs: out }))
}

async fn job_in(ctx: &Ctx, repo: &Repo, id: i64) -> Result<(JobRow, RunRow)> {
    let j = sqlx::query_as::<_, JobRow>(&format!("SELECT {} FROM workflow_jobs WHERE id = $1", runs::JOB_COLS))
        .bind(id)
        .fetch_optional(&ctx.db)
        .await?
        .ok_or(Error::NotFound)?;
    if j.repo_id != repo.id.0 {
        return Err(Error::NotFound);
    }
    let r = runs::fetch_run(ctx, j.run_id).await?;
    Ok((j, r))
}

async fn get_job(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((o, n, id)): Path<(String, String, i64)>,
) -> Result<Json<dto::WorkflowJob>> {
    let repo = repo_for(&state, &actor, &o, &n, Permission::Read).await?;
    let ctx = Ctx::from_state(&state);
    let (j, r) = job_in(&ctx, &repo, id).await?;
    Ok(Json(job_dto(&ctx, &Urls::new(&ctx, &repo), &j, r.attempt as u32).await?))
}

async fn job_logs(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((o, n, id)): Path<(String, String, i64)>,
) -> Result<Response> {
    let repo = repo_for(&state, &actor, &o, &n, Permission::Read).await?;
    let bytes = state.ci.log_range(&actor, repo.id, JobId(id), LogRange { from: 0, to: None }).await?;
    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8"), (header::X_CONTENT_TYPE_OPTIONS, "nosniff")], bytes).into_response())
}

/// The most job log text one run's zip holds. Past it, the rest is named in a note inside the zip.
const MAX_RUN_LOG_ZIP_BYTES: usize = 256 * 1024 * 1024;

/// Every job's log of a run, one text file per job inside a zip.
async fn run_logs(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((o, n, id)): Path<(String, String, i64)>,
) -> Result<Response> {
    let repo = repo_for(&state, &actor, &o, &n, Permission::Read).await?;
    let ctx = Ctx::from_state(&state);
    run_in(&state, &repo, id).await?;
    let mut files: Vec<(String, bytes::Bytes)> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut total = 0usize;
    let jobs = runs::fetch_jobs(&ctx, id).await?;
    for (i, j) in jobs.iter().enumerate() {
        let bytes = state.ci.log_range(&actor, repo.id, JobId(j.id), LogRange { from: 0, to: None }).await?;
        if total + bytes.len() > MAX_RUN_LOG_ZIP_BYTES {
            skipped = jobs[i..].iter().map(|j| j.name.clone()).collect();
            break;
        }
        total += bytes.len();
        files.push((log_file_name(i, &j.name), bytes));
    }
    if !skipped.is_empty() {
        let note = format!("These job logs were too large to include. Download each from its job page:\n{}", skipped.join("\n"));
        files.push(("logs-not-included.txt".to_string(), bytes::Bytes::from(note)));
    }
    let body = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, bytes) in files {
            w.start_file(name, opts).map_err(Error::internal)?;
            std::io::Write::write_all(&mut w, &bytes).map_err(Error::internal)?;
        }
        Ok(w.finish().map_err(Error::internal)?.into_inner())
    })
    .await
    .map_err(Error::internal)??;
    let disp = format!("attachment; filename=\"logs_{id}.zip\"");
    Ok((
        [
            (header::CONTENT_TYPE, "application/zip".to_string()),
            (header::CONTENT_DISPOSITION, disp),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
        ],
        body,
    )
        .into_response())
}

/// A job's entry name in a run's log zip: its place in the run and its name, made safe for a path.
fn log_file_name(index: usize, job: &str) -> String {
    let safe: String = job
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    format!("{index}_{safe}.txt")
}

// ---------------------------------------------------------------------------- artifacts and caches

fn artifact_dto(urls: &Urls, a: &store::ArtifactRow) -> dto::Artifact {
    dto::Artifact {
        id: a.id,
        name: a.name.clone(),
        size_in_bytes: a.size,
        expired: a.expires_at <= Utc::now(),
        created_at: a.created_at,
        expires_at: Some(a.expires_at),
        url: format!("{}/actions/artifacts/{}", urls.api(), a.id),
        archive_download_url: format!("{}/actions/artifacts/{}/zip", urls.api(), a.id),
    }
}

async fn list_run_artifacts(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((o, n, id)): Path<(String, String, i64)>,
) -> Result<Json<dto::ArtifactsResponse>> {
    let repo = repo_for(&state, &actor, &o, &n, Permission::Read).await?;
    let ctx = Ctx::from_state(&state);
    run_in(&state, &repo, id).await?;
    let urls = Urls::new(&ctx, &repo);
    let rows = store::artifacts_for_run(&ctx, id).await?;
    Ok(Json(dto::ArtifactsResponse { total_count: rows.len() as u32, artifacts: rows.iter().map(|a| artifact_dto(&urls, a)).collect() }))
}

async fn list_artifacts(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((o, n)): Path<(String, String)>,
    Query(p): Query<Page>,
) -> Result<Json<dto::ArtifactsResponse>> {
    let repo = repo_for(&state, &actor, &o, &n, Permission::Read).await?;
    let ctx = Ctx::from_state(&state);
    let rows: Vec<store::ArtifactRow> = sqlx::query_as(
        "SELECT a.id, a.run_id, a.job_id, a.name, a.blob_sha256, a.size, a.expires_at, a.created_at FROM artifacts a JOIN workflow_runs r ON r.id = a.run_id
         WHERE r.repo_id = $1 AND a.expires_at > now() ORDER BY a.id DESC LIMIT $2 OFFSET $3",
    )
    .bind(repo.id.0)
    .bind(p.limit())
    .bind(p.offset())
    .fetch_all(&ctx.db)
    .await?;
    let total: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM artifacts a JOIN workflow_runs r ON r.id = a.run_id WHERE r.repo_id = $1 AND a.expires_at > now()",
    )
    .bind(repo.id.0)
    .fetch_one(&ctx.db)
    .await?;
    let urls = Urls::new(&ctx, &repo);
    Ok(Json(dto::ArtifactsResponse { total_count: total as u32, artifacts: rows.iter().map(|a| artifact_dto(&urls, a)).collect() }))
}

async fn artifact_in(ctx: &Ctx, repo: &Repo, id: i64) -> Result<store::ArtifactRow> {
    let (a, repo_id) = store::artifact_by_id(ctx, id).await?;
    if repo_id != repo.id.0 {
        return Err(Error::NotFound);
    }
    Ok(a)
}

async fn get_artifact(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((o, n, id)): Path<(String, String, i64)>,
) -> Result<Json<dto::Artifact>> {
    let repo = repo_for(&state, &actor, &o, &n, Permission::Read).await?;
    let ctx = Ctx::from_state(&state);
    let a = artifact_in(&ctx, &repo, id).await?;
    Ok(Json(artifact_dto(&Urls::new(&ctx, &repo), &a)))
}

async fn artifact_zip(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((o, n, id)): Path<(String, String, i64)>,
) -> Result<Response> {
    let repo = repo_for(&state, &actor, &o, &n, Permission::Read).await?;
    let ctx = Ctx::from_state(&state);
    let a = artifact_in(&ctx, &repo, id).await?;
    if a.expires_at <= Utc::now() {
        return Err(Error::NotFound);
    }
    let body = store::blob_body(&ctx, &a.blob_sha256).await?;
    let disp = ch_core::disposition::attachment(&format!("{}.zip", a.name));
    Ok((
        [
            (header::CONTENT_TYPE, "application/zip".to_string()),
            (header::CONTENT_DISPOSITION, disp),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
        ],
        body,
    )
        .into_response())
}

async fn delete_artifact(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((o, n, id)): Path<(String, String, i64)>,
) -> Result<StatusCode> {
    let repo = repo_for(&state, &actor, &o, &n, Permission::Write).await?;
    let ctx = Ctx::from_state(&state);
    artifact_in(&ctx, &repo, id).await?;
    store::delete_artifact(&ctx, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

type CacheTuple = (i64, String, String, Option<String>, i64, DateTime<Utc>, DateTime<Utc>);

async fn list_caches(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((o, n)): Path<(String, String)>,
    Query(p): Query<Page>,
) -> Result<Json<serde_json::Value>> {
    let repo = repo_for(&state, &actor, &o, &n, Permission::Read).await?;
    let ctx = Ctx::from_state(&state);
    let rows: Vec<CacheTuple> =
        sqlx::query_as("SELECT id, key, version, git_ref, size, created_at, last_used_at FROM caches WHERE repo_id = $1 ORDER BY last_used_at DESC LIMIT $2 OFFSET $3").bind(repo.id.0).bind(p.limit()).bind(p.offset()).fetch_all(&ctx.db).await?;
    let (count, bytes): (i64, Option<i64>) =
        sqlx::query_as("SELECT count(*), sum(size)::bigint FROM caches WHERE repo_id = $1").bind(repo.id.0).fetch_one(&ctx.db).await?;
    Ok(Json(json!({
        "total_count": count,
        "total_size_in_bytes": bytes.unwrap_or(0),
        "actions_caches": rows.into_iter().map(|(id, key, version, git_ref, size, created_at, last_accessed_at)| json!({
            "id": id, "key": key, "version": version, "ref": git_ref, "size_in_bytes": size, "created_at": created_at, "last_accessed_at": last_accessed_at,
        })).collect::<Vec<_>>(),
    })))
}

async fn delete_cache(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((o, n, id)): Path<(String, String, i64)>,
) -> Result<StatusCode> {
    let repo = repo_for(&state, &actor, &o, &n, Permission::Write).await?;
    let ctx = Ctx::from_state(&state);
    let mut tx = ctx.db.begin().await?;
    let sha: Option<String> = sqlx::query_scalar("DELETE FROM caches WHERE id = $1 AND repo_id = $2 RETURNING blob_sha256")
        .bind(id)
        .bind(repo.id.0)
        .fetch_optional(&mut *tx)
        .await?;
    let sha = sha.ok_or(Error::NotFound)?;
    ch_db::blobs::release(&mut tx, &sha).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------- secrets and variables

/// An organization (or a person's account) by name, for the owner-level settings.
async fn owner_scope(state: &AppState, actor: &Actor, org: &str) -> Result<i64> {
    let ctx = Ctx::from_state(state);
    let row: Option<(i64, String)> =
        sqlx::query_as("SELECT id, kind FROM users WHERE username = $1").bind(org).fetch_optional(&ctx.db).await?;
    let Some((id, kind)) = row else { return Err(Error::NotFound) };
    // A token needs the scope that means what it is being used for.
    let scope_ok = match actor {
        Actor::Token { scopes, .. } => scopes.has(if kind == "org" { "admin:org" } else { "repo" }),
        _ => true,
    };
    let allowed = scope_ok
        && (actor.is_site_admin()
            || if kind == "org" {
                matches!(state.authz.org_role(actor, ch_core::OrgId(id)).await?, Some(ch_core::services::OrgRole::Owner))
            } else {
                actor.user_id() == Some(UserId(id))
            });
    if !allowed {
        // Someone who cannot manage it is told no more than "not found".
        return Err(Error::NotFound);
    }
    Ok(id)
}

async fn audit_secret(ctx: &Ctx, actor: &Actor, c: &ClientInfo, act: &str, scope: &str, name: &str) {
    let ev = AuditEvent::new(actor, c, act, Outcome::Success).target("secret", name).details(json!({ "scope": scope, "name": name }));
    if let Err(e) = ctx.audit.record_standalone(ev).await {
        tracing::warn!(error = %e, "could not record the audit event");
    }
}

fn secrets_response(rows: Vec<(String, DateTime<Utc>)>) -> Json<dto::SecretsResponse> {
    Json(dto::SecretsResponse {
        total_count: rows.len() as u32,
        secrets: rows.into_iter().map(|(name, t)| dto::SecretMeta { name, created_at: t, updated_at: t }).collect(),
    })
}

fn variables_response(rows: Vec<(String, String, DateTime<Utc>)>) -> Json<dto::VariablesResponse> {
    Json(dto::VariablesResponse {
        total_count: rows.len() as u32,
        variables: rows.into_iter().map(|(name, value, t)| dto::Variable { name, value, created_at: t, updated_at: t }).collect(),
    })
}

async fn repo_secrets(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((o, n)): Path<(String, String)>,
) -> Result<Json<dto::SecretsResponse>> {
    let repo = repo_for(&state, &actor, &o, &n, Permission::Admin).await?;
    Ok(secrets_response(secrets::list_secrets(&Ctx::from_state(&state), Scope::Repo(repo.id.0)).await?))
}

async fn repo_secret(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((o, n, name)): Path<(String, String, String)>,
) -> Result<Json<dto::SecretMeta>> {
    let repo = repo_for(&state, &actor, &o, &n, Permission::Admin).await?;
    let rows = secrets::list_secrets(&Ctx::from_state(&state), Scope::Repo(repo.id.0)).await?;
    let (name, t) = rows.into_iter().find(|r| r.0 == name).ok_or(Error::NotFound)?;
    Ok(Json(dto::SecretMeta { name, created_at: t, updated_at: t }))
}

async fn put_secret_in(
    state: &AppState,
    actor: &Actor,
    c: &ClientInfo,
    scope: Scope,
    label: &str,
    name: &str,
    body: dto::PutSecret,
) -> Result<StatusCode> {
    let ctx = Ctx::from_state(state);
    let created = secrets::put_secret(&ctx, scope, name, &body.value, actor.user_id()).await?;
    audit_secret(&ctx, actor, c, action::SECRET_CHANGED, label, name).await;
    Ok(if created { StatusCode::CREATED } else { StatusCode::NO_CONTENT })
}

async fn put_repo_secret(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    c: Option<Extension<ClientInfo>>,
    Path((o, n, name)): Path<(String, String, String)>,
    Json(body): Json<dto::PutSecret>,
) -> Result<StatusCode> {
    let repo = repo_for(&state, &actor, &o, &n, Permission::Admin).await?;
    put_secret_in(&state, &actor, &client(c), Scope::Repo(repo.id.0), "repo", &name, body).await
}

async fn delete_secret_in(state: &AppState, actor: &Actor, c: &ClientInfo, scope: Scope, label: &str, name: &str) -> Result<StatusCode> {
    let ctx = Ctx::from_state(state);
    if !secrets::delete_secret(&ctx, scope, name).await? {
        return Err(Error::NotFound);
    }
    audit_secret(&ctx, actor, c, action::SECRET_DELETED, label, name).await;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_repo_secret(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    c: Option<Extension<ClientInfo>>,
    Path((o, n, name)): Path<(String, String, String)>,
) -> Result<StatusCode> {
    let repo = repo_for(&state, &actor, &o, &n, Permission::Admin).await?;
    delete_secret_in(&state, &actor, &client(c), Scope::Repo(repo.id.0), "repo", &name).await
}

/// A job gets the variables it may use in its payload: a fork's job gets none. Its token may not
/// list them here, where `Read` would show every one (ch-3xn).
fn no_job_token(actor: &Actor) -> Result<()> {
    if matches!(actor, Actor::Job { .. }) {
        return Err(Error::forbidden("A job token cannot read variables. A job gets the ones it may use with the job."));
    }
    Ok(())
}

async fn repo_variables(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((o, n)): Path<(String, String)>,
) -> Result<Json<dto::VariablesResponse>> {
    no_job_token(&actor)?;
    // Values are settings: Admin, as on the settings page (ch-uph).
    let repo = repo_for(&state, &actor, &o, &n, Permission::Admin).await?;
    Ok(variables_response(secrets::list_variables(&Ctx::from_state(&state), Scope::Repo(repo.id.0)).await?))
}

async fn repo_variable(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((o, n, name)): Path<(String, String, String)>,
) -> Result<Json<dto::Variable>> {
    no_job_token(&actor)?;
    let repo = repo_for(&state, &actor, &o, &n, Permission::Admin).await?;
    let rows = secrets::list_variables(&Ctx::from_state(&state), Scope::Repo(repo.id.0)).await?;
    let (name, value, t) = rows.into_iter().find(|r| r.0 == name).ok_or(Error::NotFound)?;
    Ok(Json(dto::Variable { name, value, created_at: t, updated_at: t }))
}

async fn create_variable_in(state: &AppState, scope: Scope, body: dto::PutVariable) -> Result<StatusCode> {
    let ctx = Ctx::from_state(state);
    if !secrets::put_variable(&ctx, scope, &body.name, &body.value).await? {
        return Err(Error::invalid("Variable", "name", "A variable with this name already exists."));
    }
    Ok(StatusCode::CREATED)
}

async fn create_repo_variable(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((o, n)): Path<(String, String)>,
    Json(body): Json<dto::PutVariable>,
) -> Result<StatusCode> {
    let repo = repo_for(&state, &actor, &o, &n, Permission::Admin).await?;
    create_variable_in(&state, Scope::Repo(repo.id.0), body).await
}

#[derive(Deserialize)]
struct UpdateVariable {
    value: Option<String>,
    name: Option<String>,
}

async fn update_variable_in(state: &AppState, scope: Scope, name: &str, body: UpdateVariable) -> Result<StatusCode> {
    let ctx = Ctx::from_state(state);
    let current = secrets::list_variables(&ctx, scope).await?;
    let Some((_, old, _)) = current.into_iter().find(|r| r.0 == name) else { return Err(Error::NotFound) };
    let value = body.value.unwrap_or(old);
    match body.name.as_deref().filter(|n| *n != name) {
        Some(new) => {
            secrets::put_variable(&ctx, scope, new, &value).await?;
            secrets::delete_variable(&ctx, scope, name).await?;
        }
        None => {
            secrets::put_variable(&ctx, scope, name, &value).await?;
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn update_repo_variable(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((o, n, name)): Path<(String, String, String)>,
    Json(body): Json<UpdateVariable>,
) -> Result<StatusCode> {
    let repo = repo_for(&state, &actor, &o, &n, Permission::Admin).await?;
    update_variable_in(&state, Scope::Repo(repo.id.0), &name, body).await
}

async fn delete_repo_variable(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((o, n, name)): Path<(String, String, String)>,
) -> Result<StatusCode> {
    let repo = repo_for(&state, &actor, &o, &n, Permission::Admin).await?;
    if !secrets::delete_variable(&Ctx::from_state(&state), Scope::Repo(repo.id.0), &name).await? {
        return Err(Error::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn org_secrets(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path(org): Path<String>,
) -> Result<Json<dto::SecretsResponse>> {
    let id = owner_scope(&state, &actor, &org).await?;
    Ok(secrets_response(secrets::list_secrets(&Ctx::from_state(&state), Scope::Org(id)).await?))
}

async fn put_org_secret(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    c: Option<Extension<ClientInfo>>,
    Path((org, name)): Path<(String, String)>,
    Json(body): Json<dto::PutSecret>,
) -> Result<StatusCode> {
    let id = owner_scope(&state, &actor, &org).await?;
    put_secret_in(&state, &actor, &client(c), Scope::Org(id), "org", &name, body).await
}

async fn delete_org_secret(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    c: Option<Extension<ClientInfo>>,
    Path((org, name)): Path<(String, String)>,
) -> Result<StatusCode> {
    let id = owner_scope(&state, &actor, &org).await?;
    delete_secret_in(&state, &actor, &client(c), Scope::Org(id), "org", &name).await
}

async fn org_variables(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path(org): Path<String>,
) -> Result<Json<dto::VariablesResponse>> {
    let id = owner_scope(&state, &actor, &org).await?;
    Ok(variables_response(secrets::list_variables(&Ctx::from_state(&state), Scope::Org(id)).await?))
}

async fn create_org_variable(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path(org): Path<String>,
    Json(body): Json<dto::PutVariable>,
) -> Result<StatusCode> {
    let id = owner_scope(&state, &actor, &org).await?;
    create_variable_in(&state, Scope::Org(id), body).await
}

async fn update_org_variable(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((org, name)): Path<(String, String)>,
    Json(body): Json<UpdateVariable>,
) -> Result<StatusCode> {
    let id = owner_scope(&state, &actor, &org).await?;
    update_variable_in(&state, Scope::Org(id), &name, body).await
}

async fn delete_org_variable(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((org, name)): Path<(String, String)>,
) -> Result<StatusCode> {
    let id = owner_scope(&state, &actor, &org).await?;
    if !secrets::delete_variable(&Ctx::from_state(&state), Scope::Org(id), &name).await? {
        return Err(Error::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------- runners

fn runner_dto(r: ch_core::model::RunnerSummary) -> dto::RunnerInfo {
    dto::RunnerInfo {
        id: r.id.0,
        name: r.name,
        os: r.os.unwrap_or_default(),
        status: if r.status == "offline" { "offline" } else { "online" }.into(),
        busy: r.status == "busy",
        labels: r.labels.into_iter().map(|name| dto::RunnerLabel { id: None, name, type_: "custom".into() }).collect(),
    }
}

async fn repo_runners(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((o, n)): Path<(String, String)>,
) -> Result<Json<dto::RunnersResponse>> {
    let repo = repo_for(&state, &actor, &o, &n, Permission::Admin).await?;
    let rows = runner_summaries(&Ctx::from_state(&state), Some(repo.id.0)).await?;
    Ok(Json(dto::RunnersResponse { total_count: rows.len() as u32, runners: rows.into_iter().map(runner_dto).collect() }))
}

async fn admin_runners(State(state): State<AppState>, CurrentActor(actor): CurrentActor) -> Result<Json<dto::RunnersResponse>> {
    let rows = state.ci.runners(&actor).await?;
    Ok(Json(dto::RunnersResponse { total_count: rows.len() as u32, runners: rows.into_iter().map(runner_dto).collect() }))
}

#[derive(Deserialize, Default)]
struct TokenRequest {
    /// `owner/name`, to limit the runner to one repository.
    repository: Option<String>,
}

async fn admin_runner_token(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    c: Option<Extension<ClientInfo>>,
    body: Option<Json<TokenRequest>>,
) -> Result<Response> {
    let repo: Option<RepoId> = match body.and_then(|b| b.0.repository) {
        Some(full) => {
            let (o, n) = full.split_once('/').ok_or_else(|| Error::invalid("Runner", "repository", "Use owner/name."))?;
            Some(state.repos.get_by_name(&actor, o, n).await?.id)
        }
        None => None,
    };
    let t = state.ci.create_runner_token(&actor, repo, &client(c)).await?;
    Ok((StatusCode::CREATED, Json(dto::RunnerRegistrationToken { token: t.token, expires_at: t.expires_at })).into_response())
}

async fn repo_runner_token(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    c: Option<Extension<ClientInfo>>,
    Path((o, n)): Path<(String, String)>,
) -> Result<Response> {
    let repo = repo_for(&state, &actor, &o, &n, Permission::Admin).await?;
    let t = state.ci.create_runner_token(&actor, Some(repo.id), &client(c)).await?;
    Ok((StatusCode::CREATED, Json(dto::RunnerRegistrationToken { token: t.token, expires_at: t.expires_at })).into_response())
}

/// Revokes a runner. Its credential stops working at once and any job it was running fails when
/// its lease runs out.
async fn admin_remove_runner(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    c: Option<Extension<ClientInfo>>,
    Path(id): Path<i64>,
) -> Result<StatusCode> {
    if !actor.is_site_admin() {
        return Err(Error::forbidden("Only a site admin can remove a runner."));
    }
    let ctx = Ctx::from_state(&state);
    let mut tx = ctx.db.begin().await?;
    let done = sqlx::query("UPDATE runners SET revoked_at = now() WHERE id = $1 AND revoked_at IS NULL").bind(id).execute(&mut *tx).await?;
    if done.rows_affected() == 0 {
        return Err(Error::NotFound);
    }
    ctx.audit.record(&mut tx, AuditEvent::new(&actor, &client(c), action::RUNNER_REMOVED, Outcome::Success).target("runner", id)).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_file_name_replaces_characters_a_path_cannot_hold() {
        assert_eq!(log_file_name(0, "lint"), "0_lint.txt");
        assert_eq!(log_file_name(2, "test (a)"), "2_test (a).txt");
        assert_eq!(log_file_name(3, "a/b:c"), "3_a_b_c.txt");
    }
}
