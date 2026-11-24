//! Starting `git upload-pack`, `receive-pack` and `upload-archive` for a client
//! (ARCHITECTURE.md sections 6.2 and 6.3). Smart HTTP and SSH both come through
//! [`GitTransport::open`], so permission checks, hooks and the push pipeline are the same.

use crate::core::GitCore;
use crate::hooks::{ENV_PUSH_ID, ENV_REPO_ID, ENV_SOCKET, PushCtx, PushGuard};
use crate::protect::Origin;
use crate::store::repo_config;
use async_trait::async_trait;
use ch_core::model::ClientInfo;
use ch_core::services::{GitCaller, GitMode, GitSession, GitTransport, GitWire};
use ch_core::{Actor, Error, Permission, Result};

/// `GIT_PROTOCOL` carries `version=2` and friends. Nothing else is passed through.
fn clean_protocol(p: Option<String>) -> Option<String> {
    p.filter(|v| {
        !v.is_empty()
            && v.len() <= 64
            && v.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'=' | b':' | b',' | b'.' | b'_' | b'-'))
    })
}

fn subcommand(wire: GitWire) -> &'static str {
    match wire {
        GitWire::UploadPack => "upload-pack",
        GitWire::ReceivePack => "receive-pack",
        GitWire::UploadArchive => "upload-archive",
    }
}

