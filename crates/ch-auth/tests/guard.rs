//! The request guard: CSRF, Origin, rate limits (ARCHITECTURE.md section 7.6).
#![allow(clippy::unwrap_used)]

use axum::Router;
use axum::body::Body;
use axum::extract::{Extension, Request};
use axum::http::{Method, StatusCode, header};
use axum::middleware::{Next, from_fn};
use axum::routing::{get, post};
use ch_auth::Guard;
use ch_core::identity::CsrfToken;
use ch_core::{Actor, AuthVia, Config, TokenId, UserId};
use http_body_util::BodyExt;
use std::net::IpAddr;
use std::sync::Arc;
use tower::ServiceExt;

const ORIGIN: &str = "https://test.example";

fn config(external: &str) -> Config {
    let mut c = Config::for_tests();
    c.server.external_url = external.into();
    c
}

/// A router with the guard in place and the caller fixed to `actor`, as the gate would leave it.
fn make_app(cfg: &Config, actor: Actor, ip: &str) -> Router {
    let guard = Arc::new(Guard::with_key(cfg, [7u8; 32]));
    let ip: IpAddr = ip.parse().unwrap();
    Router::new()
        .route("/login", get(token_page).post(echo_body))
        .route("/settings", post(echo_body).get(token_page))
        .route("/static/app.css", get(|| async { "css" }))
        .route("/api/v3/thing", post(echo_body).get(|| async { "ok" }))
        .route("/api/runner/v1/register", post(|| async { "registered" }))
        .route("/org/repo/git-receive-pack", post(|| async { "pushed" }))
        .layer(from_fn(move |mut req: Request, next: Next| {
            let guard = guard.clone();
            let actor = actor.clone();
            async move {
                req.extensions_mut().insert(actor);
                guard.handle(req, next, ip).await
            }
        }))
}

async fn token_page(Extension(t): Extension<CsrfToken>) -> String {
    t.0
}

async fn echo_body(body: String) -> String {
    format!("got:{body}")
}

struct Resp {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: String,
}

async fn send(app: &Router, req: Request<Body>) -> Resp {
    let resp = app.clone().oneshot(req).await.unwrap();
    let (parts, body) = resp.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    Resp { status: parts.status, headers: parts.headers, body: String::from_utf8_lossy(&bytes).into_owned() }
}

fn get_req(path: &str, cookie: Option<&str>) -> Request<Body> {
    let mut b = Request::builder().method(Method::GET).uri(path);
    if let Some(c) = cookie {
        b = b.header(header::COOKIE, c);
    }
    b.body(Body::empty()).unwrap()
}

fn set_cookie_value(r: &Resp, name: &str) -> Option<String> {
    r.headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|s| s.strip_prefix(&format!("{name}=")).map(|rest| rest.split(';').next().unwrap().to_string()))
}

fn user() -> Actor {
    Actor::User { id: UserId(1), is_admin: false, has_2fa: false, via: AuthVia::Session }
}

#[tokio::test]
async fn first_page_gets_a_csrf_cookie_and_a_token_that_matches_it() {
    let app = make_app(&config(ORIGIN), Actor::Anonymous, "203.0.113.1");
    let r = send(&app, get_req("/login", None)).await;
    assert_eq!(r.status, StatusCode::OK);
    let cookie = set_cookie_value(&r, "__Host-ch_csrf").expect("csrf cookie");
    assert_eq!(cookie.len(), 64);
    let sc = r.headers.get_all(header::SET_COOKIE).iter().next().unwrap().to_str().unwrap().to_string();
    for part in ["Path=/", "HttpOnly", "SameSite=Lax", "Secure"] {
        assert!(sc.contains(part), "{sc}");
    }
    assert_eq!(r.body.len(), 64, "the token is shown to the page");
    // Visiting again with the cookie yields the same token and no new cookie.
    let again = send(&app, get_req("/login", Some(&format!("__Host-ch_csrf={cookie}")))).await;
    assert_eq!(again.body, r.body);
    assert!(set_cookie_value(&again, "__Host-ch_csrf").is_none());
}

