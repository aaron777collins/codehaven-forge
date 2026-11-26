//! The artifact service the Actions toolkit speaks (ch-ctk.6), for the unmodified
//! `@actions/artifact` (v2 and later, "v4 artifacts") that actions bundle.
//!
//! The toolkit takes the origin of `ACTIONS_RESULTS_URL` and calls five Twirp methods on it, with
//! `ACTIONS_RUNTIME_TOKEN`, then moves the bytes with an Azure Blob client against a signed link:
//!
//! ```text
//! POST /twirp/github.actions.results.api.v1.ArtifactService/CreateArtifact       -> signedUploadUrl
//! PUT  blobs/{grant}?comp=block&blockid=..   one block (or a plain PUT: the whole blob)
//! PUT  blobs/{grant}?comp=blocklist          the XML list that joins the blocks
//! POST .../FinalizeArtifact                  {name, size, hash}
//! POST .../ListArtifacts                     {nameFilter, idFilter}
//! POST .../GetSignedArtifactURL              {name} -> signedUrl
//! GET  blobs/{grant}                         the zip, by a signed link
//! POST .../DeleteArtifact                    {name}
//! ```
//!
//! A job reads the artifacts of its own run and writes only its own run's. A pull request from a
//! fork reads and never writes, like the cache. The two links carry no credential because the
//! Azure client sends none: each is a grant this server signed for one upload or one artifact,
//! good for an hour (`toolkit::grant`). Anything wrong with a link is a plain 404.

use crate::ctx::Ctx;
use crate::runs::JobRow;
use crate::store;
use crate::toolkit::{
    GRANT_SECONDS, MAX_JOB_RANGES, MAX_JOB_RESERVATIONS, MAX_RANGE, MAX_SERVER_RANGES, RANGE_IDLE, check_grant, grant, inflight_bytes, job,
    may_write, range_slots, tmp_dir,
};
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use bytes::Bytes;
use ch_core::access::CurrentActor;
use ch_core::state::AppState;
use ch_core::{Actor, Error};
use serde_json::{Value, json};
use sha2::Digest;
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The path the toolkit calls, below the origin of `ACTIONS_RESULTS_URL`.
pub const TWIRP_PREFIX: &str = "/twirp/github.actions.results.api.v1.ArtifactService";

/// The most blocks one upload may be joined from. Azure's own limit is 50,000.
const MAX_BLOCKS: usize = 50_000;
/// The longest block id the Azure client sends is 64 bytes.
const MAX_BLOCK_ID: usize = 128;
/// A block list is a few bytes per block.
const MAX_LIST_BYTES: usize = 4 * 1024 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new().route(&format!("{TWIRP_PREFIX}/{{method}}"), post(twirp))
}

// ------------------------------------------------------------------------------------ errors

