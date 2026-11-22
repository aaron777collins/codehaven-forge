//! `upload-artifact` and `download-artifact`.
//!
//! upload inputs: `name` (default `artifact`), `path` (patterns), `retention-days`,
//! `if-no-files-found` (`warn` by default, `error` or `ignore`). The files go up as one zip,
//! with paths relative to the deepest directory they share, like GitHub's action.
//! download inputs: `name` (required), `path` (default: the workspace).

use super::{ActionCtx, ActionOutput, inside_workspace};
use crate::fsglob;
use anyhow::{Context, Result, anyhow, bail};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use zip::write::SimpleFileOptions;

/// Most bytes a download will unpack, and the most entries.
pub const MAX_UNZIP_BYTES: u64 = 10 * 1024 * 1024 * 1024;
pub const MAX_UNZIP_ENTRIES: usize = 200_000;

pub fn check_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 200 {
        bail!("the artifact name must be 1 to 200 characters");
    }
    if name == "." || name == ".." || name.chars().any(|c| matches!(c, '"' | ':' | '<' | '>' | '|' | '*' | '?' | '\r' | '\n' | '\\' | '/'))
    {
        bail!("the artifact name {name:?} has a character that is not allowed (one of \" : < > | * ? / \\ or a line break)");
    }
    Ok(())
}

pub async fn upload(ctx: &ActionCtx<'_>) -> Result<ActionOutput> {
    let name = ctx.input("name").unwrap_or("artifact").to_string();
    check_name(&name)?;
    let patterns = fsglob::split_patterns(ctx.input("path").unwrap_or(""));
    if patterns.is_empty() {
        bail!("upload-artifact needs a `path`");
    }
    let retention = match ctx.input("retention-days") {
        None => None,
        Some(d) => match d.trim().parse::<u32>() {
            Ok(n) if (1..=400).contains(&n) => Some(n),
            _ => {
                ctx.warn(format!("retention-days {d:?} is not a number from 1 to 400, so the server's default applies.")).await;
                None
            }
        },
    };
    let policy = ctx.input("if-no-files-found").unwrap_or("warn").to_ascii_lowercase();
    if !matches!(policy.as_str(), "warn" | "error" | "ignore") {
        bail!("if-no-files-found must be warn, error or ignore");
    }

    let ws = ctx.dirs.workspace.clone();
    let pats = patterns.clone();
    let files = tokio::task::spawn_blocking(move || fsglob::expand(&ws, &pats)).await.map_err(|e| anyhow!("{e}"))??;
    if files.is_empty() {
        let msg = format!("No files were found with the provided path: {}. No artifact will be uploaded.", patterns.join(", "));
        match policy.as_str() {
            "error" => bail!("{msg}"),
            "warn" => ctx.warn(msg).await,
            _ => ctx.info(msg).await,
        }
        return Ok(ActionOutput::default());
    }
    let root = fsglob::common_ancestor(&files);
    ctx.info(format!("With the provided path, there will be {} file(s) uploaded", files.len())).await;

    let zip_path = ctx.dirs.tmp.join(format!("artifact-{}.zip", rand::random::<u32>()));
    let (ws, zp, rt, fl) = (ctx.dirs.workspace.clone(), zip_path.clone(), root.clone(), files.clone());
    let size = tokio::task::spawn_blocking(move || zip_files(&ws, &rt, &fl, &zp)).await.map_err(|e| anyhow!("{e}"))??;
    let result = ctx.client.upload_artifact(&ctx.payload.token, ctx.payload.job_id, &name, &zip_path, retention).await;
    let _ = std::fs::remove_file(&zip_path);
    let up = result.map_err(|e| anyhow!("uploading the artifact {name:?} failed: {e}"))?;
    ctx.info(format!("Uploaded {name} ({size} bytes zipped, {} bytes stored)", up.size)).await;
    let mut out = ActionOutput::default();
    out.outputs.insert("artifact-id".into(), up.id.to_string());
    Ok(out)
}

