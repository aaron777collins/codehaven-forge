//! Access to the browser workspace (spec section 5), against the real services with a real
//! database: what every route demands of the caller, and what each refusal looks like. The
//! half of the suite that needs no database is in `anonymous_surface.rs`.

#![allow(clippy::unwrap_used)]

mod common;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use ch_core::model::{ClientInfo, NewRepo, NewUser, Visibility, WorkspaceSettings};
use ch_core::state::AppState;
use ch_core::web_contract::RepoUpdate;
use ch_core::{Actor, AuthVia, Config, Error, Permission, RepoId, UserId};
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

// ------------------------------------------------------------------ harness

struct Harness {
    app: Router,
    state: AppState,
    guard: ch_auth::Guard,
    _dir: tempfile::TempDir,
}

/// A harness whose config allows the workspace when `workspace_enabled` is true. The config
/// file is the ceiling: the admin switch cannot lift it.
async fn harness(pool: PgPool, workspace_enabled: bool) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::for_tests();
    cfg.storage.data_dir = dir.path().to_path_buf();
    cfg.server.external_url = ORIGIN.into();
    cfg.workspace.enabled = workspace_enabled;
    let cfg = Arc::new(cfg);
    let state = wiring::build_state(cfg.clone(), pool).await.unwrap();
    // After `build_state` the master key file exists, so this guard and the router's agree.
    let guard = ch_auth::Guard::new(&cfg);
    let app = build_router(state.clone(), Metrics::new(), None);
    Harness { app, state, guard, _dir: dir }
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
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

/// A signed-in person: their id, their name, and one live session's cookie value.
struct Person {
    id: UserId,
    name: String,
    cookie: String,
}

fn session(id: UserId) -> Actor {
    Actor::User { id, is_admin: false, has_2fa: false, via: AuthVia::Session }
}

async fn make_person(h: &Harness, name: &str, admin: bool) -> Person {
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
    let s = h.state.credentials.sign_in(name, PW, &ClientInfo::local()).await.unwrap();
    Person { id: u.id, name: name.into(), cookie: s.grant.cookie.clone() }
}

/// The world the access tests work in: one owner with a live and an archived repository, the
/// four levels of access to them, and the editor switched on.
struct World {
    alice: Person,
    reader: Person,
    triager: Person,
    writer: Person,
    stranger: Person,
    admin: Person,
    demo: RepoId,
    old: RepoId,
}

async fn world(h: &Harness) -> World {
    let alice = make_person(h, "alice", false).await;
    let reader = make_person(h, "reader", false).await;
    let triager = make_person(h, "triager", false).await;
    let writer = make_person(h, "writer", false).await;
    let stranger = make_person(h, "stranger", false).await;
    let admin = make_person(h, "siteadmin", true).await;
    let new = |name: &str| NewRepo {
        owner_id: alice.id,
        name: name.into(),
        description: "Workspace access suite fixture.".into(),
        visibility: Visibility::Private,
        default_branch: None,
        auto_init: true,
    };
    let demo = h.state.repos.create(&session(alice.id), new("demo"), &ClientInfo::local()).await.unwrap();
    let old = h.state.repos.create(&session(alice.id), new("old"), &ClientInfo::local()).await.unwrap();
    h.state
        .repos
        .update(&session(alice.id), old.id, RepoUpdate { archived: Some(true), ..Default::default() }, &ClientInfo::local())
        .await
        .unwrap();
    for repo in [demo.id, old.id] {
        h.state.repos.set_collaborator(&session(alice.id), repo, reader.id, Permission::Read, &ClientInfo::local()).await.unwrap();
        h.state.repos.set_collaborator(&session(alice.id), repo, triager.id, Permission::Triage, &ClientInfo::local()).await.unwrap();
        h.state.repos.set_collaborator(&session(alice.id), repo, writer.id, Permission::Write, &ClientInfo::local()).await.unwrap();
    }
    set_editor(h, true).await;
    World { alice, reader, triager, writer, stranger, admin, demo: demo.id, old: old.id }
}

