//! Content-addressed blob store on the filesystem: `data/blobs/sha256/<aa>/<bb>/<hash>`.
//!
//! Writes go through a temp file and a rename, so a reader never sees half a blob.
//! Reference counts live in the `blobs` table; `add_ref` and `release` keep them and
//! `collect_garbage` deletes blobs unreferenced for more than a day (ARCHITECTURE.md 5.2).

use async_trait::async_trait;
use bytes::Bytes;
use ch_core::services::{BlobRef, BlobStore, DbTx};
use ch_core::{Error, Result};
use futures::StreamExt;
use futures::stream::BoxStream;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;

#[derive(Clone, Debug)]
pub struct FsBlobStore {
    root: PathBuf,
}

/// Only lowercase hex of the right length reaches the filesystem, so a digest can never be
/// a path.
fn valid_digest(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

impl FsBlobStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub async fn init(&self) -> Result<()> {
        tokio::fs::create_dir_all(self.root.join("tmp")).await.map_err(Error::internal)?;
        tokio::fs::create_dir_all(self.root.join("sha256")).await.map_err(Error::internal)?;
        Ok(())
    }

    fn path_for(&self, digest: &str) -> Result<PathBuf> {
        if !valid_digest(digest) {
            return Err(Error::invalid("Blob", "sha256", "not a sha256 digest"));
        }
        Ok(self.root.join("sha256").join(&digest[0..2]).join(&digest[2..4]).join(digest))
    }
}

#[async_trait]
impl BlobStore for FsBlobStore {
    async fn put_stream(&self, mut body: BoxStream<'static, std::io::Result<Bytes>>, max_bytes: u64) -> Result<BlobRef> {
        let tmp_dir = self.root.join("tmp");
        tokio::fs::create_dir_all(&tmp_dir).await.map_err(Error::internal)?;
        let tmp = tmp_dir.join(uuid::Uuid::new_v4().to_string());
        let mut file = tokio::fs::File::create(&tmp).await.map_err(Error::internal)?;
        let mut hasher = Sha256::new();
        let mut size: u64 = 0;
        let result: Result<()> = async {
            while let Some(chunk) = body.next().await {
                let chunk = chunk.map_err(Error::internal)?;
                size += chunk.len() as u64;
                if size > max_bytes {
                    return Err(Error::invalid("Blob", "size", format!("larger than the {max_bytes} byte limit")));
                }
                hasher.update(&chunk);
                file.write_all(&chunk).await.map_err(Error::internal)?;
            }
            file.flush().await.map_err(Error::internal)?;
            file.sync_all().await.map_err(Error::internal)?;
            Ok(())
        }
        .await;
        drop(file);
        if let Err(e) = result {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(e);
        }
        let digest = hex::encode(hasher.finalize());
        let dest = self.path_for(&digest)?;
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(Error::internal)?;
        }
        if tokio::fs::try_exists(&dest).await.unwrap_or(false) {
            // Same content already stored.
            let _ = tokio::fs::remove_file(&tmp).await;
        } else {
            tokio::fs::rename(&tmp, &dest).await.map_err(Error::internal)?;
        }
        Ok(BlobRef { sha256: digest, size })
    }

    async fn put_bytes(&self, data: Bytes) -> Result<BlobRef> {
        let len = data.len() as u64;
        self.put_stream(futures::stream::once(async move { Ok(data) }).boxed(), len.max(1)).await
    }

    async fn get_stream(&self, sha256: &str) -> Result<BoxStream<'static, std::io::Result<Bytes>>> {
        let path = self.path_for(sha256)?;
        match tokio::fs::File::open(&path).await {
            Ok(f) => Ok(ReaderStream::new(f).boxed()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(Error::NotFound),
            Err(e) => Err(Error::internal(e)),
        }
    }

    async fn size(&self, sha256: &str) -> Result<Option<u64>> {
        let path = self.path_for(sha256)?;
        match tokio::fs::metadata(&path).await {
            Ok(m) => Ok(Some(m.len())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::internal(e)),
        }
    }

    async fn delete(&self, sha256: &str) -> Result<()> {
        let path = self.path_for(sha256)?;
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::internal(e)),
        }
    }
}

