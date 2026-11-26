//! Read paths (ARCHITECTURE.md section 6.1): refs, revisions, trees, blobs, history, compare,
//! diff, blame, grep and archives.
//!
//! Every read runs the `git` plumbing through [`GitCore::exec`]. The design called for `gix`
//! on the hot paths; this crate starts with plumbing because it is the reference behaviour,
//! needs no second implementation of git's rules to keep in step, and is fast enough at this
//! scale. The functions here are the seam: swapping one for a `gix` version changes no caller.

use crate::command::{GitCommand, GitError, check_path, check_rev};
use crate::core::GitCore;
use crate::diff::{DiffLimits, assemble, parse_numstat_z, parse_patch, parse_raw_z};
use bytes::Bytes;
use ch_core::model::*;
use ch_core::services::ArchiveFormat;
use ch_core::{Error, RepoId, Result};
use chrono::{DateTime, Utc};
use futures::stream::BoxStream;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, ReadBuf};

/// The empty tree, which a root commit is diffed against.
pub const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// `git log` format: fields split by the unit separator, records by NUL (`-z`).
const LOG_FORMAT: &str = "--format=%H%x1f%P%x1f%T%x1f%an%x1f%ae%x1f%at%x1f%cn%x1f%ce%x1f%ct%x1f%B";

/// Splits a revision into its name and the suffix that walks from it: `main~2` is `main` and
/// `~2`, `main:src/a.rs` is `main` and `:src/a.rs`. A branch name cannot hold `~`, `^`, `:` or `@{`.
fn split_rev(rev: &str) -> (&str, &str) {
    let at = rev.find("@{").unwrap_or(rev.len());
    let cut = rev.find(['~', '^', ':']).unwrap_or(rev.len()).min(at);
    rev.split_at(cut)
}

fn is_missing(e: &GitError) -> bool {
    let s = e.stderr();
    s.contains("unknown revision")
        || s.contains("bad revision")
        || s.contains("Not a valid object name")
        || s.contains("not a tree object")
        || s.contains("does not exist")
        || s.contains("ambiguous argument")
        || s.contains("bad object")
        || s.contains("invalid object name")
        || s.contains("no such path")
        || s.contains("not a valid object")
        || s.contains("path '") && s.contains("exist")
}

fn map_missing(e: GitError) -> Error {
    if is_missing(&e) { Error::NotFound } else { e.into() }
}

fn to_time(secs: &str) -> DateTime<Utc> {
    secs.trim().parse::<i64>().ok().and_then(|s| DateTime::from_timestamp(s, 0)).unwrap_or_default()
}

pub(crate) fn parse_log(out: &[u8]) -> Vec<CommitInfo> {
    let mut commits = Vec::new();
    for record in out.split(|b| *b == 0) {
        if record.is_empty() {
            continue;
        }
        let text = String::from_utf8_lossy(record);
        let mut f = text.splitn(10, '\x1f');
        let (Some(sha), Some(parents), Some(tree), Some(an), Some(ae), Some(at), Some(cn), Some(ce), Some(ct)) =
            (f.next(), f.next(), f.next(), f.next(), f.next(), f.next(), f.next(), f.next(), f.next())
        else {
            continue;
        };
        let message = f.next().unwrap_or("").trim_end().to_string();
        commits.push(CommitInfo {
            // git log -z puts a newline between records in some modes; keep only the sha.
            sha: sha.trim().to_string(),
            parents: parents.split_whitespace().map(str::to_string).collect(),
            tree: tree.to_string(),
            author: Signature { name: an.to_string(), email: ae.to_string(), at: to_time(at) },
            committer: Signature { name: cn.to_string(), email: ce.to_string(), at: to_time(ct) },
            message,
        });
    }
    commits
}