#[async_trait]
impl GitTransport for GitCore {
    async fn open(
        &self,
        caller: GitCaller,
        owner: &str,
        repo_name: &str,
        wire: GitWire,
        mode: GitMode,
        protocol: Option<String>,
        client: &ClientInfo,
    ) -> Result<GitSession> {
        let repo = self.repo_by_name_unchecked(owner, repo_name).await?;
        let writing = wire == GitWire::ReceivePack;
        let (actor, deploy_write) = match &caller {
            GitCaller::Actor(actor) => {
                // Reading takes `View`, which a signed-out visitor has on a public repository.
                let needed = if writing { Permission::Write } else { Permission::View };
                if !actor.is_system() {
                    self.authz.require(actor, repo.id, needed).await?;
                }
                (actor.clone(), None)
            }
            GitCaller::Deploy { repo_id, read_only } => {
                if *repo_id != repo.id {
                    return Err(Error::NotFound);
                }
                if writing && *read_only {
                    return Err(Error::forbidden("This deploy key is read-only."));
                }
                (Actor::System, Some(!*read_only))
            }
        };
        if writing && repo.archived {
            return Err(Error::forbidden("This repository is archived and read-only."));
        }
        // Branch protection lives in the hooks, and git silently skips a hook it cannot
        // execute, so a push is refused outright while they cannot run. Every mode, even the
        // advertisement, so the client learns before sending a pack. Reads are unaffected.
        if writing && let Err(msg) = self.store.verify_hooks() {
            tracing::error!(repo = repo.id.0, %msg, "refusing a push: the git hooks cannot run");
            return Err(Error::unavailable(
                "Pushes are disabled: the server cannot run its branch protection hooks. Ask the administrator to run codehaven doctor.",
            ));
        }
        if !self.store.exists(repo.id) {
            return Err(Error::internal(anyhow::anyhow!("repository {} has no files on disk", repo.id)));
        }
        // A push may bring up to 2 GiB. It is refused, before any of it arrives, when the data
        // disk could not take that and still keep its floor free (ch-ctk.2).
        if writing {
            let floor = self.cfg.storage.min_free_gib.saturating_mul(1 << 30);
            ch_db::blobs::check_floor(&self.store.repo_path(repo.id), floor, if mode == GitMode::AdvertiseRefs { 0 } else { 1 << 30 })?;
        }
        if mode != GitMode::Interactive && wire == GitWire::UploadArchive {
            return Err(Error::invalid("Git", "service", "upload-archive is only available over SSH"));
        }

        let mut cmd = self.store.bare_cmd();
        for (k, v) in repo_config(&self.store.hooks_dir) {
            cmd = cmd.args(["-c".to_string(), format!("{k}={v}")]);
        }
        cmd = cmd.args(["-c", "receive.maxInputSize=2147483648"]);
        // A signed-out clone of a public repository gets a smaller share of the server: one pack
        // thread, bounded delta memory and a shorter deadline (spec section 7).
        let stranger = matches!(&caller, GitCaller::Actor(a) if a.is_anonymous());
        if stranger {
            cmd = cmd.args(["-c", "pack.threads=1", "-c", "pack.windowMemory=64m", "-c", "core.deltaBaseCacheLimit=64m"]);
        }
        cmd = cmd.arg(subcommand(wire));
        match mode {
            GitMode::Interactive => {}
            GitMode::AdvertiseRefs => cmd = cmd.args(["--stateless-rpc", "--advertise-refs"]),
            GitMode::StatelessRpc => cmd = cmd.arg("--stateless-rpc"),
        }
        cmd = cmd.arg(self.store.repo_path(repo.id));
        if let Some(p) = clean_protocol(protocol) {
            cmd = cmd.env("GIT_PROTOCOL", p);
        }

        // Only a push that can change refs needs the hook plumbing, and only the part of an
        // HTTP push that carries the pack (the advertisement runs no hooks).
        let mut guard: Option<PushGuard> = None;
        let mut ticket = None;
        if writing && mode != GitMode::AdvertiseRefs {
            // A store rebuild waits for this push to end, and a push waits for a rebuild.
            ticket = Some(self.writes.write(repo.id).await?);
            let g = self.pushes.register(PushCtx {
                actor,
                repo: repo.id,
                client: client.clone(),
                origin: Origin::Push,
                deploy_write,
                server_built: false,
            });
            cmd = cmd.env(ENV_PUSH_ID, g.id()).env(ENV_REPO_ID, repo.id.0.to_string()).env(ENV_SOCKET, self.hook_socket.to_string_lossy());
            guard = Some(g);
        }

        let deadline = if stranger { self.cfg.public_access.git_timeout_secs.max(1) } else { 3600 };
        let stall = if stranger { STRANGER_STALL_SECS } else { MEMBER_STALL_SECS };
        // A signed-in transfer takes one of a fixed number of places. Signed-out ones were
        // already counted by the gate's own pools (ch-ulq).
        let permit = if stranger {
            None
        } else {
            Some(
                self.transfers
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| Error::unavailable("Too many Git transfers are running right now. Try again in a minute."))?,
            )
        };
        let mut child = cmd.timeout(std::time::Duration::from_secs(deadline)).own_group().spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| Error::internal(anyhow::anyhow!("git has no stdin")))?;
        let stdout = child.stdout.take().ok_or_else(|| Error::internal(anyhow::anyhow!("git has no stdout")))?;
        let stderr = child.stderr.take().ok_or_else(|| Error::internal(anyhow::anyhow!("git has no stderr")))?;
        let repo_id = repo.id;
        let clock = Clock::shared();
        let stdin = Progress::new(stdin, clock.clone());
        let stdout = Progress::new(stdout, clock.clone());
        let done = Box::pin(async move {
            // Holding the guard to the end keeps the push id valid until git has exited.
            let _guard = guard;
            let _ticket = ticket;
            let _permit = permit;
            let pid = child.id();
            // Whatever happens to this future, git and everything it started end with it.
            let mut group = GroupKill(pid);
            // `spawn` does not apply the command's timeout, so the deadline is enforced here,
            // together with a limit on how long no byte may move either way.
            let started = std::time::Instant::now();
            let code = loop {
                tokio::select! {
                    status = child.wait() => {
                        // Reaped: its id may be reused from now on, so it is never signalled.
                        group.0 = None;
                        break status.ok().and_then(|s| s.code()).unwrap_or(-1);
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                        let idle = clock.idle_secs();
                        if started.elapsed().as_secs() >= deadline || idle >= stall {
                            tracing::warn!(repo = repo_id.0, deadline, idle, "git transport ran past its deadline or stalled and was stopped");
                            crate::command::kill_group(pid);
                            let _ = child.kill().await;
                            group.0 = None;
                            break -1;
                        }
                    }
                }
            };
            tracing::debug!(repo = repo_id.0, code, "git transport finished");
            code
        });
        Ok(GitSession { stdin: Box::new(stdin), stdout: Box::new(stdout), stderr: Box::new(stderr), done })
    }
}

