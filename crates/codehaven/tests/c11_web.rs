//! Pages and flows added in batch C11 (ch-wyy, ch-2fc, ch-dxk, environments): real services, real
//! git repositories, a real database, the real router.

#![allow(clippy::unwrap_used, dead_code)]

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode};
use ch_core::model::*;
use ch_core::state::AppState;
use ch_core::web_contract::EnvironmentDraft;
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

impl Who {
    fn clone_who(&self) -> Who {
        Who { id: self.id, actor: self.actor.clone(), cookie: self.cookie.clone() }
    }
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

fn new_issue(repo: RepoId, title: &str) -> NewIssue {
    NewIssue { repo_id: repo, title: title.into(), body: String::new(), labels: vec![], assignees: vec![], milestone_id: None }
}

async fn make_repo(h: &H, who: &Who, name: &str, visibility: Visibility) -> RepoId {
    h.s.repos
        .create(
            &who.actor,
            NewRepo { owner_id: who.id, name: name.into(), description: String::new(), visibility, default_branch: None, auto_init: true },
            &c(),
        )
        .await
        .unwrap()
        .id
}

/// ch-wyy: the pinned block is set apart from the issues below it, and a pinned issue that was
/// closed does not sit in the Open tab.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn pinned_issues_have_a_divider_and_a_closed_one_leaves_the_open_tab(pool: PgPool) {
    let h = harness(pool).await;
    let alice = user(&h, "alice").await;
    let repo = make_repo(&h, &alice, "demo", Visibility::Private).await;
    for t in ["Pinned open", "Pinned closed", "Plain one"] {
        h.s.issues.create(&alice.actor, new_issue(repo, t), &c()).await.unwrap();
    }
    h.s.issues.set_pinned(&alice.actor, repo, 1, true, &c()).await.unwrap();
    h.s.issues.set_pinned(&alice.actor, repo, 2, true, &c()).await.unwrap();
    h.s.issues.set_state(&alice.actor, repo, 2, false, None, &c()).await.unwrap();
    let (st, _, page) = send(&h, &alice, Method::GET, "/alice/demo/issues", "").await;
    assert_eq!(st, StatusCode::OK);
    assert!(page.contains("Pinned open"), "the open pinned issue shows");
    assert!(!page.contains("Pinned closed"), "a closed pinned issue is not in the Open tab");
    let at = |needle: &str| page.find(needle).unwrap_or_else(|| panic!("no {needle} on the page"));
    assert!(at("Pinned open") < at("pinned-divider") && at("pinned-divider") < at("Plain one"), "the divider sits between them");
    // With nothing pinned there is no divider.
    h.s.issues.set_pinned(&alice.actor, repo, 1, false, &c()).await.unwrap();
    let (_, _, page) = send(&h, &alice, Method::GET, "/alice/demo/issues", "").await;
    assert!(!page.contains("pinned-divider"));
}

async fn add_collaborator(h: &H, owner: &Who, repo: RepoId, who: &Who, perm: Permission) {
    h.s.repos.set_collaborator(&owner.actor, repo, who.id, perm, &c()).await.unwrap();
}

