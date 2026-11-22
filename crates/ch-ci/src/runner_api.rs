//! `/api/runner/v1`: what `haven-runner` calls (ARCHITECTURE.md section 9.4).
//!
//! Two credentials open this door and each opens only its own calls. A runner credential
//! registers nothing, asks for jobs, and reports on the jobs it holds. A job token reads and
//! writes artifacts and the cache for its own job. Anything else is `404`, the same as a job that
//! does not exist, so a runner learns nothing about jobs it was not given.

use crate::ctx::Ctx;
use crate::payload::{LEASE_SECONDS, build_payload};
use crate::runs::{self, Effects, JOB_COLS, JobRow};
use crate::store;
use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use ch_api_types::ci::{Artifact, ArtifactsResponse};
use ch_api_types::runner as rn;
use ch_core::access::CurrentActor;
use ch_core::audit::{AuditEvent, Outcome, action};
use ch_core::model::ClientInfo;
use ch_core::state::AppState;
use ch_core::{Actor, Error, Result};
use ch_db::tokens::{hash_token, new_token};
use std::collections::BTreeMap;
use std::time::Duration;

pub fn routes() -> Router<AppState> {
    let uploads = Router::new()
        .route("/jobs/{id}/artifacts/{name}", put(put_artifact))
        .route("/cache/{key}", put(cache_put))
        // Uploads are streamed to disk, so the default 2 MiB body limit does not apply.
        .layer(DefaultBodyLimit::disable());
    Router::new()
        .route("/register", post(register))
        .route("/jobs/request", post(request_job))
        .route("/jobs/{id}/heartbeat", post(heartbeat))
        .route("/jobs/{id}/logs", post(logs))
        .route("/jobs/{id}/steps/{n}", post(step))
        .route("/jobs/{id}/complete", post(complete))
        .route("/jobs/{id}/artifacts", get(list_artifacts))
        .route("/jobs/{id}/artifacts/{name}", get(get_artifact))
        .route("/cache/lookup", post(cache_lookup))
        .route("/cache/{key}", get(cache_get))
        .merge(uploads)
}

fn client(c: Option<Extension<ClientInfo>>) -> ClientInfo {
    c.map(|e| e.0).unwrap_or_else(ClientInfo::local)
}

fn runner_id(actor: &Actor) -> Result<i64> {
    match actor {
        Actor::Runner { runner_id } => Ok(runner_id.0),
        _ => Err(Error::NotFound),
    }
}

/// The job a runner may report on: held by this runner and still running.
async fn owned_job(ctx: &Ctx, runner: i64, job: i64) -> Result<JobRow> {
    let row = sqlx::query_as::<_, JobRow>(&format!("SELECT {JOB_COLS} FROM workflow_jobs WHERE id = $1 AND runner_id = $2"))
        .bind(job)
        .bind(runner)
        .fetch_optional(&ctx.db)
        .await?;
    row.ok_or(Error::NotFound)
}

/// The job a job token belongs to. The token is only ever valid for its own job, and only while
/// the job runs, so this is a match on the id in the path.
async fn token_job(ctx: &Ctx, actor: &Actor, path_job: i64, write: bool) -> Result<JobRow> {
    let Actor::Job { job_id, perms, .. } = actor else { return Err(Error::NotFound) };
    if job_id.0 != path_job {
        return Err(Error::NotFound);
    }
    if write && !perms.write_checks {
        return Err(Error::forbidden("This job may not write artifacts or caches."));
    }
    sqlx::query_as::<_, JobRow>(&format!("SELECT {JOB_COLS} FROM workflow_jobs WHERE id = $1 AND status = 'in_progress'"))
        .bind(path_job)
        .fetch_optional(&ctx.db)
        .await?
        .ok_or(Error::NotFound)
}

// ---------------------------------------------------------------------------- register

