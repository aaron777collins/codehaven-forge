//! The push pipeline's wire (ARCHITECTURE.md section 6.4).
//!
//! `git receive-pack` runs the shared `pre-receive` and `post-receive` scripts, which exec
//! `codehaven hook <stage>`. That program is [`client::run`]: it reads the ref updates git
//! gives it on stdin and sends them over a Unix socket to the server, which decides
//! (pre-receive) or records (post-receive). The server finds out who is pushing from the push
//! id it put in the environment when it started git, so a hook run by hand, or by anything
//! that is not a push the server started, is refused.

use crate::core::GitCore;
use crate::protect::Origin;
use ch_core::model::ClientInfo;
use ch_core::{Actor, RepoId};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

pub const ENV_PUSH_ID: &str = "CODEHAVEN_PUSH_ID";
pub const ENV_REPO_ID: &str = "CODEHAVEN_REPO_ID";
pub const ENV_SOCKET: &str = "CODEHAVEN_SOCKET";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Pre,
    Post,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefUpdate {
    pub old: String,
    pub new: String,
    #[serde(rename = "ref")]
    pub name: String,
    /// The new commit does not descend from the old one. Worked out by the hook, which runs
    /// with the pushed objects in reach.
    pub forced: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookRequest {
    pub push_id: String,
    pub repo_id: i64,
    pub stage: Stage,
    pub updates: Vec<RefUpdate>,
    /// Where git keeps the pushed objects until pre-receive accepts them (`GIT_QUARANTINE_PATH`).
    /// The server reads the pushed commits from there; without it they look missing (ch-s7r).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quarantine: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookResponse {
    pub ok: bool,
    /// Shown to the person pushing, line by line, prefixed `remote:` by git.
    pub message: String,
}

/// What the server knows about a push it started.
#[derive(Debug, Clone)]
pub struct PushCtx {
    pub actor: Actor,
    pub repo: RepoId,
    pub client: ClientInfo,
    pub origin: Origin,
    /// A deploy key pushing. `Some(true)` when it may write. Deploy keys are not accounts, so
    /// they are checked here and not by the permission service.
    pub deploy_write: Option<bool>,
    /// The server wrote the new commit on top of the branch for the actor (see `RefChange`).
    /// Always false for a client's push.
    pub server_built: bool,
}

/// Pushes in flight, by push id.
#[derive(Clone, Default)]
pub struct PushRegistry(Arc<Mutex<HashMap<String, Arc<PushCtx>>>>);

/// Removes the push from the registry when dropped, so a finished push's id stops working.
pub struct PushGuard {
    id: String,
    registry: PushRegistry,
}

impl PushGuard {
    pub fn id(&self) -> &str {
        &self.id
    }
}

impl Drop for PushGuard {
    fn drop(&mut self) {
        if let Ok(mut m) = self.registry.0.lock() {
            m.remove(&self.id);
        }
    }
}

impl PushRegistry {
    pub fn register(&self, ctx: PushCtx) -> PushGuard {
        let id = ch_db::tokens::random_hex_256();
        if let Ok(mut m) = self.0.lock() {
            m.insert(id.clone(), Arc::new(ctx));
        }
        PushGuard { id, registry: self.clone() }
    }

    pub fn get(&self, id: &str) -> Option<Arc<PushCtx>> {
        self.0.lock().ok()?.get(id).cloned()
    }

    pub fn len(&self) -> usize {
        self.0.lock().map(|m| m.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The quarantine directory a pre-receive hook named, when it really is one inside this
/// repository's objects directory: `objects/tmp_objdir-incoming-*`, the name git gives it. Both
/// paths are resolved first, so a link or `..` cannot point the check somewhere else.
pub(crate) fn valid_quarantine(objects: &std::path::Path, given: &str) -> Option<std::path::PathBuf> {
    let objects = std::fs::canonicalize(objects).ok()?;
    let q = std::fs::canonicalize(given).ok()?;
    let named = q.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("tmp_objdir-incoming-"));
    (named && q.parent() == Some(objects.as_path()) && q.is_dir()).then_some(q)
}

impl GitCore {
    /// The checked quarantine directory of a push into `repo`, or `None` for anything else.
    pub(crate) fn quarantine_dir(&self, repo: RepoId, given: &str) -> Option<std::path::PathBuf> {
        valid_quarantine(&self.store.repo_path(repo).join("objects"), given)
    }
}

/// Answers hook connections until cancelled.
pub async fn serve(core: Arc<GitCore>, listener: tokio::net::UnixListener, cancel: CancellationToken) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let core = core.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle(core, stream).await {
                                tracing::warn!(error = %e, "hook connection failed");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "hook socket accept failed");
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    }
                }
            }
        }
    }
    let _ = std::fs::remove_file(&core.hook_socket);
}

