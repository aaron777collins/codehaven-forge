//! `cache`, `cache/restore` and `cache/save`.
//!
//! Inputs: `key` (required), `restore-keys` (one per line, each matched as a prefix), `path`
//! (patterns, one per line, relative to the workspace). Restoring sets the outputs `cache-hit`
//! (`true` only for an exact match) and `cache-matched-key`. With plain `cache`, a job that did
//! not hit exactly saves the cache when it ends, if it succeeded.
//!
//! Caches are tar.gz archives of regular files, written once per key. The restore side is
//! careful about what the archive may contain, because it unpacks into a directory the job
//! controls and runs in the runner's own process.

use super::{ActionCtx, ActionOutput, CacheMode, PostAction};
use crate::fsglob;
use anyhow::{Context, Result, anyhow, bail};
use ch_api_types::runner::CacheLookupRequest;
use sha2::{Digest, Sha256};
use std::path::Path;

/// Largest cache the runner will save.
pub const MAX_SAVE_BYTES: u64 = 5 * 1024 * 1024 * 1024;
/// Most bytes and entries a restore will unpack.
pub const MAX_UNPACK_BYTES: u64 = 12 * 1024 * 1024 * 1024;
pub const MAX_UNPACK_ENTRIES: u64 = 2_000_000;

/// The cache "version": it changes when the cached paths do, so a cache of one set of paths is
/// never restored as another.
pub fn version_of(patterns: &[String]) -> String {
    let mut h = Sha256::new();
    for p in patterns {
        h.update(p.as_bytes());
        h.update(b"\n");
    }
    h.update(b"|tar.gz|v1");
    hex::encode(&h.finalize()[..16])
}

pub async fn run(ctx: &ActionCtx<'_>, mode: CacheMode) -> Result<ActionOutput> {
    let key = ctx.input("key").ok_or_else(|| anyhow!("the cache action needs a `key`"))?.trim().to_string();
    if key.len() > 512 || key.contains(',') {
        bail!("the cache key must be at most 512 characters and have no commas");
    }
    let patterns = fsglob::split_patterns(ctx.input("path").unwrap_or(""));
    if patterns.is_empty() {
        bail!("the cache action needs a `path`");
    }
    let version = version_of(&patterns);
    let mut out = ActionOutput::default();

    if mode == CacheMode::SaveOnly {
        save(ctx, &key, &version, &patterns).await?;
        return Ok(out);
    }

    let mut keys = vec![key.clone()];
    keys.extend(fsglob::split_patterns(ctx.input("restore-keys").unwrap_or("")));
    ctx.info(format!("Cache version {version}. Looking for {}", keys.join(", "))).await;
    let hit = ctx
        .client
        .cache_lookup(&ctx.payload.token, &CacheLookupRequest { keys, version: version.clone() })
        .await
        .map_err(|e| anyhow!("cache lookup failed: {e}"))?
        .hit;
    match hit {
        Some(h) => {
            let archive = ctx.dirs.private.join(format!("cache-{}.tar.gz", h.id));
            ctx.client.cache_download(&ctx.payload.token, h.id, &archive).await.map_err(|e| anyhow!("cache download failed: {e}"))?;
            let ws = ctx.dirs.workspace.clone();
            let a2 = archive.clone();
            let n = tokio::task::spawn_blocking(move || unpack(&a2, &ws)).await.map_err(|e| anyhow!("unpack task failed: {e}"))??;
            let _ = std::fs::remove_file(&archive);
            ctx.info(format!(
                "Restored {n} files from cache {}{}",
                h.key,
                if h.exact { "" } else { " (a restore key matched, not the exact key)" }
            ))
            .await;
            out.outputs.insert("cache-hit".into(), h.exact.to_string());
            out.outputs.insert("cache-matched-key".into(), h.key.clone());
            if !h.exact && mode == CacheMode::Both {
                out.post = Some(PostAction::CacheSave { key: key.clone(), version, patterns });
            }
        }
        None => {
            ctx.info("No cache found.").await;
            out.outputs.insert("cache-hit".into(), "false".into());
            if mode == CacheMode::Both {
                out.post = Some(PostAction::CacheSave { key: key.clone(), version, patterns });
            }
        }
    }
    out.outputs.insert("cache-primary-key".into(), key);
    Ok(out)
}

