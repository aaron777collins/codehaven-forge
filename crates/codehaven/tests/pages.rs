//! Codehaven Pages against the real services and a real database (`docs/design/pages.md`):
//! the pages host never reaches the app, signed out a site that is not public looks exactly
//! like no site, the per-site token works for one site and one person only, and copies,
//! limits, rollback and turning off behave as the design says.

#![allow(clippy::unwrap_used)]

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use ch_core::model::{ClientInfo, CommitFileChange, FileChange, NewRepo, NewUser, Signature, Visibility};
use ch_core::pages::{PagesConfigInput, PagesFolder, PagesSettings};
use ch_core::state::AppState;
use ch_core::{Actor, AuthVia, Config, Permission, RepoId, UserId};
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
const PAGES: &str = "pages.example.com";

struct H {
    app: Router,
    state: AppState,
    pool: PgPool,
    _dir: tempfile::TempDir,
}

async fn harness(pool: PgPool, max_files: u32) -> H {
    harness_with(pool, |c| c.pages.max_files = max_files).await
}

async fn harness_with(pool: PgPool, tune: impl FnOnce(&mut Config)) -> H {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::for_tests();
    cfg.storage.data_dir = dir.path().to_path_buf();
    cfg.server.external_url = ORIGIN.into();
    cfg.pages.host = PAGES.into();
    tune(&mut cfg);
    let state = wiring::build_state(Arc::new(cfg), pool.clone()).await.unwrap();
    let app = build_router(state.clone(), Metrics::new(), None);
    H { app, state, pool, _dir: dir }
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Reply {
    fn h(&self, name: &str) -> String {
        self.headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or("").to_string()
    }
}

async fn send(app: &Router, host: &str, path: &str, hdrs: &[(&str, &str)]) -> Reply {
    let mut b = Request::builder().method(Method::GET).uri(path).header("host", host);
    for (k, v) in hdrs {
        b = b.header(*k, *v);
    }
    let mut req = b.body(Body::empty()).unwrap();
    req.extensions_mut().insert(ConnectInfo("203.0.113.9:5555".parse::<SocketAddr>().unwrap()));
    let resp = app.clone().oneshot(req).await.unwrap();
    let (parts, body) = resp.into_parts();
    Reply {
        status: parts.status,
        headers: parts.headers,
        body: String::from_utf8_lossy(&body.collect().await.unwrap().to_bytes()).into_owned(),
    }
}

fn session(id: UserId) -> Actor {
    Actor::User { id, is_admin: false, has_2fa: false, via: AuthVia::Session }
}

/// A person and one live session's cookie value.
async fn person(h: &H, name: &str) -> (UserId, String) {
    let c = ClientInfo::local();
    let u = h
        .state
        .accounts
        .create_user(
            &Actor::System,
            NewUser { username: name.into(), email: format!("{name}@example.org"), display_name: None, is_admin: false, can_run_ci: false },
            &c,
        )
        .await
        .unwrap();
    h.state.accounts.set_password(&Actor::System, u.id, PW, &c).await.unwrap();
    let s = h.state.credentials.sign_in(name, PW, &c).await.unwrap();
    (u.id, s.grant.cookie.clone())
}

async fn repo(h: &H, owner: UserId, name: &str) -> RepoId {
    let new = NewRepo {
        owner_id: owner,
        name: name.into(),
        description: "Pages suite fixture.".into(),
        visibility: Visibility::Private,
        default_branch: None,
        auto_init: true,
    };
    h.state.repos.create(&session(owner), new, &ClientInfo::local()).await.unwrap().id
}

async fn commit(h: &H, owner: UserId, repo: RepoId, files: &[(&str, &str)]) {
    let files: Vec<(&str, Vec<u8>)> = files.iter().map(|(p, c)| (*p, c.as_bytes().to_vec())).collect();
    commit_bytes(h, owner, repo, &files).await;
}

async fn commit_bytes(h: &H, owner: UserId, repo: RepoId, files: &[(&str, Vec<u8>)]) {
    h.state
        .git
        .commit_file_change(
            &session(owner),
            CommitFileChange {
                repo_id: repo,
                branch: "main".into(),
                new_branch: None,
                expected_sha: None,
                changes: files
                    .iter()
                    .map(|(p, c)| FileChange { path: (*p).into(), content: Some(c.clone()), renamed_from: None })
                    .collect(),
                message: "Site".into(),
                author: Signature { name: "Alice".into(), email: "alice@example.org".into(), at: chrono::Utc::now() },
            },
            &ClientInfo::local(),
        )
        .await
        .unwrap();
}

async fn set_tier(h: &H, repo: RepoId, tier: &str) {
    sqlx::query("UPDATE visibility_settings SET allow_public_repos = true").execute(&h.pool).await.unwrap();
    sqlx::query("UPDATE repositories SET access_tier = $2, public_since = CASE WHEN $2 = 'public' THEN now() END WHERE id = $1")
        .bind(repo.0)
        .bind(tier)
        .execute(&h.pool)
        .await
        .unwrap();
}

/// Builds every queued copy, after turning pushes into queued copies.
async fn publish(h: &H) {
    let core = h.state.ext.get::<ch_pages::Core>().unwrap();
    ch_pages::deploy::queue_from_events(&core).await.unwrap();
    while ch_pages::deploy::build_next(&core).await.unwrap() {}
}

async fn enable(h: &H, owner: UserId, repo: RepoId, audience: Visibility) {
    let input = PagesConfigInput { branch: "main".into(), folder: PagesFolder::Docs, audience };
    h.state.pages.configure(&session(owner), repo, input, &ClientInfo::local()).await.unwrap();
    publish(h).await;
}

const INDEX: &str =
    "<!doctype html><html><head><title>Docs</title></head><body>PLANTED-PAGE <script>localStorage.x=1</script></body></html>";

/// A world: alice with a site in each tier, all published from `/docs`.
struct W {
    alice: (UserId, String),
    bob: (UserId, String),
    private: RepoId,
}

async fn world(h: &H) -> W {
    let alice = person(h, "alice").await;
    let bob = person(h, "bob").await;
    for (name, tier) in [("priv", "private"), ("inter", "internal"), ("pub", "public")] {
        let r = repo(h, alice.0, name).await;
        commit(h, alice.0, r, &[("docs/index.html", INDEX), ("docs/a.css", "body{}"), ("docs/guide/index.html", "<p>guide</p>")]).await;
        set_tier(h, r, tier).await;
        enable(h, alice.0, r, Visibility::parse(tier).unwrap()).await;
    }
    let private = h.state.repos.get_by_name(&Actor::System, "alice", "priv").await.unwrap().id;
    W { alice, bob, private }
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_pages_host_never_reaches_the_app(pool: PgPool) {
    let h = harness(pool, 2000).await;
    let w = world(&h).await;
    let cookie = format!("__Host-ch_session={}", w.alice.1);
    for path in
        ["/", "/login", "/api/v3/user", "/alice/priv", "/admin", "/static/app.css", "/alice/priv/settings/pages", "/-/t/x/alice/pub/"]
    {
        let r = send(&h.app, PAGES, path, &[("cookie", &cookie)]).await;
        assert!(r.h("set-cookie").is_empty(), "{path}: no cookie is ever set on the pages host");
        assert!(!r.body.contains("_csrf") && !r.body.contains("Sign in to Codehaven"), "{path}: an app page: {}", r.body);
        assert!(r.status != StatusCode::OK || path == "/-/t/x/alice/pub/", "{path}: {}", r.status);
    }
    // The same paths on the app's host are the app.
    assert_eq!(send(&h.app, "code.example.com", "/login", &[]).await.status, StatusCode::OK);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn signed_out_private_internal_and_missing_look_the_same(pool: PgPool) {
    let h = harness(pool, 2000).await;
    let w = world(&h).await;
    // Disabled: a repository with no site.
    repo(&h, w.alice.0, "nosite").await;
    let norm = |name: &str, r: &Reply| r.body.replace(&format!("/alice/{name}/"), "/X/");
    let base = send(&h.app, PAGES, "/alice/missing/", &[("sec-fetch-dest", "iframe")]).await;
    assert_eq!(base.status, StatusCode::NOT_FOUND);
    assert!(base.body.contains("Sign in to see this site"), "{}", base.body);
    for name in ["priv", "inter", "nosite"] {
        let r = send(&h.app, PAGES, &format!("/{}/{name}/", "alice"), &[("sec-fetch-dest", "iframe")]).await;
        assert_eq!(r.status, base.status, "{name}");
        assert_eq!(norm(name, &r), norm("missing", &base), "{name}: the same page as no site at all");
        assert_eq!(r.h("content-security-policy"), base.h("content-security-policy"));
        assert!(!r.body.contains("PLANTED-PAGE"), "{name}");
    }
    // Public content is served, sandboxed, with the bridge first after the doctype.
    let p = send(&h.app, PAGES, "/alice/pub/", &[("sec-fetch-dest", "iframe")]).await;
    assert_eq!(p.status, StatusCode::OK);
    assert!(p.body.contains("PLANTED-PAGE"));
    assert!(p.body.starts_with("<!doctype html><script>"), "{}", &p.body[..80]);
    let csp = p.h("content-security-policy");
    assert!(csp.starts_with("sandbox allow-scripts"), "{csp}");
    assert!(!csp.contains("allow-same-origin") && !csp.contains("escape-sandbox"), "{csp}");
    assert_eq!(p.h("referrer-policy"), "no-referrer");
    // A document request gets the shell, which looks nothing up: the same for every address.
    let s1 = send(&h.app, PAGES, "/alice/pub/", &[("sec-fetch-dest", "document")]).await;
    let s2 = send(&h.app, PAGES, "/alice/missing/", &[("sec-fetch-dest", "document")]).await;
    assert_eq!(s1.status, StatusCode::OK);
    assert_eq!(s1.body.replace("pub", "missing"), s2.body);
    assert!(s1.h("content-security-policy").contains("frame-ancestors 'none'"));
    assert_eq!(s1.h("x-frame-options"), "DENY");
    assert!(!s1.body.contains("PLANTED-PAGE"));
    // Files, folders and the trailing slash.
    assert_eq!(send(&h.app, PAGES, "/alice/pub/a.css", &[]).await.status, StatusCode::OK);
    assert_eq!(send(&h.app, PAGES, "/alice/pub", &[]).await.h("location"), "/alice/pub/");
    assert_eq!(send(&h.app, PAGES, "/alice/pub/guide", &[]).await.h("location"), "/alice/pub/guide/");
    assert!(send(&h.app, PAGES, "/alice/pub/guide/", &[]).await.body.contains("guide"));
    for bad in ["/alice/pub/../priv/", "/alice/pub/%2e%2e/x", "/alice/pub/a%2fb", "/alice/pub/nope.html"] {
        let r = send(&h.app, PAGES, bad, &[]).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{bad}");
        assert!(!r.body.contains("PLANTED-PAGE"), "{bad}");
    }
    // The shell's own files.
    assert!(send(&h.app, PAGES, "/-/shell.js", &[]).await.h("content-type").starts_with("text/javascript"));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_token_is_for_one_site_and_one_person_and_checked_live(pool: PgPool) {
    let h = harness(pool, 2000).await;
    let w = world(&h).await;
    let open = |who: &str| format!("__Host-ch_session={who}");
    // Signed out, /pages/open is the ordinary sign-in redirect, whatever the repository.
    for name in ["priv", "missing"] {
        let r = send(&h.app, "code.example.com", &format!("/alice/{name}/pages/open?path=/"), &[]).await;
        assert_eq!(r.status, StatusCode::SEE_OTHER);
        assert!(r.h("location").starts_with("/login?next="), "{}", r.h("location"));
    }
    // Bob has no access: the app's ordinary not found, no token.
    let r = send(&h.app, "code.example.com", "/alice/priv/pages/open?path=/", &[("cookie", &open(&w.bob.1))]).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    // Alice gets a token for this site and a redirect to it.
    let r = send(&h.app, "code.example.com", "/alice/priv/pages/open?path=/guide/%0d%0ax", &[("cookie", &open(&w.alice.1))]).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let loc = r.h("location");
    assert!(loc.starts_with("https://pages.example.com/-/t/v1."), "{loc}");
    assert!(loc.ends_with("/alice/priv/guide/%0D%0Ax"), "{loc}");
    assert_eq!(r.h("cache-control"), "no-store");
    let path = loc.strip_prefix("https://pages.example.com").unwrap().to_string();
    let content = send(&h.app, PAGES, &path.replace("guide/%0D%0Ax", ""), &[("sec-fetch-dest", "iframe")]).await;
    assert_eq!(content.status, StatusCode::OK);
    assert!(content.body.contains("PLANTED-PAGE"));
    // The same token on another of alice's sites is no token there.
    let other = path.replace("/alice/priv/", "/alice/inter/").replace("guide/%0D%0Ax", "");
    let r = send(&h.app, PAGES, &other, &[("sec-fetch-dest", "iframe")]).await;
    assert!(r.body.contains("Sign in to see this site"), "a token names one site");
    // A changed token is no token.
    let tampered = path.replacen(".", ".9", 2).replace("guide/%0D%0Ax", "");
    assert!(send(&h.app, PAGES, &tampered, &[]).await.body.contains("Sign in to see this site"));
    // The app session on the pages host counts for nothing.
    let r = send(&h.app, PAGES, "/alice/priv/", &[("cookie", &open(&w.alice.1)), ("authorization", "Basic YWxpY2U6eA==")]).await;
    assert!(r.body.contains("Sign in to see this site"));
    // Bob with a grant gets in; once the grant goes, his token stops at the next request.
    h.state.repos.set_collaborator(&session(w.alice.0), w.private, w.bob.0, Permission::Read, &ClientInfo::local()).await.unwrap();
    let r = send(&h.app, "code.example.com", "/alice/priv/pages/open?path=/", &[("cookie", &open(&w.bob.1))]).await;
    let bob_path = r.h("location").strip_prefix("https://pages.example.com").unwrap().to_string();
    assert!(send(&h.app, PAGES, &bob_path, &[]).await.body.contains("PLANTED-PAGE"));
    h.state.repos.remove_collaborator(&session(w.alice.0), w.private, w.bob.0, &ClientInfo::local()).await.unwrap();
    assert!(send(&h.app, PAGES, &bob_path, &[]).await.body.contains("Sign in to see this site"));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_site_is_never_wider_than_its_repository_or_its_copy(pool: PgPool) {
    let h = harness(pool, 2000).await;
    let w = world(&h).await;
    let pub_id = h.state.repos.get_by_name(&Actor::System, "alice", "pub").await.unwrap().id;
    // Lowering the repository hides the site from strangers at once.
    set_tier(&h, pub_id, "internal").await;
    assert!(send(&h.app, PAGES, "/alice/pub/", &[]).await.body.contains("Sign in to see this site"));
    set_tier(&h, pub_id, "public").await;
    assert!(send(&h.app, PAGES, "/alice/pub/", &[]).await.body.contains("PLANTED-PAGE"));
    // A copy made while private stays private after the repository goes public.
    set_tier(&h, w.private, "public").await;
    assert!(send(&h.app, PAGES, "/alice/priv/", &[]).await.body.contains("Sign in to see this site"));
    // The audience can never be set wider than the repository.
    set_tier(&h, w.private, "private").await;
    let wide = PagesConfigInput { branch: "main".into(), folder: PagesFolder::Docs, audience: Visibility::Public };
    assert!(h.state.pages.configure(&session(w.alice.0), w.private, wide, &ClientInfo::local()).await.is_err());
    // Only an admin of the repository may change Pages.
    let input = PagesConfigInput { branch: "main".into(), folder: PagesFolder::Root, audience: Visibility::Private };
    assert!(h.state.pages.configure(&session(w.bob.0), w.private, input, &ClientInfo::local()).await.is_err());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn copies_limits_rollback_and_turning_off(pool: PgPool) {
    let h = harness(pool, 3).await;
    let alice = person(&h, "alice").await;
    let r = repo(&h, alice.0, "site").await;
    commit(&h, alice.0, r, &[("docs/index.html", "<p>v1 PLANTED-PAGE</p>")]).await;
    set_tier(&h, r, "public").await;
    enable(&h, alice.0, r, Visibility::Public).await;
    assert!(send(&h.app, PAGES, "/alice/site/", &[]).await.body.contains("v1"));
    // A push publishes a new copy.
    commit(&h, alice.0, r, &[("docs/index.html", "<p>v2</p>")]).await;
    publish(&h).await;
    assert!(send(&h.app, PAGES, "/alice/site/", &[]).await.body.contains("v2"));
    // Too many files: the copy fails, says why, and v2 stays live.
    commit(&h, alice.0, r, &[("docs/a.txt", "a"), ("docs/b.txt", "b"), ("docs/c.txt", "c")]).await;
    publish(&h).await;
    let site = h.state.pages.site(&session(alice.0), r).await.unwrap().unwrap();
    let failed = &site.deployments[0];
    assert!(failed.error.as_deref().unwrap_or("").starts_with("Too big: 4 files"), "{:?}", failed.error);
    assert!(send(&h.app, PAGES, "/alice/site/", &[]).await.body.contains("v2"));
    // Rollback to v1.
    let v1 = site.deployments.iter().rev().find(|d| d.state == ch_core::pages::DeployState::Ready).unwrap().id;
    h.state.pages.rollback(&session(alice.0), r, v1, &ClientInfo::local()).await.unwrap();
    assert!(send(&h.app, PAGES, "/alice/site/", &[]).await.body.contains("v1"));
    // Turning off deletes every copy and releases every blob.
    h.state.pages.disable(&session(alice.0), r, &ClientInfo::local()).await.unwrap();
    assert!(send(&h.app, PAGES, "/alice/site/", &[]).await.body.contains("Sign in to see this site"));
    let files: i64 = sqlx::query_scalar("SELECT count(*) FROM pages_files").fetch_one(&h.pool).await.unwrap();
    assert_eq!(files, 0);
    let held: i64 = sqlx::query_scalar("SELECT count(*) FROM blobs WHERE refcount > 0").fetch_one(&h.pool).await.unwrap();
    assert_eq!(held, 0, "every blob a copy held is released");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn full_mode_is_off_by_default_and_an_admin_switch(pool: PgPool) {
    let h = harness(pool, 2000).await;
    let w = world(&h).await;
    let r = send(&h.app, "alice.pages.example.com", "/pub/", &[]).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert!(!r.body.contains("PLANTED-PAGE"));
    // Not a site admin: refused.
    assert!(h.state.pages.set_settings(&session(w.alice.0), PagesSettings { full_mode: true }, &ClientInfo::local()).await.is_err());
    let admin = Actor::User { id: w.alice.0, is_admin: true, has_2fa: true, via: AuthVia::Session };
    h.state.pages.set_settings(&admin, PagesSettings { full_mode: true }, &ClientInfo::local()).await.unwrap();
    let r = send(&h.app, "alice.pages.example.com", "/pub/", &[]).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("PLANTED-PAGE") && !r.body.contains("storage bridge"), "no bridge in full mode");
    assert!(r.h("content-security-policy").is_empty(), "no sandbox in full mode");
    assert!(r.h("set-cookie").is_empty());
    // Signed out, a document request for a private site goes to sign in; anything else is 404.
    let d = send(&h.app, "alice.pages.example.com", "/priv/x.html", &[("sec-fetch-dest", "document")]).await;
    assert_eq!(d.status, StatusCode::SEE_OTHER);
    assert_eq!(d.h("location"), "https://code.example.com/alice/priv/pages/open?path=/x.html");
    assert_eq!(send(&h.app, "alice.pages.example.com", "/priv/a.css", &[]).await.status, StatusCode::NOT_FOUND);
    // The path form's document request moves to the subdomain.
    let m = send(&h.app, PAGES, "/alice/pub/guide/?q=1", &[("sec-fetch-dest", "document")]).await;
    assert_eq!(m.h("location"), "https://alice.pages.example.com/pub/guide/?q=1");
    // Two labels deep is not a pages host at all.
    assert_ne!(send(&h.app, "a.alice.pages.example.com", "/pub/", &[]).await.status, StatusCode::OK);
}

/// A reply on the pages host, its body not read yet.
async fn open_reply(app: &Router, path: &str) -> axum::response::Response {
    open_reply_from(app, path, "203.0.113.9").await
}

/// The same, from one client address.
async fn open_reply_from(app: &Router, path: &str, ip: &str) -> axum::response::Response {
    let mut req = Request::builder().method(Method::GET).uri(path).header("host", PAGES).body(Body::empty()).unwrap();
    req.extensions_mut().insert(ConnectInfo(SocketAddr::new(ip.parse().unwrap(), 5555)));
    app.clone().oneshot(req).await.unwrap()
}

/// One address of many, for filling every place without the per-address cap.
fn nth_address(i: usize) -> String {
    format!("198.18.{}.{}", i / 250, 1 + i % 250)
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_big_file_is_sent_in_pieces_and_an_unread_reply_holds_its_place(pool: PgPool) {
    // A signed-out visitor who opens many replies and never reads them must not make the
    // server hold whole files in memory (docs/design/pages.md, section 7).
    let h = harness(pool, 2000).await;
    let alice = person(&h, "alice").await;
    let r = repo(&h, alice.0, "big").await;
    let json = format!("[{}0]", "1234567,".repeat(40_000));
    let html = format!("<!doctype html><title>t</title>{}<p>END-OF-PAGE</p>", "<p>filler text</p>".repeat(20_000));
    let files = vec![
        ("docs/big.json", json.clone().into_bytes()),
        ("docs/index.html", html.clone().into_bytes()),
        ("docs/latin.txt", b"caf\xe9".to_vec()),
        ("docs/a.css", b"body{}".to_vec()),
    ];
    commit_bytes(&h, alice.0, r, &files).await;
    set_tier(&h, r, "public").await;
    enable(&h, alice.0, r, Visibility::Public).await;
    let core = h.state.ext.get::<ch_pages::Core>().unwrap();
    let all = ch_pages::core::OPEN_REPLIES;
    assert_eq!(core.open.available_permits(), all);

    // Text is streamed: the first piece is small, and the reply keeps its place until it ends.
    let reply = open_reply(&h.app, "/alice/big/big.json").await;
    assert_eq!(reply.status(), StatusCode::OK);
    assert_eq!(reply.headers()["content-type"], "application/json; charset=utf-8");
    let mut body = reply.into_body();
    let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
    assert!(first.len() <= 64 * 1024, "the first piece of big.json is {} bytes", first.len());
    assert_eq!(core.open.available_permits(), all - 1, "an unread reply holds its place");
    let rest = body.collect().await.unwrap().to_bytes();
    assert_eq!(first.len() + rest.len(), json.len());
    assert_eq!(core.open.available_permits(), all, "a finished reply gives its place back");

    // HTML gets the bridge after the doctype, from a small first piece, and the rest follows.
    let reply = open_reply(&h.app, "/alice/big/").await;
    assert_eq!(reply.headers()["content-type"], "text/html; charset=utf-8");
    let mut body = reply.into_body();
    let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
    assert!(first.len() <= 80 * 1024, "the first piece of index.html is {} bytes", first.len());
    let mut whole = first.to_vec();
    whole.extend_from_slice(&body.collect().await.unwrap().to_bytes());
    let whole = String::from_utf8(whole).unwrap();
    assert!(whole.starts_with("<!doctype html><script>"), "the bridge goes right after the doctype");
    assert!(whole.contains("storage bridge"));
    assert!(whole.ends_with("<p>END-OF-PAGE</p>"));
    let bridge = whole.len() - html.len();
    assert_eq!(whole.replacen(&whole[15..15 + bridge], "", 1), html, "the page is otherwise unchanged");

    // Text that is not UTF-8 says no charset, as before.
    let t = send(&h.app, PAGES, "/alice/big/latin.txt", &[]).await;
    assert_eq!(t.h("content-type"), "text/plain");

    // Past the limit of open replies, a new one gets 503; one finishing makes room.
    let mut held = Vec::new();
    for i in 0..all {
        let reply = open_reply_from(&h.app, "/alice/big/a.css", &nth_address(i)).await;
        assert_eq!(reply.status(), StatusCode::OK);
        held.push(reply);
    }
    let full = send(&h.app, PAGES, "/alice/big/a.css", &[]).await;
    assert_eq!(full.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(full.h("retry-after"), "5");
    held.pop();
    assert_eq!(send(&h.app, PAGES, "/alice/big/a.css", &[]).await.status, StatusCode::OK);
    drop(held);
    assert_eq!(core.open.available_permits(), all);
}

/// A public site with one file of `size` bytes at `/alice/big/big.bin`, and its `Core`.
async fn big_site(h: &H, size: usize) -> Arc<ch_pages::Core> {
    let alice = person(h, "alice").await;
    let r = repo(h, alice.0, "big").await;
    let bin: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    commit_bytes(h, alice.0, r, &[("docs/big.bin", bin), ("docs/a.css", b"body{}".to_vec())]).await;
    set_tier(h, r, "public").await;
    enable(h, alice.0, r, Visibility::Public).await;
    h.state.ext.get::<ch_pages::Core>().unwrap()
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_reply_that_stops_moving_or_runs_too_long_gives_its_place_back(pool: PgPool) {
    // A client that opens a reply and never reads it must not keep its place for ever, or a
    // few such clients turn every site away (docs/design/pages.md, section 7).
    let h = harness_with(pool, |c| {
        c.pages.reply_stall_secs = 1;
        c.pages.reply_deadline_secs = 3;
    })
    .await;
    let core = big_site(&h, 2 * 1024 * 1024).await;
    let all = ch_pages::core::OPEN_REPLIES;

    // Read one piece, then stop: the place comes back, and the rest is an error, so the
    // client never gets a file that only looks complete.
    let mut body = open_reply(&h.app, "/alice/big/big.bin").await.into_body();
    body.frame().await.unwrap().unwrap();
    assert_eq!(core.open.available_permits(), all - 1);
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
    assert_eq!(core.open.available_permits(), all, "a reply that stopped moving gives its place back");
    assert!(body.collect().await.is_err(), "a stopped reply ends in an error");

    // Read slowly but steadily: the reply ends at the deadline, in an error.
    let start = std::time::Instant::now();
    let mut body = open_reply(&h.app, "/alice/big/big.bin").await.into_body();
    let ended = loop {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        match body.frame().await {
            Some(Ok(_)) => continue,
            Some(Err(_)) => break "error",
            None => break "complete",
        }
    };
    assert_eq!(ended, "error", "a slow reply is cut at the deadline");
    assert!(start.elapsed() < std::time::Duration::from_secs(6), "cut after {:?}", start.elapsed());
    assert_eq!(core.open.available_permits(), all);

    // A reply read at once is whole.
    let r = send(&h.app, PAGES, "/alice/big/a.css", &[]).await;
    assert_eq!((r.status, r.body.as_str()), (StatusCode::OK, "body{}"));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn one_address_gets_a_few_open_replies_and_everyone_else_still_gets_in(pool: PgPool) {
    let h = harness(pool, 2000).await;
    let _core = big_site(&h, 1024).await;
    let per = h.state.config.pages.replies_per_address as usize;
    let mut held = Vec::new();
    for _ in 0..per {
        let r = open_reply_from(&h.app, "/alice/big/a.css", "198.51.100.7").await;
        assert_eq!(r.status(), StatusCode::OK);
        held.push(r);
    }
    let over = open_reply_from(&h.app, "/alice/big/a.css", "198.51.100.7").await;
    assert_eq!(over.status(), StatusCode::SERVICE_UNAVAILABLE, "one address past its cap waits");
    assert_eq!(over.headers()["retry-after"], "5");
    assert_eq!(open_reply_from(&h.app, "/alice/big/a.css", "203.0.113.9").await.status(), StatusCode::OK, "others still get in");
    held.pop();
    assert_eq!(open_reply_from(&h.app, "/alice/big/a.css", "198.51.100.7").await.status(), StatusCode::OK, "a finished reply makes room");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn clients_that_never_read_cannot_lock_every_site_out(pool: PgPool) {
    // The review's reproduction, on a real listener: one address opens many sockets with a
    // tiny receive buffer, asks for a big file and never reads.
    use tokio::io::AsyncWriteExt;
    let h = harness_with(pool, |c| c.pages.reply_stall_secs = 1).await;
    let core = big_site(&h, 16 * 1024 * 1024).await;
    let all = ch_pages::core::OPEN_REPLIES;
    let per = h.state.config.pages.replies_per_address as usize;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = h.app.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap();
    });
    let mut sockets = Vec::new();
    for _ in 0..per + 8 {
        let s = tokio::net::TcpSocket::new_v4().unwrap();
        s.set_recv_buffer_size(4096).unwrap();
        let mut c = s.connect(addr).await.unwrap();
        c.write_all(b"GET /alice/big/big.bin HTTP/1.1\r\nHost: pages.example.com\r\n\r\n").await.unwrap();
        sockets.push(c);
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let held = all - core.open.available_permits();
    assert!(held > 0 && held <= per, "one address holds at most {per} places, and holds {held}");
    assert_eq!(send(&h.app, PAGES, "/alice/big/a.css", &[]).await.status, StatusCode::OK, "a normal visitor gets in at once");
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
    assert_eq!(core.open.available_permits(), all, "every stalled reply gave its place back");
    assert_eq!(send(&h.app, PAGES, "/alice/big/a.css", &[]).await.status, StatusCode::OK);
    drop(sockets);
    server.abort();
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_burst_from_one_address_waits_a_moment_for_a_place(pool: PgPool) {
    // A page with many small files asks for them all at once. Past the address's share, a
    // request waits briefly for one of its own replies to finish instead of getting 503.
    let h = harness(pool, 2000).await;
    let _core = big_site(&h, 1024).await;
    let per = h.state.config.pages.replies_per_address as usize;
    let mut held = Vec::new();
    for _ in 0..per {
        held.push(open_reply_from(&h.app, "/alice/big/a.css", "198.51.100.8").await);
    }
    let finishing = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        drop(held.pop());
        held
    });
    let start = std::time::Instant::now();
    let r = open_reply_from(&h.app, "/alice/big/a.css", "198.51.100.8").await;
    assert_eq!(r.status(), StatusCode::OK, "the request waited for a place");
    assert!(start.elapsed() >= std::time::Duration::from_millis(250), "it waited {:?}", start.elapsed());
    let held = finishing.await.unwrap();
    drop(r);
    drop(held);
    // The burst itself: twice the share at once, every body read, every one 200.
    let app = h.app.clone();
    let all = futures::future::join_all((0..per * 2).map(|_| {
        let app = app.clone();
        async move {
            let r = open_reply_from(&app, "/alice/big/a.css", "198.51.100.9").await;
            let s = r.status();
            let _ = r.into_body().collect().await;
            s
        }
    }))
    .await;
    assert!(all.iter().all(|s| *s == StatusCode::OK), "{all:?}");
}