#[tokio::test]
async fn post_needs_origin_and_token() {
    let app = make_app(&config(ORIGIN), Actor::Anonymous, "203.0.113.2");
    let page = send(&app, get_req("/login", None)).await;
    let cookie = format!("__Host-ch_csrf={}", set_cookie_value(&page, "__Host-ch_csrf").unwrap());
    let token = page.body.clone();

    let post = |origin: Option<&str>, referer: Option<&str>, cookie: Option<&str>, header_token: Option<&str>, body: &str, ctype: &str| {
        let mut b = Request::builder().method(Method::POST).uri("/settings").header(header::CONTENT_TYPE, ctype);
        if let Some(o) = origin {
            b = b.header(header::ORIGIN, o);
        }
        if let Some(r) = referer {
            b = b.header(header::REFERER, r);
        }
        if let Some(c) = cookie {
            b = b.header(header::COOKIE, c);
        }
        if let Some(t) = header_token {
            b = b.header("x-csrf-token", t);
        }
        b.body(Body::from(body.to_string())).unwrap()
    };
    let form = "application/x-www-form-urlencoded";
    let good_body = format!("username=a&_csrf={token}");

    // Everything right: the handler sees the whole body, token included.
    let ok = send(&app, post(Some(ORIGIN), None, Some(&cookie), None, &good_body, form)).await;
    assert_eq!(ok.status, StatusCode::OK);
    assert_eq!(ok.body, format!("got:{good_body}"), "the body is passed on intact");
    // Header token works too (htmx).
    assert_eq!(send(&app, post(Some(ORIGIN), None, Some(&cookie), Some(&token), "x", "text/plain")).await.status, StatusCode::OK);
    // Referer stands in for a missing Origin.
    assert_eq!(
        send(&app, post(None, Some("https://test.example/login?next=/x"), Some(&cookie), None, &good_body, form)).await.status,
        StatusCode::OK
    );

    // Each missing piece is refused.
    let cases = [
        ("no origin or referer", post(None, None, Some(&cookie), None, &good_body, form)),
        ("wrong origin", post(Some("https://evil.example"), None, Some(&cookie), None, &good_body, form)),
        ("null origin", post(Some("null"), None, Some(&cookie), None, &good_body, form)),
        ("http origin", post(Some("http://test.example"), None, Some(&cookie), None, &good_body, form)),
        ("origin with port", post(Some("https://test.example:8443"), None, Some(&cookie), None, &good_body, form)),
        ("lookalike referer", post(None, Some("https://test.example.evil.example/"), Some(&cookie), None, &good_body, form)),
        ("no token", post(Some(ORIGIN), None, Some(&cookie), None, "username=a", form)),
        ("wrong token", post(Some(ORIGIN), None, Some(&cookie), None, "username=a&_csrf=deadbeef", form)),
        ("empty token", post(Some(ORIGIN), None, Some(&cookie), None, "username=a&_csrf=", form)),
        ("no cookie", post(Some(ORIGIN), None, None, None, &good_body, form)),
        ("someone else's cookie", post(Some(ORIGIN), None, Some(&format!("__Host-ch_csrf={}", "a".repeat(64))), None, &good_body, form)),
        (
            "token in a json body is not read",
            post(Some(ORIGIN), None, Some(&cookie), None, &format!("{{\"_csrf\":\"{token}\"}}"), "application/json"),
        ),
    ];
    for (name, req) in cases {
        let r = send(&app, req).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{name}");
        assert_eq!(r.headers.get(header::CACHE_CONTROL).unwrap(), "no-store");
    }
}

