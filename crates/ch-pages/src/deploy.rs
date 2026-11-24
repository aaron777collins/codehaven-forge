//! The publisher (`docs/design/pages.md`, section 6): turns pushes to a site's branch into
//! queued copies, and builds one copy at a time.
//!
//! A copy is the site's folder at one commit, read from git with symbolic links, submodules
//! and names that are not UTF-8 left out, stored file by file in the blob store. The newest
//! ready copy goes live; a failed one says why and the live one stays.

use crate::core::{Core, KEEP_FAILED, KEEP_READY};
use ch_core::events::DomainEvent;
use ch_core::model::WorkspaceSnapshotLimits;
use ch_core::{RepoId, Result};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const CONSUMER: &str = "pages";
const IDLE: Duration = Duration::from_secs(2);

pub async fn run(core: Arc<Core>, stop: CancellationToken) {
    // A copy left building by a restart is built again.
    if let Err(e) = sqlx::query("UPDATE pages_deployments SET state = 'queued' WHERE state = 'building'").execute(&core.db).await {
        tracing::warn!(error = %e, "could not requeue pages copies");
    }
    loop {
        if let Err(e) = queue_from_events(&core).await {
            tracing::warn!(error = %e, "pages could not read the outbox");
        }
        loop {
            if stop.is_cancelled() {
                return;
            }
            match build_next(&core).await {
                Ok(true) => continue,
                Ok(false) => break,
                Err(e) => {
                    tracing::warn!(error = %e, "a pages copy could not be built");
                    break;
                }
            }
        }
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = core.wake.notified() => {}
            _ = tokio::time::sleep(IDLE) => {}
        }
    }
}

/// Pushes and new branches that land on a site's branch queue a copy of the new head.
pub async fn queue_from_events(core: &Core) -> Result<usize> {
    let mut queued = 0;
    loop {
        let batch = core.events.poll(CONSUMER, 100).await?;
        if batch.is_empty() {
            return Ok(queued);
        }
        for ev in &batch {
            let hit = match &ev.event {
                DomainEvent::Push { repo_id, git_ref, after, pusher, .. } => Some((*repo_id, git_ref, after, pusher.map(|u| u.0))),
                DomainEvent::RefCreated { repo_id, git_ref, sha, actor } => Some((*repo_id, git_ref, sha, actor.map(|u| u.0))),
                _ => None,
            };
            if let Some((repo, git_ref, sha, by)) = hit
                && let Some(branch) = git_ref.strip_prefix("refs/heads/")
                && sha.bytes().any(|b| b != b'0')
            {
                queued += queue_for_push(core, repo, branch, sha, by).await?;
            }
            core.events.ack(CONSUMER, ev.id).await?;
        }
    }
}

async fn queue_for_push(core: &Core, repo: RepoId, branch: &str, sha: &str, by: Option<i64>) -> Result<usize> {
    let done = sqlx::query(
        "INSERT INTO pages_deployments (repo_id, commit_sha, branch, folder, tier, state, created_by) \
         SELECT s.repo_id, $3, s.branch, s.folder, r.access_tier, 'queued', $4 \
         FROM pages_sites s JOIN repositories r ON r.id = s.repo_id WHERE s.repo_id = $1 AND s.branch = $2 \
           AND $3 ~ '^[0-9a-f]{40}([0-9a-f]{24})?$'",
    )
    .bind(repo.0)
    .bind(branch)
    .bind(sha)
    .bind(by)
    .execute(&core.db)
    .await?;
    Ok(done.rows_affected() as usize)
}

/// Builds the oldest queued copy. `false` when there was none.
pub async fn build_next(core: &Core) -> Result<bool> {
    let row: Option<(i64, i64, String, String)> = sqlx::query_as(
        "UPDATE pages_deployments SET state = 'building' WHERE id = ( \
            SELECT id FROM pages_deployments WHERE state = 'queued' ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED) \
         RETURNING id, repo_id, commit_sha, folder",
    )
    .fetch_optional(&core.db)
    .await?;
    let Some((id, repo, sha, folder)) = row else { return Ok(false) };
    let repo = RepoId(repo);
    match build(core, id, repo, &sha, &folder).await {
        Ok(()) => {}
        Err(reason) => {
            sqlx::query("UPDATE pages_deployments SET state = 'failed', error = $2, finished_at = now() WHERE id = $1")
                .bind(id)
                .bind(&reason)
                .execute(&core.db)
                .await?;
        }
    }
    prune(core, repo).await?;
    Ok(true)
}

