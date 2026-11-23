//! Artifacts and the build cache, kept in the blob store.
//!
//! Both are content-addressed: the same bytes uploaded twice are stored once. Each row holds a
//! reference on its blob, released when the row is deleted, and the blob store collects what
//! nothing references any more.

use crate::ctx::Ctx;
use axum::body::Body;
use bytes::Bytes;
use ch_api_types::runner::{ArtifactUploaded, CacheHit, CacheLookupRequest, CacheLookupResponse};
use ch_core::services::BlobRef;
use ch_core::{Error, Result};
use ch_db::blobs::{add_ref, release};
use chrono::{DateTime, Utc};
use futures::StreamExt;
use futures::stream::BoxStream;

pub const MAX_ARTIFACT_BYTES: u64 = 2 << 30;
pub const MAX_ARTIFACTS_PER_RUN: i64 = 100;
pub const DEFAULT_RETENTION_DAYS: i64 = 90;
pub const MAX_CACHE_ENTRY_BYTES: u64 = 2 << 30;
/// A repository's cache is trimmed, least recently used first, to stay under this.
pub const MAX_CACHE_BYTES_PER_REPO: i64 = 10 << 30;
/// A cache entry unused for this long is deleted.
pub const CACHE_UNUSED_DAYS: i32 = 7;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ArtifactRow {
    pub id: i64,
    pub run_id: i64,
    pub job_id: Option<i64>,
    pub name: String,
    pub blob_sha256: String,
    pub size: i64,
    pub expires_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

const ART_COLS: &str = "id, run_id, job_id, name, blob_sha256, size, expires_at, created_at";

pub fn valid_artifact_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 100
        && n.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b' ' | b'(' | b')'))
        && !n.starts_with('.')
        && n.trim() == n
}

fn body_stream(body: Body) -> BoxStream<'static, std::io::Result<Bytes>> {
    body.into_data_stream().map(|r| r.map_err(std::io::Error::other)).boxed()
}

/// Stores a job's upload under `name`. A second upload of the same name in the same run replaces
/// the first.
pub async fn put_artifact(
    ctx: &Ctx,
    run_id: i64,
    job_id: i64,
    name: &str,
    retention_days: Option<i64>,
    body: Body,
) -> Result<ArtifactUploaded> {
    if !valid_artifact_name(name) {
        return Err(Error::invalid(
            "Artifact",
            "name",
            "Names may use letters, digits, spaces and . _ - ( ) and are at most 100 characters.",
        ));
    }
    let days = retention_days.unwrap_or(DEFAULT_RETENTION_DAYS).clamp(1, DEFAULT_RETENTION_DAYS);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM artifacts WHERE run_id = $1 AND name <> $2")
        .bind(run_id)
        .bind(name)
        .fetch_one(&ctx.db)
        .await?;
    if count >= MAX_ARTIFACTS_PER_RUN {
        return Err(Error::invalid("Artifact", "name", format!("A run may hold at most {MAX_ARTIFACTS_PER_RUN} artifacts.")));
    }
    let blob = ctx.blobs.put_stream(body_stream(body), MAX_ARTIFACT_BYTES).await?;
    let mut tx = ctx.db.begin().await?;
    let old: Option<String> = sqlx::query_scalar("SELECT blob_sha256 FROM artifacts WHERE run_id = $1 AND name = $2 FOR UPDATE")
        .bind(run_id)
        .bind(name)
        .fetch_optional(&mut *tx)
        .await?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO artifacts (run_id, job_id, name, blob_sha256, size, expires_at) VALUES ($1, $2, $3, $4, $5, now() + make_interval(days => $6))
         ON CONFLICT (run_id, name) DO UPDATE SET blob_sha256 = EXCLUDED.blob_sha256, size = EXCLUDED.size, job_id = EXCLUDED.job_id,
                expires_at = EXCLUDED.expires_at, created_at = now()
         RETURNING id",
    )
    .bind(run_id)
    .bind(job_id)
    .bind(name)
    .bind(&blob.sha256)
    .bind(blob.size as i64)
    .bind(days as i32)
    .fetch_one(&mut *tx)
    .await?;
    add_ref(&mut tx, &blob).await?;
    if let Some(o) = old {
        release(&mut tx, &o).await?;
    }
    tx.commit().await?;
    Ok(ArtifactUploaded { id, name: name.to_string(), size: blob.size as i64 })
}

pub async fn artifacts_for_run(ctx: &Ctx, run_id: i64) -> Result<Vec<ArtifactRow>> {
    Ok(sqlx::query_as::<_, ArtifactRow>(&format!("SELECT {ART_COLS} FROM artifacts WHERE run_id = $1 ORDER BY id"))
        .bind(run_id)
        .fetch_all(&ctx.db)
        .await?)
}

