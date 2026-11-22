use crate::command::{self, GitCommandLine, Refusal};
use crate::hostkey;
use bytes::Bytes;
use ch_core::model::{ClientInfo, SshIdentity};
use ch_core::services::{GitCaller, GitMode, GitSession};
use ch_core::state::AppState;
use ch_core::{Actor, AuthVia, Error};
use russh::keys::HashAlg;
use russh::server::{Auth, Handler, Msg, Server as _, Session, VerifiedPublicKey};
use russh::{Channel, ChannelId, ChannelMsg, MethodKind};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

/// How many SSH connections may be open at once.
const MAX_CONNECTIONS: usize = 256;
/// How many sessions one connection may have open at once.
const MAX_CHANNELS: usize = 4;

pub async fn serve(state: AppState, cancel: CancellationToken) -> Result<(), String> {
    let addr: SocketAddr = state.config.ssh.bind.parse().map_err(|e| format!("ssh.bind: {e}"))?;
    let listener = TcpListener::bind(addr).await.map_err(|e| format!("binding {addr}: {e}"))?;
    tracing::info!(%addr, "ssh listening");
    serve_on(state, listener, cancel).await
}

/// Serves on an already bound listener until cancelled. Tests bind port 0 and pass it in.
pub async fn serve_on(state: AppState, listener: TcpListener, cancel: CancellationToken) -> Result<(), String> {
    let key = hostkey::load_or_create(&state.config.secrets_dir().join("ssh_host_ed25519"))?;
    tracing::info!(fingerprint = %key.public_key().fingerprint(HashAlg::Sha256), "ssh host key");
    let config = russh::server::Config {
        // Public keys only. A password or keyboard prompt would be one more thing to guess.
        methods: [MethodKind::PublicKey][..].into(),
        max_auth_attempts: 6,
        auth_rejection_time: Duration::from_secs(1),
        auth_rejection_time_initial: Some(Duration::ZERO),
        inactivity_timeout: Some(Duration::from_secs(900)),
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 3,
        window_size: 4 * 1024 * 1024,
        keys: vec![key],
        ..Default::default()
    };
    let mut server = SshServer { state, limit: Arc::new(Semaphore::new(MAX_CONNECTIONS)) };
    let running = server.run_on_socket(Arc::new(config), &listener);
    let handle = running.handle();
    tokio::spawn(async move {
        cancel.cancelled().await;
        handle.shutdown("Codehaven is shutting down".into());
    });
    running.await.map_err(|e| e.to_string())
}

#[derive(Clone)]
struct SshServer {
    state: AppState,
    limit: Arc<Semaphore>,
}

impl russh::server::Server for SshServer {
    type Handler = Conn;

    fn new_client(&mut self, peer: Option<SocketAddr>) -> Conn {
        let permit = self.limit.clone().try_acquire_owned().ok();
        if permit.is_none() {
            tracing::warn!(?peer, "ssh connection limit reached");
        }
        Conn { state: self.state.clone(), peer, ident: None, channels: HashMap::new(), protocol: HashMap::new(), permit }
    }

    fn handle_session_error(&mut self, error: russh::Error) {
        tracing::debug!(%error, "ssh session ended with an error");
    }
}

#[derive(Clone)]
struct Ident {
    caller: GitCaller,
    /// Who to greet in a message.
    label: String,
}

struct Conn {
    state: AppState,
    peer: Option<SocketAddr>,
    ident: Option<Ident>,
    channels: HashMap<ChannelId, Channel<Msg>>,
    protocol: HashMap<ChannelId, String>,
    /// Held for the life of the connection. `None` means the server was full.
    permit: Option<OwnedSemaphorePermit>,
}

impl Conn {
    fn client(&self) -> ClientInfo {
        ClientInfo { ip: self.peer.map(|p| p.ip()).unwrap_or(IpAddr::from([0, 0, 0, 0])), user_agent: Some("ssh".into()) }
    }

