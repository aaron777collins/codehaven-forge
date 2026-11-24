//! The real identity services behind the real gate, guard and security headers, with a real
//! database. The ch-auth suite covers the services one by one; this proves they are wired
//! together the way the binary wires them.

#![allow(clippy::unwrap_used)]

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode, header};
use ch_core::model::{ClientInfo, NewUser};
use ch_core::state::AppState;
use ch_core::{Actor, Config};
use codehaven::app::build_router;
use codehaven::metrics::Metrics;
use codehaven::wiring;
use http_body_util::BodyExt;
use sqlx::PgPool;
use std::net::SocketAddr;
use std::sync::Arc;
use tower::ServiceExt;

const PW: &str = "violet harbour lantern 83 teapot";
const ED25519: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFb6NT8qYavZlutmABtenBoJjXu0zs0KW7M8CidCX/T+ test@codehaven";

struct Harness {
    app: Router,
    state: AppState,
    guard: ch_auth::Guard,
    _dir: tempfile::TempDir,
}

async fn harness(pool: PgPool) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::for_tests();
    cfg.storage.data_dir = dir.path().to_path_buf();
    cfg.server.external_url = "https://code.example.com".into();
    let cfg = Arc::new(cfg);
    let state = wiring::build_state(cfg.clone(), pool).await.unwrap();
    let guard = ch_auth::Guard::new(&cfg);
    let app = build_router(state.clone(), Metrics::new(), None);
    Harness { app, state, guard, _dir: dir }
}

struct Reply {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: String,
}

async fn send(app: &Router, method: Method, path: &str, hdrs: &[(&str, &str)], body: Option<&str>) -> Reply {
    let mut b = Request::builder().method(method).uri(path);
    for (k, v) in hdrs {
        b = b.header(*k, *v);
    }
    let mut req = b.body(Body::from(body.unwrap_or("").to_string())).unwrap();
    req.extensions_mut().insert(ConnectInfo("203.0.113.9:5555".parse::<SocketAddr>().unwrap()));
    let resp = app.clone().oneshot(req).await.unwrap();
    let (parts, body) = resp.into_parts();
    Reply {
        status: parts.status,
        headers: parts.headers,
        body: String::from_utf8_lossy(&body.collect().await.unwrap().to_bytes()).into_owned(),
    }
}

async fn post_token(app: &Router, cookie: &str, extra: &[(&str, &str)]) -> Reply {
    let mut hdrs = vec![("cookie", cookie), ("content-type", "application/json")];
    hdrs.extend_from_slice(extra);
    let body = serde_json::json!({"name": "ci", "scopes": ["repo"], "expires_in_days": 5}).to_string();
    send(app, Method::POST, "/api/v3/user/tokens", &hdrs, Some(&body)).await
}

