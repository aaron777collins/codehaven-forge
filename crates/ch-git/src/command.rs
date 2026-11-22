//! The one way this crate runs `git`.
//!
//! Every call gets a clean environment (no system or user config, no prompts, no pager),
//! a timeout, a cap on how much output it may produce, and arguments that are never
//! user input in option position. User-supplied revisions go through [`check_rev`] and
//! user-supplied paths through [`check_path`] before they reach a command line, and
//! commands that take `--end-of-options` get it.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};

/// Where git may find its (empty) home, and which `git` binary to run.
#[derive(Debug, Clone)]
pub struct GitEnv {
    /// An empty directory used as `HOME` so no user config is ever read.
    pub home: PathBuf,
    /// The `git` program. Defaults to `git` on `PATH`.
    pub program: OsString,
}

impl GitEnv {
    pub fn new(home: PathBuf) -> Self {
        Self { home, program: OsString::from("git") }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("git {args} failed ({code}): {stderr}")]
    Failed { args: String, code: i32, stderr: String },
    #[error("git timed out after {0:?}")]
    Timeout(Duration),
    #[error("git output is larger than {0} bytes")]
    TooLarge(usize),
    #[error("could not run git: {0}")]
    Spawn(#[from] std::io::Error),
}

impl GitError {
    /// The stderr text of a failed command, for callers that map known messages.
    pub fn stderr(&self) -> &str {
        match self {
            GitError::Failed { stderr, .. } => stderr,
            _ => "",
        }
    }
}

impl From<GitError> for ch_core::Error {
    fn from(e: GitError) -> Self {
        match e {
            GitError::Timeout(_) => ch_core::Error::unavailable("git took too long"),
            other => ch_core::Error::internal(anyhow::anyhow!(other)),
        }
    }
}

#[derive(Debug)]
pub struct Output {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Output {
    pub fn stdout_str(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim_end().to_string()
    }
    pub fn stderr_str(&self) -> String {
        String::from_utf8_lossy(&self.stderr).trim().to_string()
    }
}

/// A `git` invocation under construction.
pub struct GitCommand {
    env: GitEnv,
    git_dir: Option<PathBuf>,
    args: Vec<OsString>,
    envs: Vec<(String, String)>,
    stdin: Option<Vec<u8>>,
    timeout: Duration,
    max_output: usize,
}

impl GitCommand {
    pub fn new(env: &GitEnv) -> Self {
        Self {
            env: env.clone(),
            git_dir: None,
            args: Vec::new(),
            envs: Vec::new(),
            stdin: None,
            timeout: Duration::from_secs(30),
            max_output: 64 * 1024 * 1024,
        }
    }

    /// Runs against this bare repository (`GIT_DIR`).
    pub fn git_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.git_dir = Some(dir.into());
        self
    }