/// A push with ten thousand tags is still well under this.
const MAX_REQUEST: u64 = 16 * 1024 * 1024;

async fn handle(core: Arc<GitCore>, stream: tokio::net::UnixStream) -> std::io::Result<()> {
    let (r, mut w) = stream.into_split();
    let mut line = Vec::new();
    BufReader::new(r.take(MAX_REQUEST)).read_until(b'\n', &mut line).await?;
    let resp = match serde_json::from_slice::<HookRequest>(&line) {
        Ok(req) => respond(&core, req).await,
        Err(_) => HookResponse { ok: false, message: "Codehaven could not read the hook request.".into() },
    };
    let mut out = serde_json::to_vec(&resp).unwrap_or_else(|_| b"{\"ok\":false,\"message\":\"error\"}".to_vec());
    out.push(b'\n');
    w.write_all(&out).await?;
    w.shutdown().await
}

async fn respond(core: &Arc<GitCore>, req: HookRequest) -> HookResponse {
    let Some(ctx) = core.pushes.get(&req.push_id) else {
        return HookResponse {
            ok: false,
            message: "Refs can only be updated through Codehaven. This hook was not started by a push the server knows about.".into(),
        };
    };
    if ctx.repo.0 != req.repo_id {
        return HookResponse { ok: false, message: "This push does not belong to this repository.".into() };
    }
    match req.stage {
        Stage::Pre => {
            // A quarantine that is not this repository's own is refused, not ignored: the checks
            // would otherwise read the wrong objects.
            let quarantine = match req.quarantine.as_deref() {
                None => None,
                Some(q) => match core.quarantine_dir(ctx.repo, q) {
                    Some(dir) => Some(dir),
                    None => return HookResponse { ok: false, message: "Codehaven could not read the pushed objects.".into() },
                },
            };
            match crate::pipeline::authorize(core, &ctx, &req.updates, quarantine.as_deref()).await {
                Ok(()) => HookResponse { ok: true, message: String::new() },
                Err(message) => HookResponse { ok: false, message },
            }
        }
        Stage::Post => {
            if let Err(e) = crate::pipeline::record(core, &ctx, &req.updates).await {
                tracing::error!(error = %e, repo = ctx.repo.0, "recording a push failed");
            }
            HookResponse { ok: true, message: String::new() }
        }
    }
}

/// The program git runs. Synchronous and dependency-light: it runs on every push.
pub mod client {
    use super::*;
    use std::io::{BufRead, Read, Write};
    use std::os::unix::net::UnixStream;
    use std::process::{Command, Stdio};
    use std::time::Duration;