/// ch-bj5: the environment settings pages. Admins only, secrets never shown back, names escaped.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn environment_settings_are_for_admins_and_never_show_a_secret_back(pool: PgPool) {
    let h = harness(pool).await;
    let alice = user(&h, "alice").await;
    let bob = user(&h, "bob").await;
    let repo = make_repo(&h, &alice, "demo", Visibility::Private).await;
    add_collaborator(&h, &alice, repo, &bob, Permission::Write).await;

    let (st, _, _) = send(&h, &bob, Method::GET, "/alice/demo/settings/environments", "").await;
    assert!(matches!(st, StatusCode::FORBIDDEN | StatusCode::NOT_FOUND), "write is not admin: {st}");
    let (st, _, page) = send(&h, &alice, Method::GET, "/alice/demo/settings/environments", "").await;
    assert_eq!(st, StatusCode::OK);
    assert!(page.contains("No environments"), "{page}");

    // Create, then fill in the rules and a secret.
    let (st, loc, _) = send(&h, &alice, Method::POST, "/alice/demo/settings/environments", "name=prod").await;
    assert_eq!(st, StatusCode::SEE_OTHER, "{loc}");
    assert_eq!(loc, "/alice/demo/settings/environments/prod");
    let form = format!("kind=rules&wait_minutes=5&branches=main%0Arelease%2F*&reviewer={}", bob.id.0);
    let (st, _, _) = send(&h, &alice, Method::POST, "/alice/demo/settings/environments/prod", &form).await;
    assert_eq!(st, StatusCode::SEE_OTHER);
    let (st, _, _) =
        send(&h, &alice, Method::POST, "/alice/demo/settings/environments/prod", "kind=secret&name=api_key&value=hunter2-value").await;
    assert_eq!(st, StatusCode::SEE_OTHER);
    let env = h.s.pipelines.environment(&alice.actor, repo, "prod").await.unwrap();
    assert_eq!((env.wait_minutes, env.branches.clone(), env.reviewers.len()), (5, vec!["main".to_string(), "release/*".to_string()], 1));
    assert_eq!(env.secrets[0].name, "API_KEY");
    let (_, _, page) = send(&h, &alice, Method::GET, "/alice/demo/settings/environments/prod", "").await;
    assert!(page.contains("API_KEY") && page.contains("bob") && page.contains("release/*"), "{page}");
    assert!(!page.contains("hunter2-value"), "a secret is never shown back");

    // Bad input is refused on the page, and text is escaped.
    let (st, _, page) = send(&h, &alice, Method::POST, "/alice/demo/settings/environments", "name=%3Cb%3Ex%3C%2Fb%3E").await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(!page.contains("<b>x</b>"));
    let (st, _, _) =
        send(&h, &alice, Method::POST, "/alice/demo/settings/environments/prod", "kind=rules&wait_minutes=soon&branches=").await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    let (st, _, _) = send(&h, &bob, Method::POST, "/alice/demo/settings/environments/prod/delete", "").await;
    assert!(matches!(st, StatusCode::FORBIDDEN | StatusCode::NOT_FOUND));
    let (st, _, _) = send(&h, &alice, Method::POST, "/alice/demo/settings/environments/prod/secrets/API_KEY/delete", "").await;
    assert_eq!(st, StatusCode::SEE_OTHER);
    let (st, _, _) = send(&h, &alice, Method::POST, "/alice/demo/settings/environments/prod/delete", "").await;
    assert_eq!(st, StatusCode::SEE_OTHER);
    assert!(h.s.pipelines.environments(&alice.actor, repo).await.unwrap().is_empty());
}

fn packages(h: &H) -> Arc<ch_core::packages::PackagesHandle> {
    h.s.ext.get::<ch_core::packages::PackagesHandle>().expect("the packages service is wired")
}

/// A package row with tags, as a push would leave it.
async fn make_package(pool: &PgPool, owner: &Who, repo: Option<RepoId>, kind: &str, name: &str, tags: &[&str]) {
    let id: i64 = sqlx::query_scalar("INSERT INTO packages (owner_id, repo_id, kind, name) VALUES ($1, $2, $3, $4) RETURNING id")
        .bind(owner.id.0)
        .bind(repo.map(|r| r.0))
        .bind(kind)
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap();
    for (i, t) in tags.iter().enumerate() {
        if kind == "container" {
            let digest = format!("sha256:{}", format!("{:x}", i + 1).repeat(64));
            let m: i64 = sqlx::query_scalar(
                "INSERT INTO oci_manifests (package_id, digest, media_type, size, content) VALUES ($1, $2, 'application/vnd.oci.image.index.v1+json', 1234,
                 convert_to('{\"manifests\":[{\"platform\":{\"os\":\"linux\",\"architecture\":\"amd64\"}},{\"platform\":{\"os\":\"unknown\",\"architecture\":\"unknown\"}}]}', 'UTF8')) RETURNING id",
            )
            .bind(id)
            .bind(&digest)
            .fetch_one(pool)
            .await
            .unwrap();
            sqlx::query("INSERT INTO oci_tags (package_id, name, manifest_id) VALUES ($1, $2, $3)")
                .bind(id)
                .bind(t)
                .bind(m)
                .execute(pool)
                .await
                .unwrap();
        }
    }
}

