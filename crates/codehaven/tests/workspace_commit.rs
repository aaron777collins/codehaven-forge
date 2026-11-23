//! Commits from the browser workspace, through the real gate, guard and services (spec section
//! 9, tests C1 to C8): many files in one commit and one push event, races, every kind of change,
//! protected branches, the first commit, workflow confirmation, and the refusals.

#![allow(clippy::unwrap_used)]

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode};
use base64::Engine;
use ch_core::model::{BranchProtection, ClientInfo, NewRepo, NewUser, Visibility, WorkspaceSettings};
use ch_core::state::AppState;
use ch_core::web_contract::RepoUpdate;
use ch_core::{Actor, Config, Permission, RepoId, UserId};
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
const ORIGIN: &str = "https://code.example.com";

struct H {
    app: Router,
    state: AppState,
    cfg: Arc<Config>,
    guard: ch_auth::Guard,
    _dir: tempfile::TempDir,
}

struct Who {
    id: UserId,
    cookie: String,
}

async fn harness(pool: PgPool) -> H {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::for_tests();
    cfg.storage.data_dir = dir.path().to_path_buf();
    cfg.server.external_url = ORIGIN.into();
    let cfg = Arc::new(cfg);
    let state = wiring::build_state(cfg.clone(), pool).await.unwrap();
    let guard = ch_auth::Guard::new(&cfg);
    let app = build_router(state.clone(), Metrics::new(), None);
    state
        .repos
        .set_workspace_settings(&Actor::System, WorkspaceSettings { editor: true, terminal: false }, &ClientInfo::local())
        .await
        .unwrap();
    H { app, state, cfg, guard, _dir: dir }
}

async fn user(h: &H, name: &str) -> Who {
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
    Who { id: u.id, cookie: s.grant.cookie }
}

async fn repo(h: &H, owner: &Who, name: &str, init: bool) -> RepoId {
    let actor = Actor::User { id: owner.id, is_admin: false, has_2fa: false, via: ch_core::AuthVia::Session };
    let new = NewRepo {
        owner_id: owner.id,
        name: name.into(),
        description: String::new(),
        visibility: Visibility::Private,
        default_branch: None,
        auto_init: init,
    };
    h.state.repos.create(&actor, new, &ClientInfo::local()).await.unwrap().id
}

async fn call(h: &H, who: &Who, repo: RepoId, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let key = ch_web::views::workspace::keys::workspace_key(&h.cfg, &who.cookie, who.id, repo);
    let token = h.guard.token_for_session(&who.cookie);
    let cookie = format!("__Host-ch_session={}", who.cookie);
    let mut b = Request::builder()
        .method(method)
        .uri(path)
        .header("cookie", cookie)
        .header("origin", ORIGIN)
        .header("x-csrf-token", token)
        .header("x-workspace-key", key);
    if body.is_some() {
        b = b.header("content-type", "application/json");
    }
    let mut req = b.body(Body::from(body.map(|v| v.to_string()).unwrap_or_default())).unwrap();
    req.extensions_mut().insert(ConnectInfo("203.0.113.9:5555".parse::<SocketAddr>().unwrap()));
    let resp = h.app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn head(h: &H, repo: RepoId, branch: &str) -> Option<String> {
    h.state.git.workspace_branch_head(repo, branch).await.unwrap()
}

fn b64(s: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(s)
}

fn put(path: &str, text: &str) -> Value {
    json!({"op": "put", "path": path, "content_b64": b64(text)})
}

async fn pushes(h: &H) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM outbox_events WHERE kind = 'push'").fetch_one(&h.state.db).await.unwrap()
}

