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
            let (root, ws) = (ctx.dirs.root.clone(), ctx.dirs.workspace.clone());
            let a2 = archive.clone();
            let n = tokio::task::spawn_blocking(move || unpack(&a2, &root, &ws)).await.map_err(|e| anyhow!("unpack task failed: {e}"));
            let _ = std::fs::remove_file(&archive);
            let n = n??;
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
        let added = crate::safefs::open_regular(root, &full).and_then(|f| {
            // The header's size is taken once, and exactly that many bytes follow it. A file a
            // job process is still writing cannot add bytes that a restore would read as more
            // entries, and one that shrinks is padded (ch-3xn).
            let meta = f.metadata()?;
            let mut h = tar::Header::new_gnu();
            h.set_metadata(&meta);
            tar.append_data(&mut h, rel, exactly(f, meta.len()))
        });
        match added {
            Ok(()) => n += 1,
            Err(e) => tracing::debug!("cache: skipping {}: {e}", rel.display()),
        }
    }
    let gz = tar.into_inner()?;
    gz.finish()?.sync_all()?;
    Ok((n, std::fs::metadata(archive)?.len()))
}

/// Exactly `len` bytes of `r`: cut if it holds more, padded with zeros if it holds fewer.
fn exactly(r: impl std::io::Read, len: u64) -> impl std::io::Read {
    use std::io::Read;
    r.take(len).chain(std::io::repeat(0)).take(len)
}

/// Unpacks a cache archive into `dest`, which lies inside `root` (the job root). Only regular
/// files and directories are written; links, devices, absolute paths and `..` are skipped, and
/// setuid bits are dropped. Every directory and file is made through `safefs` from a handle on
/// `root`, so a link the job planted anywhere between `root` and an entry, `dest` included, is
/// an error, never a way out (ch-ctk.1). tar's own `unpack_in` is not used: it resolves `dest`
/// through links and creates and opens files by path.
pub fn unpack(archive: &Path, root: &Path, dest: &Path) -> Result<u64> {
    use std::io::{Read, Write};
    let f = std::fs::File::open(archive).with_context(|| format!("opening {}", archive.display()))?;
    let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(f));
    let refused = |p: &Path, e: std::io::Error| anyhow!("cannot restore the cache at {}: {e}", p.display());
    crate::safefs::create_dir_all(root, dest, 0o755).map_err(|e| refused(dest, e))?;
    let mut files = 0u64;
    let mut entries = 0u64;
    let mut bytes = 0u64;
    let mut buf = vec![0u8; 64 * 1024];
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
        let Some(rel) = entry_path(&entry) else {
            tracing::warn!("cache entry {:?} is outside the workspace and was skipped", String::from_utf8_lossy(&entry.path_bytes()));
            continue;
        };
        let target = dest.join(&rel);
        if kind.is_dir() {
            crate::safefs::create_dir_all(root, &target, 0o755).map_err(|e| refused(&target, e))?;
            continue;
        }
        if let Some(parent) = target.parent() {
            crate::safefs::create_dir_all(root, parent, 0o755).map_err(|e| refused(parent, e))?;
        }
        // Permissions as packed, without setuid, setgid or sticky.
        let mode = entry.header().mode().unwrap_or(0o644) & 0o777;
        let mtime = entry.header().mtime().ok();
        // Whatever the job left at this name, a link included, is replaced, never written through.
        let mut out = crate::safefs::create_new(root, &target, mode).map_err(|e| refused(&target, e))?;
        loop {
            let n = entry.read(&mut buf).context("the cache archive is damaged")?;
            if n == 0 {
                break;
            }
            // Count what is really written, not what a header claims.
            bytes = bytes.saturating_add(n as u64);
            if bytes > MAX_UNPACK_BYTES {
                bail!("the cache archive is too large to unpack");
            }
            out.write_all(&buf[..n])?;
        }
        out.flush()?;
        // Times are set on the open file, never by path.
        if let Some(t) = mtime {
            let _ = out.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(t));
        }
        files += 1;
    }
    Ok(files)
}