/// ch-2fc: who sees a package, who may link and delete it, and that unlinking never widens.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn packages_follow_their_repository_and_only_the_owner_changes_them(pool: PgPool) {
    let h = harness(pool.clone()).await;
    let alice = user(&h, "alice").await;
    let bob = user(&h, "bob").await;
    let carol = user(&h, "carol").await;
    let repo = make_repo(&h, &alice, "demo", Visibility::Private).await;
    make_repo(&h, &alice, "other", Visibility::Private).await;
    add_collaborator(&h, &alice, repo, &bob, Permission::Read).await;
    make_package(&pool, &alice, Some(repo), "container", "demo", &["v1", "latest"]).await;
    let p = packages(&h);
    let p = &p.0;

    // A reader of the repository sees its package; someone with no grant sees nothing and no count.
    let names = |v: Vec<ch_core::packages::PackageSummary>| v.into_iter().map(|s| s.name).collect::<Vec<_>>();
    assert_eq!(names(p.list(&alice.actor, None, None).await.unwrap()), vec!["demo"]);
    assert_eq!(names(p.list(&bob.actor, None, None).await.unwrap()), vec!["demo"]);
    assert!(p.list(&carol.actor, None, None).await.unwrap().is_empty());
    assert!(matches!(p.detail(&carol.actor, "alice", "container", "demo").await, Err(ch_core::Error::NotFound)));
    assert!(p.list(&carol.actor, None, Some(repo)).await.is_err(), "the repository's own list needs the repository");
    assert_eq!(names(p.list(&bob.actor, Some("alice"), Some(repo)).await.unwrap()), vec!["demo"]);

    // The detail: tags newest first, platforms from the index (attestations left out), and who may change it.
    let d = p.detail(&bob.actor, "alice", "container", "demo").await.unwrap();
    assert_eq!(d.summary.versions, 2);
    assert_eq!(d.versions[0].platforms, vec!["linux/amd64"]);
    assert!(!d.can_admin && d.linkable.is_empty());
    let d = p.detail(&alice.actor, "alice", "container", "demo").await.unwrap();
    assert!(d.can_admin);
    assert_eq!(d.linkable, vec!["alice/demo", "alice/other"]);

    // Unlink leaves it to its owner: the reader loses it, and a reader cannot unlink.
    assert!(matches!(p.unlink(&bob.actor, "alice", "container", "demo", &c()).await, Err(ch_core::Error::Forbidden(_))));
    p.unlink(&alice.actor, "alice", "container", "demo", &c()).await.unwrap();
    assert!(p.list(&bob.actor, None, None).await.unwrap().is_empty());
    assert_eq!(names(p.list(&alice.actor, None, None).await.unwrap()), vec!["demo"]);

    // Linking needs a repository of the same owner, and a confirmation that says who gains access.
    let carols = make_repo(&h, &carol, "mine", Visibility::Private).await;
    let _ = carols;
    assert!(matches!(p.preview_link(&alice.actor, "alice", "container", "demo", "carol/mine").await, Err(ch_core::Error::Validation(_))));
    let preview = p.preview_link(&alice.actor, "alice", "container", "demo", "alice/demo").await.unwrap();
    assert!(preview.audience.contains("People who can read alice/demo") && preview.widens, "{preview:?}");
    let unconfirmed = p.link(&alice.actor, "alice", "container", "demo", "alice/demo", false, &c()).await;
    assert!(matches!(unconfirmed, Err(ch_core::Error::Validation(_))));
    p.link(&alice.actor, "alice", "container", "demo", "alice/demo", true, &c()).await.unwrap();
    assert_eq!(names(p.list(&bob.actor, None, None).await.unwrap()), vec!["demo"], "the repository's readers pull it again");

    // Deleting needs the owner and the typed name.
    assert!(matches!(p.delete_package(&bob.actor, "alice", "container", "demo", "demo", &c()).await, Err(ch_core::Error::Forbidden(_))));
    assert!(matches!(
        p.delete_version(&alice.actor, "alice", "container", "demo", "v1", "nope", &c()).await,
        Err(ch_core::Error::Validation(_))
    ));
    p.delete_version(&alice.actor, "alice", "container", "demo", "v1", "demo", &c()).await.unwrap();
    assert_eq!(p.detail(&alice.actor, "alice", "container", "demo").await.unwrap().versions.len(), 1);
    assert!(matches!(
        p.delete_version(&alice.actor, "alice", "container", "demo", "v1", "demo", &c()).await,
        Err(ch_core::Error::NotFound)
    ));
    p.delete_package(&alice.actor, "alice", "container", "demo", "DEMO", &c()).await.unwrap();
    assert!(p.list(&alice.actor, None, None).await.unwrap().is_empty());
    let actions: Vec<String> =
        sqlx::query_scalar("SELECT action FROM audit_events WHERE action LIKE 'package.%' ORDER BY id").fetch_all(&pool).await.unwrap();
    assert_eq!(actions, ["package.unlinked", "package.linked", "package.version_deleted", "package.deleted"]);
}