/// A Twirp error: the JSON the toolkit reads `msg` from.
struct Tw(StatusCode, &'static str, String);

impl IntoResponse for Tw {
    fn into_response(self) -> Response {
        (self.0, axum::Json(json!({ "code": self.1, "msg": self.2 }))).into_response()
    }
}

impl From<Error> for Tw {
    fn from(e: Error) -> Self {
        match e {
            Error::NotFound => Tw(StatusCode::NOT_FOUND, "not_found", "Not found.".into()),
            Error::Forbidden(m) => Tw(StatusCode::FORBIDDEN, "permission_denied", m),
            Error::Conflict(m) => Tw(StatusCode::CONFLICT, "already_exists", m),
            Error::Validation(f) => Tw(
                StatusCode::BAD_REQUEST,
                "invalid_argument",
                f.first().and_then(|f| f.message.clone()).unwrap_or_else(|| "The request is not valid.".into()),
            ),
            Error::Unavailable(m) => Tw(StatusCode::SERVICE_UNAVAILABLE, "unavailable", m),
            Error::RateLimited { .. } => Tw(StatusCode::TOO_MANY_REQUESTS, "resource_exhausted", "Slow down.".into()),
            _ => Tw(StatusCode::INTERNAL_SERVER_ERROR, "internal", "Something went wrong.".into()),
        }
    }
}

fn bad(msg: &str) -> Tw {
    Tw(StatusCode::BAD_REQUEST, "invalid_argument", msg.to_string())
}

// ------------------------------------------------------------------------------------ twirp

async fn twirp(State(state): State<AppState>, CurrentActor(actor): CurrentActor, Path(method): Path<String>, body: Bytes) -> Response {
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v @ Value::Object(_)) => v,
        _ => return bad("The body must be a JSON object.").into_response(),
    };
    let ctx = Ctx::from_state(&state);
    let done = match method.as_str() {
        "CreateArtifact" => create(&ctx, &actor, &req).await,
        "FinalizeArtifact" => finalize(&ctx, &actor, &req).await,
        "ListArtifacts" => list(&ctx, &actor, &req).await,
        "GetSignedArtifactURL" => signed_url(&ctx, &actor, &req).await,
        "DeleteArtifact" => delete(&ctx, &actor, &req).await,
        _ => Err(Tw(StatusCode::NOT_FOUND, "bad_route", "No such method.".into())),
    };
    match done {
        Ok(v) => axum::Json(v).into_response(),
        Err(e) => e.into_response(),
    }
}

fn text<'a>(req: &'a Value, field: &str) -> Option<&'a str> {
    req.get(field).and_then(Value::as_str)
}

/// An `int64` or a wrapper of one, which protobuf JSON writes as a string and a hand-written
/// client may write as a number.
fn number(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        Value::Object(o) => o.get("value").and_then(number),
        _ => None,
    }
}

/// A string, or the `{value}` of a wrapper.
fn wrapped_text(v: &Value) -> Option<&str> {
    match v {
        Value::String(s) => Some(s),
        Value::Object(o) => o.get("value").and_then(Value::as_str),
        _ => None,
    }
}

/// The run and job ids the request names must be the token's own. The toolkit reads them out of
/// the token, so a mismatch is a client that is not the toolkit, or another job's ids.
fn same_job(req: &Value, row: &JobRow) -> Result<(), Tw> {
    let ids_match = |field: &str, want: i64| req.get(field).and_then(number).is_none_or(|n| n == want);
    if ids_match("workflowRunBackendId", row.run_id) && ids_match("workflowJobRunBackendId", row.id) {
        Ok(())
    } else {
        Err(Tw(StatusCode::FORBIDDEN, "permission_denied", "These ids are not this job's.".into()))
    }
}

fn artifact_name(req: &Value) -> Result<&str, Tw> {
    let name = text(req, "name").ok_or_else(|| bad("Send the artifact's name."))?;
    if !store::valid_artifact_name(name) {
        return Err(bad("Names may use letters, digits, spaces and . _ - ( ) and are at most 100 characters."));
    }
    Ok(name)
}

fn upload_file(ctx: &Ctx, id: i64) -> PathBuf {
    tmp_dir(ctx).join(format!("art-{id}.part"))
}

fn block_file(ctx: &Ctx, id: i64, block_id: &str) -> PathBuf {
    let h = hex::encode(sha2::Sha256::digest(block_id.as_bytes()));
    tmp_dir(ctx).join(format!("art-{id}-{}.blk", &h[..32]))
}

/// Removes every file an upload left: the joined blob and any blocks.
pub async fn remove_upload_files(dir: &std::path::Path, id: i64) {
    let _ = tokio::fs::remove_file(dir.join(format!("art-{id}.part"))).await;
    let prefix = format!("art-{id}-");
    let Ok(mut rd) = tokio::fs::read_dir(dir).await else { return };
    while let Ok(Some(e)) = rd.next_entry().await {
        if e.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = tokio::fs::remove_file(e.path()).await;
        }
    }
}

// ------------------------------------------------------------------------------------ methods