/// An entry's path as plain names, or `None` when it is absolute, climbs with `..` or is empty.
fn entry_path<R: std::io::Read>(entry: &tar::Entry<'_, R>) -> Option<std::path::PathBuf> {
    use std::path::Component;
    let p = entry.path().ok()?;
    let mut out = std::path::PathBuf::new();
    for c in p.components() {
        match c {
            Component::Normal(n) => out.push(n),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_file_adds_exactly_the_bytes_its_header_says() {
        use std::io::Read;
        let mut out = Vec::new();
        exactly(&b"grown past the header"[..], 5).read_to_end(&mut out).unwrap();
        assert_eq!(out, b"grown");
        out.clear();
        exactly(&b"ab"[..], 4).read_to_end(&mut out).unwrap();
        assert_eq!(out, b"ab\0\0");
    }

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
        assert_eq!(unpack(&archive, dest.path(), dest.path()).unwrap(), 2);
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
        unpack(&archive, dest.path(), dest.path()).unwrap();
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
        let r = unpack(&archive, &ws, &ws);
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
        assert!(unpack(&p, t.path(), t.path()).is_err());
        assert!(unpack(&t.path().join("missing"), t.path(), t.path()).is_err());
    }

    fn outside_is_untouched(outside: &Path) {
        let left: Vec<_> = std::fs::read_dir(outside).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left, vec![std::ffi::OsString::from("credential")], "something was written outside the job directory");
        assert_eq!(std::fs::read_to_string(outside.join("credential")).unwrap(), "secret");
    }

    /// The ch-ctk.1 reproduction: the job swaps its workspace for a link to the runner's home and
    /// restores a cache. Nothing may land there.
    #[cfg(unix)]
    #[test]
    fn a_workspace_swapped_for_a_link_is_refused_not_followed() {
        let t = tempfile::tempdir().unwrap();
        let (root, outside) = (t.path().join("job"), t.path().join("home"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("credential"), "secret").unwrap();
        let src = t.path().join("src");
        write(&src, "pwned.txt", "x");
        write(&src, "credential", "replaced");
        let archive = t.path().join("c.tar.gz");
        pack(&src, &[PathBuf::from("pwned.txt"), PathBuf::from("credential")], &archive).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("workspace")).unwrap();
        assert!(unpack(&archive, &root, &root.join("workspace")).is_err());
        outside_is_untouched(&outside);
    }

    /// A directory inside the workspace that is a link, there before the restore or swapped in
    /// while it runs, is refused for every entry below it.
    #[cfg(unix)]
    #[test]
    fn a_linked_directory_inside_the_workspace_is_never_followed() {
        let t = tempfile::tempdir().unwrap();
        let (root, outside) = (t.path().join("job"), t.path().join("home"));
        let ws = root.join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("credential"), "secret").unwrap();
        let src = t.path().join("src");
        let mut rels = Vec::new();
        for i in 0..400 {
            write(&src, &format!("d/f{i}"), "x");
            rels.push(PathBuf::from(format!("d/f{i}")));
        }
        write(&src, "d/credential", "replaced");
        rels.push(PathBuf::from("d/credential"));
        let archive = t.path().join("c.tar.gz");
        pack(&src, &rels, &archive).unwrap();
        // Before the restore.
        std::os::unix::fs::symlink(&outside, ws.join("d")).unwrap();
        assert!(unpack(&archive, &root, &ws).is_err());
        outside_is_untouched(&outside);
        // During it: another thread keeps swapping `d` between a directory and the link.
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let swapper = {
            let (stop, d, outside) = (stop.clone(), ws.join("d"), outside.clone());
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = std::fs::remove_file(&d);
                    let _ = std::fs::create_dir(&d);
                    std::thread::yield_now();
                    let aside = d.with_extension("old");
                    let _ = std::fs::remove_dir_all(&aside);
                    let _ = std::fs::rename(&d, &aside);
                    let _ = std::os::unix::fs::symlink(&outside, &d);
                }
            })
        };
        for _ in 0..30 {
            let _ = unpack(&archive, &root, &ws);
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        swapper.join().unwrap();
        outside_is_untouched(&outside);
    }

    #[test]
    fn modification_times_are_restored() {
        let src = tempfile::tempdir().unwrap();
        write(src.path(), "a.txt", "one");
        let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
        std::fs::File::options().write(true).open(src.path().join("a.txt")).unwrap().set_modified(when).unwrap();
        let archive = src.path().join("c.tar.gz");
        pack(src.path(), &[PathBuf::from("a.txt")], &archive).unwrap();
        let dest = tempfile::tempdir().unwrap();
        unpack(&archive, dest.path(), dest.path()).unwrap();
        assert_eq!(std::fs::metadata(dest.path().join("a.txt")).unwrap().modified().unwrap(), when);
    }
}
