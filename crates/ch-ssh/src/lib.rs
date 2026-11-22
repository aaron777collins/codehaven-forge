//! SSH server for git (`russh`): public key auth, exec of `git-upload-pack`,
//! `git-receive-pack` and `git-upload-archive` only (ARCHITECTURE.md section 6.3).
//!
//! The key a client offers is turned into a fingerprint and looked up through `Authn`, so
//! account keys and deploy keys both work, and a key that was removed stops working at once.
//! What happens after a command is accepted is `GitTransport::open`, the same call smart
//! HTTP makes, so permissions, hooks and the push pipeline are identical. A shell, a pty, a
//! subsystem or any other command gets a short message naming the user, and the session ends.

pub mod command;
pub mod hostkey;
mod server;

pub use server::{serve, serve_on};

use ch_core::state::{AppState, Worker};

/// The SSH listener on `ssh.bind`, started when `ssh.enabled` is on.
pub fn workers(state: &AppState) -> Vec<Worker> {
    if !state.config.ssh.enabled {
        return Vec::new();
    }
    let state = state.clone();
    vec![Worker::new("ssh", move |cancel| async move {
        if let Err(e) = serve(state, cancel).await {
            tracing::error!(error = %e, "the SSH server could not start: git over SSH is off");
        }
    })]
}