fn clean_labels(labels: &[String]) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for l in labels {
        let l = l.trim().to_lowercase();
        if l.is_empty() {
            continue;
        }
        if l.len() > 64 || !l.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':' | b'/')) {
            return Err(Error::invalid("Runner", "labels", format!("`{l}` is not a valid label.")));
        }
        if !out.contains(&l) {
            out.push(l);
        }
    }
    if out.is_empty() || out.len() > 30 {
        return Err(Error::invalid("Runner", "labels", "Give between 1 and 30 labels."));
    }
    Ok(out)
}

async fn register(
    State(state): State<AppState>,
    c: Option<Extension<ClientInfo>>,
    Json(req): Json<rn::RegisterRequest>,
) -> Result<Json<rn::RegisterResponse>> {
    let ctx = Ctx::from_state(&state);
    let client = client(c);
    if let Some(retry_after_secs) = ctx.shared.register_throttle.blocked(client.ip) {
        return Err(Error::RateLimited { retry_after_secs });
    }
    let name = req.name.trim().to_string();
    if name.is_empty() || name.len() > 100 || name.chars().any(|c| c.is_control()) {
        return Err(Error::invalid("Runner", "name", "Give a name of at most 100 characters."));
    }
    let labels = clean_labels(&req.labels)?;
    let clip = |s: &Option<String>| s.as_ref().map(|v| v.chars().filter(|c| !c.is_control()).take(64).collect::<String>());

    let mut tx = ctx.db.begin().await?;
    // Single use: the update only matches an unused, unexpired token, so two runners racing one
    // token get one runner between them.
    let tok: Option<(i64, Option<i64>, Option<i64>)> = sqlx::query_as(
        "UPDATE runner_registration_tokens SET used_at = now() WHERE token_hash = $1 AND used_at IS NULL AND expires_at > now() RETURNING id, repo_id, created_by",
    )
    .bind(hash_token(req.token.trim()))
    .fetch_optional(&mut *tx)
    .await?;
    let Some((token_id, repo_id, created_by)) = tok else {
        drop(tx);
        ctx.shared.register_throttle.failed(client.ip);
        let _ = ctx
            .audit
            .record_standalone(
                AuditEvent::new(&Actor::Anonymous, &client, action::RUNNER_REGISTERED, Outcome::Failure)
                    .details(serde_json::json!({"reason": "bad registration token"})),
            )
            .await;
        // Used, expired and made-up tokens all look the same.
        return Err(Error::Unauthorized("The registration token is not valid.".into()));
    };
    let credential = new_token(ch_db::ci_auth::RUNNER_CREDENTIAL_PREFIX, 40);
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO runners (name, labels, credential_hash, version, os, arch, repo_id, registered_by, last_seen_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, now()) RETURNING id",
    )
    .bind(&name)
    .bind(&labels)
    .bind(hash_token(&credential))
    .bind(clip(&req.version))
    .bind(clip(&req.os))
    .bind(clip(&req.arch))
    .bind(repo_id)
    .bind(created_by)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("UPDATE runner_registration_tokens SET runner_id = $2 WHERE id = $1").bind(token_id).bind(id).execute(&mut *tx).await?;
    ctx.audit
        .record(
            &mut tx,
            AuditEvent::new(&Actor::Runner { runner_id: ch_core::RunnerId(id) }, &client, action::RUNNER_REGISTERED, Outcome::Success)
                .target("runner", id)
                .details(serde_json::json!({"name": name, "labels": labels, "repo_id": repo_id})),
        )
        .await?;
    tx.commit().await?;
    Ok(Json(rn::RegisterResponse { runner_id: id, name, credential }))
}

// ---------------------------------------------------------------------------- jobs