#[tokio::test]
async fn gets_never_need_a_token_and_other_methods_do() {
    let app = make_app(&config(ORIGIN), user(), "203.0.113.3");
    assert_eq!(send(&app, get_req("/settings", Some("__Host-ch_session=abc"))).await.status, StatusCode::OK);
    for m in [Method::PUT, Method::PATCH, Method::DELETE, Method::POST] {
        let req = Request::builder().method(m.clone()).uri("/settings").header(header::ORIGIN, ORIGIN).body(Body::empty()).unwrap();
        let r = send(&app, req).await;
        assert_ne!(r.status, StatusCode::OK, "{m}");
    }
}

#[tokio::test]
async fn a_session_token_is_bound_to_that_session() {
    let app = make_app(&config(ORIGIN), user(), "203.0.113.4");
    let sess_a = format!("__Host-ch_session={}", "a".repeat(64));
    let sess_b = format!("__Host-ch_session={}", "b".repeat(64));
    let token_a = send(&app, get_req("/settings", Some(&sess_a))).await.body;
    let token_b = send(&app, get_req("/settings", Some(&sess_b))).await.body;
    assert_ne!(token_a, token_b);
    let post = |cookie: &str, token: &str| {
        Request::builder()
            .method(Method::POST)
            .uri("/settings")
            .header(header::ORIGIN, ORIGIN)
            .header(header::COOKIE, cookie.to_string())
            .header("x-csrf-token", token.to_string())
            .body(Body::from("x"))
            .unwrap()
    };
    assert_eq!(send(&app, post(&sess_a, &token_a)).await.status, StatusCode::OK);
    assert_eq!(send(&app, post(&sess_a, &token_b)).await.status, StatusCode::FORBIDDEN, "another session's token");
    // A signed-in visitor is not handed a csrf cookie: the session is the binding.
    let r = send(&app, get_req("/settings", Some(&sess_a))).await;
    assert!(set_cookie_value(&r, "__Host-ch_csrf").is_none());
}

#[tokio::test]
async fn tokens_in_authorization_skip_csrf_and_non_browser_routes_are_exempt() {
    let token_actor = Actor::Token { user_id: UserId(1), token_id: TokenId(5), scopes: ch_core::Scopes::new(["repo"]) };
    let app = make_app(&config(ORIGIN), token_actor, "203.0.113.5");
    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/v3/thing")
        .header(header::AUTHORIZATION, "Bearer chp_x")
        .body(Body::from("hi"))
        .unwrap();
    assert_eq!(send(&app, req).await.status, StatusCode::OK, "no cookies in play, so nothing to forge");
    // The same call with only a cookie is an API request by a browser: it needs the token.
    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/v3/thing")
        .header(header::COOKIE, "__Host-ch_session=x")
        .body(Body::from("hi"))
        .unwrap();
    let r = send(&app, req).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(r.body.contains("\"message\""), "API refusals are JSON: {}", r.body);

    let anon = make_app(&config(ORIGIN), Actor::Anonymous, "203.0.113.6");
    for path in ["/api/runner/v1/register", "/org/repo/git-receive-pack"] {
        let req = Request::builder().method(Method::POST).uri(path).body(Body::from("x")).unwrap();
        assert_eq!(send(&anon, req).await.status, StatusCode::OK, "{path} never uses cookies");
    }
}

