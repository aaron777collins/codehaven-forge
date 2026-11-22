//! Garbage collection (ARCHITECTURE.md section 10): abandoned uploads, untagged manifests and
//! blobs no manifest uses any more. Everything has a grace period, so a push in progress is
//! never collected.

use ch_core::state::AppState;
use serde_json::Value;
use std::collections::HashSet;
use std::time::Duration;

/// An upload with no activity for this long is abandoned.
pub const UPLOAD_GRACE: Duration = Duration::from_secs(24 * 3600);
/// An untagged manifest younger than this might be the middle of a multi-arch push.
pub const MANIFEST_GRACE: Duration = Duration::from_secs(7 * 24 * 3600);
/// A blob no manifest references, younger than this, might be waiting for its manifest.
pub const BLOB_GRACE: Duration = Duration::from_secs(24 * 3600);

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Swept {
    pub uploads: u64,
    pub manifests: u64,
    pub blobs: u64,
    pub stored_files: u64,
}

/// Digests a manifest's JSON points at: config, layers and child manifests.
pub fn references(content: &[u8]) -> Vec<String> {
    let Ok(v) = serde_json::from_slice::<Value>(content) else { return Vec::new() };
    let mut out = Vec::new();
    if let Some(d) = v.get("config").and_then(|c| c.get("digest")).and_then(Value::as_str) {
        out.push(d.to_string());
    }
    for field in ["layers", "manifests", "blobs"] {
        if let Some(arr) = v.get(field).and_then(Value::as_array) {
            out.extend(arr.iter().filter_map(|e| e.get("digest").and_then(Value::as_str)).map(str::to_string));
        }
    }
    out
}

pub async fn sweep(state: &AppState) -> Result<Swept, ch_core::Error> {
    let mut swept = Swept::default();

    // Uploads nobody finished.
    let stale: Vec<(uuid::Uuid, String)> =
        sqlx::query_as("DELETE FROM oci_uploads WHERE updated_at < now() - make_interval(secs => $1) RETURNING id, tmp_path")
            .bind(UPLOAD_GRACE.as_secs_f64())
            .fetch_all(&state.db)
            .await?;
    for (_, path) in &stale {
        let _ = tokio::fs::remove_file(path).await;
    }
    swept.uploads = stale.len() as u64;

    let packages: Vec<(i64,)> = sqlx::query_as("SELECT id FROM packages WHERE kind = 'container' ORDER BY id").fetch_all(&state.db).await?;
    for (pkg,) in packages {
        // Manifests with no tag, that no index names, and old enough.
        let manifests: Vec<(i64, String, Vec<u8>, bool, bool)> = sqlx::query_as(
            "SELECT m.id, m.digest, m.content, \
                    EXISTS (SELECT 1 FROM oci_tags t WHERE t.manifest_id = m.id) AS tagged, \
                    m.created_at < now() - make_interval(secs => $2) AS old \
             FROM oci_manifests m WHERE m.package_id = $1",
        )
        .bind(pkg)
        .bind(MANIFEST_GRACE.as_secs_f64())
        .fetch_all(&state.db)
        .await?;
        let named_by_an_index: HashSet<String> = manifests.iter().flat_map(|(_, _, c, ..)| references(c)).collect();
        for (id, digest, _, tagged, old) in &manifests {
            if !tagged && *old && !named_by_an_index.contains(digest) {
                sqlx::query("DELETE FROM oci_manifests WHERE id = $1").bind(id).execute(&state.db).await?;
                swept.manifests += 1;
            }
        }

        // Blobs no remaining manifest references, and old enough.
        let remaining: Vec<(Vec<u8>,)> =
            sqlx::query_as("SELECT content FROM oci_manifests WHERE package_id = $1").bind(pkg).fetch_all(&state.db).await?;
        let used: HashSet<String> = remaining.iter().flat_map(|(c,)| references(c)).collect();
        let held: Vec<(String, bool)> =
            sqlx::query_as("SELECT digest, created_at < now() - make_interval(secs => $2) FROM oci_blobs WHERE package_id = $1")
                .bind(pkg)
                .bind(BLOB_GRACE.as_secs_f64())
                .fetch_all(&state.db)
                .await?;
        for (digest, old) in held {
            if old && !used.contains(&digest) {
                let mut tx = state.db.begin().await?;
                let gone = sqlx::query("DELETE FROM oci_blobs WHERE package_id = $1 AND digest = $2")
                    .bind(pkg)
                    .bind(&digest)
                    .execute(&mut *tx)
                    .await?;
                if gone.rows_affected() == 1 {
                    ch_db::blobs::release(&mut tx, digest.strip_prefix("sha256:").unwrap_or(&digest)).await?;
                    swept.blobs += 1;
                }
                tx.commit().await?;
            }
        }
    }

    // Stored files nothing references any more, after the blob store's own grace period.
    // The store is shared with other features, so this only removes what has been unreferenced.
    let store = ch_db::FsBlobStore::new(state.config.blobs_dir());
    swept.stored_files = ch_db::blobs::collect_garbage(&state.db, &store, BLOB_GRACE).await?;
    Ok(swept)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_of_each_shape() {
        let image = br#"{"schemaVersion":2,"config":{"digest":"sha256:c"},"layers":[{"digest":"sha256:l1"},{"digest":"sha256:l2"}]}"#;
        assert_eq!(references(image), vec!["sha256:c", "sha256:l1", "sha256:l2"]);
        let index = br#"{"schemaVersion":2,"manifests":[{"digest":"sha256:m1"},{"digest":"sha256:m2"}]}"#;
        assert_eq!(references(index), vec!["sha256:m1", "sha256:m2"]);
        assert!(references(b"not json").is_empty());
        assert!(references(b"{}").is_empty());
    }
}
