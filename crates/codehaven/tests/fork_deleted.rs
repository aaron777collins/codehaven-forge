//! ch-t2j: a pull request whose fork was deleted. Deleting the fork clears the stored head
//! repository (ON DELETE SET NULL), which on its own reads like a branch of the base repository.
//! Its head branch name can match a branch the base owns, so nothing may treat it as that
//! branch: no Delete branch button, no delete on POST, no merge that deletes or merges the base
//! branch, and the API does not report the base as the head repository. Real services, real git
//! repositories, a real database, the real router.

#![allow(clippy::unwrap_used)]

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode};
use ch_core::collab::MergeOptions;
use ch_core::model::*;
use ch_core::state::AppState;
use ch_core::{Actor, AuthVia, Config, RepoId, UserId};
use codehaven::app::build_router;
use codehaven::metrics::Metrics;
use codehaven::wiring;
use http_body_util::BodyExt;
use serde_json::Value;
use sqlx::PgPool;
use std::net::SocketAddr;
use std::sync::Arc;
use tower::ServiceExt;

const PW: &str = "violet harbour lantern 83 teapot";
const ORIGIN: &str = "https://code.example.com";

struct H {
    app: Router,
    s: AppState,
    guard: ch_auth::Guard,
    _dir: tempfile::TempDir,
}

struct Who {
    id: UserId,
    actor: Actor,
    cookie: String,
}

fn c() -> ClientInfo {
    ClientInfo::local()
}

async fn harness(pool: PgPool) -> H {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::for_tests();
    cfg.storage.data_dir = dir.path().to_path_buf();
    cfg.server.external_url = ORIGIN.into();
    let cfg = Arc::new(cfg);
    let s = wiring::build_state(cfg.clone(), pool).await.unwrap();
    let guard = ch_auth::Guard::new(&cfg);
    let app = build_router(s.clone(), Metrics::new(), None);
    s.repos.set_workspace_settings(&Actor::System, WorkspaceSettings { editor: true, terminal: false }, &c()).await.unwrap();
    H { app, s, guard, _dir: dir }
}

async fn user(h: &H, name: &str) -> Who {
    let u = h
        .s
        .accounts
        .create_user(
            &Actor::System,
            NewUser { username: name.into(), email: format!("{name}@example.org"), display_name: None, is_admin: false, can_run_ci: false },
            &c(),
        )
        .await
        .unwrap();
    h.s.accounts.set_password(&Actor::System, u.id, PW, &c()).await.unwrap();
    let sess = h.s.credentials.sign_in(name, PW, &c()).await.unwrap();
    Who { id: u.id, actor: Actor::User { id: u.id, is_admin: false, has_2fa: true, via: AuthVia::Session }, cookie: sess.grant.cookie }
}

async fn commit(h: &H, who: &Actor, r: RepoId, branch: &str, path: &str, text: &str) -> String {
    let expected = h.s.git.resolve(r, &format!("refs/heads/{branch}")).await.unwrap();
    h.s.git
        .commit_file_change(
            who,
            CommitFileChange {
                repo_id: r,
                branch: branch.into(),
                new_branch: None,
                expected_sha: Some(expected),
                changes: vec![FileChange { path: path.into(), content: Some(text.as_bytes().to_vec()), renamed_from: None }],
                message: format!("Change {path}"),
                author: Signature { name: "A".into(), email: "a@example.org".into(), at: chrono::Utc::now() },
            },
            &c(),
        )
        .await
        .unwrap()
}

async fn send(h: &H, who: &Who, method: Method, path: &str, form: &str) -> (StatusCode, String, String) {
    send_with(h, who, method, path, form, "").await
}

/// A POST, then the page it redirects to, carrying the flash cookie it set. Returns the page.
async fn post_and_follow(h: &H, who: &Who, path: &str, form: &str) -> String {
    let (st, loc, set) = send_raw(h, who, Method::POST, path, form, "").await;
    assert_eq!(st, StatusCode::SEE_OTHER, "{path}");
    let flash = set.split(';').next().unwrap_or("").to_string();
    send_with(h, who, Method::GET, &loc, "", &flash).await.2
}