pub async fn artifact_by_name(ctx: &Ctx, run_id: i64, name: &str) -> Result<ArtifactRow> {
    sqlx::query_as::<_, ArtifactRow>(&format!("SELECT {ART_COLS} FROM artifacts WHERE run_id = $1 AND name = $2 AND expires_at > now()"))
        .bind(run_id)
        .bind(name)
        .fetch_optional(&ctx.db)
        .await?
        .ok_or(Error::NotFound)
}

type ArtifactWithRepo = (i64, i64, Option<i64>, String, String, i64, DateTime<Utc>, DateTime<Utc>, i64);

pub async fn artifact_by_id(ctx: &Ctx, id: i64) -> Result<(ArtifactRow, i64)> {
    let row: Option<ArtifactWithRepo> = sqlx::query_as(
        "SELECT a.id, a.run_id, a.job_id, a.name, a.blob_sha256, a.size, a.expires_at, a.created_at, r.repo_id
         FROM artifacts a JOIN workflow_runs r ON r.id = a.run_id WHERE a.id = $1",
    )
    .bind(id)
    .fetch_optional(&ctx.db)
    .await?;
    let Some((id, run_id, job_id, name, blob_sha256, size, expires_at, created_at, repo)) = row else { return Err(Error::NotFound) };
    Ok((ArtifactRow { id, run_id, job_id, name, blob_sha256, size, expires_at, created_at }, repo))
}

pub async fn delete_artifact(ctx: &Ctx, id: i64) -> Result<()> {
    let mut tx = ctx.db.begin().await?;
    let sha: Option<String> =
        sqlx::query_scalar("DELETE FROM artifacts WHERE id = $1 RETURNING blob_sha256").bind(id).fetch_optional(&mut *tx).await?;
    let Some(sha) = sha else { return Err(Error::NotFound) };
    release(&mut tx, &sha).await?;
    tx.commit().await?;
    Ok(())
}

/// Deletes artifacts past their retention. Returns how many.
pub async fn expire_artifacts(ctx: &Ctx) -> Result<u64> {
    let mut tx = ctx.db.begin().await?;
    let shas: Vec<String> =
        sqlx::query_scalar("DELETE FROM artifacts WHERE expires_at < now() RETURNING blob_sha256").fetch_all(&mut *tx).await?;
    for s in &shas {
        release(&mut tx, s).await?;
    }
    tx.commit().await?;
    Ok(shas.len() as u64)
}

pub async fn blob_body(ctx: &Ctx, sha: &str) -> Result<Body> {
    let s = ctx.blobs.get_stream(sha).await?;
    Ok(Body::from_stream(s))
}

// ---------------------------------------------------------------------------- cache

fn valid_cache_key(k: &str) -> bool {
    !k.is_empty() && k.len() <= 512 && !k.chars().any(|c| c.is_control())
}

/// `refs` are the refs whose caches the job may read, most preferred first: its own, then for a
/// pull request its base branch's, then the default branch's (ch-3xn). Each ref is searched in
/// turn, the key exactly and then each restore key by prefix, as on GitHub. A cache saved with
/// no ref (from before caches had one) is readable by all and is searched last.
pub async fn cache_lookup(ctx: &Ctx, repo: i64, req: &CacheLookupRequest, refs: &[String]) -> Result<CacheLookupResponse> {
    if req.keys.is_empty() || req.keys.len() > 10 || req.keys.iter().any(|k| !valid_cache_key(k)) || req.version.len() > 128 {
        return Err(Error::invalid("Cache", "keys", "Give between 1 and 10 keys of at most 512 characters."));
    }
    let tiers = refs.iter().map(|r| Some(r.as_str())).chain(std::iter::once(None));
    let mut hit = None;
    'tiers: for git_ref in tiers {
        let exact: Option<(i64, String, i64)> = sqlx::query_as(
            "SELECT id, key, size FROM caches
              WHERE repo_id = $1 AND key = $2 AND version = $3 AND git_ref IS NOT DISTINCT FROM $4",
        )
        .bind(repo)
        .bind(&req.keys[0])
        .bind(&req.version)
        .bind(git_ref)
        .fetch_optional(&ctx.db)
        .await?;
        if let Some((id, key, size)) = exact {
            hit = Some(CacheHit { id, key, size, exact: true });
            break;
        }
        for k in &req.keys[1..] {
            let row: Option<(i64, String, i64)> = sqlx::query_as(
                "SELECT id, key, size FROM caches WHERE repo_id = $1 AND version = $2 AND starts_with(key, $3)
                   AND git_ref IS NOT DISTINCT FROM $4 ORDER BY created_at DESC, id DESC LIMIT 1",
            )
            .bind(repo)
            .bind(&req.version)
            .bind(k)
            .bind(git_ref)
            .fetch_optional(&ctx.db)
            .await?;
            if let Some((id, key, size)) = row {
                hit = Some(CacheHit { id, key, size, exact: false });
                break 'tiers;
            }
        }
    }
    if let Some(h) = &hit {
        sqlx::query("UPDATE caches SET last_used_at = now() WHERE id = $1").bind(h.id).execute(&ctx.db).await?;
    }
    Ok(CacheLookupResponse { hit })
}

