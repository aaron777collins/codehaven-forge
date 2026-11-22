//! Temporary git repositories built with the real `git` binary, with fixed identities and
//! dates so shas are stable across runs.

use std::path::{Path, PathBuf};
use std::process::Command;

pub struct TempRepo {
    /// Owns the directory.
    pub dir: tempfile::TempDir,
    /// The working tree.
    pub work: PathBuf,
    clock: std::cell::Cell<u64>,
}

impl TempRepo {
    /// A new repo with `main` as the initial branch and no commits.
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).expect("mkdir");
        let r = TempRepo { dir, work, clock: std::cell::Cell::new(1_700_000_000) };
        r.git(&["init", "--initial-branch=main", "--quiet"]);
        r
    }

    /// A repo with one commit holding `README.md`.
    pub fn with_readme() -> Self {
        let r = Self::new();
        r.commit_file("README.md", "# demo\n", "Add README");
        r
    }

    fn command(&self, cwd: &Path) -> Command {
        let t = self.clock.get();
        self.clock.set(t + 60);
        let date = format!("{t} +0000");
        let mut c = Command::new("git");
        c.current_dir(cwd)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", self.dir.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_AUTHOR_NAME", "Alice Example")
            .env("GIT_AUTHOR_EMAIL", "alice@test.example")
            .env("GIT_COMMITTER_NAME", "Alice Example")
            .env("GIT_COMMITTER_EMAIL", "alice@test.example")
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date);
        c
    }

    /// Runs git in the working tree and returns trimmed stdout. Panics with stderr on failure.
    pub fn git(&self, args: &[&str]) -> String {
        self.git_in(&self.work, args)
    }

    pub fn git_in(&self, cwd: &Path, args: &[&str]) -> String {
        let out = self.command(cwd).args(args).output().expect("run git");
        assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Like `git` but returns whether it succeeded, for expected failures such as a conflict.
    pub fn git_ok(&self, args: &[&str]) -> bool {
        self.command(&self.work).args(args).output().expect("run git").status.success()
    }

    /// Writes a file and commits it. Returns the new commit sha.
    pub fn commit_file(&self, path: &str, content: &str, message: &str) -> String {
        let full = self.work.join(path);
        if let Some(p) = full.parent() {
            std::fs::create_dir_all(p).expect("mkdir");
        }
        std::fs::write(&full, content).expect("write");
        self.git(&["add", "--", path]);
        self.git(&["commit", "--quiet", "-m", message]);
        self.head()
    }

    pub fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"])
    }

    pub fn branch(&self, name: &str) {
        self.git(&["checkout", "--quiet", "-b", name]);
    }

    pub fn checkout(&self, name: &str) {
        self.git(&["checkout", "--quiet", name]);
    }

    pub fn tag(&self, name: &str) {
        self.git(&["tag", name]);
    }

    /// Two branches that change the same line of `conflict.txt` differently, so merging
    /// them conflicts. Leaves `main` checked out. Returns the two branch names.
    pub fn conflicting_branches(&self) -> (String, String) {
        self.checkout("main");
        self.commit_file("conflict.txt", "line one\nshared\nline three\n", "Add conflict.txt");
        self.branch("left");
        self.commit_file("conflict.txt", "line one\nleft side\nline three\n", "Change on left");
        self.checkout("main");
        self.branch("right");
        self.commit_file("conflict.txt", "line one\nright side\nline three\n", "Change on right");
        self.checkout("main");
        ("left".into(), "right".into())
    }

    /// A bare clone of this repo, the shape the server stores repositories in.
    pub fn bare_clone(&self) -> PathBuf {
        let dest = self.dir.path().join("bare.git");
        self.git_in(
            self.dir.path(),
            &["clone", "--quiet", "--bare", self.work.to_str().expect("utf8 path"), dest.to_str().expect("utf8 path")],
        );
        dest
    }
}

impl Default for TempRepo {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_commits_with_stable_shas() {
        let a = TempRepo::with_readme();
        let b = TempRepo::with_readme();
        assert_eq!(a.head(), b.head(), "same content, identity and dates give the same sha");
        assert_eq!(a.head().len(), 40);
    }

    #[test]
    fn conflicting_branches_really_conflict() {
        let r = TempRepo::with_readme();
        let (l, rt) = r.conflicting_branches();
        r.checkout(&l);
        assert!(!r.git_ok(&["merge", "--no-edit", &rt]), "merge should conflict");
        r.git(&["merge", "--abort"]);
    }

    #[test]
    fn bare_clone_has_the_history() {
        let r = TempRepo::with_readme();
        r.commit_file("src/lib.rs", "fn main() {}\n", "Add lib");
        let bare = r.bare_clone();
        let n = r.git_in(&bare, &["rev-list", "--count", "main"]);
        assert_eq!(n, "2");
    }

    #[test]
    fn branches_and_tags() {
        let r = TempRepo::with_readme();
        r.branch("feature");
        r.commit_file("a.txt", "a", "A");
        r.tag("v1");
        assert!(r.git(&["branch", "--list"]).contains("feature"));
        assert_eq!(r.git(&["tag", "--list"]), "v1");
    }
}