async fn set_editor(h: &Harness, on: bool) {
    h.state
        .repos
        .set_workspace_settings(&Actor::System, WorkspaceSettings { editor: on, terminal: false }, &ClientInfo::local())
        .await
        .unwrap();
}

// ------------------------------------------------------------------ request helpers

/// The workspace key for this person, session and repository.
fn key(h: &Harness, u: &Person, repo: RepoId) -> String {
    ch_web::views::workspace::keys::workspace_key(&h.state.config, &u.cookie, u.id, repo)
}

/// The headers every tab sends: the cookie, the exact origin and the CSRF token.
fn tab(h: &Harness, u: &Person) -> Vec<(String, String)> {
    vec![
        ("cookie".into(), format!("__Host-ch_session={}", u.cookie)),
        ("origin".into(), ORIGIN.into()),
        ("x-csrf-token".into(), h.guard.token_for_session(&u.cookie)),
    ]
}

fn as_strs(v: &[(String, String)]) -> Vec<(&str, &str)> {
    v.iter().map(|(k, w)| (k.as_str(), w.as_str())).collect()
}

/// One request with every header right: the tab, the workspace key, and for a POST an empty
/// JSON body. `x_key` chooses the key header, so the stale checks can send a wrong one or none.
async fn route_with_key(h: &Harness, u: &Person, method: &str, path: &str, x_key: Option<&str>) -> Reply {
    let mut hdrs = tab(h, u);
    if let Some(k) = x_key {
        hdrs.push(("x-workspace-key".into(), k.to_string()));
    }
    let post = method == "POST";
    if post {
        hdrs.push(("content-type".into(), "application/json".into()));
    }
    let hdrs = as_strs(&hdrs);
    send(&h.app, Method::from_bytes(method.as_bytes()).unwrap(), path, &hdrs, if post { Some("{}") } else { None }).await
}

/// The same, with the right key for this person, session and repository.
async fn route(h: &Harness, u: &Person, repo: RepoId, method: &str, path: &str) -> Reply {
    route_with_key(h, u, method, path, Some(&key(h, u, repo))).await
}

// ------------------------------------------------------------------ judgement helpers

/// Every check passed and the handler answered. A handler that is not finished answers 501 or
/// 422 after every check, and that counts.
fn admitted(r: &Reply) -> bool {
    !matches!(r.status.as_u16(), 303 | 404 | 403 | 409)
}

fn assert_admitted(r: &Reply, what: &str) {
    assert!(admitted(r), "{what}: answered {} with {}", r.status, r.body);
}

/// A 403 whose JSON says this caller may read the repository but not change it.
fn assert_read_only(r: &Reply, what: &str) {
    assert_eq!(r.status.as_u16(), 403, "{what}: answered {} with {}", r.status, r.body);
    let v: serde_json::Value = serde_json::from_str(&r.body).unwrap_or_else(|e| panic!("{what}: not JSON ({e}): {}", r.body));
    assert_eq!(v["code"], "read_only", "{what}: {}", r.body);
}

/// A 409 whose JSON says the tab's key did not check out.
fn assert_stale(r: &Reply, what: &str) {
    assert_eq!(r.status.as_u16(), 409, "{what}: answered {} with {}", r.status, r.body);
    let v: serde_json::Value = serde_json::from_str(&r.body).unwrap_or_else(|e| panic!("{what}: not JSON ({e}): {}", r.body));
    assert_eq!(v["code"], "stale", "{what}: {}", r.body);
}

/// The location header without its query, which only carries the caller's own path back.
fn location_path(r: &Reply) -> &str {
    r.headers[header::LOCATION].to_str().unwrap().split('?').next().unwrap()
}

/// Rows whose handler answers the plain 404 after every check passed: the terminal is off in
/// this version, and the run named in the path does not exist. A stranger's 404 and these look
/// the same by design, so these rows cannot show permission on their own.
fn blank_after_checks(pattern: &str) -> bool {
    pattern.contains("/terminal") || pattern.ends_with("/workspace/snapshot") || pattern.contains("/runs/{run}")
}