#[tokio::test]
async fn a_junk_authorization_header_does_not_buy_a_csrf_bypass() {
    // The gate leaves a caller with an unrecognised credential anonymous, so the form checks
    // still apply to them.
    let app = make_app(&config(ORIGIN), Actor::Anonymous, "203.0.113.90");
    let req = Request::builder()
        .method(Method::POST)
        .uri("/settings")
        .header(header::AUTHORIZATION, "Bearer junk")
        .header(header::ORIGIN, ORIGIN)
        .body(Body::from("x"))
        .unwrap();
    assert_eq!(send(&app, req).await.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn plain_http_uses_unprefixed_cookie_names() {
    let app = make_app(&config("http://127.0.0.1:3200"), Actor::Anonymous, "127.0.0.1");
    let r = send(&app, get_req("/login", None)).await;
    let sc = r.headers.get_all(header::SET_COOKIE).iter().next().unwrap().to_str().unwrap().to_string();
    assert!(sc.starts_with("ch_csrf="), "{sc}");
    assert!(!sc.contains("Secure"));
    let cookie = format!("ch_csrf={}", set_cookie_value(&r, "ch_csrf").unwrap());
    let req = Request::builder()
        .method(Method::POST)
        .uri("/login")
        .header(header::ORIGIN, "http://127.0.0.1:3200")
        .header(header::COOKIE, cookie)
        .header("x-csrf-token", r.body.clone())
        .body(Body::from("x"))
        .unwrap();
    assert_eq!(send(&app, req).await.status, StatusCode::OK);
}

#[tokio::test]
async fn sign_in_pages_are_limited_per_address() {
    let cfg = config(ORIGIN);
    let app = make_app(&cfg, Actor::Anonymous, "203.0.113.50");
    let page = send(&app, get_req("/login", None)).await;
    let cookie = format!("__Host-ch_csrf={}", set_cookie_value(&page, "__Host-ch_csrf").unwrap());
    let post = |cookie: &str| {
        Request::builder()
            .method(Method::POST)
            .uri("/login")
            .header(header::ORIGIN, ORIGIN)
            .header(header::COOKIE, cookie.to_string())
            .header("x-csrf-token", page.body.clone())
            .body(Body::from("x"))
            .unwrap()
    };
    let mut statuses = Vec::new();
    for _ in 0..7 {
        statuses.push(send(&app, post(&cookie)).await);
    }
    assert!(statuses[..5].iter().all(|r| r.status == StatusCode::OK), "burst of 5");
    assert_eq!(statuses[5].status, StatusCode::TOO_MANY_REQUESTS);
    assert!(statuses[5].headers.get(header::RETRY_AFTER).is_some());
    assert_eq!(statuses[6].status, StatusCode::TOO_MANY_REQUESTS);
    // Another address has its own budget.
    let other = make_app(&cfg, Actor::Anonymous, "203.0.113.51");
    assert_eq!(send(&other, post(&cookie)).await.status, StatusCode::OK);
}

#[tokio::test]
async fn anonymous_traffic_is_capped_but_static_files_are_not() {
    let app = make_app(&config(ORIGIN), Actor::Anonymous, "203.0.113.60");
    for i in 0..150 {
        let r = send(&app, get_req("/static/app.css", None)).await;
        assert_eq!(r.status, StatusCode::OK, "static {i}");
    }
    let mut limited = 0;
    for _ in 0..130 {
        if send(&app, get_req("/login", None)).await.status == StatusCode::TOO_MANY_REQUESTS {
            limited += 1;
        }
    }
    assert!(limited >= 9, "120 a minute for everything else, limited {limited}");
}

#[tokio::test]
async fn api_calls_report_the_rate_limit() {
    let token_actor = Actor::Token { user_id: UserId(1), token_id: TokenId(9), scopes: ch_core::Scopes::new(["repo"]) };
    let app = make_app(&config(ORIGIN), token_actor, "203.0.113.70");
    let req = || Request::builder().uri("/api/v3/thing").header(header::AUTHORIZATION, "Bearer x").body(Body::empty()).unwrap();
    let r1 = send(&app, req()).await;
    let r2 = send(&app, req()).await;
    let n = |r: &Resp, h: &str| r.headers.get(h).unwrap().to_str().unwrap().parse::<i64>().unwrap();
    assert_eq!(n(&r1, "x-ratelimit-limit"), 5000);
    assert_eq!(n(&r1, "x-ratelimit-remaining"), 4999);
    assert_eq!(n(&r2, "x-ratelimit-remaining"), 4998);
    assert!(n(&r1, "x-ratelimit-reset") > chrono::Utc::now().timestamp());
    // Pages do not carry the headers.
    let page = send(&app, get_req("/login", None)).await;
    assert!(page.headers.get("x-ratelimit-limit").is_none());
}
