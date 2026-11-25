//! The cache service the Actions toolkit speaks (ch-ctk.6), for the unmodified `@actions/cache`
//! that `actions/setup-node`, `setup-python`, `setup-go`, `setup-java` and others bundle.
//!
//! The toolkit picks the v1 protocol on a host that is not github.com. It finds it at
//! `ACTIONS_CACHE_URL` (`<server>/api/runner/v1/toolkit/`) and sends `ACTIONS_RUNTIME_TOKEN`,
//! which the gate turns into the job's own actor (`ch_db::ci_auth::job_from_runtime_token`):
//!
//! ```text
//! GET   _apis/artifactcache/cache?keys=a,b&version=v   look up (204 when nothing matches)
//! POST  _apis/artifactcache/caches                     reserve {key, version} -> {cacheId}
//! PATCH _apis/artifactcache/caches/{id}                one range, Content-Range: bytes a-b/*
//! POST  _apis/artifactcache/caches/{id}                commit {size}
//! GET   blobs/{grant}                                  the archive, by a signed link
//! ```
//!
//! The same rules as the built-in cache action: a job reads its own ref's caches, then for a pull
//! request its base branch's, then the default branch's, and writes only its own ref's. A fork
//! pull request reads and never writes. The archive link carries no credential, because the
//! toolkit downloads it without one, so it is a grant signed by this server for one cache entry
//! that stops working after an hour.

use crate::ctx::Ctx;
use crate::runs::{JOB_COLS, JobRow};
use crate::store;
use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::handler::Handler;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use ch_api_types::runner::CacheLookupRequest;
use ch_core::access::CurrentActor;
use ch_core::state::AppState;
use ch_core::{Actor, Error, Result};
use hmac::Mac;
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

/// How long an archive link works.
pub(crate) const GRANT_SECONDS: i64 = 3600;
/// The most one upload range may carry.
pub(crate) const MAX_RANGE: u64 = 64 * 1024 * 1024;
/// How many ranges one job may have open at once. The toolkit sends four at a time.
pub const MAX_JOB_RANGES: usize = 8;
/// How many caches one job may be saving at once.
pub const MAX_JOB_RESERVATIONS: i64 = 20;
/// How many ranges the whole server takes at once, over every job.
pub const MAX_SERVER_RANGES: usize = 32;
/// How long a range may go without sending a byte before it is dropped.
pub(crate) const RANGE_IDLE: std::time::Duration = std::time::Duration::from_secs(60);

/// Who has a range open, so a burst of them cannot tie the server up. Ranges are written to the
/// disk as they arrive, so one costs a read buffer and a file handle, never its 64 MiB; this
/// caps those too.
#[derive(Default)]
pub struct RangeSlots(std::sync::Mutex<(usize, std::collections::HashMap<i64, usize>)>);

/// A place in `RangeSlots`, given back when it is dropped.
pub struct RangeSlot {
    slots: std::sync::Arc<RangeSlots>,
    job: i64,
}

impl RangeSlots {
    pub fn take(self: &std::sync::Arc<Self>, job: i64, max_total: usize, max_job: usize) -> Option<RangeSlot> {
        let mut g = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mine = g.1.get(&job).copied().unwrap_or(0);
        if g.0 >= max_total || mine >= max_job {
            return None;
        }
        g.0 += 1;
        g.1.insert(job, mine + 1);
        Some(RangeSlot { slots: self.clone(), job })
    }
}

impl Drop for RangeSlot {
    fn drop(&mut self) {
        let mut g = self.slots.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        g.0 = g.0.saturating_sub(1);
        if let Some(n) = g.1.get_mut(&self.job) {
            *n -= 1;
            if *n == 0 {
                g.1.remove(&self.job);
            }
        }
    }
}

pub(crate) fn range_slots() -> &'static std::sync::Arc<RangeSlots> {
    static SLOTS: std::sync::OnceLock<std::sync::Arc<RangeSlots>> = std::sync::OnceLock::new();
    SLOTS.get_or_init(Default::default)
}

pub fn routes() -> Router<AppState> {
    // A range may be up to 64 MiB, and is written to disk as it arrives; every other body is
    // small and keeps the default limit.
    let ranges = Router::new()
        .route("/toolkit/_apis/artifactcache/caches/{id}", axum::routing::patch(upload).post(commit))
        .layer(DefaultBodyLimit::max(MAX_RANGE as usize + 1024));
    Router::new()
        .route("/toolkit/_apis/artifactcache/cache", get(lookup))
        .route("/toolkit/_apis/artifactcache/caches", post(reserve))
        .route(
            "/toolkit/blobs/{grant}",
            get(download).put(crate::toolkit_artifacts::put_blob.layer(DefaultBodyLimit::max(MAX_RANGE as usize + 1024))),
        )
        .merge(ranges)
}