/// Builds one copy. `Err` holds the reason in plain words, for the settings page.
async fn build(core: &Core, id: i64, repo: RepoId, sha: &str, folder: &str) -> std::result::Result<(), String> {
    let limits = WorkspaceSnapshotLimits { max_files: core.config.pages.max_files as usize, max_bytes: core.config.pages.max_bytes };
    let snap = match core.git.workspace_snapshot(repo, sha, folder.as_bytes(), limits).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, id, "pages snapshot failed");
            return Err("The commit could not be read. Push again or press Publish again.".into());
        }
    };
    let short = &sha[..7.min(sha.len())];
    let shown = if folder.is_empty() { "/".to_string() } else { format!("/{folder}") };
    if snap.too_large {
        return Err(format!(
            "Too big: {} files and {} in {shown} at {short}. The limit is {} files and {}.",
            snap.total_files,
            mib(snap.total_bytes),
            limits.max_files,
            mib(limits.max_bytes)
        ));
    }
    let strip = if folder.is_empty() { String::new() } else { format!("{folder}/") };
    let mut files: Vec<(String, ch_core::services::BlobRef, bool)> = Vec::with_capacity(snap.files.len());
    for f in snap.files {
        let Some(rel) = f.path.strip_prefix(&strip) else { continue };
        // git refuses these names, but a copy is checked anyway: a path is only ever looked up,
        // and these could never be asked for.
        if rel.is_empty() || rel.len() > 1024 || rel.split('/').any(|s| s.is_empty() || s == "." || s == "..") {
            continue;
        }
        let utf8 = std::str::from_utf8(&f.bytes).is_ok();
        let blob = core.blobs.put_bytes(bytes::Bytes::from(f.bytes)).await.map_err(|e| {
            tracing::warn!(error = %e, id, "pages blob write failed");
            "The server could not store the files. Try again later.".to_string()
        })?;
        files.push((rel.to_string(), blob, utf8));
    }
    if files.is_empty() {
        return Err(format!("No files in {shown} at {short}."));
    }
    let total: u64 = files.iter().map(|(_, b, _)| b.size).sum();
    let saved = async {
        let mut tx = core.db.begin().await?;
        for (path, blob, utf8) in &files {
            sqlx::query("INSERT INTO pages_files (deployment_id, path, blob_sha256, size, utf8) VALUES ($1, $2, $3, $4, $5)")
                .bind(id)
                .bind(path)
                .bind(&blob.sha256)
                .bind(blob.size as i64)
                .bind(utf8)
                .execute(&mut *tx)
                .await?;
            ch_db::blobs::add_ref(&mut tx, blob).await?;
        }
        sqlx::query("UPDATE pages_deployments SET state = 'ready', file_count = $2, total_bytes = $3, finished_at = now() WHERE id = $1")
            .bind(id)
            .bind(files.len() as i64)
            .bind(total as i64)
            .execute(&mut *tx)
            .await?;
        // The newest ready copy goes live; an older one finishing late never replaces it.
        sqlx::query("UPDATE pages_sites SET live_id = $2 WHERE repo_id = $1 AND (live_id IS NULL OR live_id < $2)")
            .bind(repo.0)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok::<(), ch_core::Error>(())
    };
    saved.await.map_err(|e| {
        tracing::warn!(error = %e, id, "pages copy could not be saved");
        "The server could not save the copy. Try again later.".to_string()
    })
}

/// Keeps the live copy, the newest ready ones and a few failed ones. Each file row deleted
/// releases its blob.
async fn prune(core: &Core, repo: RepoId) -> Result<()> {
    sqlx::query(
        "DELETE FROM pages_deployments d WHERE d.repo_id = $1 \
           AND d.id IS DISTINCT FROM (SELECT live_id FROM pages_sites WHERE repo_id = $1) \
           AND ((d.state = 'ready' AND d.id NOT IN (SELECT id FROM pages_deployments WHERE repo_id = $1 AND state = 'ready' ORDER BY id DESC LIMIT $2)) \
             OR (d.state = 'failed' AND d.id NOT IN (SELECT id FROM pages_deployments WHERE repo_id = $1 AND state = 'failed' ORDER BY id DESC LIMIT $3)))",
    )
    .bind(repo.0)
    .bind(KEEP_READY)
    .bind(KEEP_FAILED)
    .execute(&core.db)
    .await?;
    Ok(())
}

fn mib(b: u64) -> String {
    format!("{:.1} MB", b as f64 / (1024.0 * 1024.0))
}