async fn send_with(h: &H, who: &Who, method: Method, path: &str, form: &str, extra_cookie: &str) -> (StatusCode, String, String) {
    let (st, loc, _, body) = send_full(h, who, method, path, form, extra_cookie).await;
    (st, loc, body)
}

async fn send_raw(h: &H, who: &Who, method: Method, path: &str, form: &str, extra_cookie: &str) -> (StatusCode, String, String) {
    let (st, loc, set, _) = send_full(h, who, method, path, form, extra_cookie).await;
    (st, loc, set)
}

async fn send_full(h: &H, who: &Who, method: Method, path: &str, form: &str, extra_cookie: &str) -> (StatusCode, String, String, String) {
    let token = h.guard.token_for_session(&who.cookie);
    let mut cookie = format!("__Host-ch_session={}", who.cookie);
    if !extra_cookie.is_empty() {
        cookie.push_str("; ");
        cookie.push_str(extra_cookie);
    }
    let mut b = Request::builder().method(method.clone()).uri(path).header("cookie", cookie).header("origin", ORIGIN);
    let body = if method == Method::POST {
        b = b.header("content-type", "application/x-www-form-urlencoded");
        format!("_csrf={token}&{form}")
    } else {
        String::new()
    };
    let mut req = b.body(Body::from(body)).unwrap();
    req.extensions_mut().insert(ConnectInfo("203.0.113.9:5555".parse::<SocketAddr>().unwrap()));
    let resp = h.app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let location = resp.headers().get("location").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let set =
        resp.headers().get_all("set-cookie").iter().filter_map(|v| v.to_str().ok()).find(|v| v.contains("flash")).unwrap_or("").to_string();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, location, set, String::from_utf8_lossy(&bytes).into_owned())
}

async fn api(h: &H, token: &str, path: &str) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .header("accept", "application/vnd.github+json")
        .body(Body::empty())
        .unwrap();
    req.extensions_mut().insert(ConnectInfo("203.0.113.9:5555".parse::<SocketAddr>().unwrap()));
    let resp = h.app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn tip(h: &H, r: RepoId, branch: &str) -> Option<String> {
    h.s.git.resolve(r, &format!("refs/heads/{branch}")).await.ok()
}