/// Zips `files` (relative to `workspace`), naming entries relative to `root`. Returns the size.
pub fn zip_files(workspace: &Path, root: &Path, files: &[PathBuf], out: &Path) -> Result<u64> {
    let f = std::fs::File::create(out).with_context(|| format!("creating {}", out.display()))?;
    let mut z = zip::ZipWriter::new(f);
    for rel in files {
        let full = workspace.join(rel);
        let meta = match std::fs::symlink_metadata(&full) {
            Ok(m) if m.file_type().is_file() => m,
            _ => continue,
        };
        let entry = rel.strip_prefix(root).unwrap_or(rel).to_string_lossy().replace('\\', "/");
        let mut opts =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated).large_file(meta.len() >= 0xFFFF_FFFF);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            opts = opts.unix_permissions(meta.permissions().mode() & 0o777);
        }
        z.start_file(entry, opts)?;
        let mut src = std::fs::File::open(&full).with_context(|| format!("reading {}", full.display()))?;
        std::io::copy(&mut src, &mut z)?;
    }
    let mut f = z.finish()?;
    f.flush()?;
    Ok(std::fs::metadata(out)?.len())
}

pub async fn download(ctx: &ActionCtx<'_>) -> Result<ActionOutput> {
    let name = ctx.input("name").ok_or_else(|| anyhow!("download-artifact needs a `name`"))?.to_string();
    check_name(&name)?;
    let dest = inside_workspace(&ctx.dirs.workspace, ctx.input("path").unwrap_or("."))?;
    let zip_path = ctx.dirs.tmp.join(format!("download-{}.zip", rand::random::<u32>()));
    let got = ctx.client.download_artifact(&ctx.payload.token, ctx.payload.job_id, &name, &zip_path).await;
    if let Err(e) = got {
        let _ = std::fs::remove_file(&zip_path);
        return Err(anyhow!("downloading the artifact {name:?} failed: {e}"));
    }
    let (zp, d) = (zip_path.clone(), dest.clone());
    let n = tokio::task::spawn_blocking(move || unzip(&zp, &d)).await.map_err(|e| anyhow!("{e}"))?;
    let _ = std::fs::remove_file(&zip_path);
    let n = n?;
    ctx.info(format!("Downloaded {name}: {n} file(s) into {}", dest.display())).await;
    let mut out = ActionOutput::default();
    out.outputs.insert("download-path".into(), dest.to_string_lossy().into_owned());
    Ok(out)
}

