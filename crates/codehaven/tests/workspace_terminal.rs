//! The workspace terminal on the server (spec sections 5, 7 and 11), against the real services
//! and a real database: the frame document and its headers, the snapshot of files for the shell,
//! and that both stay signed in only with the repository's read permission. The terminal itself
//! runs in the browser; tests/e2e/terminal.mjs drives it there.

#![allow(clippy::unwrap_used)]

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use ch_core::model::{ClientInfo, NewRepo, NewUser, Visibility, WorkspaceSettings};
use ch_core::state::AppState;
use ch_core::{Actor, AuthVia, Config, RepoId, UserId};
use codehaven::app::build_router;
use codehaven::metrics::Metrics;
use codehaven::wiring;
use http_body_util::BodyExt;
use sqlx::PgPool;
use std::net::SocketAddr;
use std::sync::Arc;
use tower::ServiceExt;

const PW: &str = "violet harbour lantern 83 teapot";
const ORIGIN: &str = "https://code.example.com";

struct Harness {
    app: Router,
    state: AppState,
    _dir: tempfile::TempDir,
}

async fn harness(pool: PgPool, terminal_allowed: bool) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::for_tests();
    cfg.storage.data_dir = dir.path().to_path_buf();
    cfg.server.external_url = ORIGIN.into();
    cfg.workspace.enabled = true;
    cfg.workspace.terminal = terminal_allowed;
    let cfg = Arc::new(cfg);
    let state = wiring::build_state(cfg.clone(), pool).await.unwrap();
    let app = build_router(state.clone(), Metrics::new(), None);
    Harness { app, state, _dir: dir }
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Reply {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    fn header(&self, name: &str) -> &str {
        self.headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or("")
    }
}

async fn send(h: &Harness, path: &str, cookie: Option<&str>, extra: &[(&str, &str)]) -> Reply {
    let mut b = Request::builder().method(Method::GET).uri(path);
    if let Some(c) = cookie {
        b = b.header(header::COOKIE, format!("__Host-ch_session={c}"));
    }
    for (k, v) in extra {
        b = b.header(*k, *v);
    }
    let mut req = b.body(Body::empty()).unwrap();
    req.extensions_mut().insert(ConnectInfo("203.0.113.9:5555".parse::<SocketAddr>().unwrap()));
    let resp = h.app.clone().oneshot(req).await.unwrap();
    let (parts, body) = resp.into_parts();
    Reply { status: parts.status, headers: parts.headers, body: body.collect().await.unwrap().to_bytes().to_vec() }
}

struct Person {
    id: UserId,
    cookie: String,
}

async fn person(h: &Harness, name: &str) -> Person {
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
    let s = h.state.credentials.sign_in(name, PW, &ClientInfo::local()).await.unwrap();
    Person { id: u.id, cookie: s.grant.cookie.clone() }
}

fn session(id: UserId) -> Actor {
    Actor::User { id, is_admin: false, has_2fa: false, via: AuthVia::Session }
}

async fn switches(h: &Harness, editor: bool, terminal: bool) {
    h.state.repos.set_workspace_settings(&Actor::System, WorkspaceSettings { editor, terminal }, &ClientInfo::local()).await.unwrap();
}

/// alice owns a private repository with one commit; bob reads it; eve has no access.
async fn world(h: &Harness) -> (Person, Person, Person, RepoId) {
    let alice = person(h, "alice").await;
    let bob = person(h, "bob").await;
    let eve = person(h, "eve").await;
    let repo = h
        .state
        .repos
        .create(
            &session(alice.id),
            NewRepo {
                owner_id: alice.id,
                name: "demo".into(),
                description: String::new(),
                visibility: Visibility::Private,
                default_branch: None,
                auto_init: true,
            },
            &ClientInfo::local(),
        )
        .await
        .unwrap();
    h.state.repos.set_collaborator(&session(alice.id), repo.id, bob.id, ch_core::Permission::Read, &ClientInfo::local()).await.unwrap();
    (alice, bob, eve, repo.id)
}

fn key(h: &Harness, p: &Person, repo: RepoId) -> String {
    ch_web::views::workspace::keys::workspace_key(&h.state.config, &p.cookie, p.id, repo)
}