/// The tree, file and workflows routes read the commit the page pinned, so without a branch and
/// sha in the query they answer `head_moved` rather than run.
fn pinned_query(pattern: &str, head: Option<&str>) -> String {
    let pinned = pattern.ends_with("/workspace/tree") || pattern.ends_with("/workspace/file") || pattern.ends_with("/workspace/workflows");
    if pinned { format!("?branch=main&sha={}", head.unwrap_or_default()) } else { String::new() }
}

/// Signed out, every workspace route redirects to the sign-in page with an empty body, and
/// `alice/demo` and `nobody/missing` answer the same.
async fn signed_out_looks_the_same(h: &Harness) {
    for &(m, pattern, _, _) in common::WORKSPACE_ROUTES {
        let method = Method::from_bytes(m.as_bytes()).unwrap();
        let real = common::path_for(pattern, "alice", "demo");
        let missing = common::path_for(pattern, "nobody", "missing");
        let a = send(&h.app, method.clone(), &real, &[], None).await;
        let b = send(&h.app, method.clone(), &missing, &[], None).await;
        assert_eq!(a.status.as_u16(), 303, "{m} {real} signed out");
        assert!(location_path(&a).starts_with("/login"), "{m} {real}");
        assert!(a.body.is_empty(), "{m} {real}");
        // The query of the location carries the caller's own path back, so compare without it.
        assert_eq!((a.status, &a.body), (b.status, &b.body), "{m} {real} vs {missing}");
        assert_eq!(location_path(&a), location_path(&b), "{m} {real} vs {missing}");
    }
}

/// How many push events sit in the outbox. The table is `outbox_events`; there is no
/// `domain_events` table.
async fn push_count(db: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM outbox_events WHERE kind = 'push'").fetch_one(db).await.unwrap()
}