async fn create(ctx: &Ctx, actor: &Actor, req: &Value) -> Result<Value, Tw> {
    let row = job(ctx, actor).await?;
    may_write(actor)?;
    same_job(req, &row)?;
    let name = artifact_name(req)?;
    // The retention the action asked for, in whole days, at most what the server keeps.
    let days = req
        .get("expiresAt")
        .and_then(Value::as_str)
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|t| ((t.timestamp() - chrono::Utc::now().timestamp() + 86_399) / 86_400).clamp(1, store::DEFAULT_RETENTION_DAYS) as i32);
    // A name is written once per run, as on GitHub: the action deletes it first to overwrite.
    let taken: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM artifacts WHERE run_id = $1 AND name = $2 AND expires_at > now())")
        .bind(row.run_id)
        .bind(name)
        .fetch_one(&ctx.db)
        .await
        .map_err(Error::from)?;
    if taken {
        return Err(Error::conflict("An artifact with this name already exists on the workflow run.").into());
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM artifacts WHERE run_id = $1")
        .bind(row.run_id)
        .fetch_one(&ctx.db)
        .await
        .map_err(Error::from)?;
    if count >= store::MAX_ARTIFACTS_PER_RUN {
        return Err(bad("A run may hold at most 100 artifacts."));
    }
    let mut tx = ctx.db.begin().await.map_err(Error::from)?;
    sqlx::query("SELECT id FROM workflow_jobs WHERE id = $1 FOR UPDATE")
        .bind(row.id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(Error::from)?;
    // An unfinished upload of the same name from this job is replaced (the toolkit retries).
    let old: Vec<i64> = sqlx::query_scalar("DELETE FROM toolkit_uploads WHERE job_id = $1 AND kind = 'artifact' AND key = $2 RETURNING id")
        .bind(row.id)
        .bind(name)
        .fetch_all(&mut *tx)
        .await
        .map_err(Error::from)?;
    let open: i64 = sqlx::query_scalar("SELECT count(*) FROM toolkit_uploads WHERE job_id = $1")
        .bind(row.id)
        .fetch_one(&mut *tx)
        .await
        .map_err(Error::from)?;
    if open >= MAX_JOB_RESERVATIONS {
        return Err(bad("This job has too many uploads open at once."));
    }
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO toolkit_uploads (job_id, kind, key, version, retention_days) VALUES ($1, 'artifact', $2, '', $3) RETURNING id",
    )
    .bind(row.id)
    .bind(name)
    .bind(days)
    .fetch_one(&mut *tx)
    .await
    .map_err(Error::from)?;
    tx.commit().await.map_err(Error::from)?;
    for o in old {
        remove_upload_files(&tmp_dir(ctx), o).await;
    }
    let url = format!("{}/api/runner/v1/toolkit/blobs/{}", ctx.base_url(), grant(ctx, "u", id));
    Ok(json!({ "ok": true, "signedUploadUrl": url }))
}

/// The unfinished upload of this name from this job.
async fn upload_row(ctx: &Ctx, row: &JobRow, name: &str) -> Result<(i64, Option<i32>), Tw> {
    let r: Option<(i64, Option<i32>)> =
        sqlx::query_as("SELECT id, retention_days FROM toolkit_uploads WHERE job_id = $1 AND kind = 'artifact' AND key = $2")
            .bind(row.id)
            .bind(name)
            .fetch_optional(&ctx.db)
            .await
            .map_err(Error::from)?;
    r.ok_or_else(|| Tw(StatusCode::NOT_FOUND, "not_found", "No upload of this name is open. Create the artifact first.".into()))
}