/// Tries to hand out one job. Returns the claimed job row and the token minted for it.
async fn try_claim(ctx: &Ctx, runner: i64, labels: &[String], repo_scope: Option<i64>) -> Result<Option<(JobRow, String)>> {
    let token = new_token(ch_db::ci_auth::JOB_TOKEN_PREFIX, 40);
    let mut tx = ctx.db.begin().await?;
    let claimed = sqlx::query_as::<_, JobRow>(&format!(
        "UPDATE workflow_jobs SET status = 'in_progress', runner_id = $1, started_at = now(),
                lease_expires_at = now() + make_interval(secs => $2), token_hash = $3
         WHERE id = (
             SELECT c.id FROM workflow_jobs c
             WHERE c.status = 'queued' AND c.runs_on <@ $4 AND ($5::bigint IS NULL OR c.repo_id = $5)
             ORDER BY c.created_at, c.id
             FOR UPDATE OF c SKIP LOCKED LIMIT 1)
         RETURNING {JOB_COLS}"
    ))
    .bind(runner)
    .bind(f64::from(LEASE_SECONDS))
    .bind(hash_token(&token))
    .bind(labels)
    .bind(repo_scope)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(job) = claimed else { return Ok(None) };
    let mut fx = Effects::default();
    runs::note_job_started(ctx, &mut tx, &job, &mut fx).await?;
    fx.emit(ctx, &mut tx).await?;
    tx.commit().await?;
    Ok(Some((job, token)))
}

async fn request_job(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    body: Option<Json<rn::JobRequest>>,
) -> Result<Json<rn::JobResponse>> {
    let ctx = Ctx::from_state(&state);
    let runner = runner_id(&actor)?;
    let req = body.map(|b| b.0).unwrap_or_default();
    let wait = Duration::from_secs(u64::from(req.wait_seconds.unwrap_or(30).min(30)));
    let (registered, repo_scope): (Vec<String>, Option<i64>) =
        sqlx::query_as("SELECT labels, repo_id FROM runners WHERE id = $1 AND revoked_at IS NULL")
            .bind(runner)
            .fetch_optional(&ctx.db)
            .await?
            .ok_or(Error::NotFound)?;
    // Offer a job only if the runner has all its labels now. Asking with fewer labels than it
    // registered with narrows the offer, asking with unregistered ones does not widen it.
    let labels: Vec<String> = if req.labels.is_empty() {
        registered
    } else {
        let want: Vec<String> = req.labels.iter().map(|l| l.trim().to_lowercase()).collect();
        registered.into_iter().filter(|l| want.contains(l)).collect()
    };
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        // Listen before looking, so a job queued in between wakes this call instead of being missed.
        let notified = ctx.shared.hub.queue.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        sqlx::query("UPDATE runners SET last_seen_at = now() WHERE id = $1").bind(runner).execute(&ctx.db).await?;
        if let Some((job, token)) = try_claim(&ctx, runner, &labels, repo_scope).await? {
            match build_payload(&ctx, &job, token).await {
                Ok(p) => return Ok(Json(rn::JobResponse { job: Some(p) })),
                Err(e) => {
                    // A job that cannot be built fails now rather than sitting until its lease runs out.
                    tracing::error!(job = job.id, error = %e, "could not build the job payload");
                    if let Ok(run) = runs::fetch_run(&ctx, job.run_id).await {
                        crate::logs::append_system_line(&ctx, &run, &job, "The server could not prepare this job.").await;
                    }
                    runs::finish_job(&ctx, job.id, "failure", &BTreeMap::new()).await?;
                    continue;
                }
            }
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Ok(Json(rn::JobResponse { job: None }));
        }
        let nap = (deadline - now).min(Duration::from_secs(5));
        let _ = tokio::time::timeout(nap, notified).await;
    }
}

