//! Smart HTTP (ARCHITECTURE.md section 6.2):
//!
//! ```text
//! GET  /{owner}/{repo}(.git)/info/refs?service=git-upload-pack|git-receive-pack
//! POST /{owner}/{repo}(.git)/git-upload-pack
//! POST /{owner}/{repo}(.git)/git-receive-pack
//! ```
//!
//! The gate has already checked credentials, so a handler here
//! has a signed-in caller, except a clone or fetch of a public repository, which the gate
//! admits signed out.
//! Bodies are streamed in both directions and never held whole.

use async_compression::tokio::bufread::GzipDecoder;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bytes::Bytes;
use ch_core::access::CurrentActor;
use ch_core::model::ClientInfo;
use ch_core::services::{GitCaller, GitMode, GitSession, GitWire};
use ch_core::state::AppState;
use ch_core::{Error, Result};
use futures::StreamExt;
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::io::{ReaderStream, StreamReader};

/// A push may be large. A fetch request is only a list of wants and haves.
const MAX_PUSH_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_FETCH_REQUEST_BYTES: u64 = 64 * 1024 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/{owner}/{repo}/info/refs", get(info_refs))
        .route("/{owner}/{repo}/git-upload-pack", post(upload_pack))
        .route("/{owner}/{repo}/git-receive-pack", post(receive_pack))
}

#[derive(Deserialize)]
struct RefsQuery {
    service: Option<String>,
}

fn no_cache(mut r: Response) -> Response {
    let h = r.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache, max-age=0, must-revalidate"));
    h.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    h.insert(header::EXPIRES, HeaderValue::from_static("Fri, 01 Jan 1980 00:00:00 GMT"));
    r
}

/// Errors for git clients are plain text, which `git` prints after `fatal:` or `remote:`.
fn git_error(e: Error) -> Response {
    let status = match &e {
        Error::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        other => other.status(),
    };
    let text = match &e {
        Error::NotFound => "Repository not found.".to_string(),
        other => other.public_message(),
    };
    if let Error::Internal(err) = &e {
        tracing::error!(error = ?err, "git http error");
    }
    let mut r = (status, format!("{text}\n")).into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    if status == StatusCode::UNAUTHORIZED {
        r.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Basic realm=\"Codehaven\""));
    }
    no_cache(r)
}

fn git_protocol(headers: &HeaderMap) -> Option<String> {
    headers.get("git-protocol").and_then(|v| v.to_str().ok()).map(str::to_string)
}

fn client_of(ext: Option<axum::Extension<ClientInfo>>) -> ClientInfo {
    ext.map(|e| e.0).unwrap_or_else(ClientInfo::local)
}

fn wire_of(service: &str) -> Option<GitWire> {
    match service {
        "git-upload-pack" => Some(GitWire::UploadPack),
        "git-receive-pack" => Some(GitWire::ReceivePack),
        _ => None,
    }
}

/// `001e# service=git-upload-pack\n0000`
fn service_header(service: &str) -> Vec<u8> {
    let line = format!("# service={service}\n");
    format!("{:04x}{line}0000", line.len() + 4).into_bytes()
}

async fn info_refs(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    client: Option<axum::Extension<ClientInfo>>,
    Path((owner, repo)): Path<(String, String)>,
    Query(q): Query<RefsQuery>,
    headers: HeaderMap,
) -> Response {
    let Some(service) = q.service.as_deref() else {
        return git_error(Error::forbidden("This server only speaks the smart git protocol."));
    };
    let Some(wire) = wire_of(service) else {
        return git_error(Error::forbidden("Unknown git service."));
    };
    let protocol = git_protocol(&headers);
    let v2 = protocol.as_deref().is_some_and(|p| p.contains("version=2"));
    let session = match open(&state, actor, &owner, &repo, wire, GitMode::AdvertiseRefs, protocol, &client_of(client)).await {
        Ok(s) => s,
        Err(e) => return git_error(e),
    };
    let GitSession { stdin, stdout, stderr: _, done } = session;
    drop(stdin);
    tokio::spawn(done);
    let head = if v2 { Vec::new() } else { service_header(service) };
    let body = futures::stream::once(async move { Ok::<_, std::io::Error>(Bytes::from(head)) }).chain(ReaderStream::new(stdout));
    let mut r = Response::new(Body::from_stream(body));
    let ct = format!("application/x-{service}-advertisement");
    if let Ok(v) = HeaderValue::from_str(&ct) {
        r.headers_mut().insert(header::CONTENT_TYPE, v);
    }
    no_cache(r)
}

