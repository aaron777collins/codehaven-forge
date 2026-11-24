//! Works out who is really calling: the client address and scheme, honouring
//! `X-Forwarded-*` only from trusted proxies (ARCHITECTURE.md section 2.4). Also removes
//! every identity header a proxy might add, so "no proxy-header login" holds structurally.

use axum::extract::{ConnectInfo, Request, State};
use axum::middleware::Next;
use axum::response::Response;
use ch_core::model::ClientInfo;
use ch_core::net;
use ch_core::state::AppState;
use std::net::{IpAddr, SocketAddr};

/// Per-request facts every later layer and handler can read from the extensions.
#[derive(Debug, Clone)]
pub struct RequestMeta {
    pub client: ClientInfo,
    /// The original request was https, from a trusted proxy's say-so.
    pub secure: bool,
}

/// Headers an auth proxy sets. Codehaven never reads them.
pub const IDENTITY_HEADERS: &[&str] =
    &["remote-user", "remote-groups", "remote-email", "remote-name", "x-remote-user", "x-forwarded-user", "x-forwarded-email"];

pub async fn request_info(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let (ip, secure) = client_of(&state, &req);
    let user_agent = req.headers().get(axum::http::header::USER_AGENT).and_then(|v| v.to_str().ok()).map(|s| s.chars().take(300).collect());

    for h in IDENTITY_HEADERS {
        req.headers_mut().remove(*h);
    }
    let client = ClientInfo { ip, user_agent };
    // Domain crates read it with the `ch_core::access::Client` extractor.
    req.extensions_mut().insert(client.clone());
    req.extensions_mut().insert(RequestMeta { client, secure });
    next.run(req).await
}

/// The client address, honouring `X-Forwarded-For` only from a trusted proxy, and whether the
/// original request was https.
pub fn client_of(state: &AppState, req: &Request) -> (IpAddr, bool) {
    let peer: IpAddr = req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip()).unwrap_or(IpAddr::from([0, 0, 0, 0]));
    let trusted = match state.ext.get::<ch_core::net::ProxyHosts>() {
        Some(h) => h.trusted(&state.config.http.trusted_proxies),
        None => state.config.http.trusted_proxies.clone(),
    };
    let xff = req.headers().get_all("x-forwarded-for").iter().filter_map(|v| v.to_str().ok()).collect::<Vec<_>>().join(",");
    let xfp = req.headers().get("x-forwarded-proto").and_then(|v| v.to_str().ok());
    let ip = net::client_ip(peer, if xff.is_empty() { None } else { Some(&xff) }, &trusted);
    (ip, net::is_secure(peer, xfp, &trusted))
}

/// Hands the request to the identity guard (CSRF, Origin check, rate limits) with the trusted
/// client address. It sits inside the gate, so the caller is already in the extensions.
pub async fn auth_guard(State(guard): State<std::sync::Arc<ch_auth::Guard>>, req: Request, next: Next) -> Response {
    let ip = req.extensions().get::<RequestMeta>().map(|m| m.client.ip).unwrap_or(IpAddr::from([0, 0, 0, 0]));
    guard.handle(req, next, ip).await
}