    pub fn arg(mut self, a: impl Into<OsString>) -> Self {
        self.args.push(a.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn env(mut self, key: &str, value: impl Into<String>) -> Self {
        self.envs.push((key.to_string(), value.into()));
        self
    }

    pub fn stdin_bytes(mut self, data: Vec<u8>) -> Self {
        self.stdin = Some(data);
        self
    }

    pub fn timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }

    pub fn max_output(mut self, n: usize) -> Self {
        self.max_output = n;
        self
    }

    fn summary(&self) -> String {
        self.args.iter().take(4).map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>().join(" ")
    }

    fn build(&self) -> Command {
        let mut c = Command::new(&self.env.program);
        c.env_clear();
        c.env("PATH", std::env::var_os("PATH").unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".into()));
        c.env("HOME", &self.env.home);
        c.env("XDG_CONFIG_HOME", &self.env.home);
        c.env("GIT_CONFIG_NOSYSTEM", "1");
        c.env("GIT_CONFIG_GLOBAL", "/dev/null");
        c.env("GIT_TERMINAL_PROMPT", "0");
        c.env("GIT_ASKPASS", "true");
        c.env("GIT_PAGER", "cat");
        c.env("GIT_OPTIONAL_LOCKS", "0");
        c.env("LC_ALL", "C");
        if let Some(d) = &self.git_dir {
            c.env("GIT_DIR", d);
        }
        for (k, v) in &self.envs {
            c.env(k, v);
        }
        c.args(["-c", "core.quotePath=false"]);
        c.args(&self.args);
        c.kill_on_drop(true);
        c
    }

    /// Runs to completion and returns the output whatever the exit code.
    pub async fn run_any(self) -> Result<Output, GitError> {
        let summary = self.summary();
        let timeout = self.timeout;
        let mut cmd = self.build();
        cmd.stdin(if self.stdin.is_some() { Stdio::piped() } else { Stdio::null() });
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn()?;
        let mut stdin = child.stdin.take();
        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();
        let input = self.stdin;
        let max = self.max_output;
        let work = async {
            let write = async {
                if let (Some(mut w), Some(data)) = (stdin.take(), input) {
                    // The process may exit before reading everything; that is not our error.
                    let _ = w.write_all(&data).await;
                    let _ = w.shutdown().await;
                }
            };
            let read_out = async {
                let mut buf = Vec::new();
                if let Some(o) = stdout.as_mut() {
                    // One extra byte tells "exactly at the limit" from "over it".
                    o.take(max as u64 + 1).read_to_end(&mut buf).await?;
                }
                Ok::<_, std::io::Error>(buf)
            };
            let read_err = async {
                let mut buf = Vec::new();
                if let Some(e) = stderr.as_mut() {
                    e.take(64 * 1024).read_to_end(&mut buf).await?;
                    // Drain the rest so the process never blocks on a full pipe.
                    let _ = tokio::io::copy(e, &mut tokio::io::sink()).await;
                }
                Ok::<_, std::io::Error>(buf)
            };
            let (_, out, err) = tokio::join!(write, read_out, read_err);
            Ok::<_, std::io::Error>((out?, err?))
        };
        let (out, err) = match tokio::time::timeout(timeout, work).await {
            Ok(r) => r?,
            Err(_) => {
                let _ = child.kill().await;
                tracing::warn!(args = %summary, "git timed out");
                return Err(GitError::Timeout(timeout));
            }
        };
        if out.len() > max {
            let _ = child.kill().await;
            return Err(GitError::TooLarge(max));
        }
        let status = child.wait().await?;
        Ok(Output { code: status.code().unwrap_or(-1), stdout: out, stderr: err })
    }

    /// Runs to completion and fails unless git exits 0.
    pub async fn run(self) -> Result<Output, GitError> {
        let summary = self.summary();
        let out = self.run_any().await?;
        if out.code != 0 {
            return Err(GitError::Failed { args: summary, code: out.code, stderr: out.stderr_str() });
        }
        Ok(out)
    }

    /// Starts the process with piped stdin, stdout and stderr for streaming. The caller owns
    /// the child, which is killed if dropped.
    pub fn spawn(self) -> Result<Child, GitError> {
        let mut cmd = self.build();
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        Ok(cmd.spawn()?)
    }
}

/// A revision or ref that is safe to put on a command line: not empty, not an option, no
/// control characters, no whitespace, nothing git treats specially in a path position.
pub fn check_rev(rev: &str) -> Result<&str, ch_core::Error> {
    let bad = rev.is_empty()
        || rev.len() > 1024
        || rev.starts_with('-')
        || rev.chars().any(|c| c.is_control() || c.is_whitespace() || matches!(c, '\\' | '*' | '?' | '[' | ' '));
    if bad {
        return Err(ch_core::Error::invalid("Revision", "ref", "not a valid revision"));
    }
    Ok(rev)
}

/// A repository path (no leading slash, no `..`, no NUL or newline). Empty means the root.
pub fn check_path(path: &str) -> Result<&str, ch_core::Error> {
    let p = path.trim_matches('/');
    let bad = p.len() > 4096 || p.contains('\0') || p.contains('\n') || p.split('/').any(|s| s == ".." || s == ".");
    if bad {
        return Err(ch_core::Error::invalid("Path", "path", "not a valid path"));
    }
    Ok(p)
}

/// The path of the `hooks` shell script for a stage.
pub fn hook_path(hooks_dir: &Path, stage: &str) -> PathBuf {
    hooks_dir.join(stage)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> GitEnv {
        GitEnv::new(std::env::temp_dir())
    }

    #[tokio::test]
    async fn runs_and_captures() {
        let out = GitCommand::new(&env()).args(["--version"]).run().await.unwrap();
        assert!(out.stdout_str().starts_with("git version"));
    }

    #[tokio::test]
    async fn failure_carries_stderr() {
        let e = GitCommand::new(&env()).args(["definitely-not-a-command"]).run().await.unwrap_err();
        assert!(matches!(e, GitError::Failed { .. }));
    }

    #[tokio::test]
    async fn output_cap_is_enforced() {
        let e = GitCommand::new(&env()).args(["--version"]).max_output(3).run().await.unwrap_err();
        assert!(matches!(e, GitError::TooLarge(3)));
    }

    #[tokio::test]
    async fn timeout_kills() {
        // A shell alias that sleeps stands in for a hung git.
        let dir = tempfile::tempdir().unwrap();
        let e = GitCommand::new(&env())
            .args(["-c", "alias.nap=!sleep 5", "nap"])
            .git_dir(dir.path())
            .timeout(Duration::from_millis(200))
            .run()
            .await
            .unwrap_err();
        assert!(matches!(e, GitError::Timeout(_)));
    }

    #[tokio::test]
    async fn environment_is_clean() {
        // No system config is read, whatever the parent process has set.
        let out = GitCommand::new(&env()).args(["config", "--list", "--show-origin"]).run_any().await.unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(!text.contains("/etc/gitconfig"));
    }

    #[test]
    fn rev_checks() {
        assert!(check_rev("main").is_ok());
        assert!(check_rev("feature/x~2").is_ok());
        assert!(check_rev("HEAD^{commit}").is_ok());
        assert!(check_rev("--upload-pack=evil").is_err());
        assert!(check_rev("-x").is_err());
        assert!(check_rev("a b").is_err());
        assert!(check_rev("a\nb").is_err());
        assert!(check_rev("").is_err());
        assert!(check_rev("ma*").is_err());
    }

    #[test]
    fn path_checks() {
        assert_eq!(check_path("/src/lib.rs").unwrap(), "src/lib.rs");
        assert_eq!(check_path("").unwrap(), "");
        assert!(check_path("../etc/passwd").is_err());
        assert!(check_path("a/../b").is_err());
        assert!(check_path("a\0b").is_err());
    }
}