async fn heartbeat(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path(id): Path<i64>,
) -> Result<Json<rn::HeartbeatResponse>> {
    let ctx = Ctx::from_state(&state);
    let runner = runner_id(&actor)?;
    let job = owned_job(&ctx, runner, id).await?;
    if job.status != "in_progress" {
        // The job already ended (cancelled, or the lease ran out). The runner should stop.
        return Ok(Json(rn::HeartbeatResponse { cancel: true }));
    }
    sqlx::query("UPDATE workflow_jobs SET lease_expires_at = now() + make_interval(secs => $2) WHERE id = $1 AND status = 'in_progress'")
        .bind(id)
        .bind(f64::from(LEASE_SECONDS))
        .execute(&ctx.db)
        .await?;
    sqlx::query("UPDATE runners SET last_seen_at = now() WHERE id = $1").bind(runner).execute(&ctx.db).await?;
    let run = runs::fetch_run(&ctx, job.run_id).await?;
    let (_, meta) = crate::spec::split_payload(&run.event_payload);
    let cancel = meta.cancel_requested || job.spec().map(|s| s.cancel).unwrap_or(false);
    Ok(Json(rn::HeartbeatResponse { cancel }))
}

async fn logs(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path(id): Path<i64>,
    Json(batch): Json<rn::LogBatch>,
) -> Result<Response> {
    let ctx = Ctx::from_state(&state);
    let runner = runner_id(&actor)?;
    owned_job(&ctx, runner, id).await?;
    let (ack, accepted) = crate::logs::append_batch(&ctx, id, runner, batch).await?;
    Ok(if accepted { Json(ack).into_response() } else { (StatusCode::CONFLICT, Json(ack)).into_response() })
}

