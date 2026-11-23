//! The deny-by-default gate (ARCHITECTURE.md sections 4.1a and 7.3).
//!
//! It wraps the whole router, so it runs before routing: a route that does not exist and a
//! route that does look exactly the same to a signed-out request. The gate resolves the
//! caller from the credentials in the request, and lets the request through only when
//!
//! 1. the method and path are in `ch_core::access::PUBLIC_ROUTES`, or
//! 2. the caller is signed in with a credential that this kind of route accepts, or
//! 3. the caller sent no credential, the route is on `PUBLIC_REPO_ROUTES`, the instance allows
//!    public repositories and the repository or owner it names is public now (see
//!    `public_gate`).
//!
//! Anything else gets the rejection for its kind. A new route is private the moment it is
//! added and stays private until someone edits `PUBLIC_ROUTES` on purpose.

use crate::reqinfo::RequestMeta;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use ch_core::access::{self, RouteKind};
use ch_core::live::Credential;
use ch_core::model::ClientInfo;
use ch_core::state::AppState;
use ch_core::{Actor, AuthVia, Error};

pub async fn gate(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    // With MCP off the seven MCP paths are ordinary unknown paths: same kind, never public.
    let mcp_on = state.config.mcp.enabled;
    let kind = access::classify_for(&path, mcp_on);
    let public = access::is_public_for(&method, &path, mcp_on);
    let client = req.extensions().get::<RequestMeta>().map(|m| m.client.clone()).unwrap_or_else(ClientInfo::local);

    // The two machine route kinds have their own doors and never touch the cookie or Basic paths.
    match kind {
        RouteKind::OAuth => {
            // No credential is read and no cookie is set. A public route runs, anything else
            // (a wrong method) is refused here, before any handler.
            if public {
                req.extensions_mut().insert(Actor::Anonymous);
                return next.run(req).await;
            }
            return method_not_allowed();
        }
        RouteKind::Mcp => return gate_mcp(&state, req, next, &client).await,
        _ => {}
    }

    // Set when the password was right but the account needs a token. The only refusal that says
    // anything more than "authentication required", and only after the right password.
    let mut hint: Option<String> = None;
    let mut session_hash: Option<Vec<u8>> = None;
    let actor = match resolve_actor(&state, req.headers(), &client, &mut session_hash).await {
        Ok(a) => a,
        Err(Error::RateLimited { retry_after_secs }) => return Error::RateLimited { retry_after_secs }.into_response(),
        // Bad credentials count as no credentials.
        Err(Error::Unauthorized(m)) => {
            if m.starts_with("This account uses two-factor sign-in") {
                hint = Some(m);
            }
            Actor::Anonymous
        }
        Err(Error::NotFound) | Err(Error::Forbidden(_)) => Actor::Anonymous,
        // Fail closed with no detail.
        Err(e) => {
            tracing::error!(error = %e, "authentication failed internally");
            return Error::unavailable("Service unavailable").into_response();
        }
    };

    // A signed-in page may be framed only as the workspace terminal. This also covers public
    // routes, and runs before any handler, so a framed document that navigates itself elsewhere
    // (a redirect, a typed URL) reaches nothing. Signed-out answers do not change.
    if matches!(actor, Actor::User { via: AuthVia::Session, .. }) && framed(req.headers()) && !is_terminal_frame_path(&path) {
        let mut r = (StatusCode::FORBIDDEN, "Framing not allowed\n").into_response();
        r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        return r;
    }

    if access::actor_accepted(&actor, kind) {
        match credential_for(&state, &actor, session_hash).await {
            Ok(Some(c)) => {
                req.extensions_mut().insert(c);
            }
            Ok(None) => {}
            Err(e) => {
                tracing::error!(error = %e, "could not record the credential");
                return Error::unavailable("Service unavailable").into_response();
            }
        }
        req.extensions_mut().insert(actor);
        return next.run(req).await;
    }
    if public {
        req.extensions_mut().insert(Actor::Anonymous);
        return next.run(req).await;
    }
    // Public repositories (epic ch-4pn). Only a request with no credential at all: a bad or
    // unaccepted `Authorization` header never turns into a public read. A cookie that did not
    // sign anyone in is ignored.
    let has_auth_header = req.headers().contains_key(header::AUTHORIZATION);
    if actor.is_anonymous()
        && !has_auth_header
        && hint.is_none()
        && let Some(m) = access::public_repo_route(&method, &path, req.uri().query())
    {
        match crate::public_gate::admit(&state, &m, &access::address_key(client.ip)).await {
            crate::public_gate::Admit::Yes(read, permit) => {
                let cost = read.cost;
                req.extensions_mut().insert(Actor::Anonymous);
                req.extensions_mut().insert(read);
                let resp = next.run(req).await;
                return crate::public_gate::hold_until_sent(crate::public_gate::finish(resp), permit, cost, &state.config.public_access);
            }
            crate::public_gate::Admit::Busy(wait) => return crate::public_gate::busy(wait),
            crate::public_gate::Admit::No => {}
        }
    }
    let mut resp = reject(&state, kind, &method, &path, req.uri().query());
    if let Some(h) = hint
        && matches!(kind, RouteKind::GitHttp | RouteKind::Api)
    {
        // Git prints the body of a refusal after `remote:`, so a person sees what to do.
        let body = if kind == RouteKind::GitHttp { format!("{h}\n") } else { serde_json::json!({ "message": h }).to_string() };
        *resp.body_mut() = Body::from(body);
    }
    resp
}