    /// True when `new` is a descendant of `old`. Anything unclear counts as not.
    fn is_fast_forward(old: &str, new: &str) -> bool {
        Command::new("git")
            .args(["merge-base", "--is-ancestor", old, new])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Reads `<old> <new> <ref>` lines, the way git hands them to both hooks.
    pub fn parse_updates(input: impl BufRead) -> Vec<RefUpdate> {
        let zero = crate::core::ZERO_SHA;
        let mut out = Vec::new();
        for line in input.lines().map_while(Result::ok) {
            let mut it = line.splitn(3, ' ');
            let (Some(old), Some(new), Some(name)) = (it.next(), it.next(), it.next()) else { continue };
            // Object ids go to `git merge-base` below, so they must really be object ids.
            let hex = |s: &str| (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit());
            if !hex(old) || !hex(new) {
                continue;
            }
            let forced = old != zero && new != zero && !is_fast_forward(old, new);
            out.push(RefUpdate { old: old.into(), new: new.into(), name: name.trim().into(), forced });
        }
        out
    }

    /// Runs one stage. Returns the process exit code: 0 lets the push continue.
    pub fn run(stage: &str) -> i32 {
        let stage_kind = match stage {
            "pre-receive" => Stage::Pre,
            "post-receive" => Stage::Post,
            _ => {
                eprintln!("codehaven hook: unknown stage {stage}");
                return 2;
            }
        };
        let (Ok(push_id), Ok(repo_id), Ok(socket)) = (std::env::var(ENV_PUSH_ID), std::env::var(ENV_REPO_ID), std::env::var(ENV_SOCKET))
        else {
            eprintln!("Refs can only be updated through Codehaven.");
            return 1;
        };
        let Ok(repo_id) = repo_id.parse::<i64>() else {
            eprintln!("codehaven hook: bad repository id");
            return 1;
        };
        let updates = parse_updates(std::io::stdin().lock());
        let quarantine = std::env::var("GIT_QUARANTINE_PATH").ok().filter(|q| !q.is_empty());
        let req = HookRequest { push_id, repo_id, stage: stage_kind, updates, quarantine };
        match exchange(&socket, &req) {
            Ok(resp) => {
                if !resp.message.is_empty() {
                    eprintln!("{}", resp.message);
                }
                // A failure after the refs moved cannot undo the push, so post-receive
                // always succeeds.
                if resp.ok || stage_kind == Stage::Post { 0 } else { 1 }
            }
            Err(e) => {
                eprintln!("Codehaven is not answering ({e}). The push was not accepted.");
                if stage_kind == Stage::Pre { 1 } else { 0 }
            }
        }
    }

    fn exchange(socket: &str, req: &HookRequest) -> std::io::Result<HookResponse> {
        let mut s = UnixStream::connect(socket)?;
        s.set_read_timeout(Some(Duration::from_secs(120)))?;
        s.set_write_timeout(Some(Duration::from_secs(30)))?;
        let mut body = serde_json::to_vec(req).map_err(std::io::Error::other)?;
        body.push(b'\n');
        s.write_all(&body)?;
        s.flush()?;
        let mut reply = String::new();
        std::io::BufReader::new(s.take(1024 * 1024)).read_line(&mut reply)?;
        serde_json::from_str(&reply).map_err(std::io::Error::other)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parses_ref_lines() {
            let z = crate::core::ZERO_SHA;
            let input = format!("{z} {} refs/heads/new\n{} {z} refs/heads/old\n", "a".repeat(40), "b".repeat(40));
            let u = parse_updates(input.as_bytes());
            assert_eq!(u.len(), 2);
            assert_eq!(u[0].name, "refs/heads/new");
            assert!(!u[0].forced, "creation is never a force");
            assert!(!u[1].forced, "deletion is never a force");
        }

        #[test]
        fn ignores_junk_lines() {
            assert!(parse_updates("not a ref line\n\n".as_bytes()).is_empty());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> PushCtx {
        PushCtx {
            actor: Actor::Anonymous,
            repo: RepoId(1),
            client: ClientInfo::local(),
            origin: Origin::Push,
            deploy_write: None,
            server_built: false,
        }
    }

    #[test]
    fn only_a_quarantine_inside_the_repositorys_objects_counts() {
        let dir = tempfile::tempdir().unwrap();
        let objects = dir.path().join("1.git/objects");
        let q = objects.join("tmp_objdir-incoming-abc123");
        std::fs::create_dir_all(&q).unwrap();
        std::fs::create_dir_all(objects.join("pack")).unwrap();
        let other = dir.path().join("2.git/objects/tmp_objdir-incoming-xyz");
        std::fs::create_dir_all(&other).unwrap();
        let s = |p: &std::path::Path| p.to_string_lossy().into_owned();
        assert_eq!(valid_quarantine(&objects, &s(&q)), Some(std::fs::canonicalize(&q).unwrap()));
        assert_eq!(valid_quarantine(&objects, &s(&objects.join("pack"))), None, "not a quarantine");
        assert_eq!(valid_quarantine(&objects, &s(&other)), None, "another repository's");
        let sneaky = format!("{}/../../../2.git/objects/tmp_objdir-incoming-xyz", s(&q));
        assert_eq!(valid_quarantine(&objects, &sneaky), None, "a path that climbs out");
        assert_eq!(valid_quarantine(&objects, &s(&objects.join("tmp_objdir-incoming-gone"))), None, "missing");
        assert_eq!(valid_quarantine(&objects, ""), None);
    }

    #[test]
    fn push_ids_work_only_while_the_guard_lives() {
        let reg = PushRegistry::default();
        let guard = reg.register(ctx());
        let id = guard.id().to_string();
        assert_eq!(id.len(), 64);
        assert!(reg.get(&id).is_some());
        assert_eq!(reg.len(), 1);
        drop(guard);
        assert!(reg.get(&id).is_none());
        assert!(reg.is_empty());
        assert!(reg.get("guess").is_none());
    }

    #[test]
    fn ids_are_unique() {
        let reg = PushRegistry::default();
        let a = reg.register(ctx());
        let b = reg.register(ctx());
        assert_ne!(a.id(), b.id());
    }
}