const FRAME: &str = "/alice/demo/workspace/terminal";

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_frame_is_one_hashed_document_for_a_reader_and_nobody_else(pool: PgPool) {
    let h = harness(pool, true).await;
    let (_alice, bob, eve, _repo) = world(&h).await;
    switches(&h, true, true).await;

    let r = send(&h, FRAME, Some(&bob.cookie), &[("sec-fetch-dest", "iframe")]).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    assert_eq!(r.header("content-type"), "text/html; charset=utf-8");
    let csp = r.header("content-security-policy");
    assert!(csp.starts_with("sandbox allow-scripts; default-src 'none'; script-src 'sha256-"), "{csp}");
    assert!(csp.contains("connect-src 'none'") && csp.contains("frame-ancestors 'self'") && csp.contains("worker-src blob:"), "{csp}");
    assert!(!csp.contains("allow-same-origin") && !csp.contains("unsafe-eval"), "{csp}");
    assert_eq!(r.header("x-frame-options"), "SAMEORIGIN");
    assert_eq!(r.header("cache-control"), "no-store");
    assert_eq!(r.header("referrer-policy"), "no-referrer");
    let body = r.text();
    assert!(body.contains("new Terminal("), "xterm is inside");
    // Nothing about the person, the session or the repository is in the frame.
    for secret in [bob.cookie.as_str(), "bob", "demo", "alice"] {
        assert!(!body.contains(secret), "the frame holds {secret:?}");
    }
    // The browser leaves the header out on plain HTTP: the same document.
    let r = send(&h, FRAME, Some(&bob.cookie), &[]).await;
    assert_eq!(r.status, StatusCode::OK);
    // A browser that takes gzip gets it, with the same policy.
    let z = send(&h, FRAME, Some(&bob.cookie), &[("sec-fetch-dest", "iframe"), ("accept-encoding", "gzip, deflate, br")]).await;
    assert_eq!(z.header("content-encoding"), "gzip");
    assert_eq!(z.header("content-security-policy"), csp);
    assert!(z.body.len() < body.len() / 2);

    // Never a redirect for a signed-in caller: a query, a top-level visit and an embed are 404.
    for (path, dest) in [(format!("{FRAME}?x=1"), "iframe"), (FRAME.to_string(), "document"), (FRAME.to_string(), "embed")] {
        let r = send(&h, &path, Some(&bob.cookie), &[("sec-fetch-dest", dest)]).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{path} as {dest}: {}", r.status);
    }
    // No access, no frame: the same 404 as a repository that does not exist.
    let r = send(&h, FRAME, Some(&eve.cookie), &[("sec-fetch-dest", "iframe")]).await;
    let missing = send(&h, "/alice/nothing/workspace/terminal", Some(&eve.cookie), &[("sec-fetch-dest", "iframe")]).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(r.body, missing.body);
    // Signed out: the sign-in page with an empty body, the same as for any workspace route.
    let r = send(&h, FRAME, None, &[("sec-fetch-dest", "iframe")]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(r.header("location").starts_with("/login"));
    assert!(r.body.is_empty());
    // A non-canonical spelling is a 404, never a move, because the frame policy is per path.
    let r = send(&h, "/Alice/demo/workspace/terminal", Some(&bob.cookie), &[("sec-fetch-dest", "iframe")]).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    // Stage 4 is not here: no Linux source to serve.
    let r = send(&h, "/alice/demo/workspace/terminal/source.tar", Some(&bob.cookie), &[]).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_switch_and_the_config_both_turn_the_terminal_off(pool: PgPool) {
    let h = harness(pool, true).await;
    let (_alice, bob, _eve, repo) = world(&h).await;
    let k = key(&h, &bob, repo);
    // Switched off: the frame and the snapshot are 404, the shell names no frame and boot says off.
    switches(&h, true, false).await;
    assert_eq!(send(&h, FRAME, Some(&bob.cookie), &[("sec-fetch-dest", "iframe")]).await.status, StatusCode::NOT_FOUND);
    let shell = send(&h, "/alice/demo/workspace", Some(&bob.cookie), &[]).await;
    assert!(shell.header("content-security-policy").contains("frame-src 'none';"));
    let boot = send(&h, "/alice/demo/workspace/boot", Some(&bob.cookie), &[("x-workspace-key", &k)]).await;
    let v: serde_json::Value = serde_json::from_slice(&boot.body).unwrap();
    assert_eq!(v["terminal"], false);
    assert_eq!(v["terminal_runtime"].as_array().unwrap().len(), 0);
    // Switched on: the shell may frame exactly the terminal, and boot hands out the runtime.
    switches(&h, true, true).await;
    let shell = send(&h, "/alice/demo/workspace", Some(&bob.cookie), &[]).await;
    assert!(shell.header("content-security-policy").contains(&format!("frame-src {ORIGIN}/alice/demo/workspace/terminal;")));
    let boot = send(&h, "/alice/demo/workspace/boot", Some(&bob.cookie), &[("x-workspace-key", &k)]).await;
    let v: serde_json::Value = serde_json::from_slice(&boot.body).unwrap();
    assert_eq!(v["terminal"], true);
    let rt = v["terminal_runtime"].as_array().unwrap();
    assert_eq!(rt.len(), 2);
    for piece in rt {
        let url = piece["url"].as_str().unwrap();
        assert!(url.starts_with("/alice/demo/workspace/assets/"), "{url}");
        assert!(piece["integrity"].as_str().unwrap().starts_with("sha256-"));
        // The piece is served to this reader, and its bytes match the integrity boot gave.
        let r = send(&h, url, Some(&bob.cookie), &[]).await;
        assert_eq!(r.status, StatusCode::OK, "{url}");
    }
    // The config file is the ceiling: the switch cannot lift it.
    let mut cfg = (*h.state.config).clone();
    cfg.workspace.terminal = false;
    assert!(!WorkspaceSettings { editor: true, terminal: true }.terminal_on(&cfg));
    cfg.workspace.terminal = true;
    cfg.workspace.enabled = false;
    assert!(!WorkspaceSettings { editor: true, terminal: true }.terminal_on(&cfg));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_config_without_the_terminal_answers_404_whatever_the_switch(pool: PgPool) {
    let h = harness(pool, false).await;
    let (_alice, bob, _eve, repo) = world(&h).await;
    switches(&h, true, true).await;
    assert_eq!(send(&h, FRAME, Some(&bob.cookie), &[("sec-fetch-dest", "iframe")]).await.status, StatusCode::NOT_FOUND);
    let head = h.state.git.workspace_branch_head(repo, "main").await.unwrap().unwrap();
    let path = format!("/alice/demo/workspace/snapshot?branch=main&sha={head}");
    let r = send(&h, &path, Some(&bob.cookie), &[("x-workspace-key", &key(&h, &bob, repo))]).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_snapshot_is_framed_bytes_for_a_reader_with_the_key(pool: PgPool) {
    let h = harness(pool, true).await;
    let (alice, bob, eve, repo) = world(&h).await;
    switches(&h, true, true).await;
    let head = h.state.git.workspace_branch_head(repo, "main").await.unwrap().unwrap();
    let path = format!("/alice/demo/workspace/snapshot?branch=main&sha={head}");
    let k = key(&h, &bob, repo);
    let r = send(&h, &path, Some(&bob.cookie), &[("x-workspace-key", &k)]).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text());
    assert_eq!(r.header("content-type"), "application/octet-stream");
    assert_eq!(r.header("content-disposition"), "attachment");
    assert_eq!(r.header("cache-control"), "no-store");
    assert_eq!(&r.body[..8], b"CHSNAP1\n");
    let count = u32::from_be_bytes(r.body[8..12].try_into().unwrap());
    assert_eq!(count, 1, "README.md from the first commit");
    let plen = u32::from_be_bytes(r.body[13..17].try_into().unwrap()) as usize;
    assert_eq!(&r.body[17..17 + plen], b"README.md");
    // Without the key, or with another person's, the tab is stale.
    let r = send(&h, &path, Some(&bob.cookie), &[]).await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    let r = send(&h, &path, Some(&bob.cookie), &[("x-workspace-key", &key(&h, &alice, repo))]).await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    // No access: 404 even with a key made for this repository.
    let r = send(&h, &path, Some(&eve.cookie), &[("x-workspace-key", &key(&h, &eve, repo))]).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    // A commit that is not on the branch, and a folder that is not a folder.
    let r = send(
        &h,
        &format!("/alice/demo/workspace/snapshot?branch=main&sha={}", "a".repeat(40)),
        Some(&bob.cookie),
        &[("x-workspace-key", &k)],
    )
    .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    let r = send(&h, &format!("{path}&path_b64=L3NyYw=="), Some(&bob.cookie), &[("x-workspace-key", &k)]).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "/src is not a folder name: {}", r.text());
}
