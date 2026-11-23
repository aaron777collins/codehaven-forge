//! Access that ends while a page or a stream is open (browser workspace spec, step 1): a reset
//! link ends sessions when it is issued, the live re-check sees every way access ends without
//! keeping a session alive, and the gate refuses a signed-in page loaded in a frame.

#![allow(clippy::unwrap_used)]

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode};
use ch_core::live::Credential;
use ch_core::model::{ClientInfo, LinkPurpose, NewUser};
use ch_core::state::AppState;
use ch_core::{Actor, Config, UserId};
use codehaven::app::build_router;
use codehaven::metrics::Metrics;
use codehaven::wiring;
use sqlx::PgPool;
use std::net::SocketAddr;
use std::sync::Arc;
use tower::ServiceExt;

const PW: &str = "violet harbour lantern 83 teapot";

struct Harness {
    app: Router,
    state: AppState,
    _dir: tempfile::TempDir,
}

async fn harness(pool: PgPool) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::for_tests();
    cfg.storage.data_dir = dir.path().to_path_buf();
    cfg.server.external_url = "https://code.example.com".into();
    let cfg = Arc::new(cfg);
    let state = wiring::build_state(cfg.clone(), pool).await.unwrap();
    let app = build_router(state.clone(), Metrics::new(), None);
    Harness { app, state, _dir: dir }
}

async fn status(app: &Router, path: &str, hdrs: &[(&str, &str)]) -> StatusCode {
    let mut b = Request::builder().method(Method::GET).uri(path);
    for (k, v) in hdrs {
        b = b.header(*k, *v);
    }
    let mut req = b.body(Body::empty()).unwrap();
    req.extensions_mut().insert(ConnectInfo("203.0.113.9:5555".parse::<SocketAddr>().unwrap()));
    app.clone().oneshot(req).await.unwrap().status()
}

async fn make_user(h: &Harness, name: &str) -> UserId {
    let u = h
        .state
        .accounts
        .create_user(
            &Actor::System,
            NewUser { username: name.into(), email: format!("{name}@example.org"), display_name: None, is_admin: false, can_run_ci: false },
            &ClientInfo::local(),
        )
        .await
        .unwrap();
    h.state.accounts.set_password(&Actor::System, u.id, PW, &ClientInfo::local()).await.unwrap();
    u.id
}