fn method_not_allowed() -> Response {
    let mut r = (StatusCode::METHOD_NOT_ALLOWED, "Method not allowed\n").into_response();
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

/// `/mcp`: an app access token and nothing else. Not a cookie, not Basic, not a personal access
/// token, a job token or a runner credential. The browser `Origin` check comes first. Rejected
/// requests are rate limited here, because a rejection never reaches the guard.
async fn gate_mcp(state: &AppState, mut req: Request, next: Next, client: &ClientInfo) -> Response {
    let origin = state.config.origin().to_string();
    if let Some(o) = req.headers().get(header::ORIGIN).and_then(|v| v.to_str().ok())
        && o != origin
        && !state.config.mcp.allowed_origins.iter().any(|a| a == o)
    {
        let mut r = (StatusCode::FORBIDDEN, "Origin not allowed\n").into_response();
        r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        return r;
    }
    let bearer = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|a| a.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, t)| t.trim().to_string())
        .filter(|t| !t.is_empty());
    let presented = bearer.is_some();
    if let Some(t) = &bearer {
        match state.authn.from_app_bearer(t, client).await {
            Ok(a @ Actor::App { .. }) => {
                req.extensions_mut().insert(a);
                return next.run(req).await;
            }
            Ok(_) | Err(Error::Unauthorized(_)) | Err(Error::NotFound) | Err(Error::Forbidden(_)) => {}
            Err(e) => {
                tracing::error!(error = %e, "app token lookup failed");
                return Error::unavailable("Service unavailable").into_response();
            }
        }
    }
    if let Some(l) = state.ext.get::<ch_auth::apps::OAuthLimits>()
        && let Err(wait) = l.mcp_reject.hit(&client.ip.to_string())
    {
        let mut r = (StatusCode::TOO_MANY_REQUESTS, "Too many requests\n").into_response();
        r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from(wait));
        r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        return r;
    }
    mcp_challenge(state, presented)
}