/// Reads at most `max` bytes of stdout and kills the child if there is more.
async fn read_prefix(mut child: tokio::process::Child, max: u64) -> std::io::Result<(Vec<u8>, bool)> {
    use tokio::io::AsyncReadExt;
    let Some(stdout) = child.stdout.take() else { return Ok((Vec::new(), false)) };
    let mut buf = Vec::new();
    stdout.take(max + 1).read_to_end(&mut buf).await?;
    let truncated = buf.len() as u64 > max;
    if truncated {
        buf.truncate(max as usize);
        let _ = child.kill().await;
    } else {
        let _ = child.wait().await;
    }
    Ok((buf, truncated))
}

impl GitCore {
    pub(crate) async fn read_refs(&self, repo: RepoId, prefix: Option<&str>) -> Result<Vec<GitRef>> {
        let mut cmd = self.git(repo).args(["for-each-ref", "--format=%(refname) %(objectname) %(*objectname)"]);
        if let Some(p) = prefix {
            let p = check_rev(p)?;
            if !p.starts_with("refs/") {
                return Err(Error::invalid("Reference", "prefix", "must start with refs/"));
            }
            cmd = cmd.arg(p);
        }
        let out = self.exec(cmd).await?;
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| {
                let mut it = l.split_whitespace();
                let name = it.next()?;
                let sha = it.next()?;
                // An annotated tag shows the commit it points at, which is what callers want.
                let peeled = it.next();
                Some(GitRef { name: name.to_string(), sha: peeled.unwrap_or(sha).to_string() })
            })
            .collect())
    }

    /// Refs with the type of the object each one points at (`commit`, `tag`, ...), unpeeled.
    pub(crate) async fn read_refs_raw(&self, repo: RepoId, prefixes: &[&str]) -> Result<Vec<(String, String, String)>> {
        let mut cmd = self.git(repo).args(["for-each-ref", "--format=%(refname) %(objectname) %(objecttype)"]);
        for p in prefixes {
            let p = check_rev(p)?;
            if !p.starts_with("refs/") {
                return Err(Error::invalid("Reference", "prefix", "must start with refs/"));
            }
            cmd = cmd.arg(p);
        }
        let out = self.exec(cmd).await?;
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| {
                let mut it = l.split_whitespace();
                Some((it.next()?.to_string(), it.next()?.to_string(), it.next()?.to_string()))
            })
            .collect())
    }

    /// Every entry under a tree, recursively, for the git data API.
    pub(crate) async fn read_tree_recursive(&self, repo: RepoId, tree_ish: &str, cap: usize) -> Result<(Vec<TreeEntry>, bool)> {
        let spec = self.pin_rev(repo, tree_ish).await?;
        let out = self
            .exec(self.git(repo).args(["ls-tree", "-z", "-l", "-r", "-t", spec.as_str()]).max_output(64 * 1024 * 1024))
            .await
            .map_err(map_missing)?;
        let mut entries = Vec::new();
        for rec in out.stdout.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            let text = String::from_utf8_lossy(rec);
            let Some((meta, path)) = text.split_once('\t') else { continue };
            let mut m = meta.split_whitespace();
            let (Some(mode), Some(kind), Some(sha), Some(size)) = (m.next(), m.next(), m.next(), m.next()) else { continue };
            let kind = match (kind, mode) {
                ("tree", _) => EntryKind::Tree,
                ("commit", _) => EntryKind::Commit,
                ("blob", "120000") => EntryKind::Symlink,
                _ => EntryKind::Blob,
            };
            let name = path.rsplit('/').next().unwrap_or(path).to_string();
            entries.push(TreeEntry {
                name,
                path: path.to_string(),
                kind,
                mode: mode.to_string(),
                sha: sha.to_string(),
                size: size.parse().ok(),
            });
            if entries.len() >= cap {
                return Ok((entries, true));
            }
        }
        Ok((entries, false))
    }

    /// A bare branch name means the branch. Git itself looks for a tag of that name first, and
    /// anyone with write access can push a tag, so on a protected branch `main` would show a
    /// commit nobody reviewed. A bare name that is a branch becomes `refs/heads/<name>`, and a
    /// short hex name is checked as a branch first, so a tag cannot shadow a branch called
    /// `beef`. Anything else (a full commit id, a tag, `HEAD`, a full ref) goes to git as written.
    pub(crate) async fn pin_rev(&self, repo: RepoId, rev: &str) -> Result<String> {
        let rev = check_rev(rev)?;
        let (base, suffix) = split_rev(rev);
        let full_sha = (base.len() == 40 || base.len() == 64) && base.bytes().all(|b| b.is_ascii_hexdigit());
        if base.is_empty() || base == "HEAD" || base.starts_with("refs/") || full_sha {
            return Ok(rev.to_string());
        }
        let full = format!("refs/heads/{base}");
        let out = self.exec_any(self.git(repo).args(["rev-parse", "--verify", "--quiet", "--end-of-options", &full])).await?;
        if out.code == 0 { Ok(format!("{full}{suffix}")) } else { Ok(rev.to_string()) }
    }

    /// Branch, tag, sha or `HEAD` to a commit sha.
    pub(crate) async fn read_resolve(&self, repo: RepoId, rev: &str) -> Result<String> {
        let rev = self.pin_rev(repo, rev).await?;
        let spec = format!("{rev}^{{commit}}");
        let out = self.exec_any(self.git(repo).args(["rev-parse", "--verify", "--quiet", "--end-of-options", &spec])).await?;
        let sha = out.stdout_str();
        if out.code != 0 || sha.is_empty() { Err(Error::NotFound) } else { Ok(sha) }
    }

    pub(crate) async fn read_tree(&self, repo: RepoId, rev: &str, path: &str) -> Result<Vec<TreeEntry>> {
        let rev = self.pin_rev(repo, rev).await?;
        let rev = rev.as_str();
        let path = check_path(path)?;
        let spec = if path.is_empty() { rev.to_string() } else { format!("{rev}:{path}") };
        let out = self.exec(self.git(repo).args(["ls-tree", "-z", "-l", &spec])).await.map_err(map_missing)?;
        let mut entries = Vec::new();
        for rec in out.stdout.split(|b| *b == 0) {
            if rec.is_empty() {
                continue;
            }
            let text = String::from_utf8_lossy(rec);
            let Some((meta, name)) = text.split_once('\t') else { continue };
            let mut m = meta.split_whitespace();
            let (Some(mode), Some(kind), Some(sha), Some(size)) = (m.next(), m.next(), m.next(), m.next()) else { continue };
            let kind = match (kind, mode) {
                ("tree", _) => EntryKind::Tree,
                ("commit", _) => EntryKind::Commit,
                ("blob", "120000") => EntryKind::Symlink,
                _ => EntryKind::Blob,
            };
            entries.push(TreeEntry {
                name: name.to_string(),
                path: if path.is_empty() { name.to_string() } else { format!("{path}/{name}") },
                kind,
                mode: mode.to_string(),
                sha: sha.to_string(),
                size: size.parse().ok(),
            });
        }
        // Folders first, then files, each in name order, the way every file browser shows them.
        entries.sort_by(|a, b| {
            let rank = |k: EntryKind| if k == EntryKind::Tree { 0 } else { 1 };
            rank(a.kind)
                .cmp(&rank(b.kind))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(entries)
    }

    /// `(sha, type, size)` of whatever `spec` names, or `NotFound`.
    pub(crate) async fn object_info(&self, repo: RepoId, spec: &str) -> Result<(String, String, u64)> {
        let out = self.exec(self.git(repo).args(["cat-file", "--batch-check"]).stdin_bytes(format!("{spec}\n").into_bytes())).await?;
        let line = out.stdout_str();
        let mut it = line.split_whitespace();
        match (it.next(), it.next(), it.next()) {
            (Some(sha), Some(kind), Some(size)) if kind != "missing" => Ok((sha.to_string(), kind.to_string(), size.parse().unwrap_or(0))),
            _ => Err(Error::NotFound),
        }
    }

    pub(crate) async fn read_blob(&self, repo: RepoId, rev: &str, path: &str, max_bytes: u64) -> Result<Blob> {
        let rev = self.pin_rev(repo, rev).await?;
        let rev = rev.as_str();
        let path = check_path(path)?;
        let spec = if path.is_empty() { rev.to_string() } else { format!("{rev}:{path}") };
        let (sha, kind, size) = self.object_info(repo, &spec).await?;
        if kind != "blob" {
            return Err(Error::NotFound);
        }
        let _permit = self.procs.acquire().await.map_err(|_| Error::unavailable("shutting down"))?;
        let child = self.git(repo).args(["cat-file", "blob", &sha]).spawn()?;
        let (content, truncated) = read_prefix(child, max_bytes).await.map_err(|e| Error::internal(anyhow::anyhow!(e)))?;
        let is_binary = content.iter().take(8000).any(|b| *b == 0);
        Ok(Blob { sha, size, is_binary, truncated, content })
    }

    pub(crate) async fn read_log(&self, repo: RepoId, rev: &str, path: Option<&str>, skip: u32, limit: u32) -> Result<Vec<CommitInfo>> {
        let rev = self.pin_rev(repo, rev).await?;
        let rev = rev.as_str();
        let limit = limit.clamp(1, 1000);
        let mut cmd = self.git(repo).args([
            "log",
            "-z",
            LOG_FORMAT,
            &format!("--skip={skip}"),
            &format!("--max-count={limit}"),
            "--end-of-options",
            rev,
            "--",
        ]);
        if let Some(p) = path {
            let p = check_path(p)?;
            if !p.is_empty() {
                cmd = cmd.arg(p);
            }
        }
        let out = self.exec(cmd).await.map_err(map_missing)?;
        Ok(parse_log(&out.stdout))
    }

    pub(crate) async fn read_commit(&self, repo: RepoId, rev: &str) -> Result<CommitInfo> {
        self.read_log(repo, rev, None, 0, 1).await?.into_iter().next().ok_or(Error::NotFound)
    }

    /// The best common ancestor, or `None` for unrelated histories.
    pub(crate) async fn merge_base(&self, repo: RepoId, a: &str, b: &str) -> Result<Option<String>> {
        self.merge_base_in(repo, None, a, b).await
    }

    /// A git command in `repo` that can also read the objects in `alt`, another repository's
    /// objects directory. Nothing is written to either store.
    fn git_reading(&self, repo: RepoId, alt: Option<&Path>) -> GitCommand {
        let cmd = self.git(repo);
        match alt {
            Some(a) => cmd.env("GIT_ALTERNATE_OBJECT_DIRECTORIES", a.to_string_lossy()),
            None => cmd,
        }
    }

    async fn merge_base_in(&self, repo: RepoId, alt: Option<&Path>, a: &str, b: &str) -> Result<Option<String>> {
        let out = self.exec_any(self.git_reading(repo, alt).args(["merge-base", a, b])).await?;
        match out.code {
            0 => Ok(Some(out.stdout_str())),
            1 => Ok(None),
            _ => Err(Error::NotFound),
        }
    }

    pub(crate) async fn read_compare(&self, repo: RepoId, base: &str, head: &str) -> Result<Compare> {
        let base_sha = self.read_resolve(repo, base).await?;
        let head_sha = self.read_resolve(repo, head).await?;
        self.compare_shas(repo, None, base_sha, head_sha).await
    }

    /// `base` in `repo` against `head` in `head_repo`, a repository of the same fork network.
    /// Forks keep their own objects, so the commands run in `repo` and read the head
    /// repository's objects as an alternate for that one process only (ch-8tj).
    pub(crate) async fn read_compare_across(&self, repo: RepoId, base: &str, head_repo: RepoId, head: &str) -> Result<Compare> {
        if repo == head_repo {
            return self.read_compare(repo, base, head).await;
        }
        let roots: Vec<(i64, i64)> = sqlx::query_as("SELECT id, COALESCE(fork_root_id, id) FROM repositories WHERE id = ANY($1)")
            .bind(vec![repo.0, head_repo.0])
            .fetch_all(&self.db)
            .await?;
        let root = |id: RepoId| roots.iter().find(|r| r.0 == id.0).map(|r| r.1);
        match (root(repo), root(head_repo)) {
            (Some(a), Some(b)) if a == b => {}
            (Some(_), Some(_)) => return Err(Error::invalid("Compare", "head", "these repositories are not in one fork network")),
            _ => return Err(Error::NotFound),
        }
        if !self.store.exists(head_repo) {
            return Err(Error::NotFound);
        }
        let base_sha = self.read_resolve(repo, base).await?;
        let head_sha = self.read_resolve(head_repo, head).await?;
        let alt = self.store.repo_path(head_repo).join("objects");
        self.compare_shas(repo, Some(&alt), base_sha, head_sha).await
    }

    async fn compare_shas(&self, repo: RepoId, alt: Option<&Path>, base_sha: String, head_sha: String) -> Result<Compare> {
        let merge_base = self
            .merge_base_in(repo, alt, &base_sha, &head_sha)
            .await?
            .ok_or_else(|| Error::invalid("Compare", "base", "these two refs share no history"))?;
        let ahead = self.count_range(repo, alt, &format!("{base_sha}..{head_sha}")).await?;
        let behind = self.count_range(repo, alt, &format!("{head_sha}..{base_sha}")).await?;
        let range = format!("{base_sha}..{head_sha}");
        let out = self
            .exec(self.git_reading(repo, alt).args([
                "log",
                "-z",
                "--reverse",
                LOG_FORMAT,
                "--max-count=250",
                "--end-of-options",
                &range,
                "--",
            ]))
            .await
            .map_err(map_missing)?;
        let commits = parse_log(&out.stdout);
        let diff = match alt {
            None => (*self.diff_between(repo, &merge_base, &head_sha).await?).clone(),
            // Not cached: the cache is keyed by repository, and this one read objects that
            // repository does not hold.
            Some(_) => self.diff_uncached(repo, alt, &merge_base, &head_sha).await?,
        };
        Ok(Compare { base_sha, head_sha, merge_base, ahead_by: ahead as u32, behind_by: behind as u32, commits, diff })
    }

    async fn count_range(&self, repo: RepoId, alt: Option<&Path>, range: &str) -> Result<u64> {
        let out = self
            .exec(self.git_reading(repo, alt).args(["rev-list", "--count", "--end-of-options", range, "--"]))
            .await
            .map_err(map_missing)?;
        Ok(out.stdout_str().parse().unwrap_or(0))
    }

    /// The exact difference from `old` to `new` (both resolved shas or the empty tree).
    pub(crate) async fn diff_between(&self, repo: RepoId, old: &str, new: &str) -> Result<Arc<Diff>> {
        let key = (repo.0, old.to_string(), new.to_string());
        if let Some(hit) = self.diff_cache.get(&key).await {
            return Ok(hit);
        }
        let diff = Arc::new(self.diff_uncached(repo, None, old, new).await?);
        self.diff_cache.insert(key, diff.clone()).await;
        Ok(diff)
    }

    async fn diff_uncached(&self, repo: RepoId, alt: Option<&Path>, old: &str, new: &str) -> Result<Diff> {
        let base = |extra: &[&str]| {
            self.git_reading(repo, alt)
                .args(["diff", "--no-ext-diff", "--no-textconv", "--no-color", "--find-renames"])
                .args(extra.iter().map(|s| s.to_string()))
                .args([old, new, "--"])
                .max_output(256 * 1024 * 1024)
                .timeout(std::time::Duration::from_secs(60))
        };
        let (raw, num, patch) =
            tokio::join!(self.exec(base(&["--raw", "-z"])), self.exec(base(&["--numstat", "-z"])), self.exec(base(&["--patch"])));
        let (raw, num, patch) = (raw?, num?, patch?);
        let to_err = |e: crate::diff::DiffParseError| Error::internal(anyhow::anyhow!(e));
        assemble(
            parse_raw_z(&raw.stdout).map_err(to_err)?,
            parse_numstat_z(&num.stdout).map_err(to_err)?,
            parse_patch(&patch.stdout),
            &DiffLimits::default(),
        )
        .map_err(to_err)
    }

    /// Three-dot diff: what `head` changed since it left `base`. For a commit and its parent
    /// this is exactly the commit's own change.
    pub(crate) async fn read_diff(&self, repo: RepoId, base: &str, head: &str) -> Result<Diff> {
        // A root commit is compared with the empty tree, which is not a commit to resolve.
        if base == EMPTY_TREE {
            let head_sha = self.read_resolve(repo, head).await?;
            return Ok((*self.diff_between(repo, EMPTY_TREE, &head_sha).await?).clone());
        }
        let base_sha = self.read_resolve(repo, base).await?;
        let head_sha = self.read_resolve(repo, head).await?;
        let from = self.merge_base(repo, &base_sha, &head_sha).await?.unwrap_or(base_sha);
        Ok((*self.diff_between(repo, &from, &head_sha).await?).clone())
    }

    /// True when `sha` is a full commit id that a branch or tag of this repository reaches. A
    /// fork shares its objects with the rest of its network, so an object that exists is not
    /// necessarily this repository's. Tree and blob ids, abbreviations and anything else are false.
    pub(crate) async fn read_reachable(&self, repo: RepoId, sha: &str) -> Result<bool> {
        if !(sha.len() == 40 || sha.len() == 64) || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(false);
        }
        let sha = sha.to_ascii_lowercase();
        let ty = self.exec_any(self.git(repo).args(["cat-file", "-t", &sha])).await?;
        if ty.code != 0 || ty.stdout_str().trim() != "commit" {
            return Ok(false);
        }
        let out = self
            .exec_any(self.git(repo).args([
                "for-each-ref",
                "--count=1",
                "--format=%(refname)",
                "--contains",
                &sha,
                "refs/heads",
                "refs/tags",
            ]))
            .await?;
        Ok(out.code == 0 && !out.stdout_str().trim().is_empty())
    }

    /// The commits `base..head` as full shas, newest first, at most `limit` (all of `head`'s
    /// history when there is no `base`). Both ends are resolved first, so neither can be read as
    /// an option.
    pub(crate) async fn read_range_shas(&self, repo: RepoId, base: Option<&str>, head: &str, limit: u32) -> Result<Vec<String>> {
        let head_sha = self.read_resolve(repo, head).await?;
        let range = match base {
            Some(b) => format!("{}..{head_sha}", self.read_resolve(repo, b).await?),
            None => head_sha,
        };
        let max = format!("--max-count={}", limit.clamp(1, 100_000));
        let out = self
            .exec(self.git_reading(repo, None).args(["rev-list", &max, "--end-of-options", &range, "--"]))
            .await
            .map_err(map_missing)?;
        Ok(out.stdout_str().lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect())
    }

    /// What one commit changed. A root commit is compared with the empty tree.
    pub async fn commit_diff(&self, repo: RepoId, sha: &str) -> Result<Arc<Diff>> {
        let c = self.read_commit(repo, sha).await?;
        let parent = c.parents.first().map(String::as_str).unwrap_or(EMPTY_TREE);
        self.diff_between(repo, parent, &c.sha).await
    }

    pub(crate) async fn read_grep(&self, repo: RepoId, rev: &str, pattern: &str, limit: u32) -> Result<Vec<SearchHit>> {
        if pattern.is_empty() || pattern.len() > 200 || pattern.contains('\n') || pattern.contains('\0') {
            return Err(Error::invalid("Search", "q", "the search text must be 1 to 200 characters on one line"));
        }
        let sha = self.read_resolve(repo, rev).await?;
        let cmd = self
            .git(repo)
            .args(["grep", "-z", "-n", "-I", "-i", "-F", "--max-count=20", "-e", pattern, &sha, "--"])
            .timeout(std::time::Duration::from_secs(5));
        let out = match self.exec_any(cmd).await {
            Ok(o) => o,
            // Out of time: say so, rather than show no results as if there were none.
            Err(GitError::Timeout(_)) => return Err(Error::unavailable("The search took too long. Try longer or rarer words.")),
            Err(e) => return Err(e.into()),
        };
        let prefix = format!("{sha}:");
        let mut hits = Vec::new();
        // With `-z` a record is `<sha>:<path>\0<line number>\0<text>\n`, so a colon in a path
        // or in a line of text cannot be mistaken for a separator.
        for line in out.stdout.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
            let Some(rest) = line.strip_prefix(prefix.as_bytes()) else { continue };
            let mut it = rest.splitn(3, |b| *b == 0);
            let (Some(path), Some(no), Some(text)) = (it.next(), it.next(), it.next()) else { continue };
            hits.push(SearchHit {
                kind: "code".into(),
                id: String::from_utf8_lossy(no).parse().unwrap_or(0),
                title: String::from_utf8_lossy(path).into_owned(),
                snippet: String::from_utf8_lossy(text).chars().take(300).collect(),
            });
            if hits.len() as u32 >= limit.clamp(1, 200) {
                break;
            }
        }
        Ok(hits)
    }

    pub(crate) async fn read_blame(&self, repo: RepoId, rev: &str, path: &str) -> Result<Blame> {
        let sha = self.read_resolve(repo, rev).await?;
        let path = check_path(path)?;
        if path.is_empty() {
            return Err(Error::invalid("Blame", "path", "a file path is required"));
        }
        let out = self
            .exec(self.git(repo).args(["blame", "--porcelain", &sha, "--", path]).timeout(std::time::Duration::from_secs(20)))
            .await
            .map_err(map_missing)?;
        crate::blame::parse_porcelain(&out.stdout).map_err(|e| Error::internal(anyhow::anyhow!(e)))
    }

    pub(crate) async fn read_archive(
        &self,
        repo: RepoId,
        rev: &str,
        format: ArchiveFormat,
    ) -> Result<BoxStream<'static, std::io::Result<Bytes>>> {
        use async_compression::tokio::bufread::GzipEncoder;
        use futures::StreamExt;
        use tokio_util::io::ReaderStream;
        let sha = self.read_resolve(repo, rev).await?;
        let row = self.repo_row(repo).await?;
        let prefix = format!("{}-{}/", row.name, &sha[..sha.len().min(12)]);
        let fmt = match format {
            ArchiveFormat::Zip => "--format=zip",
            ArchiveFormat::TarGz => "--format=tar",
        };
        let child = self.git(repo).args(["archive", fmt, &format!("--prefix={prefix}"), &sha]).own_group().spawn()?;
        let reader = ChildStdout::new(child)?.with_deadline(std::time::Duration::from_secs(ARCHIVE_DEADLINE_SECS));
        Ok(match format {
            ArchiveFormat::Zip => ReaderStream::new(reader).boxed(),
            ArchiveFormat::TarGz => ReaderStream::new(GzipEncoder::new(tokio::io::BufReader::new(reader))).boxed(),
        })
    }
}

