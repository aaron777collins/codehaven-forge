//! Actions that are not built in (ch-ctk.4): JavaScript and composite actions from GitHub, from a
//! repository on this instance, or from the job's own workspace.
//!
//! The server decides which commit an action is and whether the site allows it (ch-ctk.5). The
//! runner downloads that archive with the job's token, checks its hash, unpacks it with only
//! regular files and folders under `_runner/actions/<archive>` (so the job container sees it at
//! the same path) and reads its `action.yml`. Running it is the job engine's part.

use crate::client::Client;
use crate::jobenv::JobDirs;
use anyhow::{Context, Result, anyhow, bail};
use ch_api_types::runner::JobPayload;
use ch_workflow::action_def::ActionDef;
use sha2::Digest;
use std::path::{Path, PathBuf};

/// Composite actions nested deeper than this are refused, as on GitHub.
pub const MAX_DEPTH: u32 = 9;

/// An action ready to run.
#[derive(Debug, Clone)]
pub struct Fetched {
    /// The folder holding `action.yml`.
    pub dir: PathBuf,
    /// `owner/repo` for `GITHUB_ACTION_REPOSITORY`, empty for a local action.
    pub repository: String,
    /// The ref as written, for `GITHUB_ACTION_REF`.
    pub git_ref: String,
    pub sha: Option<String>,
    pub def: ActionDef,
}

/// Fetches and unpacks `uses` (a `Builtin::Remote`), or answers with why it cannot run.
pub async fn fetch_remote(client: &Client, dirs: &JobDirs, payload: &JobPayload, uses: &str) -> Result<Fetched> {
    let answer = client
        .resolve_actions(&payload.token, payload.job_id, &[uses.to_string()])
        .await
        .map_err(|e| anyhow!("the server could not resolve {uses}: {e}"))?;
    let r = answer.actions.into_iter().next().ok_or_else(|| anyhow!("the server did not answer for {uses}"))?;
    if let Some(e) = r.error {
        bail!("{e}");
    }
    let (Some(sha), Some(archive)) = (r.sha, r.archive) else { bail!("the server did not resolve {uses}") };
    if archive.len() != 64 || !archive.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("the server answered with a bad archive name for {uses}");
    }
    let dest = dirs.runner.join("actions").join(&archive);
    if !dest.is_dir() {
        let file = dirs.private.join(format!("action-{archive}.tar.gz"));
        client.download_action(&payload.token, payload.job_id, &archive, &file).await.map_err(|e| anyhow!("downloading {uses}: {e}"))?;
        let got = hash_file(&file)?;
        if !got.eq_ignore_ascii_case(&archive) {
            let _ = std::fs::remove_file(&file);
            bail!("{uses} did not arrive intact: its hash does not match");
        }
        let unpacked = super::cache::unpack(&file, &dirs.root, &dest).with_context(|| format!("unpacking {uses}"));
        let _ = std::fs::remove_file(&file);
        unpacked?;
    }
    let (repository, git_ref) = match ch_workflow::uses::parse_uses(uses) {
        Ok(ch_workflow::uses::Uses::Repo { host, owner, repo, git_ref, .. }) => {
            let prefix = if host == ch_workflow::uses::Host::Haven { "haven_" } else { "" };
            (format!("{prefix}{owner}/{repo}"), git_ref)
        }
        _ => (String::new(), String::new()),
    };
    let top = single_top_dir(&dest)?;
    let dir = if r.path.is_empty() { top } else { inside(&top, &r.path)? };
    let def = read_def(dirs, &dir, uses)?;
    Ok(Fetched { dir, repository, git_ref, sha: Some(sha), def })
}

/// `./path` in the workspace.
pub fn fetch_local(dirs: &JobDirs, uses: &str) -> Result<Fetched> {
    let rel = uses.trim().trim_start_matches("./");
    let dir = super::inside_workspace(&dirs.workspace, rel)?;
    let def = read_def(dirs, &dir, uses)?;
    Ok(Fetched { dir, repository: String::new(), git_ref: String::new(), sha: None, def })
}

