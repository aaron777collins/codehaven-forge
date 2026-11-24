//! The public-surface test (ARCHITECTURE.md section 7.3, ROADMAP.md section 3 item 8).
//!
//! It walks every route the real router has registered, sends each one a signed-out request
//! with every method, and fails if anything outside `PUBLIC_ROUTES` answers with anything but
//! the rejection for its kind. A new route is private the moment it is added, and stays
//! private unless someone edits `PUBLIC_ROUTES` on purpose.
//!
//! It needs no database: the pool is lazy and the unbuilt services never touch it.

#![allow(clippy::unwrap_used, clippy::type_complexity)]

mod common;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode, header};
use axum::routing::{any, get};
use ch_core::access::{PUBLIC_ROUTES, is_public};
use ch_core::actor::{AuthVia, JobPerms};
use ch_core::model::{ClientInfo, SshIdentity};
use ch_core::services::Authn;
use ch_core::state::{AppState, AppStateBuilder};
use ch_core::{Actor, Config, Error, JobId, RepoId, RunnerId, Scopes, TokenId, UserId};
use ch_db::{FsBlobStore, PgAudit, PgEventBus};
use codehaven::app::build_router;
use codehaven::metrics::Metrics;
use codehaven::reqinfo::RequestMeta;
use codehaven::wiring;
use http_body_util::BodyExt;
use std::net::SocketAddr;
use std::sync::Arc;
use tower::ServiceExt;

// ------------------------------------------------------------------ harness

/// A stand-in for the identity stream's `Authn`, with a few fixed credentials.
struct FakeAuthn;

#[async_trait]
impl Authn for FakeAuthn {
    async fn from_session(&self, cookie: &str, _c: &ClientInfo) -> ch_core::Result<Actor> {
        Ok(match cookie {
            "good-session" => Actor::User { id: UserId(1), is_admin: false, has_2fa: false, via: AuthVia::Session },
            // What the real service answers for a suspended account: nobody at all.
            "suspended-session" => Actor::Anonymous,
            _ => Actor::Anonymous,
        })
    }
    async fn from_basic(&self, user: &str, secret: &str, _c: &ClientInfo) -> ch_core::Result<Actor> {
        if user == "alice" && secret == "pw" {
            Ok(Actor::User { id: UserId(1), is_admin: false, has_2fa: false, via: AuthVia::Basic })
        } else {
            Err(Error::unauthorized())
        }
    }
    async fn from_bearer(&self, token: &str, _c: &ClientInfo) -> ch_core::Result<Actor> {
        match token {
            "chp_good" => Ok(Actor::Token { user_id: UserId(1), token_id: TokenId(1), scopes: Scopes::new(["repo"]) }),
            "job_good" => {
                Ok(Actor::Job { job_id: JobId(1), repo_id: RepoId(1), perms: JobPerms { contents_read: true, write_checks: true } })
            }
            "runner_good" => Ok(Actor::Runner { runner_id: RunnerId(1) }),
            "slow_down" => Err(Error::RateLimited { retry_after_secs: 30 }),
            "explode" => Err(Error::internal(anyhow::anyhow!("database password is hunter2"))),
            _ => Err(Error::unauthorized()),
        }
    }
    async fn from_ssh_key(&self, _f: &str) -> ch_core::Result<Option<SshIdentity>> {
        Ok(None)
    }
}

fn config(trusted: &[&str]) -> Config {
    let mut c = Config::for_tests();
    // The identity services load the master key when they are built, so the data directory
    // has to be somewhere writable.
    c.storage.data_dir = std::env::temp_dir().join(format!("ch-surface-data-{}", std::process::id()));
    c.server.external_url = "https://code.example.com".into();
    // The surface test covers the connector's routes too, so they must be registered.
    c.mcp.enabled = true;
    c.http.trusted_proxies = trusted.iter().map(|s| s.parse().unwrap()).collect();
    c
}