/// Deleting a tag must not delete a manifest that a multi-arch index in the package still names.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn deleting_a_tag_keeps_a_manifest_that_an_index_still_points_at(pool: PgPool) {
    let h = harness(pool.clone()).await;
    let alice = user(&h, "alice").await;
    let repo = make_repo(&h, &alice, "demo", Visibility::Private).await;
    make_package(&pool, &alice, Some(repo), "container", "multi", &[]).await;
    let pkg: i64 = sqlx::query_scalar("SELECT id FROM packages WHERE name = 'multi'").fetch_one(&pool).await.unwrap();
    let amd = format!("sha256:{}", "a".repeat(64));
    let arm = format!("sha256:{}", "b".repeat(64));
    let index = format!("sha256:{}", "c".repeat(64));
    let index_json = format!("{{\"manifests\":[{{\"digest\":\"{amd}\"}},{{\"digest\":\"{arm}\"}}]}}");
    for (digest, body, tag) in
        [(&amd, "{}".to_string(), "child-amd64"), (&arm, "{}".to_string(), "child-arm64"), (&index, index_json, "multi")]
    {
        let m: i64 = sqlx::query_scalar(
            "INSERT INTO oci_manifests (package_id, digest, media_type, size, content) VALUES ($1, $2, 'application/json', 10, convert_to($3, 'UTF8')) RETURNING id",
        )
        .bind(pkg)
        .bind(digest)
        .bind(body)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO oci_tags (package_id, name, manifest_id) VALUES ($1, $2, $3)")
            .bind(pkg)
            .bind(tag)
            .bind(m)
            .execute(&pool)
            .await
            .unwrap();
    }
    let p = packages(&h);
    let p = &p.0;
    p.delete_version(&alice.actor, "alice", "container", "multi", "child-amd64", "multi", &c()).await.unwrap();
    let left: Vec<String> = sqlx::query_scalar("SELECT digest FROM oci_manifests WHERE package_id = $1 ORDER BY digest")
        .bind(pkg)
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(left, vec![amd.clone(), arm.clone(), index.clone()], "the index still names the child, so it stays pullable by digest");
    // A manifest nothing names goes with its last tag.
    p.delete_version(&alice.actor, "alice", "container", "multi", "multi", "multi", &c()).await.unwrap();
    let left: Vec<String> = sqlx::query_scalar("SELECT digest FROM oci_manifests WHERE package_id = $1 ORDER BY digest")
        .bind(pkg)
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(left, vec![amd, arm], "the index went; its children are tagged or named and stay for the sweep to judge");
}

const DEPLOY_YML: &str = "name: w\non: workflow_dispatch\njobs:\n  ship:\n    runs-on: ubuntu-latest\n    environment: prod\n    steps:\n      - run: echo shipping\n";

async fn put_file(h: &H, who: &Who, repo: RepoId, path: &str, text: &str) {
    let tip = h.s.git.resolve(repo, "refs/heads/main").await.unwrap();
    h.s.git
        .commit_file_change(
            &who.actor,
            CommitFileChange {
                repo_id: repo,
                branch: "main".into(),
                new_branch: None,
                expected_sha: Some(tip),
                changes: vec![FileChange { path: path.into(), content: Some(text.as_bytes().to_vec()), renamed_from: None }],
                message: format!("add {path}"),
                author: Signature { name: "A".into(), email: "a@example.org".into(), at: chrono::Utc::now() },
            },
            &c(),
        )
        .await
        .unwrap();
}