/// Signs in and returns the cookie header and the credential the gate records for it.
async fn sign_in(h: &Harness, name: &str) -> (String, Credential) {
    let s = h.state.credentials.sign_in(name, PW, &ClientInfo::local()).await.unwrap();
    let cookie = s.grant.cookie.clone();
    (format!("__Host-ch_session={cookie}"), Credential::Session { hash: ch_db::tokens::hash_token(&cookie) })
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_reset_link_ends_sessions_when_it_is_issued(pool: PgPool) {
    let h = harness(pool).await;
    let alice = make_user(&h, "alice").await;
    let (cookie, _) = sign_in(&h, "alice").await;
    assert_eq!(status(&h.app, "/api/v3/user", &[("cookie", &cookie)]).await, StatusCode::OK);

    // A first password link leaves the session alone.
    h.state.accounts.issue_link(&Actor::System, alice, LinkPurpose::SetPassword, 60, &ClientInfo::local()).await.unwrap();
    assert_eq!(status(&h.app, "/api/v3/user", &[("cookie", &cookie)]).await, StatusCode::OK);

    // A reset ends it at once, before anyone opens the link.
    h.state.accounts.issue_link(&Actor::System, alice, LinkPurpose::ResetPassword, 60, &ClientInfo::local()).await.unwrap();
    assert_eq!(status(&h.app, "/api/v3/user", &[("cookie", &cookie)]).await, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_live_check_sees_access_end_and_keeps_no_session_alive(pool: PgPool) {
    let h = harness(pool).await;
    let alice = make_user(&h, "alice").await;
    let (_, cred) = sign_in(&h, "alice").await;
    let Some(Actor::User { id, .. }) = h.state.authn.recheck(&cred).await.unwrap() else { panic!("a live session") };
    assert_eq!(id, alice);

    // The check reads only: an old last-seen time and expiry stay as they were.
    let Credential::Session { hash } = &cred else { unreachable!() };
    sqlx::query(
        "UPDATE sessions SET last_seen_at = now() - interval '2 hours', expires_at = now() + interval '1 hour' WHERE token_hash = $1",
    )
    .bind(hash.as_slice())
    .execute(&h.state.db)
    .await
    .unwrap();
    let before: (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>) =
        sqlx::query_as("SELECT last_seen_at, expires_at FROM sessions WHERE token_hash = $1")
            .bind(hash.as_slice())
            .fetch_one(&h.state.db)
            .await
            .unwrap();
    for _ in 0..3 {
        assert!(h.state.authn.recheck(&cred).await.unwrap().is_some());
    }
    let after: (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>) =
        sqlx::query_as("SELECT last_seen_at, expires_at FROM sessions WHERE token_hash = $1")
            .bind(hash.as_slice())
            .fetch_one(&h.state.db)
            .await
            .unwrap();
    assert_eq!(before, after, "the re-check must not extend the session");

    // Sign-out everywhere.
    h.state.accounts.sign_out_everywhere(&Actor::System, alice, &ClientInfo::local()).await.unwrap();
    assert!(h.state.authn.recheck(&cred).await.unwrap().is_none());

    // Suspension.
    let (_, cred) = sign_in(&h, "alice").await;
    h.state.accounts.suspend(&Actor::System, alice, &ClientInfo::local()).await.unwrap();
    assert!(h.state.authn.recheck(&cred).await.unwrap().is_none());
    h.state.directory.unsuspend(&Actor::System, alice, &ClientInfo::local()).await.unwrap();

    // A reset link issued.
    let (_, cred) = sign_in(&h, "alice").await;
    h.state.accounts.issue_link(&Actor::System, alice, LinkPurpose::ResetPassword, 60, &ClientInfo::local()).await.unwrap();
    assert!(h.state.authn.recheck(&cred).await.unwrap().is_none());

    // Expiry.
    let (_, cred) = sign_in(&h, "alice").await;
    let Credential::Session { hash } = &cred else { unreachable!() };
    sqlx::query("UPDATE sessions SET expires_at = now() - interval '1 second' WHERE token_hash = $1")
        .bind(hash.as_slice())
        .execute(&h.state.db)
        .await
        .unwrap();
    assert!(h.state.authn.recheck(&cred).await.unwrap().is_none());

    // A credential that never existed, and a password fingerprint that changed.
    assert!(h.state.authn.recheck(&Credential::Session { hash: vec![7; 32] }).await.unwrap().is_none());
    let fp = h.state.authn.password_fingerprint(alice).await.unwrap().unwrap();
    let basic = Credential::Basic { user_id: alice, fingerprint: fp };
    assert!(h.state.authn.recheck(&basic).await.unwrap().is_some());
    h.state.accounts.set_password(&Actor::System, alice, "another long passphrase 7 kettle", &ClientInfo::local()).await.unwrap();
    assert!(h.state.authn.recheck(&basic).await.unwrap().is_none(), "a password change ends a Basic credential");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_signed_in_page_opens_in_a_frame_only_as_the_terminal(pool: PgPool) {
    let h = harness(pool).await;
    make_user(&h, "alice").await;
    let (cookie, _) = sign_in(&h, "alice").await;
    let framed = |d: &'static str| [("cookie", cookie.clone()), ("sec-fetch-dest", d.to_string())];

    for path in
        ["/alice", "/login", "/api/v3/user", "/alice/demo/workspace", "/alice/demo/workspace/terminal/", "/alice/demo/workspace/boot"]
    {
        for dest in ["iframe", "frame", "embed", "object", "fencedframe"] {
            let hdrs = framed(dest);
            let hdrs: Vec<(&str, &str)> = hdrs.iter().map(|(k, v)| (*k, v.as_str())).collect();
            assert_eq!(status(&h.app, path, &hdrs).await, StatusCode::FORBIDDEN, "{path} as {dest}");
        }
    }
    // The terminal document itself passes the gate (whatever the route then answers).
    let hdrs = framed("iframe");
    let hdrs: Vec<(&str, &str)> = hdrs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    assert_ne!(status(&h.app, "/alice/demo/workspace/terminal", &hdrs).await, StatusCode::FORBIDDEN);
    // A top-level document is unchanged.
    assert_ne!(status(&h.app, "/alice", &[("cookie", &cookie), ("sec-fetch-dest", "document")]).await, StatusCode::FORBIDDEN);
    // Signed-out answers are unchanged.
    assert_eq!(status(&h.app, "/alice", &[("sec-fetch-dest", "iframe")]).await, StatusCode::SEE_OTHER);
}