/// The job behind the runtime token, still running.
pub(crate) async fn job(ctx: &Ctx, actor: &Actor) -> Result<JobRow> {
    let Actor::Job { job_id, .. } = actor else { return Err(Error::NotFound) };
    sqlx::query_as::<_, JobRow>(&format!("SELECT {JOB_COLS} FROM workflow_jobs WHERE id = $1 AND status = 'in_progress'"))
        .bind(job_id.0)
        .fetch_optional(&ctx.db)
        .await?
        .ok_or(Error::NotFound)
}

/// The most the server holds at once in cache uploads that are not committed yet, over every
/// job. A job may open 20 reservations of 2 GiB each, so without this one workflow could fill the
/// data disk.
pub const MAX_INFLIGHT_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Where partly uploaded caches wait.
pub fn tmp_dir(ctx: &Ctx) -> std::path::PathBuf {
    ctx.config.storage.data_dir.join("tmp").join("toolkit")
}

/// What the uploads waiting in `dir` hold now. A file's length, which is what it takes on disk
/// at most.
pub(crate) async fn inflight_bytes(dir: &std::path::Path) -> u64 {
    let Ok(mut rd) = tokio::fs::read_dir(dir).await else { return 0 };
    let mut total = 0u64;
    while let Ok(Some(e)) = rd.next_entry().await {
        if let Ok(m) = e.metadata().await {
            total = total.saturating_add(m.len());
        }
    }
    total
}

/// Forgets uploads nobody will finish: those whose job has ended, and any older than an hour.
/// Their files go with them, and so does any file with no reservation left (a crash, a deleted
/// job). Returns how many reservations were dropped.
pub async fn drop_abandoned_uploads(ctx: &Ctx) -> Result<u64> {
    let ids: Vec<i64> = sqlx::query_scalar(
        "DELETE FROM toolkit_uploads u
         WHERE u.created_at < now() - interval '1 hour'
            OR NOT EXISTS (SELECT 1 FROM workflow_jobs j WHERE j.id = u.job_id AND j.status = 'in_progress')
         RETURNING u.id",
    )
    .fetch_all(&ctx.db)
    .await?;
    let dir = tmp_dir(ctx);
    for id in &ids {
        let _ = tokio::fs::remove_file(dir.join(format!("cache-{id}.part"))).await;
        crate::toolkit_artifacts::remove_upload_files(&dir, *id).await;
    }
    // A file with no reservation. A reservation is made before its first byte is written, so a
    // file this old with none was left behind.
    let Ok(mut rd) = tokio::fs::read_dir(&dir).await else { return Ok(ids.len() as u64) };
    let live: std::collections::HashSet<i64> =
        sqlx::query_scalar("SELECT id FROM toolkit_uploads").fetch_all(&ctx.db).await?.into_iter().collect();
    while let Ok(Some(e)) = rd.next_entry().await {
        let name = e.file_name().to_string_lossy().to_string();
        let id = upload_id_of(&name);
        let old = e
            .metadata()
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > std::time::Duration::from_secs(300));
        if old && id.is_none_or(|i| !live.contains(&i)) {
            let _ = tokio::fs::remove_file(e.path()).await;
        }
    }
    Ok(ids.len() as u64)
}

/// The upload a file in the toolkit's temporary directory belongs to: `cache-<id>.part`,
/// `art-<id>.part`, `art-<id>.joining` or `art-<id>-<block>.blk`.
fn upload_id_of(name: &str) -> Option<i64> {
    if let Some(n) = name.strip_prefix("cache-").and_then(|n| n.strip_suffix(".part")) {
        return n.parse().ok();
    }
    let n = name.strip_prefix("art-")?;
    n.split(['.', '-']).next()?.parse().ok()
}

pub(crate) fn may_write(actor: &Actor) -> Result<()> {
    match actor {
        Actor::Job { perms, .. } if perms.runtime_write() => Ok(()),
        _ => Err(Error::forbidden("A pull request from a fork may read caches but not save them.")),
    }
}

// ------------------------------------------------------------------------------------ grants

