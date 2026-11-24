//! ch-ejm: importing a repository from a URL. The real router serves a public repository over
//! smart HTTP on a loopback port, and a second account imports it. The same address is refused
//! while private imports are off, a redirect is never followed, and a failed import leaves no
//! repository behind. Real services, real git, a real database.

#![allow(clippy::unwrap_used)]

use axum::Router;
use axum::response::Redirect;
use ch_core::model::*;
use ch_core::state::AppState;
use ch_core::{Actor, AuthVia, Config, RepoId, UserId};
use codehaven::app::build_router;
use codehaven::metrics::Metrics;
use codehaven::wiring;
use sqlx::PgPool;
use std::net::SocketAddr;
use std::sync::Arc;

fn c() -> ClientInfo {
    ClientInfo::local()
}

async fn state(pool: PgPool, allow_private: bool, dir: &std::path::Path) -> AppState {
    let mut cfg = Config::for_tests();
    cfg.storage.data_dir = dir.to_path_buf();
    cfg.server.external_url = "http://127.0.0.1".into();
    cfg.imports.allow_private = allow_private;
    wiring::build_state(Arc::new(cfg), pool).await.unwrap()
}

async fn serve(app: Router) -> SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(l, app.into_make_service_with_connect_info::<SocketAddr>()).await;
    });
    addr
}

async fn user(s: &AppState, name: &str) -> (UserId, Actor) {
    let u = s
        .accounts
        .create_user(
            &Actor::System,
            NewUser { username: name.into(), email: format!("{name}@example.org"), display_name: None, is_admin: false, can_run_ci: false },
            &c(),
        )
        .await
        .unwrap();
    (u.id, Actor::User { id: u.id, is_admin: false, has_2fa: true, via: AuthVia::Session })
}

fn new(owner: UserId, name: &str) -> NewRepo {
    NewRepo {
        owner_id: owner,
        name: name.into(),
        description: String::new(),
        visibility: Visibility::Private,
        default_branch: None,
        auto_init: false,
    }
}

/// A public repository with a README and a second branch, served by the real router.
async fn source(s: &AppState, pool: &PgPool) -> (SocketAddr, String) {
    let (alice, a) = user(s, "alice").await;
    let r = s.repos.create(&a, NewRepo { auto_init: true, ..new(alice, "src") }, &c()).await.unwrap();
    let main = s.git.resolve(r.id, "refs/heads/main").await.unwrap();
    s.git.create_ref(&a, r.id, "topic", &main, &c()).await.unwrap();
    sqlx::query("UPDATE visibility_settings SET allow_public_repos = true").execute(pool).await.unwrap();
    sqlx::query("UPDATE repositories SET access_tier = 'public', public_since = now() WHERE id = $1")
        .bind(r.id.0)
        .execute(pool)
        .await
        .unwrap();
    let addr = serve(build_router(s.clone(), Metrics::new(), None)).await;
    (addr, main)
}

async fn count(pool: &PgPool, name: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM repositories WHERE name = $1").bind(name).fetch_one(pool).await.unwrap()
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_public_repository_imports_with_its_branches(pool: PgPool) {
    let dir = tempfile::tempdir().unwrap();
    let s = state(pool.clone(), true, dir.path()).await;
    let (addr, main) = source(&s, &pool).await;
    let (bob, b) = user(&s, "bob").await;
    let url = format!("http://{addr}/alice/src.git");
    let r = s.repos.import(&b, new(bob, "copy"), &url, &c()).await.unwrap();
    assert_eq!(r.visibility, Visibility::Private, "an import starts private like any new repository");
    assert!(!r.is_empty);
    assert_eq!(r.default_branch, "main");
    assert_eq!(s.git.resolve(r.id, "refs/heads/main").await.unwrap(), main, "the same commit");
    assert!(s.git.resolve(r.id, "refs/heads/topic").await.is_ok(), "every branch comes across");
    // A name that is taken is refused before anything is fetched.
    assert!(s.repos.import(&b, new(bob, "copy"), &url, &c()).await.is_err());
    // Signed out, nothing.
    assert!(s.repos.import(&Actor::Anonymous, new(bob, "anon"), &url, &c()).await.is_err());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_private_address_is_refused_and_nothing_is_left(pool: PgPool) {
    let dir = tempfile::tempdir().unwrap();
    let s = state(pool.clone(), false, dir.path()).await;
    let (addr, _) = source(&s, &pool).await;
    let (bob, b) = user(&s, "bob").await;
    for url in [
        format!("http://{addr}/alice/src.git"),
        format!("https://{addr}/alice/src.git"),
        format!("https://localhost:{}/alice/src.git", addr.port()),
        "https://169.254.169.254/latest/meta-data".to_string(),
        "file:///etc".to_string(),
    ] {
        let e = s.repos.import(&b, new(bob, "copy"), &url, &c()).await.unwrap_err();
        assert!(matches!(e, ch_core::Error::Validation(_)), "{url}: {e:?}");
    }
    assert_eq!(count(&pool, "copy").await, 0, "no repository was made");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_redirect_is_never_followed(pool: PgPool) {
    let dir = tempfile::tempdir().unwrap();
    let s = state(pool.clone(), true, dir.path()).await;
    let (addr, _) = source(&s, &pool).await;
    let (bob, b) = user(&s, "bob").await;
    // A server that sends every request on to the real repository.
    let to = format!("http://{addr}");
    let bounce = Router::new().fallback(move |uri: axum::http::Uri| {
        let to = to.clone();
        async move { Redirect::temporary(&format!("{to}{}", uri.path_and_query().map(|p| p.as_str()).unwrap_or("/"))) }
    });
    let b_addr = serve(bounce).await;
    let e = s.repos.import(&b, new(bob, "bounced"), &format!("http://{b_addr}/alice/src.git"), &c()).await.unwrap_err();
    assert!(e.to_string().contains("did not work") || matches!(e, ch_core::Error::Validation(_)), "{e:?}");
    assert_eq!(count(&pool, "bounced").await, 0, "the half-made repository is removed");
    let _ = RepoId(0);
}