/// ch-bj5: a run page shows the jobs held at an environment, and only a required reviewer gets the buttons.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_run_page_lets_a_required_reviewer_approve_a_held_deployment(pool: PgPool) {
    let h = harness(pool.clone()).await;
    let alice = user(&h, "alice").await;
    let bob = user(&h, "bob").await;
    sqlx::query("UPDATE users SET can_run_ci = true").execute(&pool).await.unwrap();
    let repo = make_repo(&h, &alice, "demo", Visibility::Private).await;
    add_collaborator(&h, &alice, repo, &bob, Permission::Write).await;
    put_file(&h, &alice, repo, ".github/workflows/w.yml", DEPLOY_YML).await;
    let draft = EnvironmentDraft { wait_minutes: 0, branches: vec![], reviewers: vec![bob.id] };
    h.s.pipelines.save_environment(&alice.actor, repo, "prod", draft, &c()).await.unwrap();
    let run = h.s.ci.dispatch(&alice.actor, repo, "w.yml", "main", serde_json::json!({}), &c()).await.unwrap().0;

    let page_of = |who: Who| {
        let h = &h;
        async move { send(h, &who, Method::GET, &format!("/alice/demo/actions/runs/{run}"), "").await }
    };
    let (st, _, page) = page_of(alice.clone_who()).await;
    assert_eq!(st, StatusCode::OK);
    assert!(page.contains("Deployments") && page.contains("prod") && page.contains("Waiting for a reviewer"), "{page}");
    assert!(!page.contains("Approve and deploy"), "alice is not on the list");
    let (_, _, page) = page_of(bob.clone_who()).await;
    assert!(page.contains("Approve and deploy"));
    let job: i64 = sqlx::query_scalar("SELECT id FROM workflow_jobs WHERE run_id = $1").bind(run).fetch_one(&pool).await.unwrap();
    let review = format!("/alice/demo/actions/runs/{run}/jobs/{job}/review");
    let (st, _, _) = send(&h, &alice, Method::POST, &review, "decision=approve").await;
    assert_eq!(st, StatusCode::FORBIDDEN, "alice owns the repository but is not on the reviewers list");
    let status: String = sqlx::query_scalar("SELECT status FROM workflow_jobs WHERE id = $1").bind(job).fetch_one(&pool).await.unwrap();
    assert_eq!(status, "waiting", "alice's try changed nothing");
    let (st, _, _) = send(&h, &bob, Method::POST, &review, "decision=maybe").await;
    assert!(st.is_client_error());
    let (st, loc, _) = send(&h, &bob, Method::POST, &review, "decision=approve&comment=go").await;
    assert_eq!((st, loc.as_str()), (StatusCode::SEE_OTHER, format!("/alice/demo/actions/runs/{run}").as_str()));
    let status: String = sqlx::query_scalar("SELECT status FROM workflow_jobs WHERE id = $1").bind(job).fetch_one(&pool).await.unwrap();
    assert_eq!(status, "queued");
    let (_, _, page) = page_of(bob.clone_who()).await;
    assert!(page.contains("Approved by bob") && !page.contains("Approve and deploy"), "{page}");
}

fn anonymous() -> Who {
    Who { id: UserId(0), actor: Actor::Anonymous, cookie: String::new() }
}

/// The text of the first `<pre>` line of the page that contains `needle`, unescaped for the few
/// entities askama writes.
fn shown_line(page: &str, needle: &str) -> String {
    let line = page.lines().find(|l| l.contains(needle)).unwrap_or_else(|| panic!("no line with {needle}: {page}"));
    // `echo $TOKEN | docker login` starts earlier on its line than the needle does.
    let from = if needle.starts_with("docker login") { line.find("echo ").unwrap_or(0) } else { line.find(needle).unwrap() };
    line[from..].split('<').next().unwrap_or("").replace("&#x2f;", "/").replace("&#38;", "&")
}