async fn finalize(ctx: &Ctx, actor: &Actor, req: &Value) -> Result<Value, Tw> {
    let row = job(ctx, actor).await?;
    may_write(actor)?;
    same_job(req, &row)?;
    let name = artifact_name(req)?;
    let (id, days) = upload_row(ctx, &row, name).await?;
    let path = upload_file(ctx, id);
    // Nothing has been joined yet: the upload stays open, so the client can send the rest.
    let Ok(meta) = tokio::fs::metadata(&path).await else {
        return Err(bad("No bytes were uploaded for this artifact."));
    };
    let done: Result<i64, Tw> = async {
        if let Some(size) = req.get("size").and_then(number)
            && size != meta.len() as i64
        {
            return Err(bad(&format!("The artifact is {} bytes, not {size}.", meta.len())));
        }
        if let Some(want) = req.get("hash").and_then(wrapped_text).and_then(|h| h.strip_prefix("sha256:")) {
            let p = path.clone();
            let got = tokio::task::spawn_blocking(move || -> std::io::Result<String> {
                let mut f = std::fs::File::open(p)?;
                let mut h = sha2::Sha256::new();
                std::io::copy(&mut f, &mut h)?;
                Ok(hex::encode(h.finalize()))
            })
            .await
            .map_err(Error::internal)?
            .map_err(Error::internal)?;
            if !got.eq_ignore_ascii_case(want) {
                return Err(bad("The uploaded bytes do not match the hash."));
            }
        }
        let file = tokio::fs::File::open(&path).await.map_err(Error::internal)?;
        let body = Body::from_stream(tokio_util::io::ReaderStream::new(file));
        let up = store::put_artifact(ctx, row.run_id, row.id, name, days.map(i64::from), body).await?;
        Ok(up.id)
    }
    .await;
    remove_upload_files(&tmp_dir(ctx), id).await;
    let _ = sqlx::query("DELETE FROM toolkit_uploads WHERE id = $1").bind(id).execute(&ctx.db).await;
    let artifact_id = done?;
    Ok(json!({ "ok": true, "artifactId": artifact_id.to_string() }))
}

async fn list(ctx: &Ctx, actor: &Actor, req: &Value) -> Result<Value, Tw> {
    let row = job(ctx, actor).await?;
    same_job(req, &row)?;
    let name_filter = req.get("nameFilter").and_then(wrapped_text).map(str::to_string);
    let id_filter = req.get("idFilter").and_then(number);
    let now = chrono::Utc::now();
    let artifacts: Vec<Value> = store::artifacts_for_run(ctx, row.run_id)
        .await?
        .into_iter()
        .filter(|a| a.expires_at > now)
        .filter(|a| name_filter.as_deref().is_none_or(|n| a.name == n))
        .filter(|a| id_filter.is_none_or(|i| a.id == i))
        .map(|a| {
            json!({
                "workflowRunBackendId": row.run_id.to_string(),
                "workflowJobRunBackendId": a.job_id.unwrap_or(row.id).to_string(),
                "databaseId": a.id.to_string(),
                "name": a.name,
                "size": a.size.to_string(),
                "createdAt": a.created_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            })
        })
        .collect();
    Ok(json!({ "artifacts": artifacts }))
}

async fn signed_url(ctx: &Ctx, actor: &Actor, req: &Value) -> Result<Value, Tw> {
    let row = job(ctx, actor).await?;
    same_job(req, &row)?;
    let name = text(req, "name").ok_or_else(|| bad("Send the artifact's name."))?;
    let a = store::artifact_by_name(ctx, row.run_id, name).await?;
    Ok(json!({ "signedUrl": format!("{}/api/runner/v1/toolkit/blobs/{}", ctx.base_url(), grant(ctx, "a", a.id)) }))
}

async fn delete(ctx: &Ctx, actor: &Actor, req: &Value) -> Result<Value, Tw> {
    let row = job(ctx, actor).await?;
    may_write(actor)?;
    same_job(req, &row)?;
    let name = text(req, "name").ok_or_else(|| bad("Send the artifact's name."))?;
    let a = store::artifact_by_name(ctx, row.run_id, name).await?;
    store::delete_artifact(ctx, a.id).await?;
    Ok(json!({ "ok": true, "artifactId": a.id.to_string() }))
}

// ------------------------------------------------------------------------------------ the links

/// A reply with no body, which is how the blob service answers a block or a list.
fn blob_ok(status: StatusCode) -> Response {
    let mut r = status.into_response();
    let h = r.headers_mut();
    h.insert(header::ETAG, HeaderValue::from_static("\"0x8DC0000000000\""));
    h.insert("x-ms-request-server-encrypted", HeaderValue::from_static("true"));
    h.insert("x-ms-version", HeaderValue::from_static("2021-08-06"));
    r
}

