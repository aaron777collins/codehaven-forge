//! A signed-in caller who cannot read a repository learns nothing about it (ch-hcj, ch-x0b).
//!
//! The project rule: a repository you cannot read is a 404 identical to one that does not exist.
//! This sends every route that names `{owner}/{repo}`, with every method and a JSON body of `{}`,
//! as an account with no access, holding tokens with each set of scopes. Each request goes once
//! to `alice/proj` (private, and the caller has no grant) and once to `alice/nope` (no such
//! repository). Status, body, content type, `Location` and `WWW-Authenticate` must be the same,
//! with only the repository name told apart. A scope check, a body check or a value check that
//! runs before the visibility check shows up here as a difference.

#![allow(clippy::unwrap_used)]

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, header};
use ch_core::collab::ReleaseDraft;
use ch_core::model::*;
use ch_core::state::AppState;
use ch_core::{Actor, AuthVia, Config, Permission, UserId};
use codehaven::app::build_router;
use codehaven::metrics::Metrics;
use codehaven::wiring;
use http_body_util::BodyExt;
use sqlx::PgPool;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use tower::ServiceExt;

fn c() -> ClientInfo {
    ClientInfo::local()
}

fn as_user(id: UserId) -> Actor {
    Actor::User { id, is_admin: false, has_2fa: false, via: AuthVia::Session }
}

async fn user(state: &AppState, name: &str) -> UserId {
    let new =
        NewUser { username: name.into(), email: format!("{name}@example.org"), display_name: None, is_admin: false, can_run_ci: true };
    state.accounts.create_user(&Actor::System, new, &c()).await.unwrap().id
}

struct World {
    app: Router,
    state: AppState,
    _dir: tempfile::TempDir,
    carol: UserId,
}

async fn seed(pool: PgPool) -> World {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::for_tests();
    cfg.storage.data_dir = dir.path().to_path_buf();
    cfg.server.external_url = "https://code.example.com".into();
    let cfg = Arc::new(cfg);
    let state = wiring::build_state(cfg.clone(), pool.clone()).await.unwrap();
    let app = build_router(state.clone(), Metrics::new(), None);

    let alice = user(&state, "alice").await;
    let carol = user(&state, "carol").await;
    let a = as_user(alice);
    let new = |name: &str| NewRepo {
        owner_id: alice,
        name: name.into(),
        description: String::new(),
        visibility: Visibility::Private,
        default_branch: None,
        auto_init: true,
    };
    let proj = state.repos.create(&a, new("proj"), &c()).await.unwrap();
    let sig = Signature { name: "Alice".into(), email: "alice@users.noreply.code.example.com".into(), at: chrono::Utc::now() };
    let change = CommitFileChange {
        repo_id: proj.id,
        branch: "main".into(),
        new_branch: Some("feature".into()),
        expected_sha: None,
        changes: vec![FileChange { path: "src/lib.rs".into(), content: Some(b"pub fn x() {}\n".to_vec()), renamed_from: None }],
        message: "Add lib".into(),
        author: sig,
    };
    state.git.commit_file_change(&a, change, &c()).await.unwrap();
    let issue =
        NewIssue { repo_id: proj.id, title: "An issue".into(), body: "x".into(), labels: vec![], assignees: vec![], milestone_id: None };
    state.issues.create(&a, issue, &c()).await.unwrap();
    let pull = NewPull {
        repo_id: proj.id,
        title: "A pull request".into(),
        body: String::new(),
        head_repo_id: proj.id,
        head_ref: "feature".into(),
        base_ref: "main".into(),
        draft: false,
    };
    state.pulls.create(&a, pull, &c()).await.unwrap();
    let draft = ReleaseDraft {
        tag_name: "v1".into(),
        target_commitish: Some("main".into()),
        name: None,
        body: None,
        draft: false,
        prerelease: false,
        generate_notes: false,
    };
    state.releases.create(&a, proj.id, draft, &c()).await.unwrap();
    sqlx::query("INSERT INTO webhooks (scope, repo_id, url, events) VALUES ('repo', $1, 'https://hooks.example.org/x', '{push}')")
        .bind(proj.id.0)
        .execute(&pool)
        .await
        .unwrap();

    // Carol owns an organization with a team, so the team-repository route gets as far as the
    // repository it names.
    let crew = state.people.create_org(&as_user(carol), "crew", "Crew", &c()).await.unwrap();
    state.orgs.create_team(&as_user(carol), ch_core::OrgId(crew.id.0), "t1", "", Permission::Read, &c()).await.unwrap();
    World { app, state, _dir: dir, carol }
}