fn hash_file(p: &Path) -> Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(p)?;
    let mut h = sha2::Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

/// Archives keep their files under one folder (`action/`, or `<repo>-<sha>/`).
fn single_top_dir(dest: &Path) -> Result<PathBuf> {
    let mut dirs = Vec::new();
    for e in std::fs::read_dir(dest)? {
        let e = e?;
        if e.file_type()?.is_dir() {
            dirs.push(e.path());
        } else {
            return Ok(dest.to_path_buf());
        }
    }
    Ok(if dirs.len() == 1 { dirs.remove(0) } else { dest.to_path_buf() })
}

fn inside(base: &Path, rel: &str) -> Result<PathBuf> {
    let mut out = base.to_path_buf();
    for seg in rel.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." {
            bail!("the action's path {rel} is not inside it");
        }
        out.push(seg);
    }
    Ok(out)
}

/// `action.yml`, else `action.yaml`, read without following a link.
fn read_def(dirs: &JobDirs, dir: &Path, uses: &str) -> Result<ActionDef> {
    let mut text = None;
    for name in ["action.yml", "action.yaml"] {
        let p = dir.join(name);
        if let Ok(t) = crate::safefs::read_to_string(&dirs.root, &p, crate::jobenv::MAX_STEP_FILE) {
            text = Some(t);
            break;
        }
    }
    let Some(text) = text else { bail!("{uses} has no action.yml") };
    ch_workflow::parse::parse_action(&text)
        .map_err(|d| anyhow!("{uses}: action.yml: {}", d.iter().map(|x| x.to_string()).collect::<Vec<_>>().join("; ")))
}

/// `ACTIONS_RUNTIME_TOKEN`: a JWT the Actions toolkit reads its run and job ids from (its
/// `scp` claim), signed with the sha256 of the job token, which is the key the server checks it
/// with (`ch_db::ci_auth::job_from_runtime_token`). It works while the job runs, at most until
/// `exp`.
pub fn runtime_token(payload: &JobPayload, valid_for: std::time::Duration) -> String {
    use base64::Engine;
    use hmac::Mac;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let now = chrono::Utc::now().timestamp();
    let claims = serde_json::json!({
        "iss": "codehaven-runner",
        "sub": format!("job:{}", payload.job_id),
        "scp": format!("Actions.Results:{}:{} Actions.UploadArtifacts", payload.run_id, payload.job_id),
        "iat": now,
        "exp": now + valid_for.as_secs() as i64,
    });
    let input = format!("{}.{}", b64.encode(br#"{"alg":"HS256","typ":"JWT"}"#), b64.encode(claims.to_string()));
    let key = sha2::Sha256::digest(payload.token.as_bytes());
    let Ok(mut mac) = hmac::Hmac::<sha2::Sha256>::new_from_slice(&key) else { return String::new() };
    mac.update(input.as_bytes());
    format!("{input}.{}", b64.encode(mac.finalize().into_bytes()))
}

/// The environment name GitHub gives an input: `INPUT_` and the name in upper case with spaces
/// as underscores.
pub fn input_env(name: &str) -> String {
    format!("INPUT_{}", name.replace(' ', "_").to_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_names_follow_github() {
        assert_eq!(input_env("node-version"), "INPUT_NODE-VERSION");
        assert_eq!(input_env("my input"), "INPUT_MY_INPUT");
    }

    #[test]
    fn an_action_path_never_leaves_the_archive() {
        let base = Path::new("/w/a");
        assert_eq!(inside(base, "sub/dir").unwrap(), Path::new("/w/a/sub/dir"));
        for bad in ["..", "a/../../x", "", "a//b"] {
            assert!(inside(base, bad).is_err(), "{bad}");
        }
    }
}
