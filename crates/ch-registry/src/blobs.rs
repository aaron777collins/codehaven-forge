//! Blob uploads, mounts, downloads and deletes (OCI Distribution Spec, "Pushing a blob").
//!
//! An upload is a row in `oci_uploads` and a temporary file under `data/blobs/_uploads`.
//! Finishing it hashes the file, refuses a digest that does not match, hands the bytes to the
//! shared content-addressed blob store, and links the digest to the image in `oci_blobs`. Two
//! images with the same layer share one stored file, and a layer another image holds can be
//! mounted with no upload at all.

use crate::access::{Access, Package, authorize};
use crate::error::{RegError, RegResult};
use crate::names::{hex_of, is_digest};
use crate::range::{self, RangeOutcome};
use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use ch_core::Actor;
use ch_core::model::ClientInfo;
use ch_core::state::AppState;
use futures::StreamExt;
use futures::stream::BoxStream;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

/// The largest single blob accepted: 10 GiB, well past any sane layer.
pub const MAX_BLOB_BYTES: u64 = 10 * 1024 * 1024 * 1024;

pub fn uploads_dir(state: &AppState) -> PathBuf {
    state.config.blobs_dir().join("_uploads")
}

fn set(resp: &mut Response, name: &'static str, value: &str) {
    if let Ok(v) = HeaderValue::from_str(value) {
        resp.headers_mut().insert(name, v);
    }
}

fn plain(status: StatusCode) -> Response {
    let mut r = Response::new(Body::empty());
    *r.status_mut() = status;
    set(&mut r, "docker-distribution-api-version", "registry/2.0");
    r
}

fn location(pkg: &Package, tail: &str) -> String {
    format!("/v2/{}/{tail}", pkg.full_name())
}

/// Streams a request body into a file, refusing to write past `limit`. Returns bytes written.
async fn append_body(body: Body, file: &mut tokio::fs::File, limit: u64) -> RegResult<u64> {
    let mut stream = body.into_data_stream();
    let mut written = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| RegError::upload_invalid(format!("the upload was interrupted: {e}")))?;
        written += chunk.len() as u64;
        if written > limit {
            return Err(RegError::size_invalid("the blob is larger than this registry accepts"));
        }
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    Ok(written)
}

async fn hash_file(path: &std::path::Path) -> std::io::Result<(String, u64)> {
    let mut f = tokio::fs::File::open(path).await?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    let mut total = 0u64;
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        total += n as u64;
    }
    Ok((hex::encode(h.finalize()), total))
}

#[derive(sqlx::FromRow)]
struct UploadRow {
    id: Uuid,
    package_id: i64,
    started_by: Option<i64>,
    offset_bytes: i64,
    tmp_path: String,
}

async fn load_upload(state: &AppState, actor: &Actor, pkg: &Package, id: &str) -> RegResult<UploadRow> {
    let uuid = Uuid::parse_str(id).map_err(|_| RegError::upload_unknown())?;
    let row: Option<UploadRow> = sqlx::query_as("SELECT id, package_id, started_by, offset_bytes, tmp_path FROM oci_uploads WHERE id = $1")
        .bind(uuid)
        .fetch_optional(&state.db)
        .await?;
    let row = row.ok_or_else(RegError::upload_unknown)?;
    // An upload belongs to the image and the person who started it.
    if row.package_id != pkg.id || (row.started_by.is_some() && row.started_by != actor.user_id().map(|u| u.0) && !actor.is_system()) {
        return Err(RegError::upload_unknown());
    }
    Ok(row)
}

fn upload_headers(resp: &mut Response, pkg: &Package, id: &Uuid, offset: i64) {
    set(resp, "location", &location(pkg, &format!("blobs/uploads/{id}")));
    set(resp, "docker-upload-uuid", &id.to_string());
    set(resp, "range", &format!("0-{}", (offset - 1).max(0)));
}