    /// One failure message, then a clean close.
    fn finish(session: &mut Session, channel: ChannelId, message: &str, code: u32) -> Result<(), russh::Error> {
        if !message.is_empty() {
            session.extended_data(channel, 1, Bytes::from(format!("{message}\n")))?;
        }
        session.exit_status_request(channel, code)?;
        session.eof(channel)?;
        session.close(channel)?;
        Ok(())
    }
}

/// The words a client sees for a refused open, in the shape git users know.
fn describe(e: &Error) -> String {
    match e {
        Error::NotFound => "ERROR: Repository not found.\nfatal: Could not read from remote repository.".into(),
        Error::Forbidden(m) | Error::Conflict(m) | Error::Unavailable(m) => format!("ERROR: {m}"),
        Error::Unauthorized(_) => "ERROR: Permission denied.".into(),
        Error::Validation(_) => "ERROR: That request is not valid.".into(),
        Error::RateLimited { .. } => "ERROR: Too many requests. Try again shortly.".into(),
        Error::MergeBlocked(_) | Error::Internal(_) => {
            "ERROR: Codehaven hit a problem. Try again, and tell an admin if it keeps happening.".into()
        }
    }
}

impl Handler for Conn {
    type Error = russh::Error;

    async fn auth_publickey(&mut self, _user: &str, key: VerifiedPublicKey<'_>) -> Result<Auth, Self::Error> {
        if self.permit.is_none() {
            return Ok(Auth::reject());
        }
        let fingerprint = key.public_key.fingerprint(HashAlg::Sha256).to_string();
        let found = match self.state.authn.from_ssh_key(&fingerprint).await {
            Ok(f) => f,
            Err(e) => {
                tracing::error!(error = %e, "ssh key lookup failed");
                return Ok(Auth::reject());
            }
        };
        let ident = match found {
            Some(SshIdentity::User(id)) => {
                let label = match self.state.accounts.get_user(id).await {
                    Ok(u) => u.username,
                    Err(_) => format!("user {id}"),
                };
                Ident { caller: GitCaller::Actor(Actor::User { id, is_admin: false, has_2fa: false, via: AuthVia::Ssh }), label }
            }
            Some(SshIdentity::Deploy { repo_id, read_only }) => {
                Ident { caller: GitCaller::Deploy { repo_id, read_only }, label: format!("deploy key for repository {repo_id}") }
            }
            None => {
                tracing::info!(peer = ?self.peer, %fingerprint, "ssh key not recognised");
                return Ok(Auth::reject());
            }
        };
        self.ident = Some(ident);
        Ok(Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if self.ident.is_none() || self.channels.len() >= MAX_CHANNELS {
            // Dropping the handle refuses the channel.
            return Ok(());
        }
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn env_request(&mut self, channel: ChannelId, name: &str, value: &str, session: &mut Session) -> Result<(), Self::Error> {
        // Only the git protocol version is passed along; every other variable is dropped.
        if name == "GIT_PROTOCOL" && value.len() <= 64 {
            self.protocol.insert(channel, value.to_string());
            session.channel_success(channel)?;
        } else {
            session.channel_failure(channel)?;
        }
        Ok(())
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        _col: u32,
        _row: u32,
        _px: u32,
        _py: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_failure(channel)?;
        Ok(())
    }

    async fn shell_request(&mut self, channel: ChannelId, session: &mut Session) -> Result<(), Self::Error> {
        let who = self.ident.as_ref().map(|i| i.label.clone()).unwrap_or_default();
        session.channel_success(channel)?;
        let msg = format!("Hi {who}! You have signed in, but Codehaven does not give shell access. Use git over SSH.");
        Self::finish(session, channel, &msg, 1)
    }

    async fn subsystem_request(&mut self, channel: ChannelId, _name: &str, session: &mut Session) -> Result<(), Self::Error> {
        session.channel_failure(channel)?;
        Ok(())
    }

    async fn exec_request(&mut self, channel: ChannelId, data: &[u8], session: &mut Session) -> Result<(), Self::Error> {
        let Some(ident) = self.ident.clone() else {
            session.channel_failure(channel)?;
            return Ok(());
        };
        session.channel_success(channel)?;
        let parsed = match command::parse(data) {
            Ok(p) => p,
            Err(refusal) => {
                let msg = match refusal {
                    Refusal::NotGit => format!("Hi {}! Codehaven only runs git commands over SSH, not shell commands.", ident.label),
                    Refusal::BadPath => "ERROR: Use a repository path like owner/name.git.".to_string(),
                };
                return Self::finish(session, channel, &msg, 1);
            }
        };
        let Some(chan) = self.channels.remove(&channel) else { return Self::finish(session, channel, "ERROR: No such session.", 1) };
        let state = self.state.clone();
        let client = self.client();
        let protocol = self.protocol.get(&channel).cloned();
        tokio::spawn(async move { run_git(state, ident, parsed, protocol, chan, client).await });
        Ok(())
    }
}

/// Starts git for the session and carries bytes both ways until it exits.
async fn run_git(state: AppState, ident: Ident, cmd: GitCommandLine, protocol: Option<String>, channel: Channel<Msg>, client: ClientInfo) {
    let GitCommandLine { wire, owner, repo } = cmd;
    let opened = state.git_transport.open(ident.caller.clone(), &owner, &repo, wire, GitMode::Interactive, protocol, &client).await;
    let (mut rx, tx) = channel.split();
    let session = match opened {
        Ok(s) => s,
        Err(e) => {
            tracing::info!(user = %ident.label, %owner, %repo, error = %e, "ssh git request refused");
            let _ = tx.extended_data_bytes(1, Bytes::from(format!("{}\n", describe(&e)))).await;
            let _ = tx.exit_status(1).await;
            let _ = tx.eof().await;
            let _ = tx.close().await;
            return;
        }
    };
    tracing::info!(user = %ident.label, %owner, %repo, ?wire, "ssh git session");
    let GitSession { mut stdin, mut stdout, mut stderr, done } = session;
    let mut out = tx.make_writer();
    let mut err = tx.make_writer_ext(Some(1));
    let copy_out = tokio::spawn(async move {
        let r = tokio::io::copy(&mut stdout, &mut out).await;
        let _ = out.shutdown().await;
        r
    });
    let copy_err = tokio::spawn(async move { tokio::io::copy(&mut stderr, &mut err).await });

    // The client's bytes go to git's stdin until it says it is done. The block owns `stdin`
    // and drops it at the end: that closes the pipe, which is what tells git there is no more
    // input (`shutdown` on a child's stdin does not close it).
    let input = async move {
        loop {
            match rx.wait().await {
                Some(ChannelMsg::Data { data }) => {
                    if stdin.write_all(&data).await.is_err() {
                        break;
                    }
                }
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
                Some(_) => {}
            }
        }
        drop(stdin);
    };
    tokio::pin!(input);
    let mut done = done;
    let code = tokio::select! {
        c = &mut done => c,
        _ = &mut input => (&mut done).await,
    };
    // Let the last of git's output reach the client before the channel closes.
    let _ = tokio::time::timeout(Duration::from_secs(10), async {
        let _ = copy_out.await;
        let _ = copy_err.await;
    })
    .await;
    let _ = tx.exit_status(u32::try_from(code).unwrap_or(1)).await;
    let _ = tx.eof().await;
    let _ = tx.close().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use ch_core::services::GitWire;

    #[test]
    fn refusals_read_like_git() {
        assert!(describe(&Error::NotFound).starts_with("ERROR: Repository not found."));
        assert_eq!(describe(&Error::forbidden("This deploy key is read-only.")), "ERROR: This deploy key is read-only.");
        assert!(!describe(&Error::internal(anyhow_like())).contains("secret"));
    }

    fn anyhow_like() -> std::io::Error {
        std::io::Error::other("secret detail")
    }

    #[test]
    fn exec_wires_are_the_three_git_programs() {
        assert_eq!(command::parse(b"git-receive-pack 'a/b'").map(|c| c.wire), Ok(GitWire::ReceivePack));
    }
}