fn state(cfg: Config, fake_auth: bool) -> AppState {
    let dir = std::env::temp_dir().join(format!("ch-surface-{}", std::process::id()));
    let pool = sqlx::postgres::PgPoolOptions::new().connect_lazy("postgres://nobody:nothing@127.0.0.1:1/none").unwrap();
    let mut b = AppStateBuilder::new(
        Arc::new(cfg),
        pool.clone(),
        Arc::new(PgAudit::new(pool.clone())),
        Arc::new(PgEventBus::new(pool)),
        Arc::new(FsBlobStore::new(dir)),
    );
    wiring::install_services(&mut b).unwrap();
    // This test needs no database, so the services that would query one stand down. The
    // identity services have their own suites in ch-auth, and `identity_wiring.rs` runs the
    // real ones behind the real gate.
    b.accounts = Arc::new(ch_core::not_built::NotBuilt);
    b.authn = if fake_auth { Arc::new(FakeAuthn) } else { Arc::new(ch_core::not_built::NotBuilt) };
    b.build()
}

/// Routes that exist only in this test, to prove the gate protects a route nobody has thought
/// about yet.
fn probes() -> Router<AppState> {
    async fn whoami(axum::Extension(meta): axum::Extension<RequestMeta>, headers: axum::http::HeaderMap) -> String {
        format!("ip={} secure={} remote_user={}", meta.client.ip, meta.secure, headers.contains_key("remote-user"))
    }
    Router::new()
        .route("/secret/page", get(|| async { "SECRET-PAGE" }))
        .route("/api/v3/secret", any(|| async { "SECRET-API" }))
        .route("/api/v3/whoami", get(whoami))
        .route("/owner/repo.git/info/refs", get(|| async { "SECRET-GIT" }))
        .route("/v2/secret", get(|| async { "SECRET-REGISTRY" }))
        .route("/api/runner/v1/secret", any(|| async { "SECRET-RUNNER" }))
}

fn app(cfg: Config, fake_auth: bool) -> Router {
    build_router(state(cfg, fake_auth), Metrics::new(), Some(probes()))
}

struct Reply {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: String,
}

async fn send(app: &Router, method: Method, path: &str, hdrs: &[(&str, &str)], peer: Option<&str>) -> Reply {
    let mut req = Request::builder().method(method).uri(path);
    for (k, v) in hdrs {
        req = req.header(*k, *v);
    }
    let peer: SocketAddr = format!("{}:40000", peer.unwrap_or("203.0.113.9")).parse().unwrap();
    let req = req.extension(ConnectInfo(peer)).body(Body::empty()).unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = String::from_utf8_lossy(&resp.into_body().collect().await.unwrap().to_bytes()).to_string();
    Reply { status, headers, body }
}

async fn get_(app: &Router, path: &str) -> Reply {
    send(app, Method::GET, path, &[], None).await
}