/// Builds the archive and uploads it. Failures are the caller's to report as warnings: a cache
/// that could not be saved must never fail a build.
pub async fn save(ctx: &ActionCtx<'_>, key: &str, version: &str, patterns: &[String]) -> Result<()> {
    let ws = ctx.dirs.workspace.clone();
    let pats = patterns.to_vec();
    let files = tokio::task::spawn_blocking(move || fsglob::expand(&ws, &pats)).await.map_err(|e| anyhow!("{e}"))??;
    if files.is_empty() {
        ctx.warn("Path validation: nothing matched the cache paths, so no cache is being saved.").await;
        return Ok(());
    }
    let archive = ctx.dirs.private.join(format!("save-{}.tar.gz", rand::random::<u32>()));
    let (ws, a2) = (ctx.dirs.workspace.clone(), archive.clone());
    let (count, size) = tokio::task::spawn_blocking(move || pack(&ws, &files, &a2)).await.map_err(|e| anyhow!("{e}"))??;
    let result = async {
        if size > MAX_SAVE_BYTES {
            ctx.warn(format!("The cache is {size} bytes, over the {MAX_SAVE_BYTES} byte limit, so it was not saved.")).await;
            return Ok(());
        }
        match ctx.client.cache_save(&ctx.payload.token, key, version, &archive).await {
            Ok(true) => ctx.info(format!("Saved {count} files ({size} bytes) as cache {key}")).await,
            Ok(false) => ctx.info(format!("Cache {key} already exists, so it was not saved again.")).await,
            Err(e) => bail!("cache save failed: {e}"),
        }
        Ok(())
    }
    .await;
    let _ = std::fs::remove_file(&archive);
    result
}

/// Writes the files (paths relative to `root`) into a gzip-compressed tar. Returns the file
/// count and the archive size.
pub fn pack(root: &Path, files: &[std::path::PathBuf], archive: &Path) -> Result<(usize, u64)> {
    let f = std::fs::File::create(archive).with_context(|| format!("creating {}", archive.display()))?;
    let gz = flate2::write::GzEncoder::new(f, flate2::Compression::fast());
    let mut tar = tar::Builder::new(gz);
    tar.follow_symlinks(false);
    let mut n = 0;
    for rel in files {
        let full = root.join(rel);
        // A file that vanished or cannot be read is left out, not fatal. It is opened without
        // following any link, so nothing outside the workspace lands in the cache (ch-ctk.1).
        let added = crate::safefs::open_regular(root, &full).and_then(|mut f| tar.append_file(rel, &mut f));
        match added {
            Ok(()) => n += 1,
            Err(e) => tracing::debug!("cache: skipping {}: {e}", rel.display()),
        }
    }
    let gz = tar.into_inner()?;
    gz.finish()?.sync_all()?;
    Ok((n, std::fs::metadata(archive)?.len()))
}