/// Extracts a zip into `dest`. Entries that would land outside it, links, and anything past the
/// size and count limits are refused.
pub fn unzip(zip_path: &Path, dest: &Path) -> Result<usize> {
    let f = std::fs::File::open(zip_path)?;
    let mut z = zip::ZipArchive::new(f).context("the artifact is not a valid zip file")?;
    if z.len() > MAX_UNZIP_ENTRIES {
        bail!("the artifact has too many files");
    }
    std::fs::create_dir_all(dest)?;
    let mut total = 0u64;
    let mut written = 0u64;
    let mut files = 0;
    for i in 0..z.len() {
        let mut e = z.by_index(i)?;
        // `enclosed_name` is None for absolute paths and for any `..` that climbs out.
        let Some(rel) = e.enclosed_name() else {
            tracing::warn!("artifact entry {:?} is outside the target and was skipped", e.name());
            continue;
        };
        if e.is_symlink() {
            continue;
        }
        let target = dest.join(&rel);
        if e.is_dir() {
            std::fs::create_dir_all(&target)?;
            continue;
        }
        total = total.saturating_add(e.size());
        if total > MAX_UNZIP_BYTES {
            bail!("the artifact is too large to unpack");
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
            // A directory in the way that is really a link would send the write elsewhere.
            if std::fs::symlink_metadata(parent).map(|m| m.file_type().is_symlink()).unwrap_or(false) {
                bail!("{} is a symbolic link, so the artifact cannot be unpacked there", parent.display());
            }
        }
        if std::fs::symlink_metadata(&target).map(|m| m.file_type().is_symlink()).unwrap_or(false) {
            std::fs::remove_file(&target)?;
        }
        let mut out = std::fs::File::create(&target)?;
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = e.read(&mut buf)?;
            if n == 0 {
                break;
            }
            // The size in a zip header is only a claim. Count what is really written.
            written = written.saturating_add(n as u64);
            if written > MAX_UNZIP_BYTES {
                bail!("the artifact is too large to unpack");
            }
            out.write_all(&buf[..n])?;
        }
        #[cfg(unix)]
        if let Some(mode) = e.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode & 0o777));
        }
        files += 1;
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn names() {
        for ok in ["report", "my-report_v1.2", "a b"] {
            assert!(check_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", ".", "..", "a/b", "a\\b", "a:b", "a*", "q?", "x|y", "a<b", "n\nl", &"x".repeat(201)] {
            assert!(check_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn zip_uses_paths_relative_to_the_shared_directory() {
        let ws = tempfile::tempdir().unwrap();
        write(ws.path(), "target/report/a.txt", "A");
        write(ws.path(), "target/report/sub/b.txt", "B");
        let files = vec![PathBuf::from("target/report/a.txt"), PathBuf::from("target/report/sub/b.txt")];
        let root = fsglob::common_ancestor(&files);
        assert_eq!(root, PathBuf::from("target/report"));
        let out = ws.path().join("o.zip");
        let size = zip_files(ws.path(), &root, &files, &out).unwrap();
        assert!(size > 0);
        let mut z = zip::ZipArchive::new(std::fs::File::open(&out).unwrap()).unwrap();
        let mut names: Vec<_> = (0..z.len()).map(|i| z.by_index(i).unwrap().name().to_string()).collect();
        names.sort();
        assert_eq!(names, ["a.txt", "sub/b.txt"]);
    }

    #[test]
    fn zip_round_trips_through_unzip() {
        let ws = tempfile::tempdir().unwrap();
        write(ws.path(), "out/x.bin", "payload");
        write(ws.path(), "out/y/z.txt", "zed");
        let files = vec![PathBuf::from("out/x.bin"), PathBuf::from("out/y/z.txt")];
        let zip = ws.path().join("o.zip");
        zip_files(ws.path(), &fsglob::common_ancestor(&files), &files, &zip).unwrap();
        let dest = tempfile::tempdir().unwrap();
        assert_eq!(unzip(&zip, dest.path()).unwrap(), 2);
        assert_eq!(std::fs::read_to_string(dest.path().join("x.bin")).unwrap(), "payload");
        assert_eq!(std::fs::read_to_string(dest.path().join("y/z.txt")).unwrap(), "zed");
    }

    #[test]
    fn a_zip_slip_archive_cannot_write_outside() {
        let t = tempfile::tempdir().unwrap();
        let zp = t.path().join("evil.zip");
        {
            let mut z = zip::ZipWriter::new(std::fs::File::create(&zp).unwrap());
            let o = SimpleFileOptions::default();
            z.start_file("../escape.txt", o).unwrap();
            z.write_all(b"pwned").unwrap();
            z.start_file("/abs.txt", o).unwrap();
            z.write_all(b"pwned").unwrap();
            z.start_file("ok/fine.txt", o).unwrap();
            z.write_all(b"fine").unwrap();
            z.finish().unwrap();
        }
        let dest = t.path().join("dest");
        let n = unzip(&zp, &dest).unwrap();
        assert_eq!(n, 1);
        assert!(!t.path().join("escape.txt").exists());
        assert!(!Path::new("/abs.txt").exists());
        assert_eq!(std::fs::read_to_string(dest.join("ok/fine.txt")).unwrap(), "fine");
    }

    #[cfg(unix)]
    #[test]
    fn writing_through_a_symlinked_directory_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let dest = t.path().join("dest");
        let outside = t.path().join("outside");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, dest.join("d")).unwrap();
        let zp = t.path().join("a.zip");
        {
            let mut z = zip::ZipWriter::new(std::fs::File::create(&zp).unwrap());
            z.start_file("d/file.txt", SimpleFileOptions::default()).unwrap();
            z.write_all(b"x").unwrap();
            z.finish().unwrap();
        }
        assert!(unzip(&zp, &dest).is_err());
        assert!(!outside.join("file.txt").exists());
    }

    #[test]
    fn not_a_zip_is_an_error() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("x.zip");
        std::fs::write(&p, b"nope").unwrap();
        assert!(unzip(&p, t.path()).is_err());
    }
}
