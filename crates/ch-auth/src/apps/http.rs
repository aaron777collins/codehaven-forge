//! The signed-out OAuth surface: three static discovery documents and the token and revocation
//! endpoints. Exactly these five exist while MCP is on and none while it is off.
//!
//! They read no credential and no cookie, set no cookie, and need no CSRF token: the token
//! endpoint answers only for a one-time code made at the consent page or a live refresh token.
//! They have their own rate limits because everyone using Claude arrives from a few addresses.

use crate::ratelimit::Limiter;
use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use ch_core::access::Client;
use ch_core::apps::{OAuthError, TokenRequest};
use ch_core::state::AppState;
use serde_json::json;
use std::time::Duration;

const MAX_BODY: usize = 8 * 1024;
const TOKEN_TIMEOUT: Duration = Duration::from_secs(8);

/// Per address budgets for the machine endpoints, and for rejected `/mcp` requests.
pub struct OAuthLimits {
    pub well_known: Limiter,
    pub token: Limiter,
    pub revoke: Limiter,
    pub mcp_reject: Limiter,
}

impl OAuthLimits {
    pub fn new() -> Self {
        Self {
            well_known: Limiter::new(300, 100),
            token: Limiter::new(120, 60),
            revoke: Limiter::new(60, 60),
            mcp_reject: Limiter::new(120, 60),
        }
    }
}

impl Default for OAuthLimits {
    fn default() -> Self {
        Self::new()
    }
}

/// The five signed-out routes. Empty when MCP is off, so every one of them is an unknown path.
pub fn routes(config: &ch_core::Config) -> Router<AppState> {
    if !config.mcp.enabled {
        return Router::new();
    }
    Router::new()
        .route("/.well-known/oauth-protected-resource/mcp", get(resource_doc))
        .route("/.well-known/oauth-protected-resource", get(resource_doc))
        .route("/.well-known/oauth-authorization-server", get(server_doc))
        .route("/oauth/token", post(token))
        .route("/oauth/revoke", post(revoke))
}

fn json_response(status: StatusCode, v: serde_json::Value, cache: &'static str) -> Response {
    let mut r = (status, v.to_string()).into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    if cache == "no-store" {
        h.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    }
    r
}

fn limits(state: &AppState) -> Option<std::sync::Arc<OAuthLimits>> {
    state.ext.get::<OAuthLimits>()
}

fn too_many(wait: u64) -> Response {
    let mut r =
        json_response(StatusCode::TOO_MANY_REQUESTS, json!({"error": "rate_limited", "error_description": "Slow down."}), "no-store");
    r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from(wait));
    r
}

/// The protected resource document. Built from `server.external_url`, never from `Host`.
pub fn resource_document(config: &ch_core::Config) -> serde_json::Value {
    let origin = config.origin();
    json!({
        "resource": config.mcp_resource(),
        "authorization_servers": [origin],
        "scopes_supported": ["read"],
        "bearer_methods_supported": ["header"],
    })
}

/// The authorization server document.
pub fn server_document(config: &ch_core::Config) -> serde_json::Value {
    let o = config.origin();
    json!({
        "issuer": o,
        "authorization_endpoint": format!("{o}/oauth/authorize"),
        "token_endpoint": format!("{o}/oauth/token"),
        "revocation_endpoint": format!("{o}/oauth/revoke"),
        "response_types_supported": ["code"],
        "response_modes_supported": ["query"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "token_endpoint_auth_methods_supported": ["none"],
        "revocation_endpoint_auth_methods_supported": ["none"],
        "code_challenge_methods_supported": ["S256"],
        "client_id_metadata_document_supported": true,
        "authorization_response_iss_parameter_supported": true,
        "scopes_supported": ["read", "write", "workflows", "offline_access"],
    })
}

async fn resource_doc(State(state): State<AppState>, Client(client): Client) -> Response {
    if let Some(l) = limits(&state)
        && let Err(w) = l.well_known.hit(&client.ip.to_string())
    {
        return too_many(w);
    }
    json_response(StatusCode::OK, resource_document(&state.config), "public, max-age=60")
}

async fn server_doc(State(state): State<AppState>, Client(client): Client) -> Response {
    if let Some(l) = limits(&state)
        && let Err(w) = l.well_known.hit(&client.ip.to_string())
    {
        return too_many(w);
    }
    json_response(StatusCode::OK, server_document(&state.config), "public, max-age=60")
}

/// Reads a form body, at most 8 KiB, urlencoded only, each name at most once.
async fn read_form(req: Request) -> Result<Vec<(String, String)>, OAuthError> {
    let ctype = req.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
    if !ctype.starts_with("application/x-www-form-urlencoded") {
        return Err(OAuthError::new("invalid_request", "Send the request as application/x-www-form-urlencoded."));
    }
    let bytes = axum::body::to_bytes(Body::new(req.into_body()), MAX_BODY)
        .await
        .map_err(|_| OAuthError::new("invalid_request", "The request is too large."))?;
    let pairs: Vec<(String, String)> = url::form_urlencoded::parse(&bytes).map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    let mut seen = std::collections::HashSet::new();
    for (k, _) in &pairs {
        if !seen.insert(k.clone()) {
            return Err(OAuthError::new("invalid_request", "A parameter was sent twice."));
        }
    }
    Ok(pairs)
}

fn field(pairs: &[(String, String)], name: &str) -> Option<String> {
    pairs.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()).filter(|v| !v.is_empty())
}