fn blob_error(status: StatusCode, code: &'static str) -> Response {
    let mut r = (status, format!("<?xml version=\"1.0\" encoding=\"utf-8\"?><Error><Code>{code}</Code></Error>")).into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/xml"));
    r.headers_mut().insert("x-ms-error-code", HeaderValue::from_static(code));
    r
}

/// The block ids of a `<BlockList>`, in order. Only `Latest`, `Committed` and `Uncommitted`
/// elements are read, and anything else in the document is refused.
pub fn parse_block_list(xml: &str) -> Option<Vec<String>> {
    let mut rest = xml.trim_start();
    if rest.starts_with("<?xml") {
        rest = rest[rest.find("?>")? + 2..].trim_start();
    }
    let rest = rest.strip_prefix("<BlockList>")?;
    let mut ids = Vec::new();
    let mut at = rest.trim_start();
    loop {
        if at.starts_with("</BlockList>") {
            return Some(ids);
        }
        let open = ["Latest", "Committed", "Uncommitted"].into_iter().find(|t| at.starts_with(&format!("<{t}>")))?;
        let after = &at[open.len() + 2..];
        let end = after.find(&format!("</{open}>"))?;
        let id = &after[..end];
        if id.is_empty() || id.len() > MAX_BLOCK_ID || id.contains('<') || id.contains('>') || ids.len() >= MAX_BLOCKS {
            return None;
        }
        ids.push(unescape(id)?);
        at = after[end + open.len() + 3..].trim_start();
    }
}

/// The five predefined XML entities, which a base64 block id never holds but a client may write.
fn unescape(s: &str) -> Option<String> {
    if !s.contains('&') {
        return Some(s.to_string());
    }
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let (ent, ch) = [("&amp;", '&'), ("&lt;", '<'), ("&gt;", '>'), ("&quot;", '"'), ("&apos;", '\'')]
            .into_iter()
            .find(|(e, _)| tail.starts_with(e))?;
        out.push(ch);
        rest = &tail[ent.len()..];
    }
    out.push_str(rest);
    Some(out)
}

