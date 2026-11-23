//! Capped git reads for the browser workspace (spec sections 5 and 8): the branch head by
//! exact name, ancestry, a recursive tree, one blob by raw path bytes, literal search, and
//! the direct-commit rule.
//!
//! Two properties the ordinary read paths of `read.rs` do not need. A path travels as raw
//! bytes, so a file whose name has a colon or bytes that are not UTF-8 is still one file:
//! every command that takes a path gets `GIT_LITERAL_PATHSPECS=1` and `--`, so no pathspec
//! syntax (globs, magic prefixes) is ever interpreted. And a listing is streamed with a byte
//! cap while a process permit is held, so a huge tree or a slow grep stops early and is
//! marked cut, rather than buffered whole.

use crate::command::{GitError, check_rev};
use crate::core::GitCore;
use base64::Engine;
use ch_core::model::*;
use ch_core::{Actor, Error, Permission, RepoId, Result};
use std::os::unix::ffi::OsStrExt;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

/// A full object id: 40 (SHA-1) or 64 (SHA-2) lower-case hex characters. Anything else is
/// not a commit the caller pinned, so it never reaches git at all.
fn full_id(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The raw path bytes of one `ls-tree -z` record: everything after the TAB. `None` when the
/// record has no TAB, so it is not a record we asked for.
fn record_path(rec: &[u8]) -> Option<&[u8]> {
    let tab = rec.iter().position(|b| *b == b'\t')?;
    Some(&rec[tab + 1..])
}

/// One `ls-tree -z -l` record as a workspace entry: `<mode> SP <type> SP <sha> SP <size or
/// "-">TAB<path bytes>`. The metadata before the TAB is ASCII; the path after it is raw bytes.
fn parse_ls_tree(rec: &[u8]) -> Option<WorkspaceEntry> {
    let tab = rec.iter().position(|b| *b == b'\t')?;
    let meta = std::str::from_utf8(&rec[..tab]).ok()?;
    let path = &rec[tab + 1..];
    let mut f = meta.split_whitespace();
    let (Some(mode), Some(kind), Some(sha), Some(size)) = (f.next(), f.next(), f.next(), f.next()) else { return None };
    Some(WorkspaceEntry {
        path_b64: base64::engine::general_purpose::STANDARD.encode(path),
        path: String::from_utf8(path.to_vec()).ok(),
        mode: mode.to_string(),
        kind: kind.to_string(),
        sha: sha.to_string(),
        size: if size == "-" { None } else { size.parse().ok() },
    })
}

impl GitCore {
    /// The commit `refs/heads/<branch>` points at, by exact ref name. `for-each-ref` matches
    /// patterns by prefix, so a tag or a longer branch can appear in the output: only a line
    /// whose refname is the full ref, byte for byte, answers.
    pub(crate) async fn ws_branch_head(&self, repo: RepoId, branch: &str) -> Result<Option<String>> {
        let full = format!("refs/heads/{branch}");
        check_rev(&full)?;
        let out = match self.exec(self.git(repo).args(["for-each-ref", "--format=%(objectname)%00%(refname)", &full])).await {
            Ok(o) => o,
            // A repository with no directory on disk yet has no branches either.
            Err(_) if !self.store.exists(repo) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.split_once('\0'))
            .find(|(_, name)| *name == full)
            .map(|(sha, _)| sha.to_string()))
    }

    /// True when `sha` is a full commit id that is `head` or an ancestor of it. A short id, an
    /// unknown object and anything that is not a commit are plain false, never an error, so
    /// the caller learns nothing beyond the answer.
    pub(crate) async fn ws_is_ancestor(&self, repo: RepoId, sha: &str, head: &str) -> Result<bool> {
        if !full_id(sha) || !full_id(head) {
            return Ok(false);
        }
        let ty = self.exec_any(self.git(repo).args(["cat-file", "-t", sha])).await?;
        if ty.code != 0 || ty.stdout_str().trim() != "commit" {
            return Ok(false);
        }
        let out = self.exec_any(self.git(repo).args(["merge-base", "--is-ancestor", sha, head])).await?;
        Ok(out.code == 0)
    }

    /// Every entry of the tree at `sha`, recursively, folders included. Output is read in
    /// chunks and cut at either cap, so a giant tree costs one killed process, not a buffer.
    pub(crate) async fn ws_tree(&self, repo: RepoId, sha: &str, limits: WorkspaceTreeLimits) -> Result<WorkspaceTree> {
        if !full_id(sha) {
            return Err(Error::NotFound);
        }
        let _permit = self.procs.acquire().await.map_err(|_| GitError::Spawn(std::io::Error::other("shutting down")))?;
        let mut child = self.git(repo).args(["ls-tree", "-r", "-t", "-z", "-l", "--full-tree", sha]).spawn()?;
        let mut stdout = match child.stdout.take() {
            Some(o) => o,
            None => {
                let _ = child.kill().await;
                return Err(Error::internal(anyhow::anyhow!("git ls-tree produced no stdout")));
            }
        };
        let mut entries: Vec<WorkspaceEntry> = Vec::new();
        let mut cut = false;
        let read = async {
            let mut chunk = [0u8; 64 * 1024];
            let mut buf: Vec<u8> = Vec::new();
            let mut total = 0usize;
            loop {
                if total > limits.max_bytes {
                    cut = true;
                    return Ok(());
                }
                let n = stdout.read(&mut chunk).await.map_err(|e| Error::internal(anyhow::anyhow!(e)))?;
                if n == 0 {
                    return Ok(());
                }
                total += n;
                buf.extend_from_slice(&chunk[..n]);
                // Records end at a NUL; a trailing partial record waits for more bytes.
                while let Some(end) = buf.iter().position(|b| *b == 0) {
                    let rec: Vec<u8> = buf.drain(..=end).collect();
                    if let Some(e) = parse_ls_tree(&rec[..end]) {
                        entries.push(e);
                    }
                    if entries.len() >= limits.max_entries || total > limits.max_bytes {
                        cut = true;
                        return Ok(());
                    }
                }
            }
        };
        match tokio::time::timeout(Duration::from_secs(20), read).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = child.kill().await;
                return Err(e);
            }
            Err(_) => cut = true,
        }
        if cut {
            let _ = child.kill().await;
        } else {
            let status = child.wait().await.map_err(|e| Error::internal(anyhow::anyhow!(e)))?;
            if !status.success() {
                return Err(Error::NotFound);
            }
        }
        Ok(WorkspaceTree { entries, cut })
    }

    /// The raw bytes of the blob at raw `path` bytes in commit `sha`. The size comes from the
    /// tree entry, so a file past `max_bytes` is refused before a byte of it is read. A folder
    /// is `NotFound`; a submodule is an empty file: neither reads anything.
    pub(crate) async fn ws_blob(&self, repo: RepoId, sha: &str, path: &[u8], max_bytes: u64) -> Result<WorkspaceBlob> {
        if !full_id(sha) {
            return Err(Error::NotFound);
        }
        if path.is_empty() || path.len() > 4096 || path.contains(&0) || path.contains(&b'\n') {
            return Err(Error::invalid("File", "path", "not a valid path"));
        }
        let out = self
            .exec(
                self.git(repo)
                    .args(["ls-tree", "-z", "-l", sha, "--"])
                    .arg(std::ffi::OsStr::from_bytes(path))
                    .env("GIT_LITERAL_PATHSPECS", "1"),
            )
            .await?;
        let rec = out.stdout.split(|b| *b == 0).find(|r| record_path(r) == Some(path)).ok_or(Error::NotFound)?;
        let e = parse_ls_tree(rec).ok_or(Error::NotFound)?;
        if e.kind == "tree" {
            return Err(Error::NotFound);
        }
        if e.kind == "commit" {
            return Ok(WorkspaceBlob { bytes: vec![], size: 0, mode: e.mode, too_large: false });
        }
        let size = e.size.unwrap_or(0);
        if size > max_bytes {
            return Ok(WorkspaceBlob { bytes: vec![], size, mode: e.mode, too_large: true });
        }
        let out = self.exec(self.git(repo).args(["cat-file", "blob", &e.sha]).max_output(max_bytes as usize + 1)).await?;
        Ok(WorkspaceBlob { bytes: out.stdout, size, mode: e.mode, too_large: false })
    }

    /// Every regular file under `prefix` at commit `sha`, with its bytes, so the terminal
    /// starts from the exact contents a person would have checked out. One `cat-file
    /// --batch` process reads every blob; never `git archive`, whose `export-ignore` would
    /// leave files out that then look deleted. Symlinks, submodules and names that are not
    /// UTF-8 are left out and counted in `skipped`. Over either cap nothing is read.
    pub(crate) async fn ws_snapshot(
        &self,
        repo: RepoId,
        sha: &str,
        prefix: &[u8],
        limits: WorkspaceSnapshotLimits,
    ) -> Result<WorkspaceSnapshot> {
        if !full_id(sha) {
            return Err(Error::NotFound);
        }
        if prefix.len() > 4096 || prefix.contains(&0) || prefix.contains(&b'\n') || prefix.starts_with(b"/") || prefix.ends_with(b"/") {
            return Err(Error::invalid("Snapshot", "path", "not a valid folder"));
        }
        let tree = self.ws_tree(repo, sha, WorkspaceTreeLimits::default()).await?;
        if tree.cut {
            return Ok(WorkspaceSnapshot { files: vec![], too_large: true, total_files: tree.entries.len(), total_bytes: 0, skipped: 0 });
        }
        // What to read: the path, the executable bit and the blob id.
        let mut want: Vec<(String, bool, String)> = Vec::new();
        let mut total_files = 0usize;
        let mut total_bytes = 0u64;
        let mut skipped = 0usize;
        for e in &tree.entries {
            let raw = base64::engine::general_purpose::STANDARD.decode(&e.path_b64).unwrap_or_default();
            if !prefix.is_empty() && !matches!(raw.strip_prefix(prefix), Some(rest) if rest.first() == Some(&b'/')) {
                continue;
            }
            match e.kind.as_str() {
                "blob" => {}
                // A submodule under the folder is one file left out.
                "commit" => {
                    skipped += 1;
                    continue;
                }
                _ => continue,
            }
            // A symlink is not a file to hand a terminal, and a name that is not UTF-8
            // cannot become one of our paths.
            if e.mode == "120000" {
                skipped += 1;
                continue;
            }
            let Some(path) = e.path.clone() else {
                skipped += 1;
                continue;
            };
            let executable = match e.mode.as_str() {
                "100755" => true,
                "100644" => false,
                _ => {
                    skipped += 1;
                    continue;
                }
            };
            total_files += 1;
            total_bytes += e.size.unwrap_or(0);
            want.push((path, executable, e.sha.clone()));
        }
        if total_files > limits.max_files || total_bytes > limits.max_bytes {
            return Ok(WorkspaceSnapshot { files: vec![], too_large: true, total_files, total_bytes, skipped });
        }
        if want.is_empty() {
            return Ok(WorkspaceSnapshot { files: vec![], too_large: false, total_files, total_bytes, skipped });
        }
        let _permit = self.procs.acquire().await.map_err(|_| GitError::Spawn(std::io::Error::other("shutting down")))?;
        let mut child = self.git(repo).args(["cat-file", "--batch"]).spawn()?;
        let stdin = match child.stdin.take() {
            Some(s) => s,
            None => {
                let _ = child.kill().await;
                return Err(Error::internal(anyhow::anyhow!("git cat-file produced no stdin")));
            }
        };
        let stdout = match child.stdout.take() {
            Some(o) => o,
            None => {
                let _ = child.kill().await;
                return Err(Error::internal(anyhow::anyhow!("git cat-file produced no stdout")));
            }
        };
        // One request line per blob, written by a spawned task so a full pipe can never
        // deadlock the read below.
        let mut req = Vec::new();
        for (_, _, id) in &want {
            req.extend_from_slice(id.as_bytes());
            req.push(b'\n');
        }
        let writer = tokio::spawn(async move {
            let mut stdin = stdin;
            let _ = stdin.write_all(&req).await;
            let _ = stdin.shutdown().await;
        });
        let mut reader = BufReader::new(stdout);
        let mut files: Vec<WorkspaceSnapshotFile> = Vec::with_capacity(want.len());
        let mut running = 0u64;
        let mut over_cap = false;
        let read = async {
            for (path, executable, id) in &want {
                let mut line = Vec::new();
                reader.read_until(b'\n', &mut line).await.map_err(|e| Error::internal(anyhow::anyhow!(e)))?;
                let header =
                    std::str::from_utf8(&line).map_err(|_| Error::internal(anyhow::anyhow!("cat-file returned an unexpected object")))?;
                let mut parts = header.split_whitespace();
                // `<sha> blob <size>`; `missing` or any other shape is not an answer we asked for.
                let size = match (parts.next(), parts.next(), parts.next()) {
                    (Some(h_id), Some("blob"), Some(n)) if h_id == id.as_str() => n.parse::<u64>().ok(),
                    _ => None,
                };
                let Some(size) = size else {
                    return Err(Error::internal(anyhow::anyhow!("cat-file returned an unexpected object")));
                };
                // Defence in depth: the totals were already checked, so this only trips when the
                // object is not what the tree said. Checked before anything is allocated.
                running += size;
                if running > limits.max_bytes {
                    over_cap = true;
                    return Ok(());
                }
                let mut body = vec![0u8; size as usize];
                reader.read_exact(&mut body).await.map_err(|e| Error::internal(anyhow::anyhow!(e)))?;
                let mut nl = [0u8; 1];
                reader.read_exact(&mut nl).await.map_err(|e| Error::internal(anyhow::anyhow!(e)))?;
                files.push(WorkspaceSnapshotFile { path: path.clone(), executable: *executable, bytes: body });
            }
            Ok(())
        };
        match tokio::time::timeout(Duration::from_secs(30), read).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = child.kill().await;
                writer.abort();
                return Err(e);
            }
            Err(_) => {
                let _ = child.kill().await;
                writer.abort();
                return Err(Error::internal(anyhow::anyhow!("snapshot timed out")));
            }
        }
        if over_cap {
            let _ = child.kill().await;
            writer.abort();
            return Ok(WorkspaceSnapshot { files: vec![], too_large: true, total_files, total_bytes, skipped });
        }
        let status = child.wait().await.map_err(|e| Error::internal(anyhow::anyhow!(e)))?;
        if !status.success() {
            return Err(Error::NotFound);
        }
        Ok(WorkspaceSnapshot { files, too_large: false, total_files, total_bytes, skipped })
    }

    /// Literal, case-insensitive search of the text files at `sha`. Every cap (hits, bytes,
    /// time) stops the search and marks it rather than failing, so a person still sees what
    /// was found. git grep exits 1 when nothing matched: that is an ordinary empty result.
    pub(crate) async fn ws_search(&self, repo: RepoId, sha: &str, text: &str, limits: WorkspaceSearchLimits) -> Result<WorkspaceSearch> {
        if !full_id(sha) {
            return Err(Error::NotFound);
        }
        if text.is_empty() || text.chars().count() > 200 || text.contains('\n') || text.contains('\0') {
            return Err(Error::invalid("Search", "text", "the search text must be 1 to 200 characters on one line"));
        }
        let _permit = self.procs.acquire().await.map_err(|_| GitError::Spawn(std::io::Error::other("shutting down")))?;
        // The `-e` keeps the text out of option position even if it begins with a dash.
        let mut child = self.git(repo).args(["grep", "-z", "-n", "-I", "-i", "-F", "-e", text, sha, "--"]).spawn()?;
        let mut stdout = match child.stdout.take() {
            Some(o) => o,
            None => {
                let _ = child.kill().await;
                return Err(Error::internal(anyhow::anyhow!("git grep produced no stdout")));
            }
        };
        let prefix = format!("{sha}:");
        let mut hits: Vec<WorkspaceHit> = Vec::new();
        let mut cut = false;
        let read = async {
            let mut chunk = [0u8; 64 * 1024];
            let mut buf: Vec<u8> = Vec::new();
            let mut total = 0usize;
            loop {
                if total > limits.max_bytes {
                    cut = true;
                    return Ok(());
                }
                let n = stdout.read(&mut chunk).await.map_err(|e| Error::internal(anyhow::anyhow!(e)))?;
                if n == 0 {
                    return Ok(());
                }
                total += n;
                buf.extend_from_slice(&chunk[..n]);
                // With `-z` a record is `<sha>:<path>\0<line number>\0<line text>\n`, so a
                // colon in a path or in the text cannot be mistaken for a separator.
                while let Some(end) = buf.iter().position(|b| *b == b'\n') {
                    let rec: Vec<u8> = buf.drain(..=end).collect();
                    let rec = &rec[..end];
                    if let Some(rest) = rec.strip_prefix(prefix.as_bytes()) {
                        let mut it = rest.splitn(3, |b| *b == 0);
                        if let (Some(path), Some(no), Some(t)) = (it.next(), it.next(), it.next()) {
                            hits.push(WorkspaceHit {
                                path_b64: base64::engine::general_purpose::STANDARD.encode(path),
                                path: String::from_utf8(path.to_vec()).ok(),
                                line: String::from_utf8_lossy(no).parse().unwrap_or(0),
                                text: String::from_utf8_lossy(t).chars().take(300).collect(),
                            });
                        }
                    }
                    if hits.len() >= limits.max_hits || total > limits.max_bytes {
                        cut = true;
                        return Ok(());
                    }
                }
            }
        };
        let mut timed_out = false;
        match tokio::time::timeout(Duration::from_secs(limits.timeout_secs), read).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = child.kill().await;
                return Err(e);
            }
            Err(_) => {
                cut = true;
                timed_out = true;
            }
        }
        if cut {
            let _ = child.kill().await;
        } else {
            let status = child.wait().await.map_err(|e| Error::internal(anyhow::anyhow!(e)))?;
            if !status.success() && status.code() != Some(1) {
                return Err(Error::NotFound);
            }
        }
        Ok(WorkspaceSearch { hits, cut, timed_out })
    }

    /// Whether the actor may commit straight to `branch` from the web: the repository is not
    /// archived, they have write access, and branch protection does not insist on a pull
    /// request. The same rule a push goes through, so the web is not a side door.
    pub(crate) async fn ws_direct_commit_allowed(&self, actor: &Actor, repo: RepoId, branch: &str) -> Result<bool> {
        let row = self.repo_row(repo).await?;
        if row.archived {
            return Ok(false);
        }
        if self.authz.require(actor, repo, Permission::Write).await.is_err() {
            return Ok(false);
        }
        let protections = self.protections_of(repo).await?;
        Ok(crate::protect::check_branch_update(
            &protections,
            branch,
            &row.default_branch,
            crate::protect::UpdateKind::Update { forced: false },
            crate::protect::Origin::Web,
        )
        .is_ok())
    }
}
