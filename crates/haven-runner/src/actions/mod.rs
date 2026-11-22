//! The built-in actions: the handful of `uses:` steps that run inside the runner itself,
//! with no downloading of third-party code. ARCHITECTURE.md section 9.5.
//!
//! A step's `uses:` is matched by name with or without an owner prefix and version, so
//! `checkout`, `actions/checkout@v4` and `codehaven/checkout@v1` are the same action.

use crate::client::Client;
use crate::jobenv::JobDirs;
use crate::logs::LogWriter;
use ch_api_types::runner::JobPayload;
use std::collections::BTreeMap;
use tokio_util::sync::CancellationToken;

pub mod artifact;
pub mod cache;
pub mod checkout;

/// What a `uses:` string names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Builtin {
    Checkout,
    /// `cache`, `cache/restore` or `cache/save`.
    Cache(CacheMode),
    UploadArtifact,
    DownloadArtifact,
    /// `docker://image`: run a container image once with the step's `args`.
    Docker(String),
    /// Something we do not run, with the reason to show.
    Unsupported(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheMode {
    /// Restore now, save after the job if the key did not hit exactly.
    Both,
    RestoreOnly,
    SaveOnly,
}

pub fn resolve(uses: &str) -> Builtin {
    let uses = uses.trim();
    if let Some(image) = uses.strip_prefix("docker://") {
        return if image.is_empty() {
            Builtin::Unsupported("docker:// needs an image name".into())
        } else {
            Builtin::Docker(image.to_string())
        };
    }
    if uses.starts_with("./") || uses.starts_with("../") {
        return Builtin::Unsupported(format!("{uses}: actions from a directory in the repository are not supported yet"));
    }
    let without_ref = uses.split('@').next().unwrap_or(uses);
    let lowered = without_ref.to_ascii_lowercase();
    let mut parts: Vec<&str> = lowered.split('/').filter(|p| !p.is_empty()).collect();
    // An owner prefix is allowed when it is one of ours.
    if parts.len() >= 2 && matches!(parts[0], "actions" | "codehaven") {
        parts.remove(0);
    }
    match parts.as_slice() {
        ["checkout"] => Builtin::Checkout,
        ["cache"] => Builtin::Cache(CacheMode::Both),
        ["cache", "restore"] => Builtin::Cache(CacheMode::RestoreOnly),
        ["cache", "save"] => Builtin::Cache(CacheMode::SaveOnly),
        ["upload-artifact"] => Builtin::UploadArtifact,
        ["download-artifact"] => Builtin::DownloadArtifact,
        _ => Builtin::Unsupported(format!(
            "{uses} is not one of the built-in actions (checkout, cache, upload-artifact, download-artifact, docker://). Actions that download JavaScript or other code are not supported"
        )),
    }
}

/// Work to do after every step has run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostAction {
    CacheSave { key: String, version: String, patterns: Vec<String> },
}

#[derive(Debug, Default)]
pub struct ActionOutput {
    pub outputs: BTreeMap<String, String>,
    pub post: Option<PostAction>,
}

/// What a built-in action may touch.
pub struct ActionCtx<'a> {
    pub step: i32,
    pub inputs: &'a BTreeMap<String, String>,
    pub dirs: &'a JobDirs,
    pub payload: &'a JobPayload,
    pub client: &'a Client,
    pub log: &'a LogWriter,
    pub cancel: &'a CancellationToken,
}

impl ActionCtx<'_> {
    pub fn input(&self, name: &str) -> Option<&str> {
        self.inputs.get(name).map(String::as_str).filter(|v| !v.is_empty())
    }

    pub fn flag(&self, name: &str, default: bool) -> bool {
        match self.inputs.get(name).map(|v| v.trim().to_ascii_lowercase()) {
            Some(v) if v == "true" => true,
            Some(v) if v == "false" => false,
            _ => default,
        }
    }

    pub async fn info(&self, line: impl AsRef<str>) {
        self.log.line(self.step, line.as_ref()).await;
    }

    pub async fn warn(&self, line: impl AsRef<str>) {
        self.log.line(self.step, &format!("##[warning]{}", line.as_ref())).await;
    }
}

/// A path under the workspace that a step named. Refuses anything that would leave it.
pub fn inside_workspace(workspace: &std::path::Path, rel: &str) -> anyhow::Result<std::path::PathBuf> {
    let rel = rel.trim();
    let p = std::path::Path::new(rel);
    let joined = if p.is_absolute() { p.to_path_buf() } else { workspace.join(p) };
    let mut clean = std::path::PathBuf::new();
    for c in joined.components() {
        match c {
            std::path::Component::ParentDir => {
                if !clean.pop() {
                    anyhow::bail!("{rel:?} is outside the workspace");
                }
            }
            std::path::Component::CurDir => {}
            other => clean.push(other.as_os_str()),
        }
    }
    if !clean.starts_with(workspace) {
        anyhow::bail!("{rel:?} is outside the workspace");
    }
    Ok(clean)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn names_resolve_with_or_without_owner_and_version() {
        for u in ["checkout", "actions/checkout@v4", "codehaven/checkout@v1", "Actions/Checkout@main", "checkout@v2"] {
            assert_eq!(resolve(u), Builtin::Checkout, "{u}");
        }
        assert_eq!(resolve("actions/cache@v4"), Builtin::Cache(CacheMode::Both));
        assert_eq!(resolve("actions/cache/restore@v4"), Builtin::Cache(CacheMode::RestoreOnly));
        assert_eq!(resolve("cache/save"), Builtin::Cache(CacheMode::SaveOnly));
        assert_eq!(resolve("actions/upload-artifact@v4"), Builtin::UploadArtifact);
        assert_eq!(resolve("download-artifact"), Builtin::DownloadArtifact);
        assert_eq!(resolve("docker://alpine:3.20"), Builtin::Docker("alpine:3.20".into()));
    }

    #[test]
    fn other_actions_are_refused_with_a_reason() {
        for u in ["actions/setup-node@v4", "someone/checkout@v1", "docker://", "./local-action", "../x", "", "actions/checkout/extra"] {
            assert!(matches!(resolve(u), Builtin::Unsupported(_)), "{u:?} gave {:?}", resolve(u));
        }
        match resolve("actions/setup-node@v4") {
            Builtin::Unsupported(m) => assert!(m.contains("built-in")),
            _ => unreachable!(),
        }
    }

    #[test]
    fn workspace_paths() {
        let w = Path::new("/work/job-1/workspace");
        assert_eq!(inside_workspace(w, "sub/dir").unwrap(), Path::new("/work/job-1/workspace/sub/dir"));
        assert_eq!(inside_workspace(w, ".").unwrap(), w);
        assert_eq!(inside_workspace(w, "a/../b").unwrap(), Path::new("/work/job-1/workspace/b"));
        assert_eq!(inside_workspace(w, "/work/job-1/workspace/x").unwrap(), Path::new("/work/job-1/workspace/x"));
        for bad in ["..", "../x", "a/../../x", "/etc", "/work/job-1/workspace/../other", "/work/job-1/workspace-evil/x"] {
            assert!(inside_workspace(w, bad).is_err(), "{bad:?} must be refused");
        }
    }
}