/// What the blob service does for a signed upload link: a block, the list that joins the blocks,
/// or the whole blob in one request. Anything wrong with the link is a plain 404.
pub async fn put_blob(
    State(state): State<AppState>,
    Path(g): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let ctx = Ctx::from_state(&state);
    let Some(id) = check_grant(&ctx, &g, "u") else { return StatusCode::NOT_FOUND.into_response() };
    // The upload is open and its job is still running.
    let owner: Option<i64> = match sqlx::query_scalar(
        "SELECT u.job_id FROM toolkit_uploads u JOIN workflow_jobs j ON j.id = u.job_id
         WHERE u.id = $1 AND u.kind = 'artifact' AND j.status = 'in_progress'",
    )
    .bind(id)
    .fetch_optional(&ctx.db)
    .await
    {
        Ok(o) => o,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let Some(job_id) = owner else { return StatusCode::NOT_FOUND.into_response() };
    let Some(_slot) = range_slots().take(job_id, MAX_SERVER_RANGES, MAX_JOB_RANGES) else {
        return blob_error(StatusCode::SERVICE_UNAVAILABLE, "ServerBusy");
    };
    let dir = tmp_dir(&ctx);
    if tokio::fs::create_dir_all(&dir).await.is_err() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    match q.get("comp").map(String::as_str) {
        Some("block") => {
            let Some(block_id) = q.get("blockid").filter(|b| !b.is_empty() && b.len() <= MAX_BLOCK_ID) else {
                return blob_error(StatusCode::BAD_REQUEST, "InvalidQueryParameterValue");
            };
            let path = block_file(&ctx, id, block_id);
            // What this upload already holds, so one artifact cannot pass the size limit in blocks.
            let held = held_bytes(&dir, id).await;
            match write_body(&dir, &path, body, &headers, MAX_RANGE, store::MAX_ARTIFACT_BYTES.saturating_sub(held)).await {
                Ok(_) => blob_ok(StatusCode::CREATED),
                Err(r) => r,
            }
        }
        Some("blocklist") => commit_list(&ctx, id, body).await,
        None => {
            // The whole blob in one request.
            let path = upload_file(&ctx, id);
            match write_body(&dir, &path, body, &headers, store::MAX_ARTIFACT_BYTES, store::MAX_ARTIFACT_BYTES).await {
                Ok(_) => blob_ok(StatusCode::CREATED),
                Err(r) => r,
            }
        }
        Some(_) => blob_error(StatusCode::BAD_REQUEST, "InvalidQueryParameterValue"),
    }
}

/// The bytes the blocks and joined file of one upload take now.
async fn held_bytes(dir: &std::path::Path, id: i64) -> u64 {
    let prefix = format!("art-{id}-");
    let joined = format!("art-{id}.part");
    let Ok(mut rd) = tokio::fs::read_dir(dir).await else { return 0 };
    let mut total = 0u64;
    while let Ok(Some(e)) = rd.next_entry().await {
        let name = e.file_name().to_string_lossy().to_string();
        if (name.starts_with(&prefix) || name == joined)
            && let Ok(m) = e.metadata().await
        {
            total = total.saturating_add(m.len());
        }
    }
    total
}

/// Streams a request body to `path` (replacing it), within `cap` bytes and the server's cap on
/// uploads that are not finished.
#[allow(clippy::result_large_err)]
async fn write_body(
    dir: &std::path::Path,
    path: &std::path::Path,
    body: Body,
    headers: &HeaderMap,
    per_request: u64,
    cap: u64,
) -> Result<u64, Response> {
    use futures::StreamExt;
    let limit = per_request.min(cap);
    let length = headers.get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
    if length.is_some_and(|n| n > limit) {
        return Err(blob_error(StatusCode::PAYLOAD_TOO_LARGE, "RequestBodyTooLarge"));
    }
    if inflight_bytes(dir).await.saturating_add(length.unwrap_or(0)) > crate::toolkit::MAX_INFLIGHT_BYTES {
        return Err(blob_error(StatusCode::SERVICE_UNAVAILABLE, "ServerBusy"));
    }
    let mut f = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE.into_response())?;
    let mut stream = body.into_data_stream();
    let mut got = 0u64;
    loop {
        let chunk = match tokio::time::timeout(RANGE_IDLE, stream.next()).await {
            Err(_) | Ok(Some(Err(_))) => {
                drop(f);
                let _ = tokio::fs::remove_file(path).await;
                return Err(blob_error(StatusCode::REQUEST_TIMEOUT, "OperationTimedOut"));
            }
            Ok(None) => break,
            Ok(Some(Ok(c))) => c,
        };
        got += chunk.len() as u64;
        if got > limit {
            drop(f);
            let _ = tokio::fs::remove_file(path).await;
            return Err(blob_error(StatusCode::PAYLOAD_TOO_LARGE, "RequestBodyTooLarge"));
        }
        if f.write_all(&chunk).await.is_err() {
            return Err(StatusCode::SERVICE_UNAVAILABLE.into_response());
        }
    }
    if f.flush().await.is_err() {
        return Err(StatusCode::SERVICE_UNAVAILABLE.into_response());
    }
    Ok(got)
}

/// Bytes that joins in this process have reserved, by upload directory. The check against the
/// server's cap on unfinished uploads and the reservation happen under one lock, so two joins
/// cannot both pass on the room that only one of them may take. A running join is counted twice
/// (its reservation and the file it has written so far), which only makes the cap stricter.
static JOINS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, u64>>> =
    std::sync::LazyLock::new(Default::default);

/// Bytes held for one join while it runs. Dropping it gives them back.
struct JoinReservation {
    dir: std::path::PathBuf,
    bytes: u64,
}