/// Signed-in clones, fetches and pushes that may run at once.
pub(crate) const MAX_TRANSFERS: usize = 32;
/// How long a transfer may go with no byte moving either way. `upload-pack` sends a keepalive
/// every 5 seconds while it prepares a pack, so only a client that stopped reading or sending
/// gets near these.
const STRANGER_STALL_SECS: u64 = 60;
const MEMBER_STALL_SECS: u64 = 600;

/// Kills the process group when dropped: a dropped connection ends git's children too.
struct GroupKill(Option<u32>);

impl Drop for GroupKill {
    fn drop(&mut self) {
        crate::command::kill_group(self.0);
    }
}

/// A pipe end that notes the time whenever a byte moves through it.
struct Progress<T> {
    inner: T,
    clock: std::sync::Arc<Clock>,
}

struct Clock {
    start: std::time::Instant,
    last_ms: std::sync::atomic::AtomicU64,
}

impl Clock {
    fn touch(&self) {
        let ms = u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last_ms.store(ms, std::sync::atomic::Ordering::Relaxed);
    }
    fn idle_secs(&self) -> u64 {
        let now = u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX);
        now.saturating_sub(self.last_ms.load(std::sync::atomic::Ordering::Relaxed)) / 1000
    }
}

impl Clock {
    fn shared() -> std::sync::Arc<Clock> {
        std::sync::Arc::new(Clock { start: std::time::Instant::now(), last_ms: std::sync::atomic::AtomicU64::new(0) })
    }
}

impl<T> Progress<T> {
    fn new(inner: T, clock: std::sync::Arc<Clock>) -> Self {
        Self { inner, clock }
    }
}

impl<T: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for Progress<T> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let r = std::pin::Pin::new(&mut self.inner).poll_read(cx, buf);
        if buf.filled().len() > before {
            self.clock.touch();
        }
        r
    }
}

impl<T: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for Progress<T> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let r = std::pin::Pin::new(&mut self.inner).poll_write(cx, buf);
        if let std::task::Poll::Ready(Ok(n)) = &r
            && *n > 0
        {
            self.clock.touch();
        }
        r
    }
    fn poll_flush(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_pipe_that_moves_bytes_resets_the_stall_clock() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let clock = Clock::shared();
        let (a, mut b) = tokio::io::duplex(64);
        let mut reader = Progress::new(a, clock.clone());
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        assert!(clock.idle_secs() >= 1, "nothing moved yet");
        b.write_all(b"hello").await.unwrap();
        let mut buf = [0u8; 5];
        reader.read_exact(&mut buf).await.unwrap();
        assert_eq!(clock.idle_secs(), 0, "a read counts as progress");
    }

    #[test]
    fn protocol_values_are_filtered() {
        assert_eq!(clean_protocol(Some("version=2".into())), Some("version=2".into()));
        assert_eq!(clean_protocol(Some("version=2:object-format=sha1".into())), Some("version=2:object-format=sha1".into()));
        assert_eq!(clean_protocol(Some("a b".into())), None);
        assert_eq!(clean_protocol(Some("x\ny".into())), None);
        assert_eq!(clean_protocol(Some(String::new())), None);
        assert_eq!(clean_protocol(Some("v".repeat(65))), None);
        assert_eq!(clean_protocol(None), None);
    }
}
