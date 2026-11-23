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
        let mut child = cmd.timeout(std::time::Duration::from_secs(deadline)).spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| Error::internal(anyhow::anyhow!("git has no stdin")))?;
        let stdout = child.stdout.take().ok_or_else(|| Error::internal(anyhow::anyhow!("git has no stdout")))?;
        let stderr = child.stderr.take().ok_or_else(|| Error::internal(anyhow::anyhow!("git has no stderr")))?;
        let repo_id = repo.id;
        let done = Box::pin(async move {
            // Holding the guard to the end keeps the push id valid until git has exited.
            let _guard = guard;
            let _ticket = ticket;
            // `spawn` does not apply the command's timeout, so the deadline is enforced here.
            let code = match tokio::time::timeout(std::time::Duration::from_secs(deadline), child.wait()).await {
                Ok(status) => status.ok().and_then(|s| s.code()).unwrap_or(-1),
                Err(_) => {
                    tracing::warn!(repo = repo_id.0, deadline, "git transport ran past its deadline and was stopped");
                    let _ = child.kill().await;
                    -1
                }
            };
            tracing::debug!(repo = repo_id.0, code, "git transport finished");
            code
        });
        Ok(GitSession { stdin: Box::new(stdin), stdout: Box::new(stdout), stderr: Box::new(stderr), done })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
