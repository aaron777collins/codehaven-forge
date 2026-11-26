//! OCI distribution registry at `/v2/` (ARCHITECTURE.md section 10).
//!
//! Every push needs a credential, and so does every pull except one: while the instance allows
//! public repositories, a signed-out client may pull an image linked to a public repository
//! (ch-4pn.13). `/v2/token` without credentials then answers with the literal pull-only token
//! `anonymous`, which the gate treats as no credential at all and lets through only for such an
//! image or the catalog. Everything else signed out gets the gate's Bearer challenge, the same
//! for private and missing. `docker login` works with an account name and a personal access
//! token: the client asks `/v2/token` with Basic credentials and the answer is the token it
//! already gave, which it then sends as a Bearer token. Images are `owner/image`.
//!
//! Layers live in the shared content-addressed blob store, so identical layers are stored once
//! across images. Uploads can be chunked, monolithic, or mounted from another image the caller
//! can read. A manifest is accepted only when everything it references is already in the image.

pub mod access;
pub mod blobs;
pub mod catalog;
pub mod error;
pub mod gc;
pub mod manifests;
pub mod names;
pub mod nuget;
pub mod packages;
pub use ch_core::range;

use access::{Access, authorize};
use axum::Router;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use ch_core::Actor;
use ch_core::access::CurrentActor;
use ch_core::model::ClientInfo;
use ch_core::state::{AppState, Worker};
use error::{RegError, RegResult};
use names::Op;
use std::collections::HashMap;
use std::time::Duration;

/// The token a signed-out client gets from `/v2/token`. It is not a secret and names nobody.
pub const ANONYMOUS_TOKEN: &str = "anonymous";

/// Merged at the root: `/v2/...` and `/v2/token`. The gate answers a signed-out request with a
/// Bearer challenge before any handler runs.
pub fn registry_routes() -> Router<AppState> {
    Router::new()
        .route("/v2", get(api_root))
        .route("/v2/", get(api_root))
        .route("/v2/token", get(token))
        .route("/v2/{*rest}", any(dispatch))
        .merge(nuget::routes())
}

/// Upload cleanup and blob garbage collection, once an hour.
pub fn workers(state: &AppState) -> Vec<Worker> {
    let state = state.clone();
    vec![Worker::new("registry-gc", move |cancel| async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_secs(3600)) => {}
            }
            match gc::sweep(&state).await {
                Ok(s) if s != gc::Swept::default() => tracing::info!(?s, "registry garbage collected"),
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "registry garbage collection failed"),
            }
        }
    })]
}

fn client_of(ext: Option<axum::Extension<ClientInfo>>) -> ClientInfo {
    ext.map(|e| e.0).unwrap_or_else(ClientInfo::local)
}

/// `GET /v2/`: the version check clients make first.
async fn api_root() -> Response {
    let mut r = (StatusCode::OK, "{}").into_response();
    r.headers_mut().insert("docker-distribution-api-version", HeaderValue::from_static("registry/2.0"));
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    r
}

/// `GET /v2/token`: the token exchange Docker clients do after the Bearer challenge. The caller
/// has already proved who they are with Basic credentials (the gate checked), so the answer is
/// the secret they sent, which the gate also accepts as a Bearer token. Nothing new is minted,
/// so there is nothing to leak, expire early or revoke separately. What the caller may do is
/// decided on every request, not here.
async fn token(
    headers: axum::http::HeaderMap,
    CurrentActor(actor): CurrentActor,
    public: Option<axum::Extension<ch_core::access::PublicRead>>,
) -> RegResult<Response> {
    use base64::Engine;
    let auth = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).unwrap_or("");
    let secret = if actor.is_anonymous() {
        // The gate let a signed-out request in only with no credential at all, and only while
        // public repositories are allowed. Whatever scope was asked, the answer is the same
        // pull-only token, so it says nothing about which images exist. Reached any other way,
        // it fails closed.
        if !auth.is_empty() || public.is_none() {
            return Err(RegError::denied("sign in with your account name and a personal access token"));
        }
        ANONYMOUS_TOKEN.to_string()
    } else if let Some(b64) = auth.strip_prefix("Basic ").or_else(|| auth.strip_prefix("basic ")) {
        let raw = base64::engine::general_purpose::STANDARD.decode(b64.trim()).map_err(|_| RegError::denied("bad credentials"))?;
        let text = String::from_utf8(raw).map_err(|_| RegError::denied("bad credentials"))?;
        text.split_once(':').map(|(_, s)| s.to_string()).ok_or_else(|| RegError::denied("bad credentials"))?
    } else if let Some(t) = auth.strip_prefix("Bearer ").or_else(|| auth.strip_prefix("bearer ")) {
        t.trim().to_string()
    } else {
        return Err(RegError::denied("sign in with your account name and a personal access token"));
    };
    let issued_at = chrono::Utc::now().to_rfc3339();
    let body = serde_json::json!({ "token": secret, "access_token": secret, "expires_in": 300, "issued_at": issued_at });
    let mut r = (StatusCode::OK, body.to_string()).into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(r)
}

