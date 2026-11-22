//! Where workflow files are read from.
//!
//! `ch-ci` never reads a git directory itself. It asks `GitService` (through this narrow trait)
//! for a commit, a directory listing and a file, so the same code runs against the real git
//! stream and against a temporary repository in tests.

use async_trait::async_trait;
use ch_core::model::EntryKind;
use ch_core::services::GitService;
use ch_core::{Error, RepoId, Result};
use std::sync::Arc;

/// A short commit summary for the `head_commit` part of a push event.
#[derive(Debug, Clone)]
pub struct CommitSummary {
    pub id: String,
    pub message: String,
    pub author_name: String,
    pub author_email: String,
}

#[async_trait]
pub trait WorkflowSource: Send + Sync {
    /// Resolves a branch, tag or sha to a commit sha.
    async fn resolve(&self, repo: RepoId, rev: &str) -> Result<String>;
    /// File names (not directories) in `dir` at `sha`. A missing directory is an empty list.
    async fn list_dir(&self, repo: RepoId, sha: &str, dir: &str) -> Result<Vec<String>>;
    /// A file's text, or `None` when it does not exist. Binary or oversized files are an error.
    async fn read(&self, repo: RepoId, sha: &str, path: &str, max_bytes: u64) -> Result<Option<String>>;
    /// Files changed between two commits, or `None` when that cannot be told (a new branch, or
    /// a commit the repository does not have). `None` never stops a run.
    async fn changed_paths(&self, repo: RepoId, base: &str, head: &str) -> Result<Option<Vec<String>>>;
    async fn commit(&self, repo: RepoId, sha: &str) -> Result<Option<CommitSummary>>;
}

/// Past this many changed files, path filters are not applied and the run goes ahead.
pub const MAX_CHANGED_FILES: usize = 3000;

pub struct GitSource {
    git: Arc<dyn GitService>,
}

impl GitSource {
    pub fn new(git: Arc<dyn GitService>) -> Self {
        Self { git }
    }
}

#[async_trait]
impl WorkflowSource for GitSource {
    async fn resolve(&self, repo: RepoId, rev: &str) -> Result<String> {
        self.git.resolve(repo, rev).await
    }

    async fn list_dir(&self, repo: RepoId, sha: &str, dir: &str) -> Result<Vec<String>> {
        match self.git.tree(repo, sha, dir).await {
            Ok(entries) => Ok(entries.into_iter().filter(|e| e.kind == EntryKind::Blob).map(|e| e.name).collect()),
            Err(Error::NotFound) => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    async fn read(&self, repo: RepoId, sha: &str, path: &str, max_bytes: u64) -> Result<Option<String>> {
        match self.git.blob(repo, sha, path, max_bytes).await {
            Ok(b) if b.truncated => Err(Error::invalid("Workflow", path, format!("the file is larger than {} KiB", max_bytes / 1024))),
            Ok(b) if b.is_binary => Err(Error::invalid("Workflow", path, "the file is not text")),
            Ok(b) => String::from_utf8(b.content).map(Some).map_err(|_| Error::invalid("Workflow", path, "the file is not valid UTF-8")),
            Err(Error::NotFound) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn changed_paths(&self, repo: RepoId, base: &str, head: &str) -> Result<Option<Vec<String>>> {
        if base.is_empty() || base.bytes().all(|b| b == b'0') {
            return Ok(None);
        }
        match self.git.diff(repo, base, head).await {
            // A push of thousands of files is not worth sorting through: let the run happen.
            Ok(d) if d.files.len() > MAX_CHANGED_FILES => Ok(None),
            Ok(d) => {
                let mut v: Vec<String> = d.files.into_iter().flat_map(|f| [f.new_path, f.old_path]).flatten().collect();
                v.sort();
                v.dedup();
                Ok(Some(v))
            }
            Err(Error::NotFound) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn commit(&self, repo: RepoId, sha: &str) -> Result<Option<CommitSummary>> {
        match self.git.commit(repo, sha).await {
            Ok(c) => Ok(Some(CommitSummary { id: c.sha, message: c.message, author_name: c.author.name, author_email: c.author.email })),
            Err(Error::NotFound) => Ok(None),
            Err(e) => Err(e),
        }
    }
}