/// The 401 that tells an MCP client where to sign in.
pub fn mcp_challenge(state: &AppState, token_presented: bool) -> Response {
    let origin = state.config.origin();
    let mut v = String::from("Bearer ");
    if token_presented {
        v.push_str("error=\"invalid_token\", ");
    }
    v.push_str(&format!("resource_metadata=\"{origin}{}\", scope=\"read\"", access::WELL_KNOWN_RESOURCE_MCP));
    let mut r = json_401(r#"{"error":"unauthorized"}"#);
    if let Ok(h) = HeaderValue::from_str(&v) {
        r.headers_mut().insert(header::WWW_AUTHENTICATE, h);
    }
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

/// `Sec-Fetch-Dest` says the request loads a frame or an embedded document.
fn framed(headers: &HeaderMap) -> bool {
    headers
        .get("sec-fetch-dest")
        .and_then(|v| v.to_str().ok())
        .map(|d| matches!(d.to_ascii_lowercase().as_str(), "iframe" | "frame" | "embed" | "object" | "fencedframe"))
        .unwrap_or(false)
}

/// Exactly `/{owner}/{repo}/workspace/terminal`: two plain segments, then those two words.
/// No trailing slash, no extra segment, no encoded slash. Case is checked by the route itself.
pub fn is_terminal_frame_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix('/') else { return false };
    let parts: Vec<&str> = rest.split('/').collect();
    parts.len() == 4
        && parts[2] == "workspace"
        && parts[3] == "terminal"
        && parts[..2].iter().all(|p| !p.is_empty() && *p != "." && *p != ".." && !p.contains('%'))
}

/// What signed this request in, with no secret, for checks that run again later (`LiveAuth`).
async fn credential_for(state: &AppState, actor: &Actor, session_hash: Option<Vec<u8>>) -> Result<Option<Credential>, Error> {
    Ok(match actor {
        Actor::User { via: AuthVia::Session, .. } => session_hash.map(|hash| Credential::Session { hash }),
        Actor::User { id, via: AuthVia::Basic, .. } => {
            state.authn.password_fingerprint(*id).await?.map(|fingerprint| Credential::Basic { user_id: *id, fingerprint })
        }
        Actor::Token { token_id, .. } => Some(Credential::Token { token_id: *token_id }),
        Actor::Job { job_id, .. } => Some(Credential::Job { job_id: *job_id }),
        Actor::Runner { runner_id } => Some(Credential::Runner { runner_id: *runner_id }),
        Actor::App { grant_id, .. } => Some(Credential::App { grant_id: *grant_id }),
        _ => None,
    })
}

/// Reads the credentials: `Authorization` first, then the session cookie. When the cookie
/// signed someone in, `session_hash` gets its hash.
async fn resolve_actor(
    state: &AppState,
    headers: &HeaderMap,
    client: &ClientInfo,
    session_hash: &mut Option<Vec<u8>>,
) -> Result<Actor, Error> {
    if let Some(auth) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        let (scheme, rest) = auth.split_once(' ').unwrap_or((auth, ""));
        let rest = rest.trim();
        return match scheme.to_ascii_lowercase().as_str() {
            "basic" => match decode_basic(rest) {
                Some((user, secret)) => state.authn.from_basic(&user, &secret, client).await,
                None => Ok(Actor::Anonymous),
            },
            "bearer" | "token" if !rest.is_empty() => state.authn.from_bearer(rest, client).await,
            _ => Ok(Actor::Anonymous),
        };
    }
    let cookie_name = access::session_cookie_name(state.config.is_https());
    if let Some(value) = cookie_value(headers, cookie_name) {
        let actor = state.authn.from_session(&value, client).await?;
        if !actor.is_anonymous() {
            *session_hash = Some(ch_db::tokens::hash_token(&value));
        }
        return Ok(actor);
    }
    Ok(Actor::Anonymous)
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    for h in headers.get_all(header::COOKIE) {
        let Ok(s) = h.to_str() else { continue };
        for part in s.split(';') {
            let part = part.trim();
            if let Some((k, v)) = part.split_once('=')
                && k == name
                && !v.is_empty()
                && v.len() <= 256
            {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// The response a signed-out (or wrongly credentialed) request gets, decided by the shape of
/// the path alone.
pub fn reject(state: &AppState, kind: RouteKind, method: &Method, path: &str, query: Option<&str>) -> Response {
    let mut resp = match kind {
        RouteKind::Api | RouteKind::Runner => json_401(r#"{"message":"Requires authentication"}"#),
        RouteKind::Mcp => mcp_challenge(state, false),
        RouteKind::OAuth => method_not_allowed(),
        RouteKind::GitHttp => {
            let mut r = (StatusCode::UNAUTHORIZED, "Authentication required\n").into_response();
            r.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Basic realm=\"Codehaven\""));
            r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
            r
        }
        RouteKind::Registry => {
            let base = &state.config.server.external_url;
            let host = state.config.external_url_parts().map(|u| u.host).unwrap_or_default();
            let challenge = format!("Bearer realm=\"{base}/v2/token\",service=\"{host}\"");
            let mut r = json_401(r#"{"errors":[{"code":"UNAUTHORIZED","message":"authentication required"}]}"#);
            if let Ok(v) = HeaderValue::from_str(&challenge) {
                r.headers_mut().insert(header::WWW_AUTHENTICATE, v);
            }
            r.headers_mut().insert("docker-distribution-api-version", HeaderValue::from_static("registry/2.0"));
            r
        }
        RouteKind::Page => {
            let mut location = String::from("/login");
            if *method == Method::GET || *method == Method::HEAD {
                let pq = match query {
                    Some(q) => format!("{path}?{q}"),
                    None => path.to_string(),
                };
                if let Some(next) = access::safe_next(&pq) {
                    location = format!("/login?next={}", access::encode_query_value(next));
                }
            }
            let mut r = Response::new(Body::empty());
            *r.status_mut() = StatusCode::SEE_OTHER;
            if let Ok(v) = HeaderValue::from_str(&location) {
                r.headers_mut().insert(header::LOCATION, v);
            }
            r
        }
    };
    resp.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

fn json_401(body: &'static str) -> Response {
    let mut r = (StatusCode::UNAUTHORIZED, body).into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json; charset=utf-8"));
    r
}

/// Decodes `Authorization: Basic`. The secret may itself contain colons.
fn decode_basic(s: &str) -> Option<(String, String)> {
    use base64::Engine;
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
    let s = s.trim();
    if s.is_empty() || s.len() > 2048 {
        return None;
    }
    let bytes = STANDARD.decode(s).or_else(|_| STANDARD_NO_PAD.decode(s.trim_end_matches('='))).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    let (user, secret) = text.split_once(':')?;
    if user.is_empty() || user.len() > 200 || secret.len() > 1024 {
        return None;
    }
    Some((user.to_string(), secret.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_decoding() {
        // "alice:chp_abc:def" keeps everything after the first colon in the secret.
        assert_eq!(decode_basic("YWxpY2U6Y2hwX2FiYzpkZWY="), Some(("alice".into(), "chp_abc:def".into())));
        assert_eq!(decode_basic("!!!"), None);
        assert_eq!(decode_basic(""), None);
        assert_eq!(decode_basic("YWxpY2U="), None, "no colon");
        assert_eq!(decode_basic("OngA"), None, "empty user");
    }

    #[test]
    fn only_the_terminal_path_may_be_framed() {
        assert!(is_terminal_frame_path("/alice/demo/workspace/terminal"));
        for p in [
            "/alice/demo/workspace",
            "/alice/demo/workspace/terminal/",
            "/alice/demo/workspace/terminal/x",
            "/alice/demo/workspace/Terminal",
            "/alice/demo/Workspace/terminal",
            "//demo/workspace/terminal",
            "/alice/../workspace/terminal",
            "/alice/demo%2fx/workspace/terminal",
            "/alice/workspace/terminal",
            "/api/v3/user",
            "",
        ] {
            assert!(!is_terminal_frame_path(p), "{p}");
        }
        let mut h = HeaderMap::new();
        assert!(!framed(&h));
        for d in ["iframe", "frame", "embed", "object", "fencedframe", "IFRAME"] {
            h.insert("sec-fetch-dest", HeaderValue::from_str(d).unwrap());
            assert!(framed(&h), "{d}");
        }
        for d in ["document", "empty", "script"] {
            h.insert("sec-fetch-dest", HeaderValue::from_str(d).unwrap());
            assert!(!framed(&h), "{d}");
        }
    }

    #[test]
    fn cookie_parsing() {
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, HeaderValue::from_static("a=1; __Host-ch_session=abc123; b=2"));
        assert_eq!(cookie_value(&h, "__Host-ch_session"), Some("abc123".into()));
        assert_eq!(cookie_value(&h, "missing"), None);
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, HeaderValue::from_static("__Host-ch_session="));
        assert_eq!(cookie_value(&h, "__Host-ch_session"), None);
    }
}