/// `POST /v2/<name>/blobs/uploads/`, with `?digest=` (monolithic) or `?mount=&from=` (mount).
pub async fn start(
    state: &AppState,
    actor: &Actor,
    client: &ClientInfo,
    pkg: &Package,
    q: &std::collections::HashMap<String, String>,
    body: Body,
) -> RegResult<Response> {
    if let (Some(digest), Some(from)) = (q.get("mount"), q.get("from"))
        && is_digest(digest)
        && let Some(r) = try_mount(state, actor, client, pkg, digest, from).await?
    {
        return Ok(r);
    }
    let id = Uuid::new_v4();
    let dir = uploads_dir(state);
    tokio::fs::create_dir_all(&dir).await?;
    let path = dir.join(id.to_string());
    let mut file = tokio::fs::File::create(&path).await?;
    sqlx::query("INSERT INTO oci_uploads (id, package_id, started_by, offset_bytes, tmp_path) VALUES ($1, $2, $3, 0, $4)")
        .bind(id)
        .bind(pkg.id)
        .bind(actor.user_id().map(|u| u.0))
        .bind(path.to_string_lossy().as_ref())
        .execute(&state.db)
        .await?;
    // `?digest=` means the whole blob is in this request.
    if let Some(digest) = q.get("digest") {
        if !is_digest(digest) {
            let _ = cancel_row(state, &id, &path).await;
            return Err(RegError::digest_invalid("the digest must be sha256: and 64 lowercase hex characters"));
        }
        let written = match append_body(body, &mut file, MAX_BLOB_BYTES).await {
            Ok(n) => n,
            Err(e) => {
                let _ = cancel_row(state, &id, &path).await;
                return Err(e);
            }
        };
        drop(file);
        let row = UploadRow {
            id,
            package_id: pkg.id,
            started_by: actor.user_id().map(|u| u.0),
            offset_bytes: written as i64,
            tmp_path: path.to_string_lossy().into_owned(),
        };
        return finish_row(state, pkg, row, digest).await;
    }
    drop(file);
    let mut r = plain(StatusCode::ACCEPTED);
    upload_headers(&mut r, pkg, &id, 0);
    Ok(r)
}

async fn cancel_row(state: &AppState, id: &Uuid, path: &std::path::Path) -> RegResult<()> {
    sqlx::query("DELETE FROM oci_uploads WHERE id = $1").bind(id).execute(&state.db).await?;
    let _ = tokio::fs::remove_file(path).await;
    Ok(())
}

/// Links a blob another image holds, when the caller may pull from that image.
async fn try_mount(
    state: &AppState,
    actor: &Actor,
    client: &ClientInfo,
    pkg: &Package,
    digest: &str,
    from: &str,
) -> RegResult<Option<Response>> {
    let Ok(source) = authorize(state, actor, from, Access::Pull, false, client).await else { return Ok(None) };
    let size: Option<(i64,)> = sqlx::query_as("SELECT size FROM oci_blobs WHERE package_id = $1 AND digest = $2")
        .bind(source.id)
        .bind(digest)
        .fetch_optional(&state.db)
        .await?;
    let Some((size,)) = size else { return Ok(None) };
    link(state, pkg, digest, size as u64).await?;
    let mut r = plain(StatusCode::CREATED);
    set(&mut r, "location", &location(pkg, &format!("blobs/{digest}")));
    set(&mut r, "docker-content-digest", digest);
    Ok(Some(r))
}