async fn step(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((id, n)): Path<(i64, i32)>,
    Json(u): Json<rn::StepUpdate>,
) -> Result<StatusCode> {
    let ctx = Ctx::from_state(&state);
    let runner = runner_id(&actor)?;
    let job = owned_job(&ctx, runner, id).await?;
    if job.status != "in_progress" {
        return Err(Error::conflict("This job is not running."));
    }
    match u.status.as_str() {
        "in_progress" => {
            let r = sqlx::query("UPDATE workflow_steps SET status = 'in_progress', started_at = COALESCE(started_at, now()) WHERE job_id = $1 AND number = $2").bind(id).bind(n).execute(&ctx.db).await?;
            if r.rows_affected() == 0 {
                return Err(Error::NotFound);
            }
        }
        "completed" => {
            let conclusion = u.conclusion.as_deref().unwrap_or("success");
            if !matches!(conclusion, "success" | "failure" | "cancelled" | "skipped") {
                return Err(Error::invalid("Step", "conclusion", "Use success, failure, cancelled or skipped."));
            }
            let r = sqlx::query("UPDATE workflow_steps SET status = 'completed', conclusion = $3, started_at = COALESCE(started_at, now()), completed_at = now() WHERE job_id = $1 AND number = $2")
                .bind(id)
                .bind(n)
                .bind(conclusion)
                .execute(&ctx.db)
                .await?;
            if r.rows_affected() == 0 {
                return Err(Error::NotFound);
            }
        }
        _ => return Err(Error::invalid("Step", "status", "Use in_progress or completed.")),
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Job outputs are small strings. These limits keep a hostile job from filling the database.
const MAX_OUTPUTS: usize = 50;
const MAX_OUTPUT_BYTES: usize = 64 << 10;

async fn complete(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path(id): Path<i64>,
    Json(req): Json<rn::CompleteRequest>,
) -> Result<StatusCode> {
    let ctx = Ctx::from_state(&state);
    let runner = runner_id(&actor)?;
    owned_job(&ctx, runner, id).await?;
    if !matches!(req.conclusion.as_str(), "success" | "failure" | "cancelled" | "timed_out") {
        return Err(Error::invalid("Job", "conclusion", "Use success, failure, cancelled or timed_out."));
    }
    if req.outputs.len() > MAX_OUTPUTS || req.outputs.iter().any(|(k, v)| k.len() > 100 || v.len() > MAX_OUTPUT_BYTES) {
        return Err(Error::invalid("Job", "outputs", "Too many outputs, or one is too large."));
    }
    // A second report of the same ending is not an error: the runner may be retrying.
    runs::finish_job(&ctx, id, &req.conclusion, &req.outputs).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------- artifacts

#[derive(serde::Deserialize, Default)]
struct UploadQuery {
    retention_days: Option<i64>,
}

async fn put_artifact(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((id, name)): Path<(i64, String)>,
    Query(q): Query<UploadQuery>,
    body: Body,
) -> Result<Response> {
    let ctx = Ctx::from_state(&state);
    let job = token_job(&ctx, &actor, id, true).await?;
    let up = store::put_artifact(&ctx, job.run_id, job.id, &name, q.retention_days, body).await?;
    Ok((StatusCode::CREATED, Json(up)).into_response())
}

async fn get_artifact(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path((id, name)): Path<(i64, String)>,
) -> Result<Response> {
    let ctx = Ctx::from_state(&state);
    let job = token_job(&ctx, &actor, id, false).await?;
    let a = store::artifact_by_name(&ctx, job.run_id, &name).await?;
    let body = store::blob_body(&ctx, &a.blob_sha256).await?;
    Ok(([(axum::http::header::CONTENT_TYPE, "application/zip")], body).into_response())
}

async fn list_artifacts(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path(id): Path<i64>,
) -> Result<Json<ArtifactsResponse>> {
    let ctx = Ctx::from_state(&state);
    let job = token_job(&ctx, &actor, id, false).await?;
    let rows = store::artifacts_for_run(&ctx, job.run_id).await?;
    let base = ctx.base_url();
    let artifacts: Vec<Artifact> = rows
        .iter()
        .map(|a| Artifact {
            id: a.id,
            name: a.name.clone(),
            size_in_bytes: a.size,
            expired: a.expires_at <= chrono::Utc::now(),
            created_at: a.created_at,
            expires_at: Some(a.expires_at),
            url: format!("{base}/api/runner/v1/jobs/{id}/artifacts/{}", a.name),
            archive_download_url: format!("{base}/api/runner/v1/jobs/{id}/artifacts/{}", a.name),
        })
        .collect();
    Ok(Json(ArtifactsResponse { total_count: artifacts.len() as u32, artifacts }))
}

// ---------------------------------------------------------------------------- cache

fn job_of(actor: &Actor) -> Result<(i64, i64)> {
    match actor {
        Actor::Job { job_id, repo_id, .. } => Ok((job_id.0, repo_id.0)),
        _ => Err(Error::NotFound),
    }
}

async fn cache_lookup(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Json(req): Json<rn::CacheLookupRequest>,
) -> Result<Json<rn::CacheLookupResponse>> {
    let ctx = Ctx::from_state(&state);
    let (job, repo) = job_of(&actor)?;
    token_job(&ctx, &actor, job, false).await?;
    Ok(Json(store::cache_lookup(&ctx, repo, &req).await?))
}

#[derive(serde::Deserialize, Default)]
struct CacheQuery {
    #[serde(default)]
    version: String,
}

async fn cache_put(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path(key): Path<String>,
    Query(q): Query<CacheQuery>,
    body: Body,
) -> Result<Response> {
    let ctx = Ctx::from_state(&state);
    let (job, repo) = job_of(&actor)?;
    let row = token_job(&ctx, &actor, job, true).await?;
    let run = runs::fetch_run(&ctx, row.run_id).await?;
    let hit = store::cache_put(&ctx, repo, &key, &q.version, run.git_ref.as_deref(), body).await?;
    Ok((StatusCode::CREATED, Json(hit)).into_response())
}

async fn cache_get(State(state): State<AppState>, CurrentActor(actor): CurrentActor, Path(key): Path<String>) -> Result<Response> {
    let ctx = Ctx::from_state(&state);
    let (job, repo) = job_of(&actor)?;
    token_job(&ctx, &actor, job, false).await?;
    let id: i64 = key.parse().map_err(|_| Error::NotFound)?;
    let sha = store::cache_blob(&ctx, repo, id).await?;
    let body = store::blob_body(&ctx, &sha).await?;
    Ok(([(axum::http::header::CONTENT_TYPE, "application/octet-stream")], body).into_response())
}