/// Adds a reference to a blob inside the caller's transaction.
pub async fn add_ref(tx: &mut DbTx<'_>, blob: &BlobRef) -> Result<()> {
    sqlx::query!(
        "INSERT INTO blobs (sha256, size, refcount) VALUES ($1, $2, 1)
         ON CONFLICT (sha256) DO UPDATE SET refcount = blobs.refcount + 1, unreferenced_since = NULL",
        blob.sha256,
        blob.size as i64,
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Drops a reference. At zero the blob starts its 24 hour grace period.
pub async fn release(tx: &mut DbTx<'_>, sha256: &str) -> Result<()> {
    sqlx::query!(
        "UPDATE blobs SET refcount = GREATEST(refcount - 1, 0),
                unreferenced_since = CASE WHEN refcount <= 1 THEN now() ELSE unreferenced_since END
         WHERE sha256 = $1",
        sha256,
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Deletes blobs that have had no references for longer than `grace`. Returns how many.
pub async fn collect_garbage(pool: &PgPool, store: &FsBlobStore, grace: std::time::Duration) -> Result<u64> {
    let rows = sqlx::query_scalar!(
        "DELETE FROM blobs WHERE refcount = 0 AND unreferenced_since < now() - make_interval(secs => $1) RETURNING sha256",
        grace.as_secs_f64(),
    )
    .fetch_all(pool)
    .await?;
    let n = rows.len() as u64;
    for sha in rows {
        store.delete(&sha).await?;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::TryStreamExt;

    async fn store() -> (tempfile::TempDir, FsBlobStore) {
        let dir = tempfile::tempdir().unwrap();
        let s = FsBlobStore::new(dir.path());
        s.init().await.unwrap();
        (dir, s)
    }

    async fn read_all(s: &FsBlobStore, sha: &str) -> Vec<u8> {
        let chunks: Vec<Bytes> = s.get_stream(sha).await.unwrap().try_collect().await.unwrap();
        chunks.concat()
    }

    #[tokio::test]
    async fn put_get_round_trip_with_known_digest() {
        let (_d, s) = store().await;
        let r = s.put_bytes(Bytes::from_static(b"abc")).await.unwrap();
        assert_eq!(r.sha256, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(r.size, 3);
        assert_eq!(read_all(&s, &r.sha256).await, b"abc");
        assert_eq!(s.size(&r.sha256).await.unwrap(), Some(3));
    }

    #[tokio::test]
    async fn layout_is_sharded_by_digest() {
        let (d, s) = store().await;
        let r = s.put_bytes(Bytes::from_static(b"abc")).await.unwrap();
        assert!(d.path().join("sha256/ba/78").join(&r.sha256).is_file());
    }

    #[tokio::test]
    async fn streams_in_chunks_and_dedupes() {
        let (_d, s) = store().await;
        let chunks: Vec<std::io::Result<Bytes>> = vec![Ok(Bytes::from_static(b"ab")), Ok(Bytes::from_static(b"c"))];
        let r = s.put_stream(futures::stream::iter(chunks).boxed(), 100).await.unwrap();
        let again = s.put_bytes(Bytes::from_static(b"abc")).await.unwrap();
        assert_eq!(r, again);
        // No temp files left behind.
        let mut rd = tokio::fs::read_dir(s.root().join("tmp")).await.unwrap();
        assert!(rd.next_entry().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn oversized_upload_is_refused_and_cleaned_up() {
        let (_d, s) = store().await;
        let chunks: Vec<std::io::Result<Bytes>> = vec![Ok(Bytes::from(vec![0u8; 10])), Ok(Bytes::from(vec![0u8; 10]))];
        let err = s.put_stream(futures::stream::iter(chunks).boxed(), 15).await.unwrap_err();
        assert!(matches!(err, Error::Validation(_)));
        let mut rd = tokio::fs::read_dir(s.root().join("tmp")).await.unwrap();
        assert!(rd.next_entry().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn failed_stream_leaves_nothing() {
        let (_d, s) = store().await;
        let chunks: Vec<std::io::Result<Bytes>> =
            vec![Ok(Bytes::from_static(b"x")), Err(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset"))];
        assert!(s.put_stream(futures::stream::iter(chunks).boxed(), 100).await.is_err());
        let mut rd = tokio::fs::read_dir(s.root().join("tmp")).await.unwrap();
        assert!(rd.next_entry().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn digests_that_look_like_paths_are_refused() {
        let (_d, s) = store().await;
        for bad in ["../../etc/passwd", "", "ABC", &"g".repeat(64), &"A".repeat(64), &"a".repeat(63)] {
            assert!(s.get_stream(bad).await.is_err(), "{bad:?}");
            assert!(s.size(bad).await.is_err());
            assert!(s.delete(bad).await.is_err());
        }
    }

    #[tokio::test]
    async fn missing_blob_is_not_found_and_delete_is_idempotent() {
        let (_d, s) = store().await;
        let missing = "a".repeat(64);
        assert!(matches!(s.get_stream(&missing).await, Err(Error::NotFound)));
        assert_eq!(s.size(&missing).await.unwrap(), None);
        s.delete(&missing).await.unwrap();
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn refcounts_and_garbage_collection(pool: PgPool) {
        let (_d, s) = store().await;
        let r = s.put_bytes(Bytes::from_static(b"payload")).await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        add_ref(&mut tx, &r).await.unwrap();
        add_ref(&mut tx, &r).await.unwrap();
        tx.commit().await.unwrap();

        let mut tx = pool.begin().await.unwrap();
        release(&mut tx, &r.sha256).await.unwrap();
        tx.commit().await.unwrap();
        // Still one reference: never collected.
        assert_eq!(collect_garbage(&pool, &s, std::time::Duration::ZERO).await.unwrap(), 0);
        assert!(s.size(&r.sha256).await.unwrap().is_some());

        let mut tx = pool.begin().await.unwrap();
        release(&mut tx, &r.sha256).await.unwrap();
        tx.commit().await.unwrap();
        // Unreferenced but inside the grace period.
        assert_eq!(collect_garbage(&pool, &s, std::time::Duration::from_secs(3600)).await.unwrap(), 0);
        assert!(s.size(&r.sha256).await.unwrap().is_some());
        // Past the grace period.
        assert_eq!(collect_garbage(&pool, &s, std::time::Duration::ZERO).await.unwrap(), 1);
        assert_eq!(s.size(&r.sha256).await.unwrap(), None);
    }

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn re_referencing_cancels_the_grace_period(pool: PgPool) {
        let (_d, s) = store().await;
        let r = s.put_bytes(Bytes::from_static(b"again")).await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        add_ref(&mut tx, &r).await.unwrap();
        release(&mut tx, &r.sha256).await.unwrap();
        add_ref(&mut tx, &r).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(collect_garbage(&pool, &s, std::time::Duration::ZERO).await.unwrap(), 0);
    }
}