async fn text(h: &H, repo: RepoId, branch: &str, path: &str) -> Option<String> {
    h.state.git.blob(repo, branch, path, 1 << 20).await.ok().map(|b| String::from_utf8(b.content).unwrap())
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn many_files_land_as_one_commit_and_one_push(pool: PgPool) {
    let h = harness(pool).await;
    let alice = user(&h, "alice").await;
    let r = repo(&h, &alice, "demo", true).await;
    let base = head(&h, r, "main").await.unwrap();
    let before = pushes(&h).await;
    let changes: Vec<Value> = (1..=5).map(|i| put(&format!("f{i}.txt"), &format!("{i}\n"))).collect();
    let (s, v) = call(
        &h,
        &alice,
        r,
        Method::POST,
        "/alice/demo/workspace/commit",
        Some(json!({
            "branch": "main", "expected_head": base, "message": "Five files", "changes": changes
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let sha = v["sha"].as_str().unwrap().to_string();
    assert_eq!(head(&h, r, "main").await.unwrap(), sha);
    assert_eq!(h.state.git.commit(r, &sha).await.unwrap().parents, vec![base]);
    assert_eq!(pushes(&h).await, before + 1, "one push event");
    for i in 1..=5 {
        assert_eq!(text(&h, r, "main", &format!("f{i}.txt")).await.unwrap(), format!("{i}\n"));
    }
    // The author is the signed-in person, whatever the body says (the body cannot say).
    assert_eq!(h.state.git.commit(r, &sha).await.unwrap().author.name, "alice");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn racing_commits_have_one_winner_and_no_partial_tree(pool: PgPool) {
    let h = Arc::new(harness(pool).await);
    let alice = Arc::new(user(&h, "alice").await);
    let r = repo(&h, &alice, "demo", true).await;
    let base = head(&h, r, "main").await.unwrap();
    let mut tasks = Vec::new();
    for n in 0..4 {
        let (h, alice, base) = (h.clone(), alice.clone(), base.clone());
        tasks.push(tokio::spawn(async move {
            let changes = vec![put(&format!("race-{n}-a"), "a\n"), put(&format!("race-{n}-b"), "b\n")];
            call(
                &h,
                &alice,
                r,
                Method::POST,
                "/alice/demo/workspace/commit",
                Some(json!({
                    "branch": "main", "expected_head": base, "message": format!("Race {n}"), "changes": changes
                })),
            )
            .await
        }));
    }
    let mut ok = 0;
    for t in tasks {
        let (s, v) = t.await.unwrap();
        match s {
            StatusCode::OK => ok += 1,
            StatusCode::CONFLICT => assert_eq!(v["code"], "head_moved", "{v}"),
            // One at a time per account: the others may be told to wait.
            StatusCode::TOO_MANY_REQUESTS => assert_eq!(v["code"], "rate_limited"),
            other => panic!("{other} {v}"),
        }
    }
    assert_eq!(ok, 1, "exactly one winner");
    let tree = h.state.git.workspace_tree(r, &head(&h, r, "main").await.unwrap(), Default::default()).await.unwrap();
    let names: Vec<String> = tree.entries.iter().filter_map(|e| e.path.clone()).filter(|p| p.starts_with("race-")).collect();
    assert_eq!(names.len(), 2, "both files of the winner and nothing else: {names:?}");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn add_edit_rename_delete_and_file_to_folder_in_one_commit(pool: PgPool) {
    let h = harness(pool).await;
    let alice = user(&h, "alice").await;
    let r = repo(&h, &alice, "demo", true).await;
    let c = |base: String, changes: Vec<Value>| json!({"branch": "main", "expected_head": base, "message": "m", "changes": changes});
    let base = head(&h, r, "main").await.unwrap();
    let (s, v) = call(
        &h,
        &alice,
        r,
        Method::POST,
        "/alice/demo/workspace/commit",
        Some(c(base, vec![put("old.txt", "old\n"), put("gone.txt", "x\n"), put("thing", "file\n")])),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let base = head(&h, r, "main").await.unwrap();
    let changes = vec![
        put("new.txt", "new\n"),
        put("README.md", "# changed\n"),
        json!({"op": "rename", "from": "old.txt", "path": "moved/old.txt", "content_b64": b64("old, edited\n")}),
        json!({"op": "delete", "path": "gone.txt"}),
        json!({"op": "delete", "path": "thing"}),
        put("thing/inner.txt", "inside\n"),
    ];
    let (s, v) = call(&h, &alice, r, Method::POST, "/alice/demo/workspace/commit", Some(c(base, changes))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(text(&h, r, "main", "new.txt").await.unwrap(), "new\n");
    assert_eq!(text(&h, r, "main", "README.md").await.unwrap(), "# changed\n");
    assert_eq!(text(&h, r, "main", "moved/old.txt").await.unwrap(), "old, edited\n");
    assert!(text(&h, r, "main", "old.txt").await.is_none());
    assert!(text(&h, r, "main", "gone.txt").await.is_none());
    assert_eq!(text(&h, r, "main", "thing/inner.txt").await.unwrap(), "inside\n");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_protected_branch_leads_to_a_new_branch_and_a_pull_request(pool: PgPool) {
    let h = harness(pool).await;
    let alice = user(&h, "alice").await;
    let r = repo(&h, &alice, "demo", true).await;
    let rule = BranchProtection { pattern: "main".into(), required_approvals: 1, ..Default::default() };
    h.state.repos.set_protection(&Actor::System, r, rule, &ClientInfo::local()).await.unwrap();
    let (s, boot) = call(&h, &alice, r, Method::GET, "/alice/demo/workspace/boot", None).await;
    assert_eq!(s, StatusCode::OK, "{boot}");
    assert_eq!(boot["direct_commit"], false);
    let suggested = boot["suggested_branch"].as_str().unwrap().to_string();
    assert!(suggested.starts_with("alice/patch-"), "{suggested}");
    let base = boot["head"].as_str().unwrap().to_string();
    let body = |nb: Option<&str>| json!({"branch": "main", "expected_head": base, "new_branch": nb, "message": "m", "changes": [put("a.txt", "a\n")]});
    let (s, v) = call(&h, &alice, r, Method::POST, "/alice/demo/workspace/commit", Some(body(None))).await;
    assert_eq!((s, v["code"].as_str()), (StatusCode::FORBIDDEN, Some("protected")), "{v}");
    assert_eq!(head(&h, r, "main").await.unwrap(), base, "nothing moved");
    let (s, v) = call(&h, &alice, r, Method::POST, "/alice/demo/workspace/commit", Some(body(Some(&suggested)))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["branch"], suggested.as_str());
    assert_eq!(text(&h, r, &suggested, "a.txt").await.unwrap(), "a\n");
    let (s, v) = call(&h, &alice, r, Method::POST, "/alice/demo/workspace/commit", Some(body(Some(&suggested)))).await;
    assert_eq!((s, v["code"].as_str()), (StatusCode::CONFLICT, Some("branch_exists")), "{v}");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_first_commit_in_an_empty_repository(pool: PgPool) {
    let h = harness(pool).await;
    let alice = user(&h, "alice").await;
    let r = repo(&h, &alice, "blank", false).await;
    let (s, boot) = call(&h, &alice, r, Method::GET, "/alice/blank/workspace/boot", None).await;
    assert_eq!(s, StatusCode::OK, "{boot}");
    assert_eq!(boot["repo"]["empty"], true);
    assert!(boot["head"].is_null());
    let (s, v) = call(
        &h,
        &alice,
        r,
        Method::POST,
        "/alice/blank/workspace/commit",
        Some(json!({
            "branch": "main", "expected_head": null, "message": "First", "changes": [put("README.md", "# hi\n")]
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(text(&h, r, "main", "README.md").await.unwrap(), "# hi\n");
    // Null again, now that there are commits, is refused.
    let (s, v) = call(
        &h,
        &alice,
        r,
        Method::POST,
        "/alice/blank/workspace/commit",
        Some(json!({
            "branch": "main", "expected_head": null, "message": "Again", "changes": [put("b", "b\n")]
        })),
    )
    .await;
    assert_eq!((s, v["code"].as_str()), (StatusCode::CONFLICT, Some("head_moved")), "{v}");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn workflow_changes_need_confirming_and_refusals_change_nothing(pool: PgPool) {
    let h = harness(pool).await;
    let alice = user(&h, "alice").await;
    let r = repo(&h, &alice, "demo", true).await;
    let base = head(&h, r, "main").await.unwrap();
    let before = pushes(&h).await;
    let commit = |changes: Value, extra: Value| {
        let mut b = json!({"branch": "main", "expected_head": base, "message": "m", "changes": changes});
        if let (Some(b), Some(e)) = (b.as_object_mut(), extra.as_object()) {
            b.extend(e.clone());
        }
        b
    };
    let wf = json!([put(".GitHub/Workflows/x.yml", "on: push\n")]);
    let (s, v) = call(&h, &alice, r, Method::POST, "/alice/demo/workspace/commit", Some(commit(wf.clone(), json!({})))).await;
    assert_eq!((s, v["code"].as_str()), (StatusCode::UNPROCESSABLE_ENTITY, Some("workflow_confirm")), "{v}");
    let bad: Vec<(Value, Value)> = vec![
        (json!([put("a", "1"), json!({"op": "delete", "path": "a"})]), json!({})),
        (json!([put(".g\u{200c}it/config", "x")]), json!({})),
        (json!([put("x/.git/hooks/pre-commit", "x")]), json!({})),
        (json!([put("bad\npath", "x")]), json!({})),
        (json!([put("a", "1")]), json!({"author": "mallory"})),
        (json!([]), json!({})),
    ];
    for (changes, extra) in bad {
        let (s, v) = call(&h, &alice, r, Method::POST, "/alice/demo/workspace/commit", Some(commit(changes.clone(), extra))).await;
        assert!(s.is_client_error(), "{changes}: {s} {v}");
    }
    // A missing expected_head is not the same as null.
    let (s, _) = call(
        &h,
        &alice,
        r,
        Method::POST,
        "/alice/demo/workspace/commit",
        Some(json!({"branch": "main", "message": "m", "changes": [put("a", "1")]})),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(head(&h, r, "main").await.unwrap(), base, "no refusal moved the branch");
    assert_eq!(pushes(&h).await, before);
    // A quoted name is literal.
    let (s, v) =
        call(&h, &alice, r, Method::POST, "/alice/demo/workspace/commit", Some(commit(json!([put("\"README.md\"", "q\n")]), json!({}))))
            .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(text(&h, r, "main", "README.md").await.unwrap(), "# demo\n");
    // Confirmed, the workflow change lands.
    let base = head(&h, r, "main").await.unwrap();
    let body = json!({"branch": "main", "expected_head": base, "message": "m", "workflow_confirmed": true, "changes": wf});
    let (s, v) = call(&h, &alice, r, Method::POST, "/alice/demo/workspace/commit", Some(body)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn reads_commit_and_write_rules_follow_the_permission(pool: PgPool) {
    let h = harness(pool).await;
    let alice = user(&h, "alice").await;
    let bob = user(&h, "bob").await;
    let r = repo(&h, &alice, "demo", true).await;
    let base = head(&h, r, "main").await.unwrap();
    let owner = Actor::User { id: alice.id, is_admin: false, has_2fa: false, via: ch_core::AuthVia::Session };
    h.state.repos.set_collaborator(&owner, r, bob.id, Permission::Read, &ClientInfo::local()).await.unwrap();
    let body = json!({"branch": "main", "expected_head": base, "message": "m", "changes": [put("a", "1")]});
    let (s, v) = call(&h, &bob, r, Method::POST, "/alice/demo/workspace/commit", Some(body.clone())).await;
    assert_eq!((s, v["code"].as_str()), (StatusCode::FORBIDDEN, Some("read_only")), "{v}");
    // Bob reads the tree and a file at the pinned commit.
    let (s, t) = call(&h, &bob, r, Method::GET, &format!("/alice/demo/workspace/tree?branch=main&sha={base}"), None).await;
    assert_eq!(s, StatusCode::OK, "{t}");
    assert!(t["entries"].as_array().unwrap().iter().any(|e| e["path"] == "README.md"));
    // A commit id that is not in the branch's history looks like a moved head.
    let (s, v) = call(&h, &bob, r, Method::GET, &format!("/alice/demo/workspace/tree?branch=main&sha={}", "a".repeat(40)), None).await;
    assert_eq!((s, v["code"].as_str()), (StatusCode::CONFLICT, Some("head_moved")));
    let (s, v) = call(&h, &bob, r, Method::GET, &format!("/alice/demo/workspace/tree?branch=gone&sha={base}"), None).await;
    assert_eq!((s, v["code"].as_str()), (StatusCode::CONFLICT, Some("branch_gone")));
    // Archived: reads still work, writes are refused.
    h.state.repos.update(&owner, r, RepoUpdate { archived: Some(true), ..Default::default() }, &ClientInfo::local()).await.unwrap();
    let (s, v) = call(&h, &alice, r, Method::POST, "/alice/demo/workspace/commit", Some(body)).await;
    assert_eq!((s, v["code"].as_str()), (StatusCode::FORBIDDEN, Some("read_only")), "{v}");
    let (s, _) = call(&h, &alice, r, Method::GET, "/alice/demo/workspace/boot", None).await;
    assert_eq!(s, StatusCode::OK);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_new_branch_can_start_where_the_drafts_started(pool: PgPool) {
    let h = harness(pool).await;
    let alice = user(&h, "alice").await;
    let r = repo(&h, &alice, "demo", true).await;
    let start = head(&h, r, "main").await.unwrap();
    let (s, _) = call(
        &h,
        &alice,
        r,
        Method::POST,
        "/alice/demo/workspace/commit",
        Some(json!({
            "branch": "main", "expected_head": start, "message": "Someone else", "changes": [put("other.txt", "x\n")]
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    // The branch moved, so straight to it is refused, but a new branch from the old head works.
    let body = |nb: Option<&str>| json!({"branch": "main", "expected_head": start, "new_branch": nb, "message": "Mine", "changes": [put("mine.txt", "m\n")]});
    let (s, v) = call(&h, &alice, r, Method::POST, "/alice/demo/workspace/commit", Some(body(None))).await;
    assert_eq!((s, v["code"].as_str()), (StatusCode::CONFLICT, Some("head_moved")), "{v}");
    let (s, v) = call(&h, &alice, r, Method::POST, "/alice/demo/workspace/commit", Some(body(Some("alice/from-start")))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let sha = v["sha"].as_str().unwrap();
    assert_eq!(h.state.git.commit(r, sha).await.unwrap().parents, vec![start.clone()]);
    assert!(text(&h, r, "alice/from-start", "other.txt").await.is_none(), "it starts from the old head");
    // A commit id outside the branch's history is refused even for a new branch.
    let mut b = body(Some("alice/elsewhere"));
    b["expected_head"] = json!("b".repeat(40));
    let (s, v) = call(&h, &alice, r, Method::POST, "/alice/demo/workspace/commit", Some(b)).await;
    assert_eq!((s, v["code"].as_str()), (StatusCode::CONFLICT, Some("head_moved")), "{v}");
}

/// The workflow list the commit dialog and the run panel read says which workflows a push would
/// start, with each one's branch and path filters applied the way the push trigger applies them:
/// `target` is the branch the push lands on, `changed_b64` the files it changes (NUL between
/// them), and `new=1` a new branch, whose push names no earlier commit, so its files are unknown
/// and path filters let it through.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_workflow_list_applies_branch_and_path_filters(pool: PgPool) {
    let h = harness(pool).await;
    let alice = user(&h, "alice").await;
    let r = repo(&h, &alice, "demo", true).await;
    let base = head(&h, r, "main").await.unwrap();
    let job = "jobs:\n  t:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo ok\n";
    let changes = json!([
        put(".codehaven/workflows/main.yml", &format!("name: main-only\non:\n  push:\n    branches: [main]\n{job}")),
        put(".codehaven/workflows/docs.yml", &format!("name: docs\non:\n  push:\n    paths: ['docs/**']\n{job}")),
        put(".codehaven/workflows/all.yml", &format!("name: every-push\non: push\n{job}")),
        put(".codehaven/workflows/tags.yml", &format!("name: tags-only\non:\n  push:\n    tags: ['v*']\n{job}")),
        put(".codehaven/workflows/manual.yml", &format!("name: manual\non: workflow_dispatch\n{job}")),
    ]);
    let (s, v) = call(
        &h,
        &alice,
        r,
        Method::POST,
        "/alice/demo/workspace/commit",
        Some(json!({"branch": "main", "expected_head": base, "message": "Workflows", "workflow_confirmed": true, "changes": changes})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let sha = v["sha"].as_str().unwrap().to_string();

    // The names of the workflows that would run, for one query.
    let running = |extra: String| {
        let path = format!("/alice/demo/workspace/workflows?branch=main&sha={sha}{extra}");
        let (h, alice) = (&h, &alice);
        async move {
            let (s, v) = call(h, alice, r, Method::GET, &path, None).await;
            assert_eq!(s, StatusCode::OK, "{path}: {v}");
            let mut names: Vec<String> = v["workflows"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|w| w["on_push"] == json!(true))
                .map(|w| w["name"].as_str().unwrap().to_string())
                .collect();
            names.sort();
            names
        }
    };
    let changed = |paths: &[&str]| {
        let raw = b64(&paths.join("\0"));
        format!("&changed_b64={}", raw.replace('+', "%2B").replace('/', "%2F").replace('=', "%3D"))
    };

    // Nothing said about the files: path filters let the push through, as the trigger does.
    assert_eq!(running(String::new()).await, ["docs", "every-push", "main-only"]);
    // Another branch: the main-only workflow stays quiet.
    assert_eq!(running("&target=feature".into()).await, ["docs", "every-push"]);
    // Files outside docs/: the docs workflow stays quiet.
    assert_eq!(running(format!("&target=feature{}", changed(&["src/x.rs", "README.md"]))).await, ["every-push"]);
    // A file under docs/ wakes it.
    assert_eq!(running(format!("&target=feature{}", changed(&["src/x.rs", "docs/guide.md"]))).await, ["docs", "every-push"]);
    // A new branch has no earlier commit to compare with, so its files are unknown.
    assert_eq!(running(format!("&target=feature&new=1{}", changed(&["src/x.rs"]))).await, ["docs", "every-push"]);
    // A target that is not a branch name, and a file list that is not base64, are refused.
    let (s, v) =
        call(&h, &alice, r, Method::GET, &format!("/alice/demo/workspace/workflows?branch=main&sha={sha}&target=bad..name"), None).await;
    assert_eq!((s, v["code"].as_str()), (StatusCode::UNPROCESSABLE_ENTITY, Some("invalid")), "{v}");
    let (s, v) =
        call(&h, &alice, r, Method::GET, &format!("/alice/demo/workspace/workflows?branch=main&sha={sha}&changed_b64=%%%"), None).await;
    assert_eq!((s, v["code"].as_str()), (StatusCode::UNPROCESSABLE_ENTITY, Some("invalid")), "{v}");
}
