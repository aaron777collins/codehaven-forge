//! Links and numbers the REST API hands out work when followed through the real router (batch C3:
//! ch-dyc avatars, ch-iw8 rate limit, ch-4d8 teams).

#![allow(clippy::unwrap_used)]

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode};
use ch_core::collab::MilestoneDraft;
use ch_core::model::{ClientInfo, CommitFileChange, FileChange, NewIssue, NewPull, NewRepo, NewUser, Signature, Visibility};
use ch_core::state::AppState;
use ch_core::{Actor, Config};
use codehaven::app::build_router;
use codehaven::metrics::Metrics;
use codehaven::wiring;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::net::SocketAddr;
use std::sync::Arc;
use tower::ServiceExt;

const PW: &str = "violet harbour lantern 83 teapot";
const BASE: &str = "https://code.example.com";

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
    cfg.server.external_url = BASE.into();
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

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

/// A signed-in person: the session cookie, and the CSRF token unsafe requests carry.
struct Session {
    cookie: String,
    csrf: String,
}

async fn send(h: &Harness, method: Method, path: &str, who: Option<&Session>, body: Option<Value>) -> Reply {
    let mut b = Request::builder().method(method.clone()).uri(path);
    if let Some(s) = who {
        b = b.header("cookie", &s.cookie);
        if method != Method::GET {
            b = b.header("origin", BASE).header("x-csrf-token", &s.csrf);
        }
    }
    if body.is_some() {
        b = b.header("content-type", "application/json");
    }
    let mut req = b.body(Body::from(body.map(|v| v.to_string()).unwrap_or_default())).unwrap();
    req.extensions_mut().insert(ConnectInfo("203.0.113.9:5555".parse::<SocketAddr>().unwrap()));
    let resp = h.app.clone().oneshot(req).await.unwrap();
    let (parts, body) = resp.into_parts();
    Reply {
        status: parts.status,
        headers: parts.headers,
        body: String::from_utf8_lossy(&body.collect().await.unwrap().to_bytes()).into_owned(),
    }
}

async fn sign_in(h: &Harness, name: &str) -> Session {
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
    Session { cookie: format!("__Host-ch_session={}", s.grant.cookie), csrf: h.guard.token_for_session(&s.grant.cookie) }
}