/// Every registered path, from axum's `Debug` output for the router (as `public_surface` does).
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
    while let Some((i, ch)) = chars.next() {
        if ch == '"' {
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

fn concrete(pattern: &str, repo: &str) -> String {
    pattern
        .split('/')
        .map(|s| -> String {
            match s {
                "{owner}" => "alice".into(),
                "{repo}" => repo.into(),
                "{org}" => "crew".into(),
                "{slug}" | "{team}" => "t1".into(),
                "{*rest}" => {
                    if pattern.contains("/archive/") {
                        "main.zip".into()
                    } else if pattern.contains("/commits/") || pattern.contains("/tree/") || pattern.contains("/branches/") {
                        "main".into()
                    } else {
                        "main/README.md".into()
                    }
                }
                "{*spec}" | "{basehead}" => "main...feature".into(),
                "{*path}" => "README.md".into(),
                "{*prefix}" | "{*name}" => "heads/main".into(),
                "{*file}" => "workspace.js".into(),
                "{*tag}" | "{tag}" => "v1".into(),
                "{ref}" | "{reference}" => "main".into(),
                "{number}" => {
                    if pattern.contains("/pulls/") {
                        "2".into()
                    } else {
                        "1".into()
                    }
                }
                "{username}" | "{login}" | "{user}" => "carol".into(),
                other if other.starts_with('{') => "1".into(),
                other => other.into(),
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

const METHODS: &[Method] = &[Method::GET, Method::HEAD, Method::POST, Method::PUT, Method::PATCH, Method::DELETE];

static PEER: AtomicU32 = AtomicU32::new(0);

/// What may differ between two answers: nothing but the repository's name.
#[derive(Debug, PartialEq, Eq)]
struct Seen {
    status: u16,
    content_type: Option<String>,
    location: Option<String>,
    www: Option<String>,
    body: String,
}

async fn send(app: &Router, method: &Method, uri: &str, bearer: &str, repo: &str) -> Seen {
    let n = PEER.fetch_add(1, Ordering::Relaxed);
    let peer: SocketAddr = format!("198.18.{}.{}:40000", (n / 250) % 250, n % 250 + 1).parse().unwrap();
    let mut b = Request::builder().method(method.clone()).uri(uri).header(header::AUTHORIZATION, format!("Bearer {bearer}"));
    let body = if matches!(*method, Method::POST | Method::PUT | Method::PATCH) {
        b = b.header(header::CONTENT_TYPE, "application/json");
        Body::from("{}")
    } else {
        Body::empty()
    };
    let mut req = b.body(body).unwrap();
    req.extensions_mut().insert(ConnectInfo(peer));
    let resp = app.clone().oneshot(req).await.unwrap();
    let (parts, body) = resp.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    let norm = |s: &str| s.replace(&format!("alice/{repo}"), "alice/REPO").replace(&format!("/{repo}"), "/REPO");
    let h = |name: header::HeaderName| parts.headers.get(name).map(|v| norm(v.to_str().unwrap_or("?")));
    Seen {
        status: parts.status.as_u16(),
        content_type: h(header::CONTENT_TYPE),
        location: h(header::LOCATION),
        www: h(header::WWW_AUTHENTICATE),
        body: norm(&String::from_utf8_lossy(&bytes)),
    }
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_repository_you_cannot_read_answers_like_one_that_does_not_exist(pool: PgPool) {
    let w = seed(pool).await;
    let paths: Vec<String> =
        registered_paths(&wiring::routes(&w.state.config)).into_iter().filter(|p| p.contains("{owner}") && p.contains("{repo}")).collect();
    assert!(paths.len() > 100, "the router listing found only {} repository paths", paths.len());

    let scope_sets: &[&[&str]] = &[
        &["read:org"],
        &["repo"],
        &["repo:read"],
        &["user"],
        &["admin:repo_hook"],
        &["notifications"],
        &["read:packages", "write:packages"],
        &["workflow"],
        &["admin:org", "read:org"],
    ];
    let me = as_user(w.carol);
    let mut differences = Vec::new();
    for (i, scopes) in scope_sets.iter().enumerate() {
        let scopes: Vec<String> = scopes.iter().map(|s| s.to_string()).collect();
        let (_, secret) = w
            .state
            .accounts
            .create_token(&me, w.carol, &format!("sweep{i}"), scopes.clone(), 7, &c())
            .await
            .unwrap_or_else(|e| panic!("token with {scopes:?}: {e:?}"));
        for pattern in &paths {
            for m in METHODS {
                let unreadable = send(&w.app, m, &concrete(pattern, "proj"), &secret, "proj").await;
                let missing = send(&w.app, m, &concrete(pattern, "nope"), &secret, "nope").await;
                assert!(unreadable.status < 500, "{m} {pattern} with {scopes:?} answered {}: {}", unreadable.status, unreadable.body);
                if unreadable != missing {
                    differences.push(format!("{m} {pattern} with {scopes:?}:\n  unreadable {unreadable:?}\n  missing    {missing:?}"));
                }
            }
        }
    }
    assert!(
        differences.is_empty(),
        "{} answers tell an unreadable repository from a missing one:\n{}",
        differences.len(),
        differences.join("\n")
    );
}
