//! Assembles the public router: routes, then the gate, then request info, then security
//! headers, then tracing. Layers added later are outer, so a request meets them in the
//! order: trace, headers, request info, gate, route.

use crate::gate::gate;
use crate::headers::security_headers_layer;
use crate::metrics::{self, Metrics};
use crate::reqinfo::request_info;
use crate::wiring;
use axum::Router;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::middleware::{from_fn, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use ch_core::access::{self, RouteKind};
use ch_core::state::AppState;
use std::sync::Arc;
use std::time::Duration;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::trace::TraceLayer;

/// The public application with every layer. `extra` lets tests add probe routes that sit
/// behind the same gate as the real ones.
pub fn build_router(state: AppState, metrics: Arc<Metrics>, extra: Option<Router<AppState>>) -> Router {
    let mut routes = wiring::routes(&state.config);
    if let Some(extra) = extra {
        routes = routes.merge(extra);
    }
    let x_request_id = axum::http::HeaderName::from_static("x-request-id");
    // CSRF, the Origin check and rate limits. Inside the gate, so the caller is resolved.
    let guard = Arc::new(ch_auth::Guard::new(&state.config));
    // The pages host never reaches the app: picked by `Host` before the gate, the guard and the
    // app's headers (docs/design/pages.md, section 2).
    let pages = ch_pages::host_router(&state).zip(state.config.pages.host());
    routes
        // The second lock on signed-out public reads: the router must pick the route the gate
        // admitted. A route layer, so it sees the matched pattern.
        .route_layer(from_fn_with_state(state.clone(), crate::public_gate::route_check))
        .with_state(state.clone())
        .layer(from_fn_with_state(guard, crate::reqinfo::auth_guard))
        .layer(from_fn_with_state(state.clone(), gate))
        .layer(from_fn_with_state(state.clone(), request_info))
        .layer(from_fn_with_state(state.clone(), security_headers_layer))
        .layer(from_fn_with_state(metrics, metrics::count))
        .layer(from_fn_with_state(state.clone(), request_timeout))
        .layer(from_fn_with_state(state.clone(), move |State(state): State<AppState>, req: Request, next: Next| {
            let pages = pages.clone();
            async move { pages_or_app(&state, pages, req, next).await }
        }))
        .layer(TraceLayer::new_for_http())
        .layer(PropagateRequestIdLayer::new(x_request_id.clone()))
        .layer(SetRequestIdLayer::new(x_request_id, MakeRequestUuid))
        .layer(from_fn(no_server_header))
}

/// Sends a request for the pages host, or one label under it, to the pages router. Everything
/// else goes on to the app. The pages router gets the trusted client address and nothing
/// else of the app's request info.
async fn pages_or_app(state: &AppState, pages: Option<(Router, String)>, mut req: Request, next: Next) -> Response {
    use tower::ServiceExt;
    if let Some((router, host)) = pages
        && ch_pages::host::request_host(&req).is_some_and(|h| ch_pages::host::is_pages_host(&host, &h))
    {
        let (ip, _) = crate::reqinfo::client_of(state, &req);
        req.extensions_mut().insert(ch_core::model::ClientInfo { ip, user_agent: None });
        return match router.oneshot(req).await {
            Ok(r) => r,
            Err(never) => match never {},
        };
    }
    next.run(req).await
}

/// Never say what we run on.
async fn no_server_header(req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    let mut resp = next.run(req).await;
    resp.headers_mut().remove(axum::http::header::SERVER);
    resp
}

/// Limits how long a page or API call may take to produce its response. Git transfers, the
/// registry and the runner API are exempt: a large push legitimately takes minutes.
async fn request_timeout(State(state): State<AppState>, req: Request, next: Next) -> Response {
    // Page, API and the MCP routes are timed. Git transfers, the registry and the runner API are
    // exempt: a large push legitimately takes minutes.
    // A release file upload is exempt too: it is bounded by its idle time between chunks instead
    // (`http.upload_idle_secs`, enforced in the handler), not by the clock on the whole request.
    let exempt = is_release_upload(req.method(), req.uri().path())
        || !matches!(
            access::classify_for(req.uri().path(), state.config.mcp.enabled),
            RouteKind::Page | RouteKind::Api | RouteKind::Mcp | RouteKind::OAuth
        );
    if exempt {
        return next.run(req).await;
    }
    let limit = Duration::from_secs(state.config.http.request_timeout_secs.max(1));
    match tokio::time::timeout(limit, next.run(req)).await {
        Ok(resp) => resp,
        Err(_) => (StatusCode::GATEWAY_TIMEOUT, "Timed out").into_response(),
    }
}

/// A release file upload (ch-ctk.10, ch-ctk.11): `POST /api/v3/repos/{owner}/{repo}/releases/{id}/assets`
/// from a tool, or `POST /{owner}/{repo}/releases/{id}/assets` from the release page.
fn is_release_upload(method: &axum::http::Method, path: &str) -> bool {
    if method != axum::http::Method::POST {
        return false;
    }
    let seg: Vec<&str> = path.trim_matches('/').split('/').collect();
    let id_ok = |id: &&str| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit());
    match seg.as_slice() {
        ["api", "v3", "repos", o, r, "releases", id, "assets"] => !o.is_empty() && !r.is_empty() && id_ok(id),
        [o, r, "releases", id, "assets"] => !o.is_empty() && !r.is_empty() && id_ok(id),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::is_release_upload;
    use axum::http::Method;

    #[test]
    fn only_the_release_upload_is_exempt_from_the_request_clock() {
        let up = "/api/v3/repos/o/r/releases/12/assets";
        assert!(is_release_upload(&Method::POST, up));
        assert!(!is_release_upload(&Method::GET, up));
        assert!(!is_release_upload(&Method::POST, "/api/v3/repos/o/r/releases/12/assets/3"));
        assert!(!is_release_upload(&Method::POST, "/api/v3/repos/o/r/releases/latest/assets"));
        assert!(!is_release_upload(&Method::POST, "/api/v3/repos/o/r/issues/12/assets"));
        assert!(!is_release_upload(&Method::POST, "/api/v3/repos/o/r/releases/12"));
        // The release page's own upload form.
        assert!(is_release_upload(&Method::POST, "/o/r/releases/12/assets"));
        assert!(!is_release_upload(&Method::POST, "/o/r/releases/12/assets/3/delete"));
        assert!(!is_release_upload(&Method::POST, "/o/r/releases/new/assets"));
    }
}