/// The path part of a link the API gave, to follow it through the router.
fn path_of(url: &str) -> &str {
    url.strip_prefix(BASE).unwrap_or_else(|| panic!("{url} is not on this server"))
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn avatar_links_open_an_image(pool: PgPool) {
    let h = harness(pool).await;
    let alice = sign_in(&h, "alice").await;
    let me = send(&h, Method::GET, "/api/v3/user", Some(&alice), None).await;
    assert_eq!(me.status, StatusCode::OK, "{}", me.body);
    let avatar = me.json()["avatar_url"].as_str().unwrap().to_string();
    assert_eq!(avatar, format!("{BASE}/avatar/alice.svg"));
    let img = send(&h, Method::GET, path_of(&avatar), Some(&alice), None).await;
    assert_eq!(img.status, StatusCode::OK);
    assert_eq!(img.headers.get("content-type").unwrap(), "image/svg+xml");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn rate_limit_agrees_with_the_headers(pool: PgPool) {
    let h = harness(pool).await;
    let alice = sign_in(&h, "alice").await;
    for _ in 0..3 {
        send(&h, Method::GET, "/api/v3/user", Some(&alice), None).await;
    }
    let r = send(&h, Method::GET, "/api/v3/rate_limit", Some(&alice), None).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let header = |n: &str| r.headers.get(n).unwrap().to_str().unwrap().parse::<i64>().unwrap();
    let body = r.json();
    for v in [&body["rate"], &body["resources"]["core"]] {
        assert_eq!(v["limit"].as_i64().unwrap(), header("x-ratelimit-limit"));
        assert_eq!(v["remaining"].as_i64().unwrap(), header("x-ratelimit-remaining"));
        assert_eq!(v["reset"].as_i64().unwrap(), header("x-ratelimit-reset"));
        assert_eq!(v["used"].as_i64().unwrap(), header("x-ratelimit-used"));
    }
    assert!(body["rate"]["used"].as_i64().unwrap() >= 4, "the calls before count: {body}");
    assert!(body["rate"]["reset"].as_i64().unwrap() > 0);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_team_link_opens_its_page_and_patch_changes_it(pool: PgPool) {
    let h = harness(pool).await;
    let alice = sign_in(&h, "alice").await;
    let org = send(&h, Method::POST, "/api/v3/user/orgs", Some(&alice), Some(json!({"name": "acme-labs"}))).await;
    assert_eq!(org.status, StatusCode::CREATED, "{}", org.body);
    let team =
        send(&h, Method::POST, "/api/v3/orgs/acme-labs/teams", Some(&alice), Some(json!({"name": "Backend", "permission": "pull"}))).await;
    assert_eq!(team.status, StatusCode::CREATED, "{}", team.body);
    let t = team.json();

    // The html_url is a page that opens.
    let page = send(&h, Method::GET, path_of(t["html_url"].as_str().unwrap()), Some(&alice), None).await;
    assert_eq!(page.status, StatusCode::OK, "{}", t["html_url"]);
    assert!(page.body.contains("Backend"));
    // The url is the API object.
    let api = send(&h, Method::GET, path_of(t["url"].as_str().unwrap()), Some(&alice), None).await;
    assert_eq!(api.status, StatusCode::OK);

    // PATCH changes the permission, and refuses what it cannot change.
    let p = send(&h, Method::PATCH, "/api/v3/orgs/acme-labs/teams/backend", Some(&alice), Some(json!({"permission": "push"}))).await;
    assert_eq!(p.status, StatusCode::OK, "{}", p.body);
    assert_eq!(p.json()["permission"], "push");
    let again = send(&h, Method::GET, "/api/v3/orgs/acme-labs/teams/backend", Some(&alice), None).await;
    assert_eq!(again.json()["permission"], "push");
    let bad = send(&h, Method::PATCH, "/api/v3/orgs/acme-labs/teams/backend", Some(&alice), Some(json!({"permission": "root"}))).await;
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
    let rename = send(&h, Method::PATCH, "/api/v3/orgs/acme-labs/teams/backend", Some(&alice), Some(json!({"name": "Frontend"}))).await;
    assert_eq!(rename.status, StatusCode::UNPROCESSABLE_ENTITY, "renaming is not supported and says so");
    // Someone outside the organization cannot.
    let bob = sign_in(&h, "bob").await;
    let denied = send(&h, Method::PATCH, "/api/v3/orgs/acme-labs/teams/backend", Some(&bob), Some(json!({"permission": "admin"}))).await;
    assert!(matches!(denied.status, StatusCode::NOT_FOUND | StatusCode::FORBIDDEN), "{}", denied.status);
}

/// ch-yci: errors before a handler are JSON too, and a body that does not parse is described in
/// plain words, not in the deserializer's Rust terms.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn api_errors_are_json_in_plain_words(pool: PgPool) {
    let h = harness(pool).await;
    let alice = sign_in(&h, "alice").await;
    let missing = send(&h, Method::GET, "/api/v3/no/such/thing", Some(&alice), None).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(missing.json()["message"], "Not Found", "{}", missing.body);
    assert!(missing.headers["content-type"].to_str().unwrap().starts_with("application/json"));

    let wrong = send(&h, Method::PUT, "/api/v3/user", Some(&alice), Some(json!({}))).await;
    assert_eq!(wrong.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(wrong.json()["message"], "Method Not Allowed", "{}", wrong.body);

    let bad = send(&h, Method::POST, "/api/v3/user/orgs", Some(&alice), Some(json!({"name": 5}))).await;
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
    let msg = bad.json()["message"].as_str().unwrap_or_default().to_string();
    assert!(msg.starts_with("Problems parsing JSON") && msg.contains("name"), "{}", bad.body);
    for rust in ["line 1", "column", "target type", "i64", "struct "] {
        assert!(!msg.contains(rust), "{rust} in {msg}");
    }
}

async fn with_token(h: &Harness, path: &str, token: &str) -> Reply {
    let mut req = Request::builder().uri(path).header("authorization", format!("token {token}")).body(Body::empty()).unwrap();
    req.extensions_mut().insert(ConnectInfo("203.0.113.9:5555".parse::<SocketAddr>().unwrap()));
    let resp = h.app.clone().oneshot(req).await.unwrap();
    let (parts, body) = resp.into_parts();
    Reply {
        status: parts.status,
        headers: parts.headers,
        body: String::from_utf8_lossy(&body.collect().await.unwrap().to_bytes()).into_owned(),
    }
}

/// ch-8ee: a personal access token downloads a private repository's raw file, archive and
/// release file from the same links a browser uses, and opens no other page.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_token_downloads_files_from_the_web_links_and_nothing_else(pool: PgPool) {
    let h = harness(pool).await;
    let alice = h
        .state
        .accounts
        .create_user(
            &Actor::System,
            NewUser { username: "alice".into(), email: "alice@example.org".into(), display_name: None, is_admin: false, can_run_ci: false },
            &ClientInfo::local(),
        )
        .await
        .unwrap();
    let a = Actor::User { id: alice.id, is_admin: false, has_2fa: true, via: ch_core::AuthVia::Session };
    let new = ch_core::model::NewRepo {
        owner_id: alice.id,
        name: "secret".into(),
        description: String::new(),
        visibility: ch_core::model::Visibility::Private,
        default_branch: None,
        auto_init: true,
    };
    let repo = h.state.repos.create(&a, new, &ClientInfo::local()).await.unwrap();
    let draft = ch_core::collab::ReleaseDraft {
        tag_name: "v1".into(),
        target_commitish: Some("main".into()),
        name: Some("One".into()),
        body: None,
        draft: false,
        prerelease: false,
        generate_notes: false,
    };
    let rel = h.state.releases.create(&a, repo.id, draft, &ClientInfo::local()).await.unwrap().release.id;
    let body: futures::stream::BoxStream<'static, std::io::Result<bytes::Bytes>> =
        Box::pin(futures::stream::once(async { Ok(bytes::Bytes::from_static(b"asset\n")) }));
    h.state.releases.upload_asset(&a, repo.id, rel, "notes.txt", None, "text/plain", body, &ClientInfo::local()).await.unwrap();
    let (_, token) = h.state.accounts.create_token(&a, alice.id, "dl", vec!["repo:read".into()], 30, &ClientInfo::local()).await.unwrap();
    let (_, scopeless) =
        h.state.accounts.create_token(&a, alice.id, "none", vec!["read:packages".into()], 30, &ClientInfo::local()).await.unwrap();

    let raw = with_token(&h, "/alice/secret/raw/main/README.md", &token).await;
    assert_eq!(raw.status, StatusCode::OK, "{}", raw.body);
    assert!(raw.body.starts_with("# secret"));
    assert_eq!(with_token(&h, "/alice/secret/archive/main.zip", &token).await.status, StatusCode::OK);
    let asset = with_token(&h, "/alice/secret/releases/download/v1/notes.txt", &token).await;
    assert_eq!((asset.status, asset.body.as_str()), (StatusCode::OK, "asset\n"));
    // A token without a repository scope reads nothing private, as on the API.
    assert_eq!(with_token(&h, "/alice/secret/raw/main/README.md", &scopeless).await.status, StatusCode::NOT_FOUND);
    // No other page takes a token.
    for p in ["/alice/secret", "/alice/secret/blob/main/README.md", "/settings/profile", "/alice/secret/settings"] {
        let r = with_token(&h, p, &token).await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "{p} must not open with a token");
    }
}

/// Every html_url and diff_url the API hands out is a page this server really serves.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn html_urls_from_the_api_open_real_pages(pool: PgPool) {
    let h = harness(pool).await;
    let alice = sign_in(&h, "alice").await;
    let u = h.state.accounts.get_by_username(&Actor::System, "alice").await.unwrap();
    let a = Actor::User { id: u.id, is_admin: false, has_2fa: true, via: ch_core::AuthVia::Session };
    let c = ClientInfo::local();
    let repo = h
        .state
        .repos
        .create(
            &a,
            NewRepo {
                owner_id: u.id,
                name: "links".into(),
                description: String::new(),
                visibility: Visibility::Private,
                default_branch: None,
                auto_init: true,
            },
            &c,
        )
        .await
        .unwrap();
    // A second branch with a commit of its own, the head of the pull request.
    h.state
        .git
        .commit_file_change(
            &a,
            CommitFileChange {
                repo_id: repo.id,
                branch: "main".into(),
                new_branch: Some("feature".into()),
                expected_sha: None,
                changes: vec![FileChange { path: "src/lib.rs".into(), content: Some(b"pub fn x() {}\n".to_vec()), renamed_from: None }],
                message: "Add lib".into(),
                author: Signature { name: "Alice".into(), email: "alice@example.org".into(), at: chrono::Utc::now() },
            },
            &c,
        )
        .await
        .unwrap();
    let issue = h
        .state
        .issues
        .create(
            &a,
            NewIssue {
                repo_id: repo.id,
                title: "A bug".into(),
                body: "It broke".into(),
                labels: vec![],
                assignees: vec![],
                milestone_id: None,
            },
            &c,
        )
        .await
        .unwrap();
    h.state.issues.add_comment(&a, repo.id, issue.number, "a comment", &c).await.unwrap();
    let milestone = h
        .state
        .issues
        .create_milestone(&a, repo.id, MilestoneDraft { title: "v1".into(), description: None, due_on: None, open: Some(true) }, &c)
        .await
        .unwrap();
    let pull = h
        .state
        .pulls
        .create(
            &a,
            NewPull {
                repo_id: repo.id,
                title: "Add the thing".into(),
                body: String::new(),
                head_repo_id: repo.id,
                head_ref: "feature".into(),
                base_ref: "main".into(),
                draft: false,
            },
            &c,
        )
        .await
        .unwrap();

    let api = |p: String| format!("/api/v3/repos/alice/links{p}");
    let issue_json = send(&h, Method::GET, &api(format!("/issues/{}", issue.number)), Some(&alice), None).await;
    assert_eq!(issue_json.status, StatusCode::OK, "{}", issue_json.body);
    let comments = send(&h, Method::GET, &api(format!("/issues/{}/comments", issue.number)), Some(&alice), None).await;
    assert_eq!(comments.status, StatusCode::OK, "{}", comments.body);
    let milestone_json = send(&h, Method::GET, &api(format!("/milestones/{}", milestone.number)), Some(&alice), None).await;
    assert_eq!(milestone_json.status, StatusCode::OK, "{}", milestone_json.body);
    let pull_json = send(&h, Method::GET, &api(format!("/pulls/{}", pull.issue.number)), Some(&alice), None).await;
    assert_eq!(pull_json.status, StatusCode::OK, "{}", pull_json.body);

    let links = vec![
        issue_json.json()["html_url"].as_str().unwrap().to_string(),
        comments.json()[0]["html_url"].as_str().unwrap().to_string(),
        milestone_json.json()["html_url"].as_str().unwrap().to_string(),
        pull_json.json()["html_url"].as_str().unwrap().to_string(),
        pull_json.json()["diff_url"].as_str().unwrap().to_string(),
    ];
    for url in &links {
        let page = send(&h, Method::GET, path_of(url.split('#').next().unwrap()), Some(&alice), None).await;
        assert_eq!(page.status, StatusCode::OK, "{url} opens {}", page.status);
    }
}