impl GitCore {
    /// A blob's size and its bytes as a stream, read straight from `git cat-file`, so a raw
    /// download never sits in memory whole (ch-4pn.17). `spec` is `<rev>:<path>` or a blob sha.
    /// `NotFound` for anything that is not a blob.
    pub(crate) async fn blob_stream(&self, repo: RepoId, spec: &str) -> Result<(u64, BoxStream<'static, std::io::Result<Bytes>>)> {
        use futures::StreamExt;
        use tokio_util::io::ReaderStream;
        let (sha, kind, size) = self.object_info(repo, spec).await?;
        if kind != "blob" {
            return Err(Error::NotFound);
        }
        let child = self.git(repo).args(["cat-file", "blob", &sha]).own_group().spawn()?;
        let reader = ChildStdout::new(child)?.with_deadline(std::time::Duration::from_secs(ARCHIVE_DEADLINE_SECS));
        Ok((size, ReaderStream::new(reader).boxed()))
    }
}

/// How long one archive download may take, signed in or not (ch-ulq). The gate has its own,
/// shorter limit for signed-out downloads.
const ARCHIVE_DEADLINE_SECS: u64 = 30 * 60;

/// A child's stdout that kills the child when the reader is dropped, so a download the
/// browser abandons does not leave `git archive` running. With a deadline, the child and
/// everything it started are stopped at that time even if nobody reads, and the reader then
/// fails, so a cut-off download never looks complete.
pub(crate) struct ChildStdout {
    _child: tokio::process::Child,
    out: tokio::process::ChildStdout,
    stopped: std::sync::Arc<std::sync::atomic::AtomicBool>,
    watchdog: Option<tokio::task::JoinHandle<()>>,
}

