//! The deny-by-default gate's rulebook.
//!
//! `PUBLIC_ROUTES` is the complete list of routes a signed-out request may reach
//! (ARCHITECTURE.md sections 4.1a and 7.3). Everything else is rejected before a handler
//! runs. A new route is private until someone adds it here on purpose, and every change
//! to this table is reviewed by Claude.

use crate::actor::Actor;
use axum::extract::FromRequestParts;
use axum::http::Method;
use axum::http::request::Parts;
use std::convert::Infallible;

/// What a rejected signed-out request gets, decided only by the shape of the path so that
/// paths that exist and paths that do not look identical.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteKind {
    /// A web page: `303` to `/login?next=...`.
    Page,
    /// `/api/v3/*`: `401` with a JSON body.
    Api,
    /// git smart HTTP: `401` with a Basic challenge.
    GitHttp,
    /// `/v2/*` container registry: `401` with a Bearer challenge.
    Registry,
    /// `/api/runner/v1/*`: `401` unless a runner credential or job token is present.
    Runner,
    /// The MCP endpoint, `/mcp`: `401` with a `WWW-Authenticate` challenge that points at the
    /// protected resource document. Accepts only an OAuth access token of a connected app.
    Mcp,
    /// The signed-out OAuth surface: the three discovery documents and the token and
    /// revocation endpoints. No credential is read and no cookie is set.
    OAuth,
}

/// The exact path of the MCP endpoint.
pub const MCP_PATH: &str = "/mcp";
/// The protected resource documents, with and without the resource path suffix.
pub const WELL_KNOWN_RESOURCE_MCP: &str = "/.well-known/oauth-protected-resource/mcp";
pub const WELL_KNOWN_RESOURCE: &str = "/.well-known/oauth-protected-resource";
pub const WELL_KNOWN_AUTH_SERVER: &str = "/.well-known/oauth-authorization-server";
pub const OAUTH_TOKEN_PATH: &str = "/oauth/token";
pub const OAUTH_REVOKE_PATH: &str = "/oauth/revoke";

/// True for the seven paths that exist only while MCP is on: the endpoint, the three discovery
/// documents, the token and revocation endpoints, and the signed-in authorize and consent pages.
pub fn is_mcp_path(path: &str) -> bool {
    matches!(
        path,
        MCP_PATH
            | WELL_KNOWN_RESOURCE_MCP
            | WELL_KNOWN_RESOURCE
            | WELL_KNOWN_AUTH_SERVER
            | OAUTH_TOKEN_PATH
            | OAUTH_REVOKE_PATH
            | "/oauth/authorize"
            | "/oauth/consent"
    )
}

/// A static method and path pattern.
///
/// Pattern syntax: literal segments, `{name}` for exactly one non-empty segment, and a
/// final `*` for one or more remaining segments.
#[derive(Debug, Clone, Copy)]
pub struct PublicRoute {
    pub methods: &'static [&'static str],
    pub pattern: &'static str,
    pub why: &'static str,
}