/// Records that the image holds the blob, taking a reference in the blob store the first time.
async fn link(state: &AppState, pkg: &Package, digest: &str, size: u64) -> RegResult<()> {
    let mut tx = state.db.begin().await?;
    let inserted =
        sqlx::query("INSERT INTO oci_blobs (package_id, digest, size) VALUES ($1, $2, $3) ON CONFLICT (package_id, digest) DO NOTHING")
            .bind(pkg.id)
            .bind(digest)
            .bind(size as i64)
            .execute(&mut *tx)
            .await?;
    if inserted.rows_affected() == 1 {
        ch_db::blobs::add_ref(&mut tx, &ch_core::services::BlobRef { sha256: hex_of(digest).to_string(), size }).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// `PATCH /v2/<name>/blobs/uploads/<id>`: append a chunk.
pub async fn patch(state: &AppState, actor: &Actor, pkg: &Package, id: &str, headers: &HeaderMap, body: Body) -> RegResult<Response> {
    let row = load_upload(state, actor, pkg, id).await?;
    if let Some(range) = headers.get(header::CONTENT_RANGE).and_then(|v| v.to_str().ok()) {
        let start: i64 =
            range.trim_start_matches("bytes ").split('-').next().and_then(|s| s.trim().parse().ok()).ok_or_else(RegError::range_invalid)?;
        if start != row.offset_bytes {
            return Err(RegError::range_invalid());
        }
    }
    let mut file = tokio::fs::OpenOptions::new().append(true).open(&row.tmp_path).await.map_err(|_| RegError::upload_unknown())?;
    let written = append_body(body, &mut file, MAX_BLOB_BYTES.saturating_sub(row.offset_bytes as u64)).await?;
    let new_offset = row.offset_bytes + written as i64;
    sqlx::query("UPDATE oci_uploads SET offset_bytes = $2, updated_at = now() WHERE id = $1")
        .bind(row.id)
        .bind(new_offset)
        .execute(&state.db)
        .await?;
    let mut r = plain(StatusCode::ACCEPTED);
    upload_headers(&mut r, pkg, &row.id, new_offset);
    Ok(r)
}

/// `GET /v2/<name>/blobs/uploads/<id>`: where the upload stands.
pub async fn status(state: &AppState, actor: &Actor, pkg: &Package, id: &str) -> RegResult<Response> {
    let row = load_upload(state, actor, pkg, id).await?;
    let mut r = plain(StatusCode::NO_CONTENT);
    upload_headers(&mut r, pkg, &row.id, row.offset_bytes);
    Ok(r)
}

/// `PUT /v2/<name>/blobs/uploads/<id>?digest=`: the last chunk, if any, and the finish.
pub async fn put(
    state: &AppState,
    actor: &Actor,
    pkg: &Package,
    id: &str,
    q: &std::collections::HashMap<String, String>,
    body: Body,
) -> RegResult<Response> {
    let mut row = load_upload(state, actor, pkg, id).await?;
    let digest = q.get("digest").ok_or_else(|| RegError::digest_invalid("the digest query parameter is required"))?;
    if !is_digest(digest) {
        return Err(RegError::digest_invalid("the digest must be sha256: and 64 lowercase hex characters"));
    }
    let mut file = tokio::fs::OpenOptions::new().append(true).open(&row.tmp_path).await.map_err(|_| RegError::upload_unknown())?;
    let written = append_body(body, &mut file, MAX_BLOB_BYTES.saturating_sub(row.offset_bytes as u64)).await?;
    drop(file);
    row.offset_bytes += written as i64;
    finish_row(state, pkg, row, digest).await
}

async fn finish_row(state: &AppState, pkg: &Package, row: UploadRow, expected: &str) -> RegResult<Response> {
    let path = PathBuf::from(&row.tmp_path);
    let (hex, size) = hash_file(&path).await?;
    if format!("sha256:{hex}") != expected {
        cancel_row(state, &row.id, &path).await?;
        return Err(RegError::digest_invalid("the uploaded bytes do not match the digest")
            .with_detail(serde_json::json!({ "computed": format!("sha256:{hex}") })));
    }
    let file = tokio::fs::File::open(&path).await?;
    let stream = tokio_util::io::ReaderStream::new(file).boxed();
    let stored = state.blobs.put_stream(stream, MAX_BLOB_BYTES).await.map_err(RegError::from)?;
    if stored.sha256 != hex || stored.size != size {
        cancel_row(state, &row.id, &path).await?;
        return Err(RegError::internal("the blob store disagreed about what it stored"));
    }
    link(state, pkg, expected, size).await?;
    cancel_row(state, &row.id, &path).await?;
    let mut r = plain(StatusCode::CREATED);
    set(&mut r, "location", &location(pkg, &format!("blobs/{expected}")));
    set(&mut r, "docker-content-digest", expected);
    Ok(r)
}

/// `DELETE /v2/<name>/blobs/uploads/<id>`.
pub async fn cancel(state: &AppState, actor: &Actor, pkg: &Package, id: &str) -> RegResult<Response> {
    let row = load_upload(state, actor, pkg, id).await?;
    cancel_row(state, &row.id, std::path::Path::new(&row.tmp_path)).await?;
    Ok(plain(StatusCode::NO_CONTENT))
}

/// Keeps `len` bytes of a stream after skipping the first `start`, and stops reading once it has
/// them, so a range at the front of a large layer does not read the rest of the file.
fn slice(stream: BoxStream<'static, std::io::Result<Bytes>>, start: u64, len: u64) -> BoxStream<'static, std::io::Result<Bytes>> {
    stream
        .scan((start, len), |(skip, left), item| {
            if *left == 0 {
                return std::future::ready(None);
            }
            let out = match item {
                Err(e) => {
                    *left = 0;
                    Some(Err(e))
                }
                Ok(mut chunk) => {
                    let n = chunk.len() as u64;
                    if *skip >= n {
                        *skip -= n;
                        None
                    } else {
                        let mut piece = chunk.split_off(usize::try_from(*skip).unwrap_or(0));
                        *skip = 0;
                        piece.truncate(usize::try_from(*left).unwrap_or(usize::MAX));
                        *left -= piece.len() as u64;
                        Some(Ok(piece))
                    }
                }
            };
            std::future::ready(Some(out))
        })
        .filter_map(std::future::ready)
        .boxed()
}

/// `GET` and `HEAD /v2/<name>/blobs/<digest>`. A single `Range: bytes=` header on a GET answers
/// 206 with just those bytes, or 416 when it starts past the end; a header that is not one well
/// formed range is ignored and the whole blob is sent. The digest header always names the whole
/// blob, never the slice.
pub async fn get(state: &AppState, pkg: &Package, digest: &str, head: bool, range: Option<&str>) -> RegResult<Response> {
    if !is_digest(digest) {
        return Err(RegError::digest_invalid("the digest must be sha256: and 64 lowercase hex characters"));
    }
    let row: Option<(i64,)> = sqlx::query_as("SELECT size FROM oci_blobs WHERE package_id = $1 AND digest = $2")
        .bind(pkg.id)
        .bind(digest)
        .fetch_optional(&state.db)
        .await?;
    let (size,) = row.ok_or_else(RegError::blob_unknown)?;
    let size = u64::try_from(size).unwrap_or(0);
    let outcome = if head { RangeOutcome::Full } else { range::resolve(range, size) };
    let mut r = match outcome {
        RangeOutcome::Unsatisfiable => {
            let mut r =
                RegError::unsupported(StatusCode::RANGE_NOT_SATISFIABLE, "the range starts past the end of the blob").into_response();
            set(&mut r, "content-range", &format!("bytes */{size}"));
            set(&mut r, "docker-content-digest", digest);
            return Ok(r);
        }
        _ if head => {
            let mut r = plain(StatusCode::OK);
            // HEAD carries the length a GET would, without sending a body.
            set(&mut r, "content-length", &size.to_string());
            r
        }
        RangeOutcome::Full => {
            let stream = state.blobs.get_stream(hex_of(digest)).await.map_err(|_| RegError::blob_unknown())?;
            let mut r = Response::new(Body::from_stream(stream));
            set(&mut r, "content-length", &size.to_string());
            r
        }
        RangeOutcome::Partial { start, end } => {
            let stream = state.blobs.get_stream(hex_of(digest)).await.map_err(|_| RegError::blob_unknown())?;
            let len = end - start + 1;
            let mut r = Response::new(Body::from_stream(slice(stream, start, len)));
            *r.status_mut() = StatusCode::PARTIAL_CONTENT;
            set(&mut r, "content-length", &len.to_string());
            set(&mut r, "content-range", &format!("bytes {start}-{end}/{size}"));
            r
        }
    };
    set(&mut r, "docker-distribution-api-version", "registry/2.0");
    set(&mut r, "docker-content-digest", digest);
    set(&mut r, "content-type", "application/octet-stream");
    set(&mut r, "accept-ranges", "bytes");
    set(&mut r, "etag", &format!("\"{digest}\""));
    Ok(r)
}

/// `DELETE /v2/<name>/blobs/<digest>`: unlinks the blob from this image. The bytes go when no
/// image holds them and the blob store's grace period has passed.
pub async fn delete(state: &AppState, pkg: &Package, digest: &str) -> RegResult<Response> {
    if !is_digest(digest) {
        return Err(RegError::digest_invalid("the digest must be sha256: and 64 lowercase hex characters"));
    }
    let mut tx = state.db.begin().await?;
    let gone =
        sqlx::query("DELETE FROM oci_blobs WHERE package_id = $1 AND digest = $2").bind(pkg.id).bind(digest).execute(&mut *tx).await?;
    if gone.rows_affected() == 0 {
        return Err(RegError::blob_unknown());
    }
    ch_db::blobs::release(&mut tx, hex_of(digest)).await?;
    tx.commit().await?;
    Ok(plain(StatusCode::ACCEPTED))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    async fn run(chunks: &[&[u8]], start: u64, len: u64) -> Vec<u8> {
        let items: Vec<std::io::Result<Bytes>> = chunks.iter().map(|c| Ok(Bytes::copy_from_slice(c))).collect();
        let s = slice(futures::stream::iter(items).boxed(), start, len);
        s.map(|c| c.unwrap().to_vec()).collect::<Vec<_>>().await.concat()
    }

    #[tokio::test]
    async fn slice_cuts_across_chunk_boundaries() {
        let chunks: [&[u8]; 3] = [b"0123", b"4567", b"89"];
        assert_eq!(run(&chunks, 0, 10).await, b"0123456789");
        assert_eq!(run(&chunks, 2, 4).await, b"2345");
        assert_eq!(run(&chunks, 4, 2).await, b"45", "starts exactly on a chunk boundary");
        assert_eq!(run(&chunks, 3, 1).await, b"3");
        assert_eq!(run(&chunks, 8, 2).await, b"89");
        assert_eq!(run(&chunks, 9, 1).await, b"9");
        assert_eq!(run(&chunks, 5, 100).await, b"56789", "asking for more than is left stops at the end");
        assert_eq!(run(&chunks, 0, 0).await, b"");
    }

    #[tokio::test]
    async fn slice_stops_reading_once_it_has_enough() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let read = std::sync::Arc::new(AtomicUsize::new(0));
        let counter = read.clone();
        let s = futures::stream::iter((0..100).map(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok::<_, std::io::Error>(Bytes::from_static(b"abcd"))
        }))
        .boxed();
        let got = slice(s, 0, 6).map(|c| c.unwrap().to_vec()).collect::<Vec<_>>().await.concat();
        assert_eq!(got, b"abcdab");
        assert!(read.load(Ordering::SeqCst) <= 3, "read {} chunks for 6 bytes", read.load(Ordering::SeqCst));
    }
}