#[allow(clippy::too_many_arguments)]
async fn open(
    state: &AppState,
    actor: ch_core::Actor,
    owner: &str,
    repo: &str,
    wire: GitWire,
    mode: GitMode,
    protocol: Option<String>,
    client: &ClientInfo,
) -> Result<GitSession> {
    state.git_transport.open(GitCaller::Actor(actor), owner, repo, wire, mode, protocol, client).await
}

async fn upload_pack(
    state: State<AppState>,
    actor: CurrentActor,
    client: Option<axum::Extension<ClientInfo>>,
    path: Path<(String, String)>,
    req: Request,
) -> Response {
    rpc(state.0, actor.0, client, path.0, req, GitWire::UploadPack, "git-upload-pack", MAX_FETCH_REQUEST_BYTES).await
}

async fn receive_pack(
    state: State<AppState>,
    actor: CurrentActor,
    client: Option<axum::Extension<ClientInfo>>,
    path: Path<(String, String)>,
    req: Request,
) -> Response {
    rpc(state.0, actor.0, client, path.0, req, GitWire::ReceivePack, "git-receive-pack", MAX_PUSH_BYTES).await
}

#[allow(clippy::too_many_arguments)]
async fn rpc(
    state: AppState,
    actor: ch_core::Actor,
    client: Option<axum::Extension<ClientInfo>>,
    (owner, repo): (String, String),
    req: Request,
    wire: GitWire,
    service: &'static str,
    limit: u64,
) -> Response {
    let (parts, body) = req.into_parts();
    let want_type = format!("application/x-{service}-request");
    let ct = parts.headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    if !ct.eq_ignore_ascii_case(&want_type) {
        let mut r = (StatusCode::UNSUPPORTED_MEDIA_TYPE, "Unsupported content type.\n").into_response();
        r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
        return no_cache(r);
    }
    let gzip = match parts.headers.get(header::CONTENT_ENCODING).and_then(|v| v.to_str().ok()) {
        None | Some("identity") => false,
        Some("gzip") | Some("x-gzip") => true,
        Some(_) => {
            let mut r = (StatusCode::UNSUPPORTED_MEDIA_TYPE, "Unsupported content encoding.\n").into_response();
            r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
            return no_cache(r);
        }
    };
    let protocol = git_protocol(&parts.headers);
    let session = match open(&state, actor, &owner, &repo, wire, GitMode::StatelessRpc, protocol, &client_of(client)).await {
        Ok(s) => s,
        Err(e) => return git_error(e),
    };
    let GitSession { mut stdin, stdout, stderr, done } = session;

    // Request body to git's stdin. When it ends (or fails, or runs over the limit) stdin is
    // dropped, which tells git there is no more input.
    tokio::spawn(async move {
        let stream = body.into_data_stream().map(|r| r.map_err(std::io::Error::other));
        let reader = StreamReader::new(stream);
        let copied = if gzip {
            tokio::io::copy(&mut GzipDecoder::new(reader).take(limit), &mut stdin).await
        } else {
            tokio::io::copy(&mut reader.take(limit), &mut stdin).await
        };
        if let Err(e) = copied {
            tracing::debug!(error = %e, "git request body ended early");
        }
        let _ = stdin.shutdown().await;
    });
    // Reap git and log why it failed, if it did.
    tokio::spawn(async move {
        let code = done.await;
        if code != 0 {
            let mut text = Vec::new();
            let _ = tokio::time::timeout(std::time::Duration::from_secs(1), stderr.take(2048).read_to_end(&mut text)).await;
            tracing::debug!(code, stderr = %String::from_utf8_lossy(&text), "git rpc exited with an error");
        }
    });

    let mut r = Response::new(Body::from_stream(ReaderStream::new(stdout)));
    if let Ok(v) = HeaderValue::from_str(&format!("application/x-{service}-result")) {
        r.headers_mut().insert(header::CONTENT_TYPE, v);
    }
    no_cache(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_header_is_a_pkt_line_and_a_flush() {
        let h = service_header("git-upload-pack");
        assert_eq!(h, b"001e# service=git-upload-pack\n0000");
        let h = service_header("git-receive-pack");
        assert_eq!(h, b"001f# service=git-receive-pack\n0000");
    }

    #[test]
    fn only_two_services() {
        assert_eq!(wire_of("git-upload-pack"), Some(GitWire::UploadPack));
        assert_eq!(wire_of("git-receive-pack"), Some(GitWire::ReceivePack));
        assert_eq!(wire_of("git-upload-archive"), None);
        assert_eq!(wire_of(""), None);
    }
}
