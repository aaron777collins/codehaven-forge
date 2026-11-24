//! Routes the foundation stream owns: `/healthz`, `/robots.txt` and the API meta group.
//! `/healthz` and `/robots.txt` are public and say almost nothing. The API meta routes need
//! a signed-in caller like every other API route.

use axum::Router;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Json, extract::State};
use ch_api_types::common::{RateLimit, RateLimitResource, RateLimitResources, VersionInfo};
use ch_core::access::ApiQuota;
use ch_core::state::AppState;
use serde_json::json;

pub fn public_routes() -> Router<AppState> {
    Router::new().route("/healthz", get(healthz)).route("/robots.txt", get(robots))
}

/// Mounted under `/api/v3`.
pub fn api_routes() -> Router<AppState> {
    Router::new().route("/", get(root)).route("/version", get(version)).route("/meta", get(meta)).route("/rate_limit", get(rate_limit))
}

/// The process is up. Says `ok` and nothing else, so it leaks nothing.
async fn healthz() -> Response {
    let mut r = (StatusCode::OK, "ok").into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

async fn robots() -> Response {
    let mut r = (StatusCode::OK, "User-agent: *\nDisallow: /\n").into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    r
}

async fn root(State(s): State<AppState>) -> Json<serde_json::Value> {
    let base = &s.config.server.external_url;
    Json(json!({
        "current_user_url": format!("{base}/api/v3/user"),
        "rate_limit_url": format!("{base}/api/v3/rate_limit"),
        "repository_url": format!("{base}/api/v3/repos/{{owner}}/{{repo}}"),
    }))
}

async fn version() -> Json<VersionInfo> {
    Json(VersionInfo { version: env!("CARGO_PKG_VERSION").into(), api_version: "v3".into() })
}

async fn meta(State(s): State<AppState>) -> Json<serde_json::Value> {
    // A tunnel-only address is no use to whoever reads /meta, so it is left out entirely.
    let ssh = (s.config.ssh.enabled && !s.config.ssh.tunnel_only()).then(|| s.config.ssh.external.clone());
    Json(json!({
        "installed_version": env!("CARGO_PKG_VERSION"),
        "ssh": ssh,
        "external_url": s.config.server.external_url,
    }))
}

/// The caller's API quota, the same numbers the guard sends in the `X-RateLimit-*` headers of
/// this response. Search has no separate budget here, so it reports its own fixed ceiling.
async fn rate_limit(quota: Option<Extension<ApiQuota>>) -> Json<RateLimit> {
    let core = match quota {
        Some(Extension(q)) => RateLimitResource { limit: q.limit, remaining: q.remaining, reset: q.reset, used: q.limit - q.remaining },
        None => RateLimitResource { limit: 5000, remaining: 5000, reset: 0, used: 0 },
    };
    Json(RateLimit {
        resources: RateLimitResources { core: core.clone(), search: RateLimitResource { limit: 30, remaining: 30, reset: 0, used: 0 } },
        rate: core,
    })
}