// ------------------------------------------------------------------ the tests

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn workspace_access_follows_the_repository_permission(pool: PgPool) {
    let h = harness(pool, true).await;
    let w = world(&h).await;
    let demo_head = h.state.git.workspace_branch_head(w.demo, "main").await.unwrap();
    let old_head = h.state.git.workspace_branch_head(w.old, "main").await.unwrap();

    // The main sweep. The asset row needs a real build id and is checked below, and the blank
    // rows are checked separately.
    for &(m, pattern, need, keyed) in common::WORKSPACE_ROUTES {
        if pattern.contains("/assets/") || blank_after_checks(pattern) {
            continue;
        }
        let path = format!("{}{}", common::path_for(pattern, "alice", "demo"), pinned_query(pattern, demo_head.as_deref()));
        let what = format!("{m} {path}");

        // A stranger learns nothing: the same 404 as for a repository that is not there.
        let r = route(&h, &w.stranger, w.demo, m, &path).await;
        assert_eq!(r.status.as_u16(), 404, "{what} as stranger");

        // Reading is enough where the route only reads. A reader and a triager are refused on
        // a route that writes.
        for u in [&w.reader, &w.triager] {
            let r = route(&h, u, w.demo, m, &path).await;
            let as_who = format!("{what} as {}", u.name);
            if need == "write" {
                assert_read_only(&r, &as_who);
            } else {
                assert_admitted(&r, &as_who);
            }
        }

        // The owner and a writer pass every check.
        for u in [&w.writer, &w.alice] {
            let r = route(&h, u, w.demo, m, &path).await;
            assert_admitted(&r, &format!("{what} as {}", u.name));
        }

        // An archived repository is read only even for a writer.
        let old_path = format!("{}{}", common::path_for(pattern, "alice", "old"), pinned_query(pattern, old_head.as_deref()));
        let r = route(&h, &w.writer, w.old, m, &old_path).await;
        if need == "write" {
            assert_read_only(&r, &format!("{m} {old_path} as writer"));
        } else {
            assert_admitted(&r, &format!("{m} {old_path} as writer"));
        }

        // The key binds the tab: a wrong key, or none, is stale on every route that needs one.
        if keyed {
            let wrong = "0".repeat(64);
            let r = route_with_key(&h, &w.alice, m, &path, Some(&wrong)).await;
            assert_stale(&r, &format!("{what} with a wrong key"));
            let r = route_with_key(&h, &w.alice, m, &path, None).await;
            assert_stale(&r, &format!("{what} with no key"));
        }
    }

    // The terminal routes, the snapshot and the rows that name a run: the plain 404 even for
    // the owner with the key.
    for &(m, pattern, _, _) in common::WORKSPACE_ROUTES {
        if !blank_after_checks(pattern) {
            continue;
        }
        let path = common::path_for(pattern, "alice", "demo");
        let r = route(&h, &w.alice, w.demo, m, &path).await;
        assert_eq!(r.status.as_u16(), 404, "{m} {path}: the terminal is off and there is no run 1");
    }

    // A non-canonical spelling of the name moves the shell and 404s every other route, because
    // CSP paths are case sensitive.
    for &(m, pattern, _, _) in common::WORKSPACE_ROUTES {
        let path = common::path_for(pattern, "Alice", "Demo");
        let r = route(&h, &w.alice, w.demo, m, &path).await;
        if pattern == "/{owner}/{repo}/workspace" {
            assert_eq!(r.status.as_u16(), 303, "the shell moves to the canonical name");
            assert_eq!(r.headers[header::LOCATION].to_str().unwrap(), "/alice/demo/workspace");
        } else {
            assert_eq!(r.status.as_u16(), 404, "{m} {path} through the non-canonical name");
        }
    }

    // Signed out, with the editor on: every route sends the caller to the sign-in page, and a
    // repository that is not there looks exactly like one that is.
    signed_out_looks_the_same(&h).await;

    // The switch off: the owner gets the same 404 as anyone else, on every route.
    set_editor(&h, false).await;
    for &(m, pattern, _, _) in common::WORKSPACE_ROUTES {
        let path = common::path_for(pattern, "alice", "demo");
        let r = route(&h, &w.alice, w.demo, m, &path).await;
        assert_eq!(r.status.as_u16(), 404, "{m} {path} with the editor off");
    }
    signed_out_looks_the_same(&h).await;
    set_editor(&h, true).await;

    // A suspended account is signed out at once, and so is a session that has expired.
    let sleeper = make_person(&h, "sleeper", false).await;
    h.state.accounts.suspend(&Actor::System, sleeper.id, &ClientInfo::local()).await.unwrap();
    let hdrs = [("cookie".to_string(), format!("__Host-ch_session={}", sleeper.cookie))];
    let hdrs = as_strs(&hdrs);
    let r = send(&h.app, Method::GET, "/alice/demo/workspace", &hdrs, None).await;
    assert_eq!(r.status.as_u16(), 303, "a suspended session");
    assert!(location_path(&r).starts_with("/login"));
    assert!(r.body.is_empty());

    let later = make_person(&h, "later", false).await;
    let hash = ch_db::tokens::hash_token(&later.cookie);
    sqlx::query("UPDATE sessions SET expires_at = now() - interval '1 second' WHERE token_hash = $1")
        .bind(hash.as_slice())
        .execute(&h.state.db)
        .await
        .unwrap();
    let hdrs = [("cookie".to_string(), format!("__Host-ch_session={}", later.cookie))];
    let hdrs = as_strs(&hdrs);
    let r = send(&h.app, Method::GET, "/alice/demo/workspace", &hdrs, None).await;
    assert_eq!(r.status.as_u16(), 303, "an expired session");
    assert!(location_path(&r).starts_with("/login"));
    assert!(r.body.is_empty());

    // Boot tells the tab what it may do: a reader cannot write, a writer can, and the draft key
    // goes to anyone who may read.
    let r = route(&h, &w.reader, w.demo, "GET", "/alice/demo/workspace/boot").await;
    assert_eq!(r.status.as_u16(), 200, "{}", r.body);
    let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(v["can_write"], false, "{}", r.body);
    assert!(v["draft_key"].is_string(), "{}", r.body);
    let r = route(&h, &w.writer, w.demo, "GET", "/alice/demo/workspace/boot").await;
    assert_eq!(r.status.as_u16(), 200, "{}", r.body);
    let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
    assert_eq!(v["can_write"], true, "{}", r.body);

    // The shell holds no repository content: only the key, the CSRF token and the asset
    // address, under the strict page policy.
    let shell = route(&h, &w.reader, w.demo, "GET", "/alice/demo/workspace").await;
    assert_eq!(shell.status.as_u16(), 200, "{}", shell.body);
    let csp = shell.headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
    assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
    assert!(csp.contains("require-trusted-types-for 'script'"), "{csp}");
    assert_eq!(shell.headers[header::CACHE_CONTROL], "no-store");
    assert!(shell.body.contains("name=\"ws-key\""), "{}", shell.body);
    assert!(!shell.body.contains("# demo"), "the README heading leaked: {}", shell.body);
    assert!(!shell.body.contains("Workspace access suite fixture."), "the README body leaked: {}", shell.body);

    // The assets of this build, at the address the shell names.
    let marker = "<meta name=\"ws-assets\" content=\"";
    let at = shell.body.find(marker).expect("the ws-assets meta") + marker.len();
    let rest = &shell.body[at..];
    let assets = &rest[..rest.find('"').unwrap()];
    assert!(assets.starts_with("/alice/demo/workspace/assets/"), "{assets}");
    let js = format!("{assets}/workspace.js");

    let r = route(&h, &w.reader, w.demo, "GET", &js).await;
    assert_eq!(r.status.as_u16(), 200, "{js}");
    assert!(r.headers[header::CONTENT_TYPE].to_str().unwrap().starts_with("text/javascript"), "{js}");
    assert_eq!(r.headers[header::CACHE_CONTROL], "private, no-cache");
    let etag = r.headers[header::ETAG].to_str().unwrap().to_string();
    assert!(!etag.is_empty(), "{js}");

    // A revalidation from someone who may read answers 304.
    let mut hdrs = tab(&h, &w.reader);
    hdrs.push(("if-none-match".into(), etag));
    let hdrs = as_strs(&hdrs);
    let r = send(&h.app, Method::GET, &js, &hdrs, None).await;
    assert_eq!(r.status.as_u16(), 304, "{js}");

    // Nobody else, signed out, the wrong build, and no other spelling of the file.
    let r = route(&h, &w.stranger, w.demo, "GET", &js).await;
    assert_eq!(r.status.as_u16(), 404, "{js} as stranger");
    let r = send(&h.app, Method::GET, &js, &[], None).await;
    assert_eq!(r.status.as_u16(), 303, "{js} signed out");
    let wrong_build = "/alice/demo/workspace/assets/0000000000000000/workspace.js";
    let r = route(&h, &w.reader, w.demo, "GET", wrong_build).await;
    assert_eq!(r.status.as_u16(), 404, "{wrong_build}");
    for probe in [format!("{assets}/../workspace.js"), format!("{assets}/workspace%2ejs")] {
        let r = route(&h, &w.reader, w.demo, "GET", &probe).await;
        assert_eq!(r.status.as_u16(), 404, "{probe}");
    }
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn posts_need_origin_csrf_and_key_with_no_side_effect(pool: PgPool) {
    let h = harness(pool, true).await;
    let w = world(&h).await;

    let head_before = h.state.git.workspace_branch_head(w.demo, "main").await.unwrap();
    let pushes_before = push_count(&h.state.db).await;

    let cookie = format!("__Host-ch_session={}", w.alice.cookie);
    let token = h.guard.token_for_session(&w.alice.cookie);
    let right_key = key(&h, &w.alice, w.demo);
    let json = "application/json";
    // Each case leaves out or fakes one thing a POST must check. The request guard refuses the
    // first five before the route runs; a wrong key passes the guard and is stale in the route.
    let cases: Vec<(&str, Vec<(&str, String)>)> = vec![
        ("no token", vec![("cookie", cookie.clone()), ("origin", ORIGIN.into()), ("content-type", json.into())]),
        (
            "a wrong token",
            vec![
                ("cookie", cookie.clone()),
                ("origin", ORIGIN.into()),
                ("x-csrf-token", "0".repeat(64)),
                ("x-workspace-key", right_key.clone()),
                ("content-type", json.into()),
            ],
        ),
        (
            "no origin",
            vec![
                ("cookie", cookie.clone()),
                ("x-csrf-token", token.clone()),
                ("x-workspace-key", right_key.clone()),
                ("content-type", json.into()),
            ],
        ),
        (
            "a foreign origin",
            vec![
                ("cookie", cookie.clone()),
                ("origin", "https://evil.example".into()),
                ("x-csrf-token", token.clone()),
                ("x-workspace-key", right_key.clone()),
                ("content-type", json.into()),
            ],
        ),
        (
            "a null origin",
            vec![
                ("cookie", cookie.clone()),
                ("origin", "null".into()),
                ("x-csrf-token", token.clone()),
                ("x-workspace-key", right_key.clone()),
                ("content-type", json.into()),
            ],
        ),
        (
            "a wrong key",
            vec![
                ("cookie", cookie.clone()),
                ("origin", ORIGIN.into()),
                ("x-csrf-token", token.clone()),
                ("x-workspace-key", "1".repeat(64)),
                ("content-type", json.into()),
            ],
        ),
    ];

    for &(m, pattern, _, _) in common::WORKSPACE_ROUTES {
        if m != "POST" {
            continue;
        }
        let path = common::path_for(pattern, "alice", "demo");
        for (name, hdrs) in &cases {
            let hdrs: Vec<(&str, &str)> = hdrs.iter().map(|(k, v)| (*k, v.as_str())).collect();
            let r = send(&h.app, Method::POST, &path, &hdrs, Some("{}")).await;
            let what = format!("{m} {path} with {name}");
            if *name == "a wrong key" {
                assert_stale(&r, &what);
            } else {
                assert_eq!(r.status.as_u16(), 403, "{what}: answered {} with {}", r.status, r.body);
                assert!(r.body.contains("Request blocked"), "{what}: {}", r.body);
            }
        }
    }

    // Nothing ran: the branch has not moved and no push was recorded.
    assert_eq!(h.state.git.workspace_branch_head(w.demo, "main").await.unwrap(), head_before);
    assert_eq!(push_count(&h.state.db).await, pushes_before);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn admin_workspace_switch_needs_a_site_admin_with_two_factor(pool: PgPool) {
    let h = harness(pool.clone(), true).await;
    let w = world(&h).await;

    // Someone without the power learns nothing: the same 404 as a missing page.
    let owned = tab(&h, &w.writer);
    let hdrs = as_strs(&owned);
    let r = send(&h.app, Method::GET, "/admin/workspace", &hdrs, None).await;
    assert_eq!(r.status.as_u16(), 404, "a writer on the admin page");

    // An admin who has not turned on two-factor is told to, and the switches stay as they are.
    let owned = tab(&h, &w.admin);
    let hdrs = as_strs(&owned);
    let r = send(&h.app, Method::GET, "/admin/workspace", &hdrs, None).await;
    assert_eq!(r.status.as_u16(), 403, "an admin without two-factor");
    assert!(r.body.to_lowercase().contains("two-factor"), "{}", r.body);

    // Turning two-factor on in a test is hard, so the admin-with-two-factor case goes through
    // the service with a constructed actor: the switches write and read back, and without
    // two-factor the same call is refused with NotFound, like no power at all.
    let with_2fa = Actor::User { id: w.admin.id, is_admin: true, has_2fa: true, via: AuthVia::Session };
    h.state
        .repos
        .set_workspace_settings(&with_2fa, WorkspaceSettings { editor: false, terminal: false }, &ClientInfo::local())
        .await
        .unwrap();
    assert_eq!(h.state.repos.workspace_settings().await.unwrap(), WorkspaceSettings { editor: false, terminal: false });
    h.state
        .repos
        .set_workspace_settings(&with_2fa, WorkspaceSettings { editor: true, terminal: false }, &ClientInfo::local())
        .await
        .unwrap();
    assert_eq!(h.state.repos.workspace_settings().await.unwrap(), WorkspaceSettings { editor: true, terminal: false });
    let without_2fa = Actor::User { id: w.admin.id, is_admin: true, has_2fa: false, via: AuthVia::Session };
    assert!(matches!(
        h.state.repos.set_workspace_settings(&without_2fa, WorkspaceSettings::default(), &ClientInfo::local()).await,
        Err(Error::NotFound)
    ));

    // The config file is the ceiling: with workspace.enabled false the switch cannot lift it.
    let h2 = harness(pool, false).await;
    let owner = make_person(&h2, "ceiling", false).await;
    h2.state
        .repos
        .create(
            &session(owner.id),
            NewRepo {
                owner_id: owner.id,
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
    h2.state
        .repos
        .set_workspace_settings(&Actor::System, WorkspaceSettings { editor: true, terminal: false }, &ClientInfo::local())
        .await
        .unwrap();
    assert!(h2.state.repos.workspace_settings().await.unwrap().editor, "the switch is on");
    let r = route_with_key(&h2, &owner, "GET", "/ceiling/demo/workspace", None).await;
    assert_eq!(r.status.as_u16(), 404, "the config ceiling");
}

/// The admin page's terminal card posts its own form, and a form for one switch leaves the
/// other as it was.
async fn post_switch(h: &Harness, u: &Person, body: &str) -> Reply {
    let mut hdrs = tab(h, u);
    hdrs.push(("content-type".into(), "application/x-www-form-urlencoded".into()));
    let hdrs = as_strs(&hdrs);
    send(&h.app, Method::POST, "/admin/workspace", &hdrs, Some(body)).await
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_admin_switch_turns_the_terminal_on_and_off_and_keeps_the_editor(pool: PgPool) {
    let h = harness(pool, true).await;
    let w = world(&h).await;

    // Signing in with two-factor is hard in a test, so the admin's account is marked as having
    // it directly: the session they already hold then counts as a site admin's, and the page
    // and its forms answer for real.
    sqlx::query("UPDATE users SET totp_enabled_at = now() WHERE id = $1").bind(w.admin.id.0).execute(&h.state.db).await.unwrap();

    // The page shows the terminal card.
    let owned = tab(&h, &w.admin);
    let hdrs = as_strs(&owned);
    let r = send(&h.app, Method::GET, "/admin/workspace", &hdrs, None).await;
    assert_eq!(r.status.as_u16(), 200, "{}", r.body);
    assert!(r.body.contains("Terminal in the browser"), "{}", r.body);

    // The terminal turns on and off, and the editor stays as it was.
    let r = post_switch(&h, &w.admin, "terminal=1").await;
    assert_eq!(r.status.as_u16(), 303, "{}", r.body);
    assert_eq!(location_path(&r), "/admin/workspace");
    assert_eq!(h.state.repos.workspace_settings().await.unwrap(), WorkspaceSettings { editor: true, terminal: true });
    let r = post_switch(&h, &w.admin, "terminal=0").await;
    assert_eq!(r.status.as_u16(), 303, "{}", r.body);
    assert_eq!(h.state.repos.workspace_settings().await.unwrap(), WorkspaceSettings { editor: true, terminal: false });

    // An editor form leaves the terminal as it was.
    let r = post_switch(&h, &w.admin, "terminal=1").await;
    assert_eq!(r.status.as_u16(), 303, "{}", r.body);
    let r = post_switch(&h, &w.admin, "editor=0").await;
    assert_eq!(r.status.as_u16(), 303, "{}", r.body);
    assert_eq!(h.state.repos.workspace_settings().await.unwrap(), WorkspaceSettings { editor: false, terminal: true });
}