async fn dispatch(
    State(state): State<AppState>,
    CurrentActor(actor): CurrentActor,
    client: Option<axum::Extension<ClientInfo>>,
    Path(rest): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    req: Request,
) -> Response {
    match handle(&state, &actor, &client_of(client), &rest, &q, req).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

async fn handle(
    state: &AppState,
    actor: &Actor,
    client: &ClientInfo,
    rest: &str,
    q: &HashMap<String, String>,
    req: Request,
) -> RegResult<Response> {
    // Signed out, only what the gate let in: a read of an image linked to a public repository,
    // or the catalog. `authorize` checks the repository again.
    if actor.is_anonymous()
        && (req.extensions().get::<ch_core::access::PublicRead>().is_none() || !matches!(*req.method(), Method::GET | Method::HEAD))
    {
        return Err(RegError::denied("authentication required"));
    }
    let (parts, body) = req.into_parts();
    if rest == "_catalog" {
        if parts.method != Method::GET && parts.method != Method::HEAD {
            return Err(RegError::unsupported(StatusCode::METHOD_NOT_ALLOWED, "that method is not supported here"));
        }
        return catalog::get(state, actor, q, parts.method == Method::HEAD).await;
    }
    let (name, op) = names::parse(rest).ok_or_else(|| RegError::unsupported(StatusCode::NOT_FOUND, "that is not a registry endpoint"))?;
    let head = parts.method == Method::HEAD;
    let get_like = parts.method == Method::GET || head;
    let method = parts.method.clone();
    let not_allowed = || RegError::unsupported(StatusCode::METHOD_NOT_ALLOWED, "that method is not supported here");

    match op {
        Op::Blob { digest } => {
            if get_like {
                let pkg = authorize(state, actor, &name, Access::Pull, false, client).await?;
                let range = parts.headers.get(header::RANGE).and_then(|v| v.to_str().ok());
                blobs::get(state, &pkg, &digest, head, range).await
            } else if method == Method::DELETE {
                let pkg = authorize(state, actor, &name, Access::Push, false, client).await?;
                blobs::delete(state, &pkg, &digest).await
            } else {
                Err(not_allowed())
            }
        }
        Op::UploadStart => {
            if method != Method::POST {
                return Err(not_allowed());
            }
            let pkg = authorize(state, actor, &name, Access::Push, true, client).await?;
            blobs::start(state, actor, client, &pkg, q, body).await
        }
        Op::Upload { id } => {
            let pkg = authorize(state, actor, &name, Access::Push, false, client).await?;
            match method {
                Method::PATCH => blobs::patch(state, actor, &pkg, &id, &parts.headers, body).await,
                Method::PUT => blobs::put(state, actor, &pkg, &id, q, body).await,
                Method::GET => blobs::status(state, actor, &pkg, &id).await,
                Method::DELETE => blobs::cancel(state, actor, &pkg, &id).await,
                _ => Err(not_allowed()),
            }
        }
        Op::Manifest { reference } => {
            if get_like {
                let pkg = authorize(state, actor, &name, Access::Pull, false, client).await?;
                manifests::get(state, &pkg, &reference, head).await
            } else if method == Method::PUT {
                let pkg = authorize(state, actor, &name, Access::Push, true, client).await?;
                let ct = parts.headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
                manifests::put(state, &pkg, &reference, ct, body).await
            } else if method == Method::DELETE {
                let pkg = authorize(state, actor, &name, Access::Push, false, client).await?;
                manifests::delete(state, &pkg, &reference).await
            } else {
                Err(not_allowed())
            }
        }
        Op::TagsList => {
            if !get_like {
                return Err(not_allowed());
            }
            let pkg = authorize(state, actor, &name, Access::Pull, false, client).await?;
            manifests::tags(state, &pkg, q).await
        }
        Op::Referrers { digest } => {
            if !get_like {
                return Err(not_allowed());
            }
            let pkg = authorize(state, actor, &name, Access::Pull, false, client).await?;
            manifests::referrers(state, &pkg, &digest, q.get("artifactType").map(String::as_str)).await
        }
    }
}