fn sign(ctx: &Ctx, kind: &str, id: i64, exp: i64) -> String {
    let key = ctx.shared.toolkit_key();
    let Ok(mut mac) = hmac::Hmac::<sha2::Sha256>::new_from_slice(&key) else { return String::new() };
    mac.update(format!("{kind}:{id}:{exp}").as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// `<kind>.<id>.<expires>.<signature>`.
pub fn grant(ctx: &Ctx, kind: &str, id: i64) -> String {
    let exp = chrono::Utc::now().timestamp() + GRANT_SECONDS;
    format!("{kind}.{id}.{exp}.{}", sign(ctx, kind, id, exp))
}

/// The id a grant is for, when it is this server's, of this kind, and not expired.
pub(crate) fn check_grant(ctx: &Ctx, g: &str, want: &str) -> Option<i64> {
    let mut p = g.split('.');
    let (kind, id, exp, sig) = (p.next()?, p.next()?, p.next()?, p.next()?);
    if p.next().is_some() || kind != want || sig.len() != 64 {
        return None;
    }
    let (id, exp): (i64, i64) = (id.parse().ok()?, exp.parse().ok()?);
    if exp <= chrono::Utc::now().timestamp() {
        return None;
    }
    let key = ctx.shared.toolkit_key();
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(&key).ok()?;
    mac.update(format!("{kind}:{id}:{exp}").as_bytes());
    mac.verify_slice(&hex::decode(sig).ok()?).ok()?;
    Some(id)
}

// ------------------------------------------------------------------------------------ cache

#[derive(Deserialize)]
struct LookupQuery {
    keys: String,
    #[serde(default)]
    version: String,
}

async fn lookup(State(state): State<AppState>, CurrentActor(actor): CurrentActor, Query(q): Query<LookupQuery>) -> Result<Response> {
    let ctx = Ctx::from_state(&state);
    let row = job(&ctx, &actor).await?;
    let keys: Vec<String> = q.keys.split(',').map(|k| k.trim().to_string()).filter(|k| !k.is_empty()).collect();
    let refs = crate::runner_api::cache_refs(&ctx, &row).await?;
    let found = store::cache_lookup(&ctx, row.repo_id, &CacheLookupRequest { keys, version: q.version }, &refs).await?;
    let Some(hit) = found.hit else { return Ok(StatusCode::NO_CONTENT.into_response()) };
    let scope: Option<String> = sqlx::query_scalar("SELECT git_ref FROM caches WHERE id = $1").bind(hit.id).fetch_one(&ctx.db).await?;
    let location = format!("{}/api/runner/v1/toolkit/blobs/{}", ctx.base_url(), grant(&ctx, "c", hit.id));
    Ok(Json(json!({ "cacheKey": hit.key, "scope": scope.unwrap_or_default(), "archiveLocation": location })).into_response())
}

#[derive(Deserialize)]
struct ReserveBody {
    key: String,
    version: String,
    #[serde(default, rename = "cacheSize")]
    cache_size: Option<i64>,
}

async fn reserve(State(state): State<AppState>, CurrentActor(actor): CurrentActor, Json(b): Json<ReserveBody>) -> Result<Response> {
    let ctx = Ctx::from_state(&state);
    let row = job(&ctx, &actor).await?;
    may_write(&actor)?;
    if b.cache_size.is_some_and(|s| s < 0 || s as u64 > store::MAX_CACHE_ENTRY_BYTES) {
        return Err(Error::invalid("Cache", "cacheSize", "A cache may be at most 2 GiB."));
    }
    if b.key.is_empty() || b.key.len() > 512 || b.key.chars().any(char::is_control) || b.version.len() > 128 {
        return Err(Error::invalid("Cache", "key", "The key must be 1 to 512 characters with no control characters."));
    }
    let run = crate::runs::fetch_run(&ctx, row.run_id).await?;
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM caches WHERE repo_id = $1 AND key = $2 AND version = $3 AND git_ref IS NOT DISTINCT FROM $4)",
    )
    .bind(row.repo_id)
    .bind(&b.key)
    .bind(&b.version)
    .bind(run.git_ref.as_deref())
    .fetch_one(&ctx.db)
    .await?;
    if exists {
        return Ok((StatusCode::CONFLICT, Json(json!({ "message": "Cache already exists." }))).into_response());
    }
    // The count and the insert are one step under a lock on the job, so a burst of reservations
    // cannot all see 19 and all go in.
    let mut tx = ctx.db.begin().await?;
    sqlx::query("SELECT id FROM workflow_jobs WHERE id = $1 FOR UPDATE").bind(row.id).fetch_optional(&mut *tx).await?;
    let open: i64 = sqlx::query_scalar("SELECT count(*) FROM toolkit_uploads WHERE job_id = $1").bind(row.id).fetch_one(&mut *tx).await?;
    if open >= MAX_JOB_RESERVATIONS {
        return Err(Error::invalid("Cache", "key", "This job has too many caches being saved at once."));
    }
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO toolkit_uploads (job_id, kind, key, version, git_ref) VALUES ($1, 'cache', $2, $3, $4) RETURNING id",
    )
    .bind(row.id)
    .bind(&b.key)
    .bind(&b.version)
    .bind(run.git_ref.as_deref())
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(json!({ "cacheId": id }))).into_response())
}