/// Saves a cache entry for `git_ref`. Entries are immutable: saving a key that this ref already
/// has is a conflict, which the runner reads as "already saved". Another ref's copy of the same
/// key does not count (ch-3xn).
pub async fn cache_put(ctx: &Ctx, repo: i64, key: &str, version: &str, git_ref: Option<&str>, body: Body) -> Result<CacheHit> {
    if !valid_cache_key(key) || version.len() > 128 {
        return Err(Error::invalid("Cache", "key", "The key must be 1 to 512 characters with no control characters."));
    }
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM caches WHERE repo_id = $1 AND key = $2 AND version = $3 AND git_ref IS NOT DISTINCT FROM $4)",
    )
    .bind(repo)
    .bind(key)
    .bind(version)
    .bind(git_ref)
    .fetch_one(&ctx.db)
    .await?;
    if exists {
        return Err(Error::conflict("A cache with this key already exists."));
    }
    let blob: BlobRef = ctx.blobs.put_stream(body_stream(body), MAX_CACHE_ENTRY_BYTES).await?;
    let mut tx = ctx.db.begin().await?;
    let id: Option<i64> = sqlx::query_scalar(
        "INSERT INTO caches (repo_id, key, version, git_ref, blob_sha256, size) VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT ON CONSTRAINT caches_repo_key_version_ref DO NOTHING RETURNING id",
    )
    .bind(repo)
    .bind(key)
    .bind(version)
    .bind(git_ref)
    .bind(&blob.sha256)
    .bind(blob.size as i64)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(id) = id else {
        return Err(Error::conflict("A cache with this key already exists."));
    };
    add_ref(&mut tx, &blob).await?;
    tx.commit().await?;
    Ok(CacheHit { id, key: key.to_string(), size: blob.size as i64, exact: true })
}

/// The blob of a cache entry, if it belongs to this repository and to one of `refs`.
pub async fn cache_blob(ctx: &Ctx, repo: i64, id: i64, refs: &[String]) -> Result<String> {
    let sha: Option<String> = sqlx::query_scalar(
        "UPDATE caches SET last_used_at = now() WHERE id = $1 AND repo_id = $2 AND (git_ref IS NULL OR git_ref = ANY($3)) RETURNING blob_sha256",
    )
    .bind(id)
    .bind(repo)
    .bind(refs)
            .fetch_optional(&ctx.db)
            .await?;
    sha.ok_or(Error::NotFound)
}

/// Deletes caches unused for a week, then trims each repository to its quota, least recently
/// used first. Returns how many entries went.
pub async fn evict_caches(ctx: &Ctx) -> Result<u64> {
    let mut tx = ctx.db.begin().await?;
    let mut shas: Vec<String> =
        sqlx::query_scalar("DELETE FROM caches WHERE last_used_at < now() - make_interval(days => $1) RETURNING blob_sha256")
            .bind(CACHE_UNUSED_DAYS)
            .fetch_all(&mut *tx)
            .await?;
    let over: Vec<i64> = sqlx::query_scalar("SELECT repo_id FROM caches GROUP BY repo_id HAVING sum(size) > $1")
        .bind(MAX_CACHE_BYTES_PER_REPO)
        .fetch_all(&mut *tx)
        .await?;
    for repo in over {
        let rows: Vec<(i64, i64, String)> =
            sqlx::query_as("SELECT id, size, blob_sha256 FROM caches WHERE repo_id = $1 ORDER BY last_used_at, id")
                .bind(repo)
                .fetch_all(&mut *tx)
                .await?;
        let mut total: i64 = rows.iter().map(|r| r.1).sum();
        for (id, size, sha) in rows {
            if total <= MAX_CACHE_BYTES_PER_REPO {
                break;
            }
            sqlx::query("DELETE FROM caches WHERE id = $1").bind(id).execute(&mut *tx).await?;
            shas.push(sha);
            total -= size;
        }
    }
    for s in &shas {
        release(&mut tx, s).await?;
    }
    tx.commit().await?;
    Ok(shas.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_names() {
        for ok in ["report", "test results (linux)", "a.b-c_d", "x1"] {
            assert!(valid_artifact_name(ok), "{ok}");
        }
        for bad in ["", ".hidden", "../x", "a/b", "a\\b", " lead", "trail ", "a\0b", &"x".repeat(101), "é"] {
            assert!(!valid_artifact_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn cache_keys() {
        assert!(valid_cache_key("cargo-Linux-abc123"));
        assert!(!valid_cache_key(""));
        assert!(!valid_cache_key("a\nb"));
        assert!(!valid_cache_key(&"k".repeat(513)));
    }
}
