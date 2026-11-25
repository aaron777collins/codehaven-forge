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
    let actor = match resolve_actor(&state, &method, req.headers(), &client, &mut session_hash, path.starts_with("/nuget/")).await {
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

    // A personal access token may download a file a page serves (ch-8ee), nothing else there.
    let token_file = match (&actor, kind) {
        (Actor::Token { .. }, RouteKind::Page) => access::token_file_route(&method, &path),
        _ => None,
    };
    if let Some(pattern) = token_file {
        req.extensions_mut().insert(access::TokenFileRead { pattern });
    }
    if access::actor_accepted(&actor, kind) || token_file.is_some() {
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
    // The registry's literal pull-only token is the one header that counts as none, and only
    // under `/v2/` (ch-4pn.13).
    let has_auth_header =
        req.headers().contains_key(header::AUTHORIZATION) && !(path.starts_with("/v2/") && is_anonymous_bearer(req.headers()));
    let mut looked_up = false;
    if actor.is_anonymous()
        && !has_auth_header
        && hint.is_none()
        && let Some(m) = access::public_repo_route(&method, &path, req.uri().query())
    {
        looked_up = true;
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
    // Rejections count against the signed-out budget too (ch-ulq). The guard counts what gets in.
    // A registry challenge that looked nothing up is the first step of every `docker push` and
    // `docker pull` (the client asks, is challenged, then signs in), so it costs nothing and is
    // not counted: counting it made busy clients fail with 429 (ch-g7n). Bad credentials have
    // their own limit.
    let free_challenge = kind == RouteKind::Registry && !looked_up;
    if let Some(l) = state.ext.get::<ch_auth::guard::SignedOutRejections>()
        && !path.starts_with("/static/")
        && !free_challenge
        && let Err(wait) = l.hit(client.ip)
    {
        let mut r = match kind {
            RouteKind::Page => (StatusCode::TOO_MANY_REQUESTS, "Too many requests. Wait a moment and try again.\n").into_response(),
            _ => Error::RateLimited { retry_after_secs: wait }.into_response(),
        };
        r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from(wait));
        r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        return r;
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
/// `Authorization: Bearer anonymous`, the token `/v2/token` hands a signed-out registry client.
/// It names nobody, so it is never looked up and never counts as a failed sign-in.
fn is_anonymous_bearer(headers: &HeaderMap) -> bool {
    headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).is_some_and(|a| {
        let (scheme, rest) = a.split_once(' ').unwrap_or((a, ""));
        scheme.eq_ignore_ascii_case("bearer") && rest.trim() == ch_registry::ANONYMOUS_TOKEN
    })
}

async fn resolve_actor(
    state: &AppState,
    method: &Method,
    headers: &HeaderMap,
    client: &ClientInfo,
    session_hash: &mut Option<Vec<u8>>,
    nuget_feed: bool,
) -> Result<Actor, Error> {
    if is_anonymous_bearer(headers) {
        return Ok(Actor::Anonymous);
    }
    // `dotnet nuget push -k <key>` sends the key as `X-NuGet-ApiKey` and nothing in
    // `Authorization`. On the NuGet feed that header is the same as a bearer token, so a personal
    // access token or a job's `GITHUB_TOKEN` pushes (ch-ctk.13). Nowhere else is it read.
    if nuget_feed
        && !headers.contains_key(header::AUTHORIZATION)
        && let Some(key) =
            headers.get("x-nuget-apikey").and_then(|v| v.to_str().ok()).map(str::trim).filter(|k| !k.is_empty() && k.len() <= 512)
    {
        return state.authn.from_bearer(key, client).await;
    }
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
    if from_another_site(method, headers) {
        return Ok(Actor::Anonymous);
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

/// A request that another site's page made with the browser's cookies, which the session
/// cookie must not sign in (docs/design/pages.md, section 4). SameSite=Lax is not enough on
/// its own: Safari sends Lax cookies with every request of a sandboxed page on a sibling host
/// of the same site, a Pages site among them. So only this origin's own requests and top-level
/// GET or HEAD navigations, the case Lax exists for, may use the session. A browser that sends
/// no fetch metadata is judged by `Origin: null`, which only an opaque origin sends on a GET.
fn from_another_site(method: &Method, headers: &HeaderMap) -> bool {
    let get = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_ascii_lowercase);
    match get("sec-fetch-site").as_deref() {
        None => get("origin").as_deref() == Some("null") && matches!(*method, Method::GET | Method::HEAD),
        Some("same-origin") | Some("none") => false,
        Some(_) => {
            let top_level =
                get("sec-fetch-mode").as_deref() == Some("navigate") && matches!(get("sec-fetch-dest").as_deref(), None | Some("document"));
            !(top_level && matches!(*method, Method::GET | Method::HEAD))
        }
    }
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
    fn only_this_origin_and_top_level_navigations_carry_the_session() {
        let h = |pairs: &[(&'static str, &'static str)]| {
            let mut m = HeaderMap::new();
            for (k, v) in pairs {
                m.insert(*k, HeaderValue::from_static(v));
            }
            m
        };
        let (get, post) = (Method::GET, Method::POST);
        assert!(!from_another_site(&get, &h(&[])));
        assert!(!from_another_site(&post, &h(&[("origin", "null")])), "POST is for the CSRF check");
        assert!(from_another_site(&get, &h(&[("origin", "null")])));
        assert!(from_another_site(&Method::HEAD, &h(&[("origin", "null")])));
        assert!(!from_another_site(&post, &h(&[("sec-fetch-site", "same-origin"), ("origin", "null")])));
        assert!(!from_another_site(&get, &h(&[("sec-fetch-site", "none")])));
        let nav = [("sec-fetch-site", "cross-site"), ("sec-fetch-mode", "navigate"), ("sec-fetch-dest", "document")];
        assert!(!from_another_site(&get, &h(&nav)));
        assert!(from_another_site(&post, &h(&nav)), "a form posted from another site");
        assert!(!from_another_site(&get, &h(&[("sec-fetch-site", "same-site"), ("sec-fetch-mode", "navigate")])));
        for site in ["same-site", "cross-site", "CROSS-SITE", "something-new"] {
            for (mode, dest) in
                [("cors", "empty"), ("no-cors", "image"), ("navigate", "iframe"), ("websocket", "websocket"), ("no-cors", "script")]
            {
                let m = h(&[("sec-fetch-site", site), ("sec-fetch-mode", mode), ("sec-fetch-dest", dest)]);
                assert!(from_another_site(&get, &m), "{site} {mode} {dest}");
            }
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