/// This job's reservation, with where its bytes are kept.
async fn upload_of(ctx: &Ctx, job: &JobRow, id: i64) -> Result<(String, String, Option<String>, std::path::PathBuf)> {
    let row: Option<(String, String, Option<String>)> =
        sqlx::query_as("SELECT key, version, git_ref FROM toolkit_uploads WHERE id = $1 AND job_id = $2 AND kind = 'cache'")
            .bind(id)
            .bind(job.id)
            .fetch_optional(&ctx.db)
            .await?;
    let (key, version, git_ref) = row.ok_or(Error::NotFound)?;
    let dir = tmp_dir(ctx);
    tokio::fs::create_dir_all(&dir).await.map_err(Error::internal)?;
    Ok((key, version, git_ref, dir.join(format!("cache-{id}.part"))))
}

/// `bytes <first>-<last>/*`, both ends included.
pub fn content_range(v: &str) -> Option<(u64, u64)> {
    let r = v.trim().strip_prefix("bytes ")?;
    let (range, total) = r.split_once('/')?;
    if total != "*" && total.parse::<u64>().is_err() {
        return None;
    }
    let (a, b) = range.split_once('-')?;
    let (a, b): (u64, u64) = (a.parse().ok()?, b.parse().ok()?);
    if b < a {
        return None;
    }
    Some((a, b))
}

async fn upload(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path(id): Path<i64>,
    headers: HeaderMap,
    body: Body,
) -> Result<StatusCode> {
    use futures::StreamExt;
    let ctx = Ctx::from_state(&state);
    let row = job(&ctx, &actor).await?;
    may_write(&actor)?;
    let (_, _, _, path) = upload_of(&ctx, &row, id).await?;
    let range = headers.get(header::CONTENT_RANGE).and_then(|v| v.to_str().ok()).and_then(content_range);
    let Some((first, last)) = range else {
        return Err(Error::invalid("Cache", "Content-Range", "Send Content-Range: bytes <first>-<last>/*."));
    };
    if last >= store::MAX_CACHE_ENTRY_BYTES {
        return Err(Error::invalid("Cache", "Content-Range", "A cache may be at most 2 GiB."));
    }
    let want = last - first + 1;
    // The toolkit sends 32 MiB at a time. The body goes to the disk as it arrives, so a range
    // costs a read buffer however large it claims to be. A range that turns out short or long is
    // refused, and what it wrote stays: the job's own upload, which only commits when its size
    // is right, and the toolkit sends the range again.
    if want > MAX_RANGE {
        return Err(Error::invalid("Cache", "Content-Range", "Send at most 64 MiB in one range."));
    }
    if headers.get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok()).is_some_and(|n| n != want) {
        return Err(Error::invalid("Cache", "Content-Range", "The body is not the length of its Content-Range."));
    }
    let Some(_slot) = range_slots().take(row.id, MAX_SERVER_RANGES, MAX_JOB_RANGES) else {
        return Err(Error::unavailable("Too many uploads are open at once. Try again shortly."));
    };
    if inflight_bytes(path.parent().unwrap_or(std::path::Path::new("."))).await.saturating_add(want) > MAX_INFLIGHT_BYTES {
        return Err(Error::invalid("Cache", "body", "The server is holding too many unfinished uploads. Try again later."));
    }
    let mut f = tokio::fs::OpenOptions::new().create(true).write(true).truncate(false).open(&path).await.map_err(Error::internal)?;
    f.seek(std::io::SeekFrom::Start(first)).await.map_err(Error::internal)?;
    let mut stream = body.into_data_stream();
    let mut got = 0u64;
    loop {
        let next = tokio::time::timeout(RANGE_IDLE, stream.next()).await;
        let chunk = match next {
            Err(_) => return Err(Error::invalid("Cache", "body", "The upload stopped sending.")),
            Ok(None) => break,
            Ok(Some(c)) => c.map_err(|_| Error::invalid("Cache", "body", "The upload was cut off."))?,
        };
        got += chunk.len() as u64;
        if got > want {
            return Err(Error::invalid("Cache", "Content-Range", "The body is longer than its Content-Range."));
        }
        f.write_all(&chunk).await.map_err(Error::internal)?;
    }
    if got != want {
        return Err(Error::invalid("Cache", "Content-Range", "The body is shorter than its Content-Range."));
    }
    f.flush().await.map_err(Error::internal)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct CommitBody {
    size: i64,
}