/// Unpacks a cache archive into `dest`. Only regular files and directories are written; links,
/// devices, absolute paths and `..` are skipped, and setuid bits are dropped.
pub fn unpack(archive: &Path, dest: &Path) -> Result<u64> {
    let f = std::fs::File::open(archive).with_context(|| format!("opening {}", archive.display()))?;
    let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(f));
    ar.set_preserve_permissions(true);
    ar.set_preserve_mtime(true);
    ar.set_unpack_xattrs(false);
    ar.set_overwrite(true);
    ar.set_mask(0o7000);
    let mut files = 0u64;
    let mut entries = 0u64;
    let mut bytes = 0u64;
    for entry in ar.entries().context("the cache archive is not readable")? {
        let mut entry = entry.context("the cache archive is damaged")?;
        entries += 1;
        if entries > MAX_UNPACK_ENTRIES {
            bail!("the cache archive has too many entries");
        }
        let kind = entry.header().entry_type();
        if !(kind.is_file() || kind.is_dir()) {
            continue;
        }
        bytes = bytes.saturating_add(entry.header().size().unwrap_or(0));
        if bytes > MAX_UNPACK_BYTES {
            bail!("the cache archive is too large to unpack");
        }
        // `unpack_in` returns false for a path that would land outside `dest`.
        if entry.unpack_in(dest)? && kind.is_file() {
            files += 1;
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn version_depends_on_the_paths() {
        let a = version_of(&["target".into()]);
        assert_eq!(a, version_of(&["target".into()]));
        assert_ne!(a, version_of(&["target".into(), "~/.cargo".into()]));
        assert_eq!(a.len(), 32);
    }

    #[test]
    fn pack_and_unpack_round_trip() {
        let src = tempfile::tempdir().unwrap();
        write(src.path(), "a.txt", "one");
        write(src.path(), "d/e/f.txt", "two");
        let out = tempfile::tempdir().unwrap();
        let archive = out.path().join("c.tar.gz");
        let (n, size) = pack(src.path(), &[PathBuf::from("a.txt"), PathBuf::from("d/e/f.txt")], &archive).unwrap();
        assert_eq!((n, size > 0), (2, true));
        let dest = tempfile::tempdir().unwrap();
        assert_eq!(unpack(&archive, dest.path()).unwrap(), 2);
        assert_eq!(std::fs::read_to_string(dest.path().join("a.txt")).unwrap(), "one");
        assert_eq!(std::fs::read_to_string(dest.path().join("d/e/f.txt")).unwrap(), "two");
    }

    #[cfg(unix)]
    #[test]
    fn executable_bits_survive_and_setuid_does_not() {
        use std::os::unix::fs::PermissionsExt;
        let src = tempfile::tempdir().unwrap();
        write(src.path(), "run.sh", "#!/bin/sh");
        std::fs::set_permissions(src.path().join("run.sh"), std::fs::Permissions::from_mode(0o4755)).unwrap();
        let archive = src.path().join("c.tar.gz");
        pack(src.path(), &[PathBuf::from("run.sh")], &archive).unwrap();
        let dest = tempfile::tempdir().unwrap();
        unpack(&archive, dest.path()).unwrap();
        let mode = std::fs::metadata(dest.path().join("run.sh")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        assert_eq!(mode & 0o7000, 0, "setuid is dropped");
    }

    /// Builds a tar with whatever entries the test wants, bypassing the safety of `Builder`.
    fn evil_archive(path: &Path, entries: &[(&str, tar::EntryType, &str, &str)]) {
        let f = std::fs::File::create(path).unwrap();
        let gz = flate2::write::GzEncoder::new(f, flate2::Compression::fast());
        let mut b = tar::Builder::new(gz);
        for (name, kind, link, body) in entries {
            let mut h = tar::Header::new_gnu();
            {
                let gnu = h.as_gnu_mut().unwrap();
                let bytes = name.as_bytes();
                gnu.name[..bytes.len()].copy_from_slice(bytes);
            }
            h.set_entry_type(*kind);
            h.set_size(body.len() as u64);
            h.set_mode(0o644);
            if !link.is_empty() {
                h.set_link_name(link).unwrap();
            }
            h.set_cksum();
            b.append(&h, body.as_bytes()).unwrap();
        }
        b.into_inner().unwrap().finish().unwrap();
    }

    #[test]
    fn a_hostile_archive_cannot_write_outside_the_workspace() {
        let t = tempfile::tempdir().unwrap();
        let ws = t.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let archive = t.path().join("evil.tar.gz");
        evil_archive(
            &archive,
            &[
                ("../escape.txt", tar::EntryType::Regular, "", "pwned"),
                ("/abs-escape.txt", tar::EntryType::Regular, "", "pwned"),
                ("ok.txt", tar::EntryType::Regular, "", "fine"),
                ("link", tar::EntryType::Symlink, "../outside", ""),
                ("link/through.txt", tar::EntryType::Regular, "", "pwned"),
                ("hard", tar::EntryType::Link, "/etc/passwd", ""),
            ],
        );
        let r = unpack(&archive, &ws);
        // Whether it stops with an error or skips the bad entries, nothing may land outside.
        let _ = r;
        assert!(!t.path().join("escape.txt").exists(), "../ entry escaped");
        assert!(!Path::new("/abs-escape.txt").exists(), "absolute entry escaped");
        assert!(!t.path().join("outside").exists());
        assert!(!t.path().join("through.txt").exists());
        assert_eq!(std::fs::read_to_string(ws.join("ok.txt")).unwrap(), "fine");
        assert!(!ws.join("link").is_symlink(), "symlinks are not restored");
    }

    #[test]
    fn a_damaged_archive_is_an_error() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("bad.tar.gz");
        std::fs::write(&p, b"this is not gzip").unwrap();
        assert!(unpack(&p, t.path()).is_err());
        assert!(unpack(&t.path().join("missing"), t.path()).is_err());
    }
}