impl ChildStdout {
    pub(crate) fn new(mut child: tokio::process::Child) -> Result<Self> {
        let out = child.stdout.take().ok_or_else(|| Error::internal(anyhow::anyhow!("no stdout")))?;
        Ok(Self { _child: child, out, stopped: Default::default(), watchdog: None })
    }

    pub(crate) fn with_deadline(mut self, after: std::time::Duration) -> Self {
        let pid = self._child.id();
        let stopped = self.stopped.clone();
        self.watchdog = Some(tokio::spawn(async move {
            tokio::time::sleep(after).await;
            stopped.store(true, std::sync::atomic::Ordering::SeqCst);
            tracing::warn!(?pid, "a git download ran past its deadline and was stopped");
            crate::command::kill_group(pid);
        }));
        self
    }
}

impl Drop for ChildStdout {
    fn drop(&mut self) {
        if let Some(w) = self.watchdog.take() {
            w.abort();
        }
        crate::command::kill_group(self._child.id());
    }
}

impl AsyncRead for ChildStdout {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        if self.stopped.load(std::sync::atomic::Ordering::SeqCst) {
            return Poll::Ready(Err(std::io::Error::other("the download ran past its deadline")));
        }
        Pin::new(&mut self.out).poll_read(cx, buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_revision_splits_into_name_and_suffix() {
        assert_eq!(split_rev("main"), ("main", ""));
        assert_eq!(split_rev("feature/x~2"), ("feature/x", "~2"));
        assert_eq!(split_rev("main^"), ("main", "^"));
        assert_eq!(split_rev("main^{commit}"), ("main", "^{commit}"));
        assert_eq!(split_rev("main:src/a.rs"), ("main", ":src/a.rs"));
        assert_eq!(split_rev("main@{1}"), ("main", "@{1}"));
        assert_eq!(split_rev("v1.0"), ("v1.0", ""));
    }

    #[test]
    fn parses_log_records() {
        let rec = |sha: &str, parents: &str, msg: &str| {
            format!("{sha}\x1f{parents}\x1ftree1\x1fAlice\x1fa@x\x1f1700000000\x1fBob\x1fb@x\x1f1700000060\x1f{msg}\0")
        };
        let out = format!("{}{}", rec("aaa", "bbb ccc", "Merge\n\nbody line\n"), rec("bbb", "", "Root"));
        let c = parse_log(out.as_bytes());
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].sha, "aaa");
        assert_eq!(c[0].parents, vec!["bbb", "ccc"]);
        assert_eq!(c[0].message, "Merge\n\nbody line");
        assert_eq!(c[0].author.name, "Alice");
        assert_eq!(c[0].committer.email, "b@x");
        assert_eq!(c[0].author.at.timestamp(), 1_700_000_000);
        assert!(c[1].parents.is_empty());
    }

    #[test]
    fn a_message_may_contain_the_field_separator() {
        let out = "s\x1f\x1ft\x1fn\x1fe\x1f1\x1fn\x1fe\x1f1\x1fa\x1fb\x1fc\0";
        let c = parse_log(out.as_bytes());
        assert_eq!(c[0].message, "a\x1fb\x1fc");
    }

    #[test]
    fn garbage_records_are_skipped() {
        assert!(parse_log(b"nonsense\0\0").is_empty());
    }
}