/// Docker's reference grammar (distribution/reference): `[domain/]path[:tag][@digest]`, where the
/// domain is a host with an optional port and each path component is lowercase alphanumerics with
/// single `.`, `_`, `__` or runs of `-` between them.
fn is_docker_reference(r: &str) -> bool {
    fn alnum(c: char) -> bool {
        c.is_ascii_lowercase() || c.is_ascii_digit()
    }
    fn path_component(c: &str) -> bool {
        // alnum ( ( "." | "_" | "__" | "-"+ ) alnum )*, so it starts and ends with an alphanumeric.
        if !c.starts_with(alnum) || !c.ends_with(alnum) {
            return false;
        }
        c.split(alnum).filter(|sep| !sep.is_empty()).all(|sep| sep == "." || sep == "_" || sep == "__" || sep.chars().all(|x| x == '-'))
    }
    fn domain(d: &str) -> bool {
        let (host, port) = match d.rsplit_once(':') {
            Some((h, p)) if !h.ends_with(']') || h.starts_with('[') => (h, Some(p)),
            _ => (d, None),
        };
        if port.is_some_and(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_digit())) {
            return false;
        }
        !host.is_empty()
            && host.split('.').all(|comp| {
                !comp.is_empty()
                    && comp.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                    && !comp.starts_with('-')
                    && !comp.ends_with('-')
            })
    }
    let (name_tag, digest) = match r.split_once('@') {
        Some((n, d)) => (n, Some(d)),
        None => (r, None),
    };
    if digest.is_some_and(|d| !d.contains(':')) {
        return false;
    }
    let (name, tag) = match name_tag.rsplit_once(':') {
        Some((n, t)) if !t.contains('/') => (n, Some(t)),
        _ => (name_tag, None),
    };
    if tag.is_some_and(|t| {
        t.is_empty() || t.len() > 128 || t.starts_with(['.', '-']) || !t.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
    }) {
        return false;
    }
    let mut parts = name.split('/');
    let first = parts.next().unwrap_or("");
    let rest: Vec<&str> = parts.collect();
    if rest.is_empty() {
        return path_component(first);
    }
    let has_domain = first.contains('.') || first.contains(':') || first == "localhost";
    let (domain_ok, path): (bool, Vec<&str>) = if has_domain { (domain(first), rest) } else { (true, [vec![first], rest].concat()) };
    domain_ok && path.iter().all(|c| path_component(c))
}

#[test]
fn the_reference_check_matches_what_docker_accepts_and_rejects() {
    for ok in [
        "code.example.com/alice/demo:v1",
        "192.0.2.100:3200/alice/img:latest",
        "localhost:5000/a/b",
        "alice/demo",
        "a.b-c/d__e/f--g.h:1.0_x",
    ] {
        assert!(is_docker_reference(ok), "{ok}");
    }
    for bad in [
        "http://192.0.2.100:3200/alice/img:v2",
        "https://code.example.com/alice/demo:v1",
        "code.example.com/Alice/demo",
        "code.example.com/alice/demo:",
        "x//y",
        "a/-b",
        "a/b..c",
    ] {
        assert!(!is_docker_reference(bad), "{bad}");
    }
}