async fn commit(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    Path(id): Path<i64>,
    Json(b): Json<CommitBody>,
) -> Result<Response> {
    let ctx = Ctx::from_state(&state);
    let row = job(&ctx, &actor).await?;
    may_write(&actor)?;
    let (key, version, git_ref, path) = upload_of(&ctx, &row, id).await?;
    let size = tokio::fs::metadata(&path).await.map(|m| m.len()).unwrap_or(0);
    let done = async {
        if b.size < 0 || size != b.size as u64 {
            return Err(Error::invalid("Cache", "size", format!("The cache is {size} bytes, not {}.", b.size)));
        }
        let file = tokio::fs::File::open(&path).await.map_err(Error::internal)?;
        let body = Body::from_stream(tokio_util::io::ReaderStream::new(file));
        match store::cache_put(&ctx, row.repo_id, &key, &version, git_ref.as_deref(), body).await {
            Ok(_) => Ok(()),
            // Another job saved the same key first: caches are write-once, and that is fine.
            Err(Error::Conflict(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }
    .await;
    let _ = tokio::fs::remove_file(&path).await;
    sqlx::query("DELETE FROM toolkit_uploads WHERE id = $1").bind(id).execute(&ctx.db).await?;
    done?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// The archive behind a signed link. Anything wrong with the link is a plain 404.
async fn download(State(state): State<AppState>, Path(g): Path<String>) -> Response {
    let ctx = Ctx::from_state(&state);
    // An artifact's zip (the artifact service) or a cache archive.
    if g.starts_with("a.") {
        return crate::toolkit_artifacts::download(&ctx, &g).await.unwrap_or_else(|| StatusCode::NOT_FOUND.into_response());
    }
    let Some(id) = check_grant(&ctx, &g, "c") else { return StatusCode::NOT_FOUND.into_response() };
    let row: Option<(String, i64)> =
        match sqlx::query_as("UPDATE caches SET last_used_at = now() WHERE id = $1 RETURNING blob_sha256, size")
            .bind(id)
            .fetch_optional(&ctx.db)
            .await
        {
            Ok(r) => r,
            Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
        };
    let Some((sha, size)) = row else { return StatusCode::NOT_FOUND.into_response() };
    match store::blob_body(&ctx, &sha).await {
        Ok(body) => ([(header::CONTENT_TYPE, "application/octet-stream".to_string()), (header::CONTENT_LENGTH, size.to_string())], body)
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_ranges_are_read_strictly() {
        assert_eq!(content_range("bytes 0-9/*"), Some((0, 9)));
        assert_eq!(content_range("bytes 10-19/100"), Some((10, 19)));
        for bad in ["bytes 9-0/*", "0-9/*", "bytes 0-9", "bytes -9/*", "bytes 0-/*", "bytes 0-99999999999999999999/*", "items 0-9/*"] {
            assert_eq!(content_range(bad), None, "{bad}");
        }
    }

    #[test]
    fn range_slots_are_capped_per_job_and_over_the_server_and_given_back() {
        let slots = std::sync::Arc::new(RangeSlots::default());
        let a1 = slots.take(1, 3, 2).unwrap();
        let _a2 = slots.take(1, 3, 2).unwrap();
        assert!(slots.take(1, 3, 2).is_none(), "job 1 is at its own limit");
        let _b1 = slots.take(2, 3, 2).unwrap();
        assert!(slots.take(3, 3, 2).is_none(), "the server is at its limit");
        drop(a1);
        assert!(slots.take(3, 3, 2).is_some(), "a dropped slot is free again");
    }
}