impl JoinReservation {
    /// Takes `bytes` when what the directory holds now, what other joins have reserved and
    /// `bytes` together stay within `cap`.
    async fn take(dir: &std::path::Path, bytes: u64, cap: u64) -> Option<JoinReservation> {
        // Measured before the lock: a lock held across disk reads would make joins wait on each other.
        let on_disk = inflight_bytes(dir).await;
        let mut held = JOINS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let others = held.get(dir).copied().unwrap_or(0);
        if on_disk.saturating_add(others).saturating_add(bytes) > cap {
            return None;
        }
        *held.entry(dir.to_path_buf()).or_insert(0) += bytes;
        Some(JoinReservation { dir: dir.to_path_buf(), bytes })
    }
}

impl Drop for JoinReservation {
    fn drop(&mut self) {
        let mut held = JOINS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(n) = held.get_mut(&self.dir) {
            *n = n.saturating_sub(self.bytes);
            if *n == 0 {
                held.remove(&self.dir);
            }
        }
    }
}

/// Joins the blocks the list names, in its order, into the one file the artifact is made from.
async fn commit_list(ctx: &Ctx, id: i64, body: Body) -> Response {
    let Ok(xml) = axum::body::to_bytes(body, MAX_LIST_BYTES).await else {
        return blob_error(StatusCode::PAYLOAD_TOO_LARGE, "RequestBodyTooLarge");
    };
    let Some(ids) = std::str::from_utf8(&xml).ok().and_then(parse_block_list) else {
        return blob_error(StatusCode::BAD_REQUEST, "InvalidXmlDocument");
    };
    let dir = tmp_dir(ctx);
    let joined = upload_file(ctx, id);
    let tmp = dir.join(format!("art-{id}.joining"));
    // The joined file sits beside its blocks until they are removed, so it counts against the
    // server's cap on unfinished uploads like any other write: at most what the list names (a
    // block may be named more than once), and never more than one artifact may hold.
    let mut named = 0u64;
    for b in &ids {
        named = named.saturating_add(tokio::fs::metadata(block_file(ctx, id, b)).await.map(|m| m.len()).unwrap_or(0));
    }
    // Reserved in this process before the check and kept until the join ends, so blocklist
    // commits sent at the same moment cannot each see the same room and all pass (ch-bnh).
    let Some(_reserved) = JoinReservation::take(&dir, named.min(store::MAX_ARTIFACT_BYTES), crate::toolkit::MAX_INFLIGHT_BYTES).await
    else {
        return blob_error(StatusCode::SERVICE_UNAVAILABLE, "ServerBusy");
    };
    let result = async {
        let mut out = tokio::fs::File::create(&tmp).await.map_err(|_| StatusCode::SERVICE_UNAVAILABLE.into_response())?;
        let mut total = 0u64;
        let mut buf = vec![0u8; 256 * 1024];
        for b in &ids {
            let Ok(mut f) = tokio::fs::File::open(block_file(ctx, id, b)).await else {
                return Err(blob_error(StatusCode::BAD_REQUEST, "InvalidBlockList"));
            };
            loop {
                let n = f.read(&mut buf).await.map_err(|_| StatusCode::SERVICE_UNAVAILABLE.into_response())?;
                if n == 0 {
                    break;
                }
                total += n as u64;
                if total > store::MAX_ARTIFACT_BYTES {
                    return Err(blob_error(StatusCode::PAYLOAD_TOO_LARGE, "RequestBodyTooLarge"));
                }
                out.write_all(&buf[..n]).await.map_err(|_| StatusCode::SERVICE_UNAVAILABLE.into_response())?;
            }
        }
        out.flush().await.map_err(|_| StatusCode::SERVICE_UNAVAILABLE.into_response())?;
        drop(out);
        tokio::fs::rename(&tmp, &joined).await.map_err(|_| StatusCode::SERVICE_UNAVAILABLE.into_response())?;
        Ok(())
    }
    .await;
    match result {
        Ok(()) => {
            // The blocks are in the joined file now.
            let prefix = format!("art-{id}-");
            if let Ok(mut rd) = tokio::fs::read_dir(&dir).await {
                while let Ok(Some(e)) = rd.next_entry().await {
                    if e.file_name().to_string_lossy().starts_with(&prefix) {
                        let _ = tokio::fs::remove_file(e.path()).await;
                    }
                }
            }
            blob_ok(StatusCode::CREATED)
        }
        Err(r) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            r
        }
    }
}