/// ch-2fc: the pages. Signed in only, readers see what they could pull, nothing leaks to others.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_packages_pages_list_what_a_reader_could_pull_and_nothing_else(pool: PgPool) {
    let h = harness(pool.clone()).await;
    let alice = user(&h, "alice").await;
    let bob = user(&h, "bob").await;
    let carol = user(&h, "carol").await;
    let repo = make_repo(&h, &alice, "demo", Visibility::Private).await;
    add_collaborator(&h, &alice, repo, &bob, Permission::Read).await;
    make_package(&pool, &alice, Some(repo), "container", "demo", &["v1"]).await;

    let (st, loc, body) = send(&h, &anonymous(), Method::GET, "/packages", "").await;
    assert!(st.is_redirection() || st == StatusCode::UNAUTHORIZED, "signed out: {st} {loc}");
    assert!(!body.contains("demo"));
    let (st, _, page) = send(&h, &bob, Method::GET, "/packages", "").await;
    assert_eq!(st, StatusCode::OK);
    assert!(page.contains("demo") && page.contains("alice/demo"), "{page}");
    let (_, _, page) = send(&h, &carol, Method::GET, "/packages", "").await;
    assert!(!page.contains("alice/demo") && page.contains("No packages"), "{page}");
    let (st, _, page) = send(&h, &carol, Method::GET, "/packages/alice/container/demo", "").await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert!(!page.contains("docker pull"));
    let (st, _, page) = send(&h, &bob, Method::GET, "/packages/alice/container/demo", "").await;
    assert_eq!(st, StatusCode::OK);
    assert!(page.contains("docker pull") && page.contains("/alice/demo:v1") && page.contains("linux/amd64"), "{page}");
    // The commands shown must be ones docker accepts: no URL scheme in a reference or a login host.
    let pull = shown_line(&page, "docker pull ");
    assert_eq!(pull, "docker pull code.example.com/alice/demo:v1");
    assert!(is_docker_reference(pull.trim_start_matches("docker pull ")), "docker would reject: {pull}");
    let login = shown_line(&page, "docker login ");
    assert!(login.starts_with("echo $TOKEN | docker login code.example.com -u bob "), "{login}");
    // The NuGet source URL needs its scheme, and the container page must not print one for docker.
    assert!(!pull.contains("://") && !login.contains("://"));
    assert!(!page.contains("Delete this package"), "a reader cannot change it");
    let (_, _, page) = send(&h, &alice, Method::GET, "/packages/alice/container/demo", "").await;
    assert!(page.contains("Delete this package") && page.contains("Unlink"));
    let (st, _, page) = send(&h, &bob, Method::GET, "/alice/demo/packages", "").await;
    assert_eq!(st, StatusCode::OK);
    assert!(page.contains("demo"));
    let (st, _, _) = send(&h, &carol, Method::GET, "/alice/demo/packages", "").await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // Linking asks first, and says who gains access.
    let (st, _, _) = send(&h, &bob, Method::POST, "/packages/alice/container/demo", "do=unlink").await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let (st, _, page) = send(&h, &alice, Method::POST, "/packages/alice/container/demo", "do=preview_link&repo=alice%2Fdemo").await;
    assert_eq!(st, StatusCode::OK);
    assert!(page.contains("People who can read alice/demo"), "{page}");
    let (st, _, _) = send(&h, &alice, Method::POST, "/packages/alice/container/demo", "do=delete_package&confirm_name=wrong").await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    let (st, loc, _) = send(&h, &alice, Method::POST, "/packages/alice/container/demo", "do=delete_package&confirm_name=demo").await;
    assert_eq!((st, loc.as_str()), (StatusCode::SEE_OTHER, "/packages"));
}

const BUG_FORM: &str = "name: Bug report
description: Something broke
labels: [bug]
body:
  - type: markdown
    attributes:
      value: Tell us what happened.
  - type: input
    id: version
    attributes:
      label: Version
    validations:
      required: true
  - type: dropdown
    id: where
    attributes:
      label: Where
      options: [Linux, Mac]
";

/// ch-dxk: a YAML issue form is drawn on the server and its answers become the issue body.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_yaml_issue_form_is_drawn_checked_and_turned_into_the_issue_body(pool: PgPool) {
    let h = harness(pool.clone()).await;
    let alice = user(&h, "alice").await;
    let repo = make_repo(&h, &alice, "demo", Visibility::Private).await;
    put_file(&h, &alice, repo, ".github/ISSUE_TEMPLATE/bug.yml", BUG_FORM).await;
    put_file(&h, &alice, repo, ".github/ISSUE_TEMPLATE/broken.yml", "body: nope\n").await;

    let (_, _, page) = send(&h, &alice, Method::GET, "/alice/demo/issues/new", "").await;
    assert!(page.contains("Bug report") && page.contains("Something broke"), "{page}");
    assert!(!page.contains("broken"), "a form that cannot be read is left out");
    let tpl = "template=.github%2FISSUE_TEMPLATE%2Fbug.yml";
    let (st, _, page) = send(&h, &alice, Method::GET, &format!("/alice/demo/issues/new?{tpl}"), "").await;
    assert_eq!(st, StatusCode::OK);
    assert!(page.contains("name=\"f_version\"") && page.contains("name=\"f_where\"") && page.contains("Tell us what happened."), "{page}");

    // A missing required answer and a made-up option are refused, and what was typed stays.
    let (st, _, page) =
        send(&h, &alice, Method::POST, "/alice/demo/issues/new", &format!("{tpl}&title=Crash&f_version=&f_where=Windows")).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(page.contains("This answer is required.") && page.contains("Choose one of the listed options."), "{page}");
    assert_eq!(h.s.issues.list(&alice.actor, repo, IssueFilter::default()).await.unwrap().len(), 0);

    let form = "template=.github%2FISSUE_TEMPLATE%2Fbug.yml&title=Crash&f_version=1.2.3&f_where=Mac";
    let (st, loc, _) = send(&h, &alice, Method::POST, "/alice/demo/issues/new", form).await;
    assert_eq!((st, loc.as_str()), (StatusCode::SEE_OTHER, "/alice/demo/issues/1"));
    let issue = h.s.issues.get(&alice.actor, repo, 1).await.unwrap();
    assert_eq!(issue.body, "### Version\n\n1.2.3\n\n### Where\n\nMac\n");
}