async fn make_user(h: &Harness, name: &str, admin: bool) -> ch_core::UserId {
    let u = h
        .state
        .accounts
        .create_user(
            &Actor::System,
            NewUser { username: name.into(), email: format!("{name}@example.org"), display_name: None, is_admin: admin, can_run_ci: false },
            &ClientInfo::local(),
        )
        .await
        .unwrap();
    h.state.accounts.set_password(&Actor::System, u.id, PW, &ClientInfo::local()).await.unwrap();
    u.id
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn sessions_tokens_and_the_gate_agree(pool: PgPool) {
    let h = harness(pool).await;
    let id = make_user(&h, "alice", false).await;

    // Signed out: pages redirect, the API refuses, and a proxy identity header means nothing.
    assert_eq!(send(&h.app, Method::GET, "/anything", &[], None).await.status, StatusCode::SEE_OTHER);
    assert_eq!(send(&h.app, Method::GET, "/api/v3/user", &[], None).await.status, StatusCode::UNAUTHORIZED);
    let proxied = send(
        &h.app,
        Method::GET,
        "/api/v3/user",
        &[("remote-user", "alice"), ("x-forwarded-user", "alice"), ("remote-groups", "admins")],
        None,
    )
    .await;
    assert_eq!(proxied.status, StatusCode::UNAUTHORIZED, "no proxy-header login");

    // A real sign-in gives a cookie that opens pages and the API.
    let s = h.state.credentials.sign_in("alice", PW, &ClientInfo::local()).await.unwrap();
    let cookie = format!("__Host-ch_session={}", s.grant.cookie);
    let api = send(&h.app, Method::GET, "/api/v3/user", &[("cookie", &cookie)], None).await;
    assert_eq!(api.status, StatusCode::OK, "{}", api.body);
    assert!(api.body.contains("\"login\":\"alice\""));
    assert_eq!(api.headers.get("x-ratelimit-limit").unwrap(), "5000");
    assert!(api.headers.get("content-security-policy").is_some(), "security headers wrap everything");
    assert_ne!(
        send(&h.app, Method::GET, "/anything", &[("cookie", &cookie)], None).await.status,
        StatusCode::SEE_OTHER,
        "a session opens pages"
    );

    // An unsafe request with only a cookie needs the Origin and the CSRF token.
    assert_eq!(post_token(&h.app, &cookie, &[]).await.status, StatusCode::FORBIDDEN, "no origin, no token");
    let token = h.guard.token_for_session(&s.grant.cookie);
    assert_eq!(
        post_token(&h.app, &cookie, &[("origin", "https://evil.example"), ("x-csrf-token", &token)]).await.status,
        StatusCode::FORBIDDEN,
        "wrong origin"
    );
    assert_eq!(post_token(&h.app, &cookie, &[("origin", "https://code.example.com")]).await.status, StatusCode::FORBIDDEN, "no token");
    let created = post_token(&h.app, &cookie, &[("origin", "https://code.example.com"), ("x-csrf-token", &token)]).await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let secret = serde_json::from_str::<serde_json::Value>(&created.body).unwrap()["token"].as_str().unwrap().to_string();

    // The token opens the API and git, not pages, and needs no CSRF because it is no cookie.
    let bearer = format!("Bearer {secret}");
    assert_eq!(send(&h.app, Method::GET, "/api/v3/user", &[("authorization", &bearer)], None).await.status, StatusCode::OK);
    assert_eq!(
        send(&h.app, Method::GET, "/anything", &[("authorization", &bearer)], None).await.status,
        StatusCode::SEE_OTHER,
        "a token opens no web page"
    );
    let key = serde_json::json!({"title": "laptop", "key": ED25519}).to_string();
    let k =
        send(&h.app, Method::POST, "/api/v3/user/keys", &[("authorization", &bearer), ("content-type", "application/json")], Some(&key))
            .await;
    // An SSH key acts as the whole account in Git, so a token needs `user`, `repo` and `workflow`
    // to add one (ch-fwm, ch-s7r). This one has only `repo`.
    assert_eq!(k.status, StatusCode::FORBIDDEN, "{}", k.body);
    // A token cannot mint tokens, and never reaches the site admin routes.
    let t2 = send(
        &h.app,
        Method::POST,
        "/api/v3/user/tokens",
        &[("authorization", &bearer), ("content-type", "application/json")],
        Some(r#"{"name":"x","scopes":["repo"]}"#),
    )
    .await;
    assert_eq!(t2.status, StatusCode::FORBIDDEN);
    assert_eq!(send(&h.app, Method::GET, "/api/v3/admin/invites", &[("authorization", &bearer)], None).await.status, StatusCode::FORBIDDEN);
    assert_eq!(
        send(&h.app, Method::GET, "/api/v3/admin/invites", &[("cookie", &cookie)], None).await.status,
        StatusCode::FORBIDDEN,
        "not an admin"
    );

    // Basic with the token as the password opens the API and git (the placeholder 404s past the gate).
    use base64::Engine;
    let basic = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("alice:{secret}")));
    assert_eq!(send(&h.app, Method::GET, "/api/v3/user", &[("authorization", &basic)], None).await.status, StatusCode::OK);
    let git = send(&h.app, Method::GET, "/alice/demo.git/info/refs?service=git-upload-pack", &[("authorization", &basic)], None).await;
    assert_ne!(git.status, StatusCode::UNAUTHORIZED, "git accepts a token");
    assert_eq!(
        send(&h.app, Method::GET, "/alice/demo.git/info/refs?service=git-upload-pack", &[("cookie", &cookie)], None).await.status,
        StatusCode::UNAUTHORIZED,
        "a session opens no git endpoint"
    );

    // Suspending ends everything at once.
    h.state.accounts.suspend(&Actor::System, id, &ClientInfo::local()).await.unwrap();
    assert_eq!(send(&h.app, Method::GET, "/api/v3/user", &[("cookie", &cookie)], None).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(send(&h.app, Method::GET, "/api/v3/user", &[("authorization", &bearer)], None).await.status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn bad_credentials_are_limited_and_audited(pool: PgPool) {
    let h = harness(pool).await;
    make_user(&h, "alice", false).await;
    let bad = format!("Bearer chp_{}", "q".repeat(40));
    let mut limited = false;
    for _ in 0..40 {
        let r = send(&h.app, Method::GET, "/api/v3/user", &[("authorization", &bad)], None).await;
        if r.status == StatusCode::TOO_MANY_REQUESTS {
            limited = true;
            break;
        }
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    }
    assert!(limited, "a flood of bad tokens from one address is cut off");
    // Even a signed-out visitor to a public page still works from that address (a 429 is for credentials, not pages).
    let r = send(&h.app, Method::GET, "/healthz", &[], None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body, "ok");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_sign_in_page_counts_accounts_for_real(pool: PgPool) {
    let h = harness(pool).await;
    let r = send(&h.app, Method::GET, "/login", &[], None).await;
    assert!(r.body.contains("No accounts yet"), "{}", r.body);
    // The visitor is handed a CSRF cookie on the first page, ready for the sign-in form.
    assert!(r.headers.get_all(header::SET_COOKIE).iter().any(|v| v.to_str().unwrap().starts_with("__Host-ch_csrf=")));
    make_user(&h, "alice", false).await;
    let r = send(&h.app, Method::GET, "/login", &[], None).await;
    assert!(!r.body.contains("No accounts yet"));
}
