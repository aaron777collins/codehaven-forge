//! Git: repository records and storage, reads, smart HTTP, the push pipeline with branch
//! protection, merges, web edits and the repository REST endpoints.
//!
//! One concrete service, [`GitCore`], implements `Repos`, `GitService` and `GitTransport`.
//! `build` puts it in `AppState` as those traits and also in `AppState.ext`, so this crate's
//! own routes and workers can reach the concrete type. See ARCHITECTURE.md sections 5.1 and 6
//! and `docs/git.md`.

pub mod api;
pub mod blame;
pub mod command;
pub mod core;
pub mod diff;
pub mod hooks;
pub mod http;
pub mod maintenance;
pub mod pipeline;
pub mod protect;
pub mod read;
pub mod repos;
mod service;
pub mod store;
pub mod transport;
pub mod write;

pub use core::{GitCore, audit_action};

use axum::Router;
use ch_core::state::{AppState, AppStateBuilder, Install, Worker};
use ch_core::{Error, Result};
use std::path::PathBuf;
use std::sync::Arc;

/// Mounted under `/api/v3`: repos, branches, tags, commits, compare, contents, git data.
pub fn api_routes() -> Router<AppState> {
    api::routes()
}

/// Smart HTTP, merged at the root: `/{owner}/{repo}(.git)/info/refs`, `git-upload-pack`,
/// `git-receive-pack`. The gate has already admitted only Basic, token or job credentials.
pub fn http_routes() -> Router<AppState> {
    http::routes()
}

/// The program the hook scripts run: `CODEHAVEN_HOOK_BIN` if set, otherwise this executable.
fn hook_program() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("CODEHAVEN_HOOK_BIN") {
        return Ok(PathBuf::from(p));
    }
    std::env::current_exe().map_err(|e| Error::internal(anyhow::anyhow!("cannot find the codehaven executable for hooks: {e}")))
}

pub struct GitServices {
    pub core: Arc<GitCore>,
}

impl Install for GitServices {
    fn install(self, b: &mut AppStateBuilder) {
        b.repos = self.core.clone();
        b.git = self.core.clone();
        b.git_transport = self.core.clone();
        b.ext.insert(self.core);
    }
}

/// Builds the service. Needs the real permission service in `b.authz`, so identity is built
/// first. Writes the shared hook scripts, so it must run in the server's own data directory.
pub fn build(b: &AppStateBuilder) -> Result<GitServices> {
    let store = store::Store::new(b.config.git_dir());
    store.init_dirs()?;
    store
        .install_hooks(&hook_program()?)
        .map_err(|e| Error::internal(anyhow::anyhow!("writing hook scripts in {}: {e}", store.hooks_dir.display())))?;
    // `Error::Internal` is transparent, so main prints this whole message on stderr.
    if let Err(msg) = store.verify_hooks() {
        return Err(Error::internal(anyhow::anyhow!(
            "refusing to start: {msg}. Branch protection is enforced by git hooks and git silently skips a hook it cannot run, so pushes would bypass it. Put storage.data_dir on a filesystem that allows executing files (not noexec) and make sure the hook scripts are executable."
        )));
    }
    let socket = store.run_dir.join(format!("hook-{}-{}.sock", std::process::id(), hex::encode(rand::random::<[u8; 4]>())));
    let core = GitCore {
        cfg: b.config.clone(),
        db: b.db.clone(),
        audit: b.audit.clone(),
        events: b.events.clone(),
        authz: b.authz.clone(),
        store,
        pushes: hooks::PushRegistry::default(),
        hook_socket: socket,
        procs: tokio::sync::Semaphore::new(48),
        pushed: maintenance::Pushed::default(),
        diff_cache: moka::future::Cache::builder().max_capacity(256).time_to_live(std::time::Duration::from_secs(600)).build(),
    };
    Ok(GitServices { core: Arc::new(core) })
}

/// Removes socket files nobody is listening on, left by a crashed server.
fn sweep_stale_sockets(dir: &std::path::Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with("hook-") && name.ends_with(".sock") && std::os::unix::net::UnixStream::connect(e.path()).is_err() {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// The hook socket and housekeeping. The socket is bound here, before the server accepts any
/// request, so a push can never find nothing listening.
pub fn workers(state: &AppState) -> Vec<Worker> {
    let Some(core) = state.ext.get::<GitCore>() else { return Vec::new() };
    sweep_stale_sockets(&core.store.run_dir);
    let mut out = Vec::new();
    match std::os::unix::net::UnixListener::bind(&core.hook_socket).and_then(|l| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&core.hook_socket, std::fs::Permissions::from_mode(0o600))?;
        l.set_nonblocking(true)?;
        Ok(l)
    }) {
        Ok(listener) => {
            let c = core.clone();
            out.push(Worker::new("git-hooks", move |cancel| async move {
                match tokio::net::UnixListener::from_std(listener) {
                    Ok(l) => hooks::serve(c, l, cancel).await,
                    Err(e) => tracing::error!(error = %e, "hook socket could not start"),
                }
            }));
        }
        Err(e) => tracing::error!(error = %e, socket = %core.hook_socket.display(), "cannot bind the hook socket: pushes will be refused"),
    }
    let c = core.clone();
    out.push(Worker::new("git-housekeeping", move |cancel| maintenance::run(c, cancel)));
    out
}