async fn open_pull(h: &H, who: &Who, base: RepoId, head_repo: RepoId, branch: &str) -> i64 {
    h.s.pulls
        .create(
            &who.actor,
            NewPull {
                repo_id: base,
                title: format!("Frank's {branch}"),
                body: String::new(),
                head_repo_id: head_repo,
                head_ref: branch.into(),
                base_ref: "main".into(),
                draft: false,
            },
            &c(),
        )
        .await
        .unwrap()
        .issue
        .number
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_deleted_fork_never_stands_for_the_base_branch_of_the_same_name(pool: PgPool) {
    let h = harness(pool).await;
    let alice = user(&h, "alice").await;
    let frank = user(&h, "frank").await;
    let intern =
        h.s.repos
            .create(
                &alice.actor,
                NewRepo {
                    owner_id: alice.id,
                    name: "intern".into(),
                    description: String::new(),
                    visibility: Visibility::Internal,
                    default_branch: None,
                    auto_init: true,
                },
                &c(),
            )
            .await
            .unwrap()
            .id;
    // Alice owns two branches with commits of their own.
    let main = tip(&h, intern, "main").await.unwrap();
    for b in ["staging", "staging2"] {
        h.s.git.create_ref(&alice.actor, intern, b, &main, &c()).await.unwrap();
        commit(&h, &alice.actor, intern, b, &format!("{b}.txt"), "alice's work").await;
    }
    let alice_staging = tip(&h, intern, "staging").await.unwrap();
    let alice_staging2 = tip(&h, intern, "staging2").await.unwrap();

    // Frank has no grant. His fork holds both branches, and he builds on each.
    let fork = h.s.repos.fork(&frank.actor, intern, frank.id, &c()).await.unwrap().id;
    let frank_staging = commit(&h, &frank.actor, fork, "staging", "frank.txt", "frank one").await;
    let frank_staging2 = commit(&h, &frank.actor, fork, "staging2", "frank2.txt", "frank two").await;
    let closed = open_pull(&h, &frank, intern, fork, "staging").await;
    let open = open_pull(&h, &frank, intern, fork, "staging2").await;
    h.s.pulls.close(&frank.actor, intern, closed, &c()).await.unwrap();

    let show = |n: i64| format!("/alice/intern/pulls/{n}");
    let (st, _, page) = send(&h, &alice, Method::GET, &show(closed), "").await;
    assert_eq!(st, StatusCode::OK);
    assert!(!page.contains("/delete-branch"), "a fork's branch is not offered for deletion");

    // Frank deletes his fork.
    h.s.repos.delete(&frank.actor, fork, &c()).await.unwrap();
    let pr = h.s.pulls.get(&alice.actor, intern, closed).await.unwrap();
    assert_eq!(pr.head_repo_id, None, "the database cleared the head repository");
    assert!(pr.from_fork, "the pull request still knows it came from a fork");

    // The closed pull request: no button, and a POST deletes nothing.
    let (_, _, page) = send(&h, &alice, Method::GET, &show(closed), "").await;
    assert!(!page.contains("/delete-branch"), "no Delete branch button once the fork is gone");
    let flashed = post_and_follow(&h, &alice, &format!("{}/delete-branch", show(closed)), "").await;
    assert!(flashed.contains("cannot be deleted from here"), "the POST is refused");
    assert_eq!(tip(&h, intern, "staging").await.as_deref(), Some(alice_staging.as_str()), "alice's branch is kept");

    // The open pull request: no delete box, no workspace link to alice's branch, and a merge
    // that asks to delete the branch is refused and changes nothing.
    let (_, _, page) = send(&h, &alice, Method::GET, &show(open), "").await;
    assert!(!page.contains("name=\"delete_branch\""), "no Delete after merging box once the fork is gone");
    assert!(!page.contains("?branch=staging2"), "the workspace does not open alice's branch for frank's pull request");
    let flashed = post_and_follow(&h, &alice, &format!("{}/merge", show(open)), "method=merge&delete_branch=1").await;
    assert!(flashed.contains("was deleted"), "the merge is refused and says why");
    let refused =
        h.s.pulls.merge_with(&alice.actor, intern, open, MergeOptions { delete_branch: Some(true), ..Default::default() }, &c()).await;
    assert!(matches!(refused, Err(ch_core::Error::MergeBlocked(_))), "{refused:?}");
    assert_eq!(tip(&h, intern, "staging2").await.as_deref(), Some(alice_staging2.as_str()), "alice's branch is kept");
    assert_eq!(tip(&h, intern, "main").await.as_deref(), Some(main.as_str()), "nothing was merged");

    // The pull requests still point at frank's commits, not at alice's branches.
    let a = h.s.pulls.get(&alice.actor, intern, closed).await.unwrap();
    let b = h.s.pulls.get(&alice.actor, intern, open).await.unwrap();
    assert_eq!((a.head_sha.as_str(), b.head_sha.as_str()), (frank_staging.as_str(), frank_staging2.as_str()));

    // A push to alice's branch does not move frank's pull request.
    commit(&h, &alice.actor, intern, "staging2", "more.txt", "alice again").await;
    let core = ch_collab::Core::new(ch_collab::Deps::from_state(&h.s));
    for _ in 0..10 {
        if ch_collab::sync::run_pr_sync_once(&core).await.unwrap() == 0 {
            break;
        }
    }
    let b = h.s.pulls.get(&alice.actor, intern, open).await.unwrap();
    assert_eq!(b.head_sha, frank_staging2, "alice's push is not frank's pull request");

    // The API does not name alice's repository as the head.
    let (_, token) = h.s.accounts.create_token(&alice.actor, alice.id, "t", vec!["repo".into()], 30, &c()).await.unwrap();
    let (st, v) = api(&h, &token, &format!("/api/v3/repos/alice/intern/pulls/{open}")).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(v["head"]["repo"].is_null(), "head.repo is null for a deleted fork: {}", v["head"]);
    assert_eq!(v["head"]["label"], "staging2");
}