/// ch-dxk: the merge queue controls on the pull request page and in settings.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_merge_queue_replaces_the_merge_button_and_only_an_admin_turns_it_on(pool: PgPool) {
    let h = harness(pool.clone()).await;
    let alice = user(&h, "alice").await;
    let bob = user(&h, "bob").await;
    let repo = make_repo(&h, &alice, "demo", Visibility::Private).await;
    add_collaborator(&h, &alice, repo, &bob, Permission::Write).await;
    // A branch with a change, and a pull request from it.
    let tip = h.s.git.resolve(repo, "refs/heads/main").await.unwrap();
    h.s.git
        .commit_file_change(
            &alice.actor,
            CommitFileChange {
                repo_id: repo,
                branch: "main".into(),
                new_branch: Some("topic".into()),
                expected_sha: Some(tip),
                changes: vec![FileChange { path: "t.txt".into(), content: Some(b"t".to_vec()), renamed_from: None }],
                message: "topic".into(),
                author: Signature { name: "A".into(), email: "a@example.org".into(), at: chrono::Utc::now() },
            },
            &c(),
        )
        .await
        .unwrap();
    sqlx::query("INSERT INTO branch_protections (repo_id, pattern, required_approvals) VALUES ($1, 'main', 1)")
        .bind(repo.0)
        .execute(&pool)
        .await
        .unwrap();
    let pr =
        h.s.pulls
            .create(
                &bob.actor,
                NewPull {
                    repo_id: repo,
                    title: "Topic".into(),
                    body: String::new(),
                    head_repo_id: repo,
                    head_ref: "topic".into(),
                    base_ref: "main".into(),
                    draft: false,
                },
                &c(),
            )
            .await
            .unwrap();
    let n = pr.issue.number;
    let pull_path = format!("/alice/demo/pulls/{n}");
    let (_, _, before) = send(&h, &alice, Method::GET, &pull_path, "").await;
    assert!(!before.contains("Add to merge queue"));

    // Bob writes but does not administer.
    let (st, _, _) = send(&h, &bob, Method::POST, "/alice/demo/settings/merge-queue", "enabled=1").await;
    assert!(matches!(st, StatusCode::FORBIDDEN | StatusCode::NOT_FOUND), "{st}");
    let (st, _, page_s) = send(&h, &alice, Method::GET, "/alice/demo/settings", "").await;
    assert_eq!(st, StatusCode::OK);
    assert!(page_s.contains("Merge queue"), "{page_s}");
    let (st, _, _) = send(&h, &alice, Method::POST, "/alice/demo/settings/merge-queue", "enabled=1").await;
    assert_eq!(st, StatusCode::SEE_OTHER);
    assert!(h.s.pulls.merge_queue_enabled(&alice.actor, repo).await.unwrap());

    let (_, _, on) = send(&h, &bob, Method::GET, &pull_path, "").await;
    assert!(on.contains("Add to merge queue"), "{on}");
    let (st, _, _) = send(&h, &bob, Method::POST, &format!("/alice/demo/pulls/{n}/queue/add"), "method=merge").await;
    assert_eq!(st, StatusCode::SEE_OTHER);
    let q = h.s.pulls.queue_state(&bob.actor, repo, n).await.unwrap().unwrap();
    assert_eq!((q.position, q.total), (1, 1));
    let (_, _, in_queue) = send(&h, &bob, Method::GET, &pull_path, "").await;
    assert!(in_queue.contains("position 1 of 1") && in_queue.contains("Remove from the queue"), "{in_queue}");
    let (st, _, _) = send(&h, &bob, Method::POST, &format!("/alice/demo/pulls/{n}/queue/remove"), "").await;
    assert_eq!(st, StatusCode::SEE_OTHER);
    assert!(h.s.pulls.queue_state(&bob.actor, repo, n).await.unwrap().is_none());
}