/// Pulls every registered path out of axum's `Debug` output for the router. Brittle by design:
/// if axum changes the format, `registered_paths_are_found` fails loudly instead of the sweep
/// silently covering nothing.
fn registered_paths(router: &Router<AppState>) -> Vec<String> {
    let dbg = format!("{router:?}");
    let start = dbg.find("path_router: PathRouter").expect("path_router in Debug output");
    let rest = &dbg[start..];
    let end = rest.find("fallback_router").unwrap_or(rest.len());
    let section = &rest[..end];
    let paths_at = section.find("paths: {").expect("paths in Debug output");
    let tail = &section[paths_at..];
    let mut out = vec![];
    let mut chars = tail.char_indices();
    while let Some((i, c)) = chars.next() {
        if c == '"' {
            let from = i + 1;
            let mut to = from;
            for (j, c2) in chars.by_ref() {
                if c2 == '"' {
                    to = j;
                    break;
                }
            }
            out.push(tail[from..to].to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}

/// A concrete request path for a router pattern.
fn concrete(pattern: &str) -> String {
    pattern
        .split('/')
        .map(|s| {
            if s.starts_with("{*") {
                "x/y".to_string()
            } else if s.starts_with('{') {
                "x".to_string()
            } else {
                s.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

const METHODS: &[Method] = &[Method::GET, Method::HEAD, Method::POST, Method::PUT, Method::PATCH, Method::DELETE, Method::OPTIONS];

/// Public routes that no stream has registered yet. When a stream registers one it must delete
/// it from this list, so the list shrinks to nothing as the product lands.
const PUBLIC_NOT_BUILT_YET: &[&str] = &["/login/oidc/{provider}", "/login/oidc/{provider}/callback"];

// ------------------------------------------------------------------ the sweep

#[tokio::test]
async fn registered_paths_are_found() {
    let paths = registered_paths(&wiring::routes(&config(&[])));
    assert!(paths.contains(&"/healthz".to_string()), "{paths:?}");
    assert!(paths.contains(&"/login".to_string()), "{paths:?}");
    assert!(paths.iter().any(|p| p.starts_with("/api/v3/")), "nested api routes must be flattened: {paths:?}");
    assert!(paths.len() >= 8, "{paths:?}");
}

#[tokio::test]
async fn every_registered_route_is_private_unless_listed_public() {
    let mut routes = wiring::routes(&config(&[]));
    routes = routes.merge(probes());
    let paths = registered_paths(&routes);
    let app = app(config(&[]), false);
    let mut checked = 0;
    for pattern in &paths {
        let path = concrete(pattern);
        for method in METHODS {
            if is_public(method, &path) {
                continue;
            }
            // Each from its own address: rejections count against the signed-out budget (ch-ulq),
            // and this sweep is about which routes are open, not about that budget.
            let peer = format!("198.51.{}.{}", (checked / 250) % 250, checked % 250 + 1);
            let r = send(&app, method.clone(), &path, &[], Some(&peer)).await;
            // The discovery and token routes are machine routes: a wrong method on one is a 405,
            // never a page and never a handler.
            let machine = ch_core::access::classify(&path) == ch_core::access::RouteKind::OAuth;
            assert!(
                r.status == StatusCode::SEE_OTHER
                    || r.status == StatusCode::UNAUTHORIZED
                    || (machine && r.status == StatusCode::METHOD_NOT_ALLOWED),
                "{method} {path} answered {} to a signed-out request",
                r.status
            );
            for secret in ["SECRET-PAGE", "SECRET-API", "SECRET-GIT", "SECRET-REGISTRY", "SECRET-RUNNER"] {
                assert!(!r.body.contains(secret), "{method} {path} leaked {secret}");
            }
            checked += 1;
        }
    }
    assert!(checked > 40, "the sweep covered only {checked} requests");
}

#[tokio::test]
async fn public_routes_table_and_router_agree() {
    let paths = registered_paths(&wiring::routes(&config(&[])));
    // Every registered route that a signed-out visitor can reach is in the table.
    for pattern in &paths {
        let path = concrete(pattern);
        let reachable = METHODS.iter().any(|m| is_public(m, &path));
        let listed = PUBLIC_ROUTES.iter().any(|r| same_shape(r.pattern, pattern));
        assert_eq!(reachable, listed, "{pattern}: reachable signed-out = {reachable}, listed = {listed}");
    }
    // Every public route is registered, or is on the not-built-yet list.
    for r in PUBLIC_ROUTES {
        let registered = paths.iter().any(|p| same_shape(p, r.pattern));
        let pending = PUBLIC_NOT_BUILT_YET.contains(&r.pattern);
        assert!(registered || pending, "public route {} is neither registered nor on PUBLIC_NOT_BUILT_YET", r.pattern);
        assert!(!(registered && pending), "{} is registered now: remove it from PUBLIC_NOT_BUILT_YET", r.pattern);
    }
}

fn same_shape(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        s.split('/')
            .map(|p| {
                if p.starts_with("{*") {
                    "*".to_string()
                } else if p.starts_with('{') {
                    "{}".to_string()
                } else {
                    p.to_string()
                }
            })
            .collect::<Vec<_>>()
    };
    norm(a) == norm(b)
}

#[tokio::test]
async fn no_sign_up_route_exists_in_any_form() {
    let app = app(config(&[]), false);
    for path in
        ["/signup", "/register", "/join", "/sign-up", "/users/new", "/accounts/new", "/api/v3/users", "/setup", "/install", "/admin/setup"]
    {
        for m in [Method::GET, Method::POST, Method::PUT] {
            let r = send(&app, m.clone(), path, &[], None).await;
            assert!(r.status == StatusCode::SEE_OTHER || r.status == StatusCode::UNAUTHORIZED, "{m} {path} -> {}", r.status);
        }
    }
    assert!(!PUBLIC_ROUTES.iter().any(|r| {
        ["signup", "register", "join", "setup", "install"].iter().any(|w| r.pattern.contains(w) && r.pattern != "/api/runner/v1/register")
    }));
}

// ------------------------------------------------------------------ the e2e step 1 sweep

#[tokio::test]
async fn signed_out_sweep_gives_the_documented_answers() {
    let app = app(config(&[]), false);
    // Batch C3 routes are on the list too: the badge is for signed-in viewers only (ch-c1d).
    for path in ["/", "/explore", "/root", "/nobody/nothing", "/signup", "/metrics", "/readyz", "/x/y/actions/workflows/ci.yml/badge.svg"] {
        let r = get_(&app, path).await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "{path}");
        assert!(r.headers[header::LOCATION].to_str().unwrap().starts_with("/login"), "{path}");
    }
    for path in [
        "/api/v3/user",
        "/api/v3/meta",
        "/api/v3/version",
        "/api/v3/repos/x/y",
        "/api/v3",
        "/api/v3/rate_limit",
        "/api/v3/repos/x/y/actions/runs/1/logs",
        "/api/v3/orgs/x/teams/y",
    ] {
        let r = get_(&app, path).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{path}");
        assert_eq!(r.body, r#"{"message":"Requires authentication"}"#);
        assert!(r.headers[header::CONTENT_TYPE].to_str().unwrap().starts_with("application/json"));
    }
    let r = get_(&app, "/x/y.git/info/refs?service=git-upload-pack").await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert!(r.headers[header::WWW_AUTHENTICATE].to_str().unwrap().starts_with("Basic realm="));
    let r = send(&app, Method::POST, "/x/y/git-receive-pack", &[], None).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let r = get_(&app, "/v2/").await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let www = r.headers[header::WWW_AUTHENTICATE].to_str().unwrap();
    assert!(www.starts_with("Bearer realm=\"https://code.example.com/v2/token\""), "{www}");
    assert_eq!(r.headers["docker-distribution-api-version"], "registry/2.0");
    let r = get_(&app, "/api/runner/v1/jobs/request").await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn rejections_do_not_reveal_whether_a_thing_exists() {
    let app = app(config(&[]), false);
    // Pairs of (exists, does not exist) of the same kind.
    for (a, b) in [
        ("/api/v3/secret", "/api/v3/nothing-here"),
        ("/api/v3/user", "/api/v3/zzz"),
        ("/owner/repo.git/info/refs", "/nobody/nothing.git/info/refs"),
        ("/v2/secret", "/v2/nothing"),
        ("/api/runner/v1/secret", "/api/runner/v1/nothing"),
    ] {
        let (ra, rb) = (get_(&app, a).await, get_(&app, b).await);
        assert_eq!((ra.status, &ra.body), (rb.status, &rb.body), "{a} vs {b}");
        assert_eq!(ra.headers.get(header::WWW_AUTHENTICATE), rb.headers.get(header::WWW_AUTHENTICATE), "{a} vs {b}");
    }
    // Pages differ only in the echoed `next` path.
    let (ra, rb) = (get_(&app, "/secret/page").await, get_(&app, "/not/a/page").await);
    assert_eq!(ra.status, rb.status);
    assert_eq!(ra.body, rb.body);
}

#[tokio::test]
async fn next_is_url_encoded_and_only_for_get() {
    let app = app(config(&[]), false);
    let r = get_(&app, "/alice/demo?tab=issues&x=a%20b").await;
    assert_eq!(r.headers[header::LOCATION], "/login?next=/alice/demo%3Ftab%3Dissues%26x%3Da%2520b");
    let r = send(&app, Method::POST, "/alice/demo/settings", &[], None).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(r.headers[header::LOCATION], "/login");
    // A path that tries to bounce somewhere else never reaches `next`.
    let r = get_(&app, "//evil.example/x").await;
    assert_eq!(r.headers[header::LOCATION], "/login");
    // A path that is sign-in itself does not loop.
    let r = get_(&app, "/login/").await;
    assert_eq!(r.headers[header::LOCATION], "/login");
}

#[tokio::test]
async fn rejections_are_never_cached() {
    let app = app(config(&[]), false);
    for p in ["/x", "/api/v3/user", "/v2/"] {
        assert_eq!(get_(&app, p).await.headers[header::CACHE_CONTROL], "no-store", "{p}");
    }
}

// ------------------------------------------------------------------ what a visitor does see

#[tokio::test]
async fn the_sign_in_page_with_zero_accounts() {
    let app = app(config(&[]), false);
    let r = get_(&app, "/login").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("No accounts yet. The server administrator creates the first one."));
    let lower = r.body.to_lowercase();
    assert!(!lower.contains("sign up") && !lower.contains("register") && !lower.contains("create an account"));
    assert!(!lower.contains("<form"), "no form until the identity stream builds sign in");
}

#[tokio::test]
async fn healthz_says_ok_and_nothing_else() {
    let app = app(config(&[]), false);
    let r = get_(&app, "/healthz").await;
    assert_eq!((r.status, r.body.as_str()), (StatusCode::OK, "ok"));
    let r = get_(&app, "/robots.txt").await;
    assert_eq!(r.body, "User-agent: *\nDisallow: /\n");
    let r = send(&app, Method::POST, "/healthz", &[], None).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn readyz_and_metrics_exist_only_on_the_internal_listener() {
    let paths = registered_paths(&wiring::routes(&config(&[])));
    assert!(!paths.iter().any(|p| p == "/readyz" || p == "/metrics"), "{paths:?}");
}

#[tokio::test]
async fn static_and_favicon_are_public_and_carry_no_secrets() {
    let app = app(config(&[]), false);
    let r = get_(&app, "/favicon.svg").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.starts_with("<svg"));
    let r = get_(&app, "/static/app.css").await;
    assert_ne!(r.status, StatusCode::SEE_OTHER, "static files are public");
}

// ------------------------------------------------------------------ headers on everything

#[tokio::test]
async fn security_headers_are_on_every_kind_of_response() {
    let app = app(config(&[]), true);
    let responses = vec![
        get_(&app, "/login").await,
        get_(&app, "/healthz").await,
        get_(&app, "/").await,
        get_(&app, "/api/v3/user").await,
        get_(&app, "/v2/").await,
        get_(&app, "/x/y.git/info/refs").await,
        send(&app, Method::GET, "/secret/page", &[("cookie", "__Host-ch_session=good-session")], None).await,
        send(&app, Method::GET, "/nothing/here/at/all", &[("cookie", "__Host-ch_session=good-session")], None).await,
        send(&app, Method::GET, "/api/v3/slow", &[("authorization", "Bearer slow_down")], None).await,
    ];
    for r in responses {
        for h in [
            "strict-transport-security",
            "content-security-policy",
            "x-frame-options",
            "x-content-type-options",
            "referrer-policy",
            "cross-origin-opener-policy",
            "cross-origin-resource-policy",
            "permissions-policy",
            "x-robots-tag",
        ] {
            assert!(r.headers.contains_key(h), "{} response is missing {h}", r.status);
        }
        assert_eq!(r.headers["x-frame-options"], "DENY");
        assert!(r.headers["content-security-policy"].to_str().unwrap().contains("frame-ancestors 'none'"));
        assert!(!r.headers.contains_key("server"));
        assert!(r.headers.contains_key("x-request-id"));
    }
}

#[tokio::test]
async fn hsts_only_on_https_origins() {
    let mut cfg = config(&[]);
    cfg.server.external_url = "http://127.0.0.1:3200".into();
    let app = app(cfg, false);
    let r = get_(&app, "/healthz").await;
    assert!(!r.headers.contains_key("strict-transport-security"));
    assert!(r.headers.contains_key("content-security-policy"));
}

// ------------------------------------------------------------------ credentials open only their own doors

#[tokio::test]
async fn each_credential_opens_only_its_own_kind_of_route() {
    let app = app(config(&[]), true);
    let session = [("cookie", "__Host-ch_session=good-session")];
    let basic = [("authorization", "Basic YWxpY2U6cHc=")]; // alice:pw
    let token = [("authorization", "Bearer chp_good")];
    let job = [("authorization", "Bearer job_good")];
    let runner = [("authorization", "Bearer runner_good")];

    // (path, credential name, credential, should the handler run?)
    let page = "/secret/page";
    let api = "/api/v3/secret";
    let git = "/owner/repo.git/info/refs";
    let registry = "/v2/secret";
    let runner_api = "/api/runner/v1/secret";
    let cases: Vec<(&str, &str, &[(&str, &str)], bool)> = vec![
        (page, "session", &session, true),
        (page, "basic", &basic, false),
        (page, "token", &token, false),
        (page, "job", &job, false),
        (page, "runner", &runner, false),
        (api, "session", &session, true),
        (api, "basic", &basic, true),
        (api, "token", &token, true),
        (api, "job", &job, true),
        (api, "runner", &runner, false),
        (git, "session", &session, false),
        (git, "basic", &basic, true),
        (git, "token", &token, true),
        (git, "job", &job, true),
        (git, "runner", &runner, false),
        (registry, "session", &session, false),
        (registry, "basic", &basic, true),
        (registry, "token", &token, true),
        (registry, "job", &job, false),
        (registry, "runner", &runner, false),
        (runner_api, "session", &session, false),
        (runner_api, "basic", &basic, false),
        (runner_api, "token", &token, false),
        (runner_api, "job", &job, true),
        (runner_api, "runner", &runner, true),
    ];
    for (path, name, creds, allowed) in cases {
        let r = send(&app, Method::GET, path, creds, None).await;
        let ran = r.body.starts_with("SECRET-");
        assert_eq!(ran, allowed, "{name} on {path} -> {} {}", r.status, r.body);
        if !allowed {
            assert!(r.status == StatusCode::SEE_OTHER || r.status == StatusCode::UNAUTHORIZED);
        }
    }
}

#[tokio::test]
async fn bad_credentials_are_treated_as_no_credentials() {
    let app = app(config(&[]), true);
    for hdrs in [
        vec![("authorization", "Basic YWxpY2U6d3Jvbmc=")], // alice:wrong
        vec![("authorization", "Bearer nope")],
        vec![("authorization", "Bearer")],
        vec![("authorization", "Digest abc")],
        vec![("authorization", "Basic !!!")],
        vec![("cookie", "__Host-ch_session=forged")],
        vec![("cookie", "ch_session=good-session")], // wrong name for https
    ] {
        let r = send(&app, Method::GET, "/api/v3/secret", &hdrs, None).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{hdrs:?}");
        // And a public route still works with a bad credential.
        let r = send(&app, Method::GET, "/login", &hdrs, None).await;
        assert_eq!(r.status, StatusCode::OK, "{hdrs:?}");
    }
}

#[tokio::test]
async fn a_signed_in_unknown_path_is_a_plain_404() {
    let app = app(config(&[]), true);
    let r = send(&app, Method::GET, "/no/such/thing", &[("cookie", "__Host-ch_session=good-session")], None).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn rate_limited_and_broken_authentication_fail_closed() {
    let app = app(config(&[]), true);
    let r = send(&app, Method::GET, "/api/v3/secret", &[("authorization", "Bearer slow_down")], None).await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(r.headers[header::RETRY_AFTER], "30");
    let r = send(&app, Method::GET, "/api/v3/secret", &[("authorization", "Bearer explode")], None).await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(!r.body.contains("hunter2"), "internal detail leaked: {}", r.body);
    assert!(!r.body.contains("SECRET"));
}

// ------------------------------------------------------------------ proxy headers

#[tokio::test]
async fn identity_headers_never_sign_anyone_in_and_are_stripped() {
    let app = app(config(&[]), true);
    let evil = [
        ("remote-user", "root"),
        ("remote-groups", "admins"),
        ("remote-email", "r@x.io"),
        ("remote-name", "Root"),
        ("x-forwarded-user", "root"),
    ];
    let r = send(&app, Method::GET, "/api/v3/secret", &evil, None).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let r = send(&app, Method::GET, "/secret/page", &evil, None).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    // Even when signed in properly, the handler never sees the identity header.
    let mut hdrs = vec![("authorization", "Bearer chp_good")];
    hdrs.extend(evil);
    let r = send(&app, Method::GET, "/api/v3/whoami", &hdrs, None).await;
    assert!(r.body.contains("remote_user=false"), "{}", r.body);
}

#[tokio::test]
async fn forwarded_for_is_trusted_only_from_a_trusted_proxy() {
    let app = app(config(&["10.0.0.0/8"]), true);
    let auth = [("authorization", "Bearer chp_good"), ("x-forwarded-for", "198.51.100.7"), ("x-forwarded-proto", "https")];
    // From the proxy: the forwarded address and scheme are believed.
    let r = send(&app, Method::GET, "/api/v3/whoami", &auth, Some("10.1.2.3")).await;
    assert!(r.body.contains("ip=198.51.100.7") && r.body.contains("secure=true"), "{}", r.body);
    // From anyone else: the headers are ignored and the socket address is used.
    let r = send(&app, Method::GET, "/api/v3/whoami", &auth, Some("203.0.113.50")).await;
    assert!(r.body.contains("ip=203.0.113.50") && r.body.contains("secure=false"), "{}", r.body);
    // A client cannot add a fake hop in front of the real one.
    let spoof = [("authorization", "Bearer chp_good"), ("x-forwarded-for", "1.1.1.1, 198.51.100.7")];
    let r = send(&app, Method::GET, "/api/v3/whoami", &spoof, Some("10.1.2.3")).await;
    assert!(r.body.contains("ip=198.51.100.7"), "{}", r.body);
}

// ------------------------------------------------------------------ the API meta group

#[tokio::test]
async fn api_meta_needs_a_signed_in_caller() {
    let app = app(config(&[]), true);
    for p in ["/api/v3", "/api/v3/version", "/api/v3/meta", "/api/v3/rate_limit"] {
        assert_eq!(get_(&app, p).await.status, StatusCode::UNAUTHORIZED, "{p}");
        let r = send(&app, Method::GET, p, &[("authorization", "Bearer chp_good")], None).await;
        assert_eq!(r.status, StatusCode::OK, "{p}");
    }
}

// ------------------------------------------------------------------ the browser workspace

/// Every workspace row the two tables know: the eighteen routes and the two admin ones.
fn all_workspace_rows() -> Vec<(&'static str, &'static str)> {
    let mut rows: Vec<(&'static str, &'static str)> = common::WORKSPACE_ROUTES.iter().map(|&(m, p, _, _)| (m, p)).collect();
    rows.extend_from_slice(common::ADMIN_WORKSPACE_ROUTES);
    rows
}

#[test]
fn workspace_routes_table_matches_the_router() {
    // The router's own workspace rows, and the two tables the access suites check against.
    let mut router_rows: Vec<(&str, &str)> =
        ch_web::routes::route_table().into_iter().filter(|(_, path)| path.contains("/workspace")).collect();
    router_rows.sort();
    let mut table: Vec<(&str, &str)> = common::WORKSPACE_ROUTES.iter().map(|&(m, p, _, _)| (m, p)).collect();
    table.extend_from_slice(common::ADMIN_WORKSPACE_ROUTES);
    table.sort();
    assert_eq!(router_rows, table, "the router and the tables disagree");

    // None of it is public, by either door: the fixed public table or the public repository routes.
    for &(m, pattern) in &table {
        let method = Method::from_bytes(m.as_bytes()).unwrap();
        let path = common::path_for(pattern, "alice", "demo");
        assert!(!is_public(&method, &path), "{m} {path} is public");
        // `/admin/workspace` has the shape of `/{owner}/{repo}`, but `admin` is a reserved name
        // that no account or repository can have, so only the repository rows are checked here.
        if pattern.starts_with("/{owner}/{repo}/workspace") {
            assert!(ch_core::access::public_repo_route(&method, &path, None).is_none(), "{m} {path} is a public repository route");
        }
    }
}

#[tokio::test]
async fn signed_out_workspace_requests_learn_nothing() {
    let app = app(config(&[]), true);
    let mut checked = 0;
    for (_m, pattern) in all_workspace_rows() {
        for method in METHODS {
            let path = common::path_for(pattern, "alice", "demo");
            let missing = common::path_for(pattern, "nobody-here", "missing");
            // Its own address per pair, so the signed-out rejection budget (ch-ulq) stays out of it.
            let peer = format!("198.51.100.{}", checked % 250 + 1);
            let r = send(&app, method.clone(), &path, &[], Some(&peer)).await;
            assert!(
                r.status == StatusCode::SEE_OTHER || r.status == StatusCode::UNAUTHORIZED,
                "{method} {path} answered {} to a signed-out request",
                r.status
            );
            let cache = r.headers.get(header::CACHE_CONTROL).and_then(|v| v.to_str().ok()).unwrap_or("");
            assert!(cache.contains("no-store"), "{method} {path}: cache-control {cache:?}");
            if r.status == StatusCode::SEE_OTHER {
                assert!(r.body.is_empty(), "{method} {path}: {}", r.body);
            }
            // A repository that is not there answers the same as one that is.
            let other = send(&app, method.clone(), &missing, &[], Some(&peer)).await;
            assert_eq!((r.status, &r.body), (other.status, &other.body), "{method} {path} vs {missing}");
            checked += 1;
        }
    }
    assert!(checked > 100, "the sweep covered only {checked} requests");
}

#[tokio::test]
async fn a_suspended_session_is_signed_out() {
    let app = app(config(&[]), true);
    let hdrs = [("cookie", "__Host-ch_session=suspended-session")];
    for (m, pattern) in all_workspace_rows() {
        let path = common::path_for(pattern, "alice", "demo");
        let missing = common::path_for(pattern, "nobody-here", "missing");
        let r = send(&app, Method::GET, &path, &hdrs, None).await;
        assert!(
            r.status == StatusCode::SEE_OTHER || r.status == StatusCode::UNAUTHORIZED,
            "{m} {path} answered {} to a suspended session",
            r.status
        );
        let cache = r.headers.get(header::CACHE_CONTROL).and_then(|v| v.to_str().ok()).unwrap_or("");
        assert!(cache.contains("no-store"), "{m} {path}: cache-control {cache:?}");
        if r.status == StatusCode::SEE_OTHER {
            assert!(r.body.is_empty(), "{m} {path}: {}", r.body);
        }
        let other = send(&app, Method::GET, &missing, &hdrs, None).await;
        assert_eq!((r.status, &r.body), (other.status, &other.body), "{m} {path} vs {missing}");
    }
}

#[tokio::test]
async fn tokens_basic_jobs_and_runners_open_no_workspace_route() {
    let app = app(config(&[]), true);
    for (m, pattern) in all_workspace_rows() {
        let path = common::path_for(pattern, "alice", "demo");
        for (name, credential) in [
            ("a token", "Bearer chp_good"),
            ("a job token", "Bearer job_good"),
            ("a runner token", "Bearer runner_good"),
            ("basic auth", "Basic YWxpY2U6cHc="), // alice:pw
        ] {
            let hdrs = [("authorization", credential)];
            let r = send(&app, Method::GET, &path, &hdrs, None).await;
            assert!(
                r.status == StatusCode::SEE_OTHER || r.status == StatusCode::UNAUTHORIZED,
                "{name} on {m} {path} answered {}",
                r.status
            );
            assert!(!matches!(r.status.as_u16(), 200 | 404 | 409), "{name} on {m} {path}");
            if r.status == StatusCode::SEE_OTHER {
                assert!(r.headers[header::LOCATION].to_str().unwrap().starts_with("/login"), "{name} on {m} {path}");
            }
        }
    }
}

#[tokio::test]
async fn rejected_signed_out_requests_count_against_the_address() {
    // ch-ulq: the gate turns these away before the guard, so they used to cost nothing.
    let app = app(config(&[]), true);
    let mut limited = None;
    for i in 0..400 {
        let r = send(&app, Method::GET, "/api/v3/user", &[], Some("203.0.113.77")).await;
        if r.status == StatusCode::TOO_MANY_REQUESTS {
            assert!(r.headers.contains_key(header::RETRY_AFTER));
            limited = Some(i);
            break;
        }
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    }
    let at = limited.expect("rejections are rate limited");
    assert!(at >= 100, "the budget is the signed-out burst, not less: limited after {at}");
    // Another address still gets the ordinary answer.
    assert_eq!(send(&app, Method::GET, "/api/v3/user", &[], Some("203.0.113.78")).await.status, StatusCode::UNAUTHORIZED);
}