pub const PUBLIC_ROUTES: &[PublicRoute] = &[
    PublicRoute { methods: &["GET", "POST"], pattern: "/login", why: "Signing in" },
    PublicRoute { methods: &["GET", "POST"], pattern: "/login/two-factor", why: "Second step of signing in" },
    PublicRoute { methods: &["GET", "POST"], pattern: "/invite/{token}", why: "Accepting an invite" },
    PublicRoute { methods: &["GET", "POST"], pattern: "/password/{token}", why: "One-time set-password and reset links" },
    PublicRoute { methods: &["GET"], pattern: "/login/oidc/{provider}", why: "Only answers when a provider is configured" },
    PublicRoute { methods: &["GET"], pattern: "/login/oidc/{provider}/callback", why: "Only answers when a provider is configured" },
    PublicRoute { methods: &["GET"], pattern: "/static/*", why: "Styles, scripts and icons. Nothing user-generated" },
    PublicRoute { methods: &["GET"], pattern: "/favicon.svg", why: "The mark" },
    PublicRoute { methods: &["GET"], pattern: "/robots.txt", why: "Disallow: /" },
    PublicRoute { methods: &["GET"], pattern: "/healthz", why: "Says ok and nothing else" },
    PublicRoute {
        methods: &["POST"],
        pattern: "/api/runner/v1/register",
        why: "Rejects anything without an admin-issued registration token",
    },
    // The five below exist only while `mcp.enabled` is true. Each is static or answers only for
    // a code or token made at the consent page. There is no client registration route.
    PublicRoute {
        methods: &["GET"],
        pattern: "/.well-known/oauth-protected-resource/mcp",
        why: "Static JSON from config: where the MCP endpoint is and who signs people in",
    },
    PublicRoute { methods: &["GET"], pattern: "/.well-known/oauth-protected-resource", why: "The same document" },
    PublicRoute {
        methods: &["GET"],
        pattern: "/.well-known/oauth-authorization-server",
        why: "Static JSON from config: the OAuth endpoints",
    },
    PublicRoute {
        methods: &["POST"],
        pattern: "/oauth/token",
        why: "Answers only for a code made at the consent page or a live refresh token",
    },
    PublicRoute { methods: &["POST"], pattern: "/oauth/revoke", why: "Empty answer for any token, known or not" },
];

/// Splits a path into segments, or `None` when the path is not safe to match at all:
/// empty segments, dot segments, encoded slashes or backslashes, control characters.
fn safe_segments(path: &str) -> Option<Vec<&str>> {
    if !path.starts_with('/') || path.len() > 2048 {
        return None;
    }
    if path.bytes().any(|b| b < 0x20 || b == 0x7f || b == b'\\') {
        return None;
    }
    let lower = path.to_ascii_lowercase();
    if lower.contains("%2f") || lower.contains("%5c") || lower.contains("%00") || lower.contains("%2e") {
        return None;
    }
    if path == "/" {
        return Some(vec![]);
    }
    let segs: Vec<&str> = path[1..].split('/').collect();
    if segs.iter().any(|s| s.is_empty() || *s == "." || *s == "..") {
        return None;
    }
    Some(segs)
}

fn pattern_matches(pattern: &str, segs: &[&str]) -> bool {
    let pats: Vec<&str> = if pattern == "/" { vec![] } else { pattern[1..].split('/').collect() };
    let mut i = 0;
    for (n, p) in pats.iter().enumerate() {
        if *p == "*" {
            // `*` must be last and needs at least one segment.
            return n == pats.len() - 1 && segs.len() > n;
        }
        let Some(s) = segs.get(i) else { return false };
        if p.starts_with('{') && p.ends_with('}') {
            // Any single non-empty segment.
        } else if p != s {
            return false;
        }
        i += 1;
    }
    i == segs.len()
}

/// True when a signed-out request with this method and path may reach a handler.
pub fn is_public(method: &Method, path: &str) -> bool {
    let Some(segs) = safe_segments(path) else { return false };
    let m = if *method == Method::HEAD { "GET" } else { method.as_str() };
    PUBLIC_ROUTES.iter().any(|r| r.methods.contains(&m) && pattern_matches(r.pattern, &segs))
}

/// True when `pattern` (as written in `PUBLIC_ROUTES`) is the same route as a registered
/// router path such as `/invite/{token}`. Used by the surface test.
pub fn is_public_pattern(method: &str, router_path: &str) -> bool {
    let m = if method == "HEAD" { "GET" } else { method };
    PUBLIC_ROUTES.iter().any(|r| r.methods.contains(&m) && normalize_pattern(r.pattern) == normalize_pattern(router_path))
}