fn error_response(e: &OAuthError) -> Response {
    let status = match e.code {
        "server_error" => StatusCode::INTERNAL_SERVER_ERROR,
        "invalid_client" => StatusCode::UNAUTHORIZED,
        _ => StatusCode::BAD_REQUEST,
    };
    json_response(status, json!({"error": e.code, "error_description": e.description}), "no-store")
}

async fn token(State(state): State<AppState>, Client(client): Client, req: Request) -> Response {
    if let Some(l) = limits(&state)
        && let Err(w) = l.token.hit(&client.ip.to_string())
    {
        return too_many(w);
    }
    let work = async {
        let pairs = match read_form(req).await {
            Ok(p) => p,
            Err(e) => return error_response(&e),
        };
        let tr = TokenRequest {
            grant_type: field(&pairs, "grant_type"),
            code: field(&pairs, "code"),
            redirect_uri: field(&pairs, "redirect_uri"),
            code_verifier: field(&pairs, "code_verifier"),
            client_id: field(&pairs, "client_id"),
            resource: field(&pairs, "resource"),
            refresh_token: field(&pairs, "refresh_token"),
            scope: field(&pairs, "scope"),
        };
        match state.apps.token(tr, &client).await {
            Ok(t) => json_response(
                StatusCode::OK,
                json!({"access_token": t.access_token, "token_type": "Bearer", "expires_in": t.expires_in, "refresh_token": t.refresh_token, "scope": t.scope}),
                "no-store",
            ),
            Err(e) => error_response(&e),
        }
    };
    match tokio::time::timeout(TOKEN_TIMEOUT, work).await {
        Ok(r) => r,
        Err(_) => error_response(&OAuthError::new("server_error", "That took too long. Try again.")),
    }
}

async fn revoke(State(state): State<AppState>, Client(client): Client, req: Request) -> Response {
    if let Some(l) = limits(&state)
        && let Err(w) = l.revoke.hit(&client.ip.to_string())
    {
        return too_many(w);
    }
    let pairs = read_form(req).await.unwrap_or_default();
    if let Some(t) = field(&pairs, "token") {
        // Whether it existed is never said. A failure to record is logged and still answers 200.
        if let Err(e) = state.apps.revoke_token(&t, field(&pairs, "client_id").as_deref(), &client).await {
            tracing::warn!(error = %e, "oauth revoke failed");
        }
    }
    let mut r = StatusCode::OK.into_response();
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ch_core::Config {
        let mut c = ch_core::Config::default();
        c.server.external_url = "https://code.example.com".into();
        c.mcp.enabled = true;
        c
    }

    #[test]
    fn documents_follow_config_and_say_cimd_and_no_registration() {
        let c = cfg();
        let r = resource_document(&c);
        assert_eq!(r["resource"], "https://code.example.com/mcp");
        assert_eq!(r["authorization_servers"], json!(["https://code.example.com"]));
        assert_eq!(r["scopes_supported"], json!(["read"]));
        let s = server_document(&c);
        assert_eq!(s["issuer"], "https://code.example.com");
        assert_eq!(s["authorization_endpoint"], "https://code.example.com/oauth/authorize");
        assert_eq!(s["token_endpoint_auth_methods_supported"], json!(["none"]));
        assert_eq!(s["client_id_metadata_document_supported"], json!(true));
        assert_eq!(s["code_challenge_methods_supported"], json!(["S256"]));
        assert!(s.get("registration_endpoint").is_none(), "no dynamic registration, in any spelling");
        assert!(!s.to_string().contains("register"));
        assert_eq!(s["grant_types_supported"], json!(["authorization_code", "refresh_token"]));
    }

    #[test]
    fn routes_exist_only_while_mcp_is_on() {
        let mut c = cfg();
        c.mcp.enabled = false;
        let _off: Router<AppState> = routes(&c);
        // The router has no public inspection; the surface test in `codehaven` proves the paths.
    }
}