/// The zip behind a signed link, or `None` when the link is not an artifact's.
pub async fn download(ctx: &Ctx, g: &str) -> Option<Response> {
    let id = check_grant(ctx, g, "a")?;
    let row: Option<(String, i64)> = sqlx::query_as("SELECT blob_sha256, size FROM artifacts WHERE id = $1 AND expires_at > now()")
        .bind(id)
        .fetch_optional(&ctx.db)
        .await
        .ok()?;
    let (sha, size) = row?;
    let body = store::blob_body(ctx, &sha).await.ok()?;
    Some(
        (
            [
                (header::CONTENT_TYPE, "application/zip".to_string()),
                (header::CONTENT_LENGTH, size.to_string()),
                (header::HeaderName::from_static("x-ms-blob-type"), "BlockBlob".to_string()),
            ],
            body,
        )
            .into_response(),
    )
}

/// How long an upload or artifact link works, for the docs and the tests.
pub const LINK_SECONDS: i64 = GRANT_SECONDS;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn joins_started_together_cannot_pass_the_cap_together() {
        let dir = std::path::PathBuf::from("/nonexistent/ch-bnh-test");
        // Room for 10 bytes: the first join takes 6, so a second asking for 6 is refused.
        let first = JoinReservation::take(&dir, 6, 10).await.expect("room for the first");
        assert!(JoinReservation::take(&dir, 6, 10).await.is_none(), "the second does not see the same room");
        // One that fits beside it passes.
        let small = JoinReservation::take(&dir, 4, 10).await.expect("4 more fits");
        assert!(JoinReservation::take(&dir, 1, 10).await.is_none(), "now it is full");
        // Racing takers: of many at once, only as many as fit get through.
        drop(small);
        drop(first);
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let d = dir.clone();
            tasks.push(tokio::spawn(async move { JoinReservation::take(&d, 4, 10).await }));
        }
        let mut kept = Vec::new();
        for t in tasks {
            if let Some(r) = t.await.unwrap() {
                kept.push(r);
            }
        }
        assert_eq!(kept.len(), 2, "8 joins of 4 bytes against 10: two pass");
        drop(kept);
        assert!(JoinReservation::take(&dir, 10, 10).await.is_some(), "everything is given back when the joins end");
    }

    #[test]
    fn a_block_list_gives_its_ids_in_order() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?><BlockList><Latest>AAAA</Latest><Latest>AAAB</Latest><Committed>x&amp;y</Committed></BlockList>"#;
        assert_eq!(parse_block_list(xml), Some(vec!["AAAA".into(), "AAAB".into(), "x&y".into()]));
        assert_eq!(parse_block_list("<BlockList></BlockList>"), Some(vec![]));
    }

    #[test]
    fn a_block_list_with_anything_else_in_it_is_refused() {
        for bad in [
            "",
            "<BlockList><Latest></Latest></BlockList>",
            "<BlockList><Latest>a</Latest>",
            "<BlockList><Other>a</Other></BlockList>",
            "<BlockList><Latest>a<b/></Latest></BlockList>",
            "<BlockList><Latest>&bogus;</Latest></BlockList>",
            "<!DOCTYPE x><BlockList></BlockList>",
            "<List><Latest>a</Latest></List>",
        ] {
            assert_eq!(parse_block_list(bad), None, "{bad}");
        }
        let too_long = format!("<BlockList><Latest>{}</Latest></BlockList>", "a".repeat(MAX_BLOCK_ID + 1));
        assert_eq!(parse_block_list(&too_long), None);
    }
}