fn normalize_pattern(p: &str) -> String {
    p.split('/')
        .map(|s| {
            if s.starts_with('{') && s.ends_with('}') {
                if s.starts_with("{*") { "*".to_string() } else { "{}".to_string() }
            } else {
                s.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// `classify` for an instance where MCP may be off. With MCP off the seven MCP paths are
/// ordinary unknown pages, so a signed-out visitor gets exactly what any other unknown path gets.
pub fn classify_for(path: &str, mcp_enabled: bool) -> RouteKind {
    if !mcp_enabled && is_mcp_path(path) {
        return RouteKind::Page;
    }
    classify(path)
}

/// `is_public` for an instance where MCP may be off: the five public MCP routes are public only
/// while MCP is on.
pub fn is_public_for(method: &Method, path: &str, mcp_enabled: bool) -> bool {
    if !mcp_enabled && is_mcp_path(path) {
        return false;
    }
    is_public(method, path)
}

/// Decides the rejection kind from the path alone.
pub fn classify(path: &str) -> RouteKind {
    if path == MCP_PATH {
        return RouteKind::Mcp;
    }
    if matches!(path, WELL_KNOWN_RESOURCE_MCP | WELL_KNOWN_RESOURCE | WELL_KNOWN_AUTH_SERVER | OAUTH_TOKEN_PATH | OAUTH_REVOKE_PATH) {
        return RouteKind::OAuth;
    }
    if path == "/api/runner/v1" || path.starts_with("/api/runner/") {
        return RouteKind::Runner;
    }
    if path == "/api/v3" || path.starts_with("/api/v3/") {
        return RouteKind::Api;
    }
    if path == "/v2" || path.starts_with("/v2/") {
        return RouteKind::Registry;
    }
    if is_git_http_path(path) {
        return RouteKind::GitHttp;
    }
    RouteKind::Page
}

/// `/{owner}/{repo}(.git)/info/refs`, `/git-upload-pack` and `/git-receive-pack`.
pub fn is_git_http_path(path: &str) -> bool {
    let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    matches!(segs.as_slice(), [_, _, "info", "refs"] | [_, _, "git-upload-pack"] | [_, _, "git-receive-pack"])
}

/// A local path that is safe to put in `?next=`. Anything else becomes `None`.
pub fn safe_next(path_and_query: &str) -> Option<&str> {
    let p = path_and_query;
    // An authorize URL carries the client id, redirect, challenge and state, so it needs more
    // room than an ordinary page. Nothing else gets it.
    let cap = if p.starts_with("/oauth/authorize?") { 4096 } else { 512 };
    if !p.starts_with('/') || p.starts_with("//") || p.len() > cap {
        return None;
    }
    if p.bytes().any(|b| b < 0x20 || b == 0x7f || b == b'\\') {
        return None;
    }
    // Never bounce back to the sign-in pages.
    if p == "/login" || p.starts_with("/login/") || p.starts_with("/login?") {
        return None;
    }
    Some(p)
}

/// Percent-encodes everything outside the unreserved set, for use as a query value.
pub fn encode_query_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Session cookie name. The `__Host-` prefix needs `Secure`, so it is used only when the
/// public origin is https. Plain http (local development) gets the bare name.
pub fn session_cookie_name(https: bool) -> &'static str {
    if https { "__Host-ch_session" } else { "ch_session" }
}

/// Signed double-submit CSRF cookie for forms that have no session yet (sign-in, invite).
pub fn csrf_cookie_name(https: bool) -> &'static str {
    if https { "__Host-ch_csrf" } else { "ch_csrf" }
}

/// Reads one cookie from a request's `Cookie` headers. Empty and oversized values are `None`.
pub fn cookie_value(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    for h in headers.get_all(axum::http::header::COOKIE) {
        let Ok(s) = h.to_str() else { continue };
        for part in s.split(';') {
            if let Some((k, v)) = part.trim().split_once('=')
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

/// The `Set-Cookie` value for a session: `HttpOnly`, `SameSite=Lax`, `Path=/`, and `Secure`
/// when the public origin is https (which the `__Host-` prefix requires).
pub fn session_set_cookie(value: &str, max_age_secs: i64, https: bool) -> String {
    let name = session_cookie_name(https);
    let secure = if https { "; Secure" } else { "" };
    format!("{name}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age_secs}{secure}")
}

/// Expires the session cookie.
pub fn session_clear_cookie(https: bool) -> String {
    session_set_cookie("", 0, https)
}

/// Which credential kinds a route family accepts. A credential that is valid but not
/// accepted here is treated exactly like no credential, so a personal access token opens no
/// web page and a browser session opens no git or registry endpoint.
pub fn actor_accepted(actor: &Actor, kind: RouteKind) -> bool {
    use crate::actor::AuthVia;
    match (actor, kind) {
        (Actor::Anonymous, _) => false,
        (Actor::User { via: AuthVia::Session, .. }, RouteKind::Page | RouteKind::Api) => true,
        (Actor::User { via: AuthVia::Basic, .. }, RouteKind::Api | RouteKind::GitHttp | RouteKind::Registry) => true,
        (Actor::User { via: AuthVia::Ssh, .. }, _) => false,
        (Actor::Token { .. }, RouteKind::Api | RouteKind::GitHttp | RouteKind::Registry) => true,
        (Actor::Job { .. }, RouteKind::Api | RouteKind::GitHttp | RouteKind::Runner) => true,
        (Actor::Runner { .. }, RouteKind::Runner) => true,
        // An app token opens the MCP endpoint and nothing else, and nothing else opens it.
        (Actor::App { .. }, RouteKind::Mcp) => true,
        _ => false,
    }
}

/// Extractor for the caller. The gate inserts the `Actor` it resolved; a handler that
/// somehow runs without one sees `Anonymous`.
#[derive(Debug, Clone)]
pub struct CurrentActor(pub Actor);

impl<S: Send + Sync> FromRequestParts<S> for CurrentActor {
    type Rejection = Infallible;
    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(CurrentActor(parts.extensions.get::<Actor>().cloned().unwrap_or(Actor::Anonymous)))
    }
}

/// The caller's address and user agent, worked out by the request-info layer from the socket
/// and, from trusted proxies only, `X-Forwarded-For`. Handlers pass it to services for the
/// audit log and rate limits.
#[derive(Debug, Clone)]
pub struct Client(pub crate::model::ClientInfo);

impl<S: Send + Sync> FromRequestParts<S> for Client {
    type Rejection = Infallible;
    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Client(parts.extensions.get::<crate::model::ClientInfo>().cloned().unwrap_or_else(crate::model::ClientInfo::local)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn app() -> Actor {
        use crate::{AppGrant, GrantId, RepoSet, UserId};
        Actor::App {
            user_id: UserId(1),
            grant_id: GrantId(1),
            grant: AppGrant { read: RepoSet::All, write: RepoSet::none(), workflows: RepoSet::none() },
        }
    }

    #[test]
    fn the_connector_adds_exactly_five_public_routes_and_no_registration() {
        let mcp: Vec<&str> = PUBLIC_ROUTES
            .iter()
            .filter(|r| r.pattern.starts_with("/.well-known/") || r.pattern.starts_with("/oauth/"))
            .map(|r| r.pattern)
            .collect();
        assert_eq!(
            mcp,
            [
                "/.well-known/oauth-protected-resource/mcp",
                "/.well-known/oauth-protected-resource",
                "/.well-known/oauth-authorization-server",
                "/oauth/token",
                "/oauth/revoke",
            ]
        );
        assert_eq!(PUBLIC_ROUTES.len(), 11 + 5, "the eleven that existed before, plus five");
        for p in [
            "/oauth/register",
            "/register",
            "/oauth2/register",
            "/oauth/clients",
            "/.well-known/oauth-client",
            "/oauth/authorize",
            "/oauth/consent",
            "/mcp",
        ] {
            for m in [Method::GET, Method::POST] {
                assert!(!is_public(&m, p), "{m} {p} must not be public");
            }
        }
        assert!(is_public(&Method::GET, "/.well-known/oauth-protected-resource/mcp"));
        assert!(is_public(&Method::HEAD, "/.well-known/oauth-authorization-server"), "HEAD follows GET");
        assert!(!is_public(&Method::POST, "/.well-known/oauth-authorization-server"));
        assert!(is_public(&Method::POST, "/oauth/token") && !is_public(&Method::GET, "/oauth/token"));
        assert!(is_public(&Method::POST, "/oauth/revoke") && !is_public(&Method::GET, "/oauth/revoke"));
        assert!(!is_public(&Method::GET, "/.well-known/oauth-protected-resource/other"));
        assert!(!is_public(&Method::GET, "/.well-known/oauth-protected-resource/mcp/"));
        assert!(!is_public(&Method::POST, "/oauth/token/x"));
    }

    #[test]
    fn mcp_routes_have_their_own_kinds_and_vanish_when_mcp_is_off() {
        assert_eq!(classify("/mcp"), RouteKind::Mcp);
        assert_eq!(classify("/mcp/"), RouteKind::Page, "only the exact path");
        for p in [
            "/.well-known/oauth-protected-resource/mcp",
            "/.well-known/oauth-protected-resource",
            "/.well-known/oauth-authorization-server",
            "/oauth/token",
            "/oauth/revoke",
        ] {
            assert_eq!(classify(p), RouteKind::OAuth, "{p}");
        }
        assert_eq!(classify("/oauth/authorize"), RouteKind::Page, "signed-in pages redirect to sign in");
        assert_eq!(classify("/oauth/consent"), RouteKind::Page);
        // With MCP off, all eight are ordinary unknown pages and nothing is public.
        for p in [
            "/mcp",
            "/.well-known/oauth-protected-resource/mcp",
            "/.well-known/oauth-protected-resource",
            "/.well-known/oauth-authorization-server",
            "/oauth/token",
            "/oauth/revoke",
            "/oauth/authorize",
            "/oauth/consent",
        ] {
            assert_eq!(classify_for(p, false), RouteKind::Page, "{p}");
            for m in [Method::GET, Method::POST, Method::HEAD] {
                assert!(!is_public_for(&m, p, false), "{m} {p}");
            }
            assert_eq!(classify_for(p, true), classify(p));
        }
        assert_eq!(classify_for("/login", false), RouteKind::Page);
        assert!(is_public_for(&Method::GET, "/login", false));
    }

    #[test]
    fn an_app_token_opens_the_mcp_endpoint_and_nothing_else_opens_it() {
        use crate::actor::{AuthVia, JobPerms};
        use crate::{JobId, RepoId, RunnerId, Scopes, TokenId, UserId};
        let kinds =
            [RouteKind::Page, RouteKind::Api, RouteKind::GitHttp, RouteKind::Registry, RouteKind::Runner, RouteKind::Mcp, RouteKind::OAuth];
        for k in kinds {
            assert_eq!(actor_accepted(&app(), k), k == RouteKind::Mcp, "app on {k:?}");
        }
        let others = [
            Actor::User { id: UserId(1), is_admin: true, has_2fa: true, via: AuthVia::Session },
            Actor::User { id: UserId(1), is_admin: true, has_2fa: true, via: AuthVia::Basic },
            Actor::Token { user_id: UserId(1), token_id: TokenId(1), scopes: Scopes::new(["repo"]) },
            Actor::Job { job_id: JobId(1), repo_id: RepoId(1), perms: JobPerms { contents_read: true, write_checks: true } },
            Actor::Runner { runner_id: RunnerId(1) },
            Actor::System,
            Actor::Anonymous,
        ];
        for a in &others {
            assert!(!actor_accepted(a, RouteKind::Mcp), "{a:?} must not open /mcp");
            assert!(!actor_accepted(a, RouteKind::OAuth), "the OAuth routes are public, never credentialed: {a:?}");
        }
    }

    #[test]
    fn only_the_authorize_url_may_be_long_in_next() {
        let long = format!("/oauth/authorize?client_id={}", "a".repeat(3000));
        assert!(safe_next(&long).is_some(), "the authorize URL keeps its parameters through sign-in");
        assert!(safe_next(&format!("/oauth/authorize?x={}", "a".repeat(4100))).is_none(), "but not without limit");
        assert!(safe_next(&format!("/oauth/authorizex?x={}", "a".repeat(600))).is_none());
        assert!(safe_next(&format!("/settings/{}", "a".repeat(600))).is_none(), "every other path keeps the 512 cap");
        assert!(safe_next("/oauth/authorize?a=1\nb").is_none(), "control characters are never accepted");
        assert!(safe_next("/oauth/authorize?\u{7}").is_none());
        assert!(safe_next("//evil.example/oauth/authorize?x=1").is_none());
    }

    #[test]
    fn listed_routes_are_public() {
        assert!(is_public(&Method::GET, "/login"));
        assert!(is_public(&Method::POST, "/login"));
        assert!(is_public(&Method::HEAD, "/login"));
        assert!(is_public(&Method::GET, "/login/two-factor"));
        assert!(is_public(&Method::GET, "/invite/chi_abc"));
        assert!(is_public(&Method::POST, "/invite/chi_abc"));
        assert!(is_public(&Method::GET, "/password/tok"));
        assert!(is_public(&Method::GET, "/static/app.123.css"));
        assert!(is_public(&Method::GET, "/static/css/app.css"));
        assert!(is_public(&Method::GET, "/healthz"));
        assert!(is_public(&Method::GET, "/robots.txt"));
        assert!(is_public(&Method::GET, "/favicon.svg"));
        assert!(is_public(&Method::POST, "/api/runner/v1/register"));
        assert!(is_public(&Method::GET, "/login/oidc/github/callback"));
    }

    #[test]
    fn everything_else_is_private() {
        for (m, p) in [
            (Method::GET, "/"),
            (Method::GET, "/explore"),
            (Method::GET, "/signup"),
            (Method::GET, "/register"),
            (Method::POST, "/signup"),
            (Method::GET, "/root"),
            (Method::GET, "/nobody/nothing"),
            (Method::GET, "/api/v3/user"),
            (Method::GET, "/api/v3/meta"),
            (Method::GET, "/api/v3"),
            (Method::GET, "/v2/"),
            (Method::GET, "/metrics"),
            (Method::GET, "/readyz"),
            (Method::GET, "/api/runner/v1/jobs/request"),
            (Method::GET, "/api/runner/v1/register"),
            (Method::PUT, "/login"),
            (Method::DELETE, "/invite/x"),
            (Method::GET, "/invite"),
            (Method::GET, "/invite/a/b"),
            (Method::GET, "/static"),
            (Method::GET, "/static/"),
            (Method::GET, "/healthz/x"),
            (Method::POST, "/healthz"),
            (Method::GET, "/login/"),
            (Method::GET, "/login/oidc"),
            (Method::GET, "/x/y.git/info/refs"),
        ] {
            assert!(!is_public(&m, p), "{m} {p} must be private");
        }
    }

    #[test]
    fn tricky_paths_are_never_public() {
        for p in [
            "/static/../api/v3/user",
            "/static/%2e%2e/admin",
            "/static/a%2Fb",
            "/static/a\\b",
            "/login//",
            "//login",
            "/static//x",
            "/static/a\0b",
            "/static/./x",
        ] {
            assert!(!is_public(&Method::GET, p), "{p:?}");
        }
    }

    #[test]
    fn classify_by_shape() {
        assert_eq!(classify("/"), RouteKind::Page);
        assert_eq!(classify("/alice/demo"), RouteKind::Page);
        assert_eq!(classify("/api/v3/user"), RouteKind::Api);
        assert_eq!(classify("/api/v3"), RouteKind::Api);
        assert_eq!(classify("/api/runner/v1/jobs/request"), RouteKind::Runner);
        assert_eq!(classify("/v2/"), RouteKind::Registry);
        assert_eq!(classify("/v2/alice/img/manifests/latest"), RouteKind::Registry);
        assert_eq!(classify("/x/y.git/info/refs"), RouteKind::GitHttp);
        assert_eq!(classify("/x/y/git-upload-pack"), RouteKind::GitHttp);
        assert_eq!(classify("/x/y.git/git-receive-pack"), RouteKind::GitHttp);
        assert_eq!(classify("/x/y/info"), RouteKind::Page);
    }

    #[test]
    fn next_only_accepts_local_paths() {
        assert_eq!(safe_next("/alice/demo?tab=1"), Some("/alice/demo?tab=1"));
        assert_eq!(safe_next("//evil.example/x"), None);
        assert_eq!(safe_next("https://evil.example"), None);
        assert_eq!(safe_next("/\\evil"), None);
        assert_eq!(safe_next("/login"), None);
        assert_eq!(safe_next("/login?next=/x"), None);
        assert_eq!(safe_next("/a\r\nSet-Cookie: x"), None);
    }

    #[test]
    fn credentials_only_open_their_own_doors() {
        use crate::actor::{AuthVia, JobPerms, Scopes};
        use crate::ids::*;
        let session = Actor::User { id: UserId(1), is_admin: false, has_2fa: false, via: AuthVia::Session };
        let basic = Actor::User { id: UserId(1), is_admin: false, has_2fa: false, via: AuthVia::Basic };
        let token = Actor::Token { user_id: UserId(1), token_id: TokenId(1), scopes: Scopes::new(["repo"]) };
        let job = Actor::Job { job_id: JobId(1), repo_id: RepoId(1), perms: JobPerms { contents_read: true, write_checks: true } };
        let runner = Actor::Runner { runner_id: RunnerId(1) };
        assert!(actor_accepted(&session, RouteKind::Page));
        assert!(!actor_accepted(&session, RouteKind::GitHttp));
        assert!(!actor_accepted(&session, RouteKind::Registry));
        assert!(!actor_accepted(&basic, RouteKind::Page));
        assert!(actor_accepted(&basic, RouteKind::GitHttp));
        assert!(!actor_accepted(&token, RouteKind::Page));
        assert!(actor_accepted(&token, RouteKind::Api));
        assert!(!actor_accepted(&token, RouteKind::Runner));
        assert!(!actor_accepted(&job, RouteKind::Page));
        assert!(actor_accepted(&job, RouteKind::Runner));
        assert!(!actor_accepted(&runner, RouteKind::Api));
        assert!(!actor_accepted(&runner, RouteKind::Page));
        assert!(actor_accepted(&runner, RouteKind::Runner));
        assert!(!actor_accepted(&Actor::Anonymous, RouteKind::Page));
        assert!(!actor_accepted(&Actor::System, RouteKind::Page));
    }

    #[test]
    fn cookie_names_follow_scheme() {
        assert_eq!(session_cookie_name(true), "__Host-ch_session");
        assert_eq!(session_cookie_name(false), "ch_session");
        assert_eq!(csrf_cookie_name(true), "__Host-ch_csrf");
    }

    #[test]
    fn query_values_are_encoded() {
        assert_eq!(encode_query_value("/a b?c=d&e"), "/a%20b%3Fc%3Dd%26e");
    }

    #[test]
    fn router_pattern_comparison() {
        assert!(is_public_pattern("GET", "/invite/{token}"));
        assert!(is_public_pattern("GET", "/invite/{t}"));
        assert!(is_public_pattern("GET", "/static/{*path}"));
        assert!(!is_public_pattern("GET", "/"));
        assert!(!is_public_pattern("DELETE", "/login"));
    }

    proptest! {
        // Whatever the path, a path that is not in the table is never public.
        #[test]
        fn random_paths_under_api_are_never_public(tail in "[a-zA-Z0-9/._%-]{0,40}") {
            let p = format!("/api/v3/{tail}");
            prop_assert!(!is_public(&Method::GET, &p));
            let q = format!("/v2/{tail}");
            prop_assert!(!is_public(&Method::GET, &q));
        }
    }
}
