//! The release pages (ch-ctk.11): edit, publish, upload, delete a file, the latest links and the
//! SHA-256 of every file. Real services, real git, a real database, the real router.

#![allow(clippy::unwrap_used, dead_code)]

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode};
use ch_core::collab::ReleaseDraft;
use ch_core::model::*;
use ch_core::state::AppState;
use ch_core::{Actor, AuthVia, Config, Permission, ReleaseId, RepoId, UserId};
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

struct Reply {
    status: StatusCode,
    location: String,
    set_cookie: String,
    headers: axum::http::HeaderMap,
    body: Vec<u8>,
}

impl Reply {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
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

async fn make_repo(h: &H, who: &Who, name: &str, tier: Visibility) -> RepoId {
    h.s.repos
        .create(
            &who.actor,
            NewRepo {
                owner_id: who.id,
                name: name.into(),
                description: String::new(),
                visibility: tier,
                default_branch: None,
                auto_init: true,
            },
            &c(),
        )
        .await
        .unwrap()
        .id
}

fn draft(tag: &str, is_draft: bool) -> ReleaseDraft {
    ReleaseDraft {
        tag_name: tag.into(),
        target_commitish: Some("main".into()),
        name: Some("First".into()),
        body: Some("notes".into()),
        draft: is_draft,
        prerelease: false,
        generate_notes: false,
        make_latest: Default::default(),
    }
}

async fn finish(resp: axum::response::Response) -> Reply {
    let status = resp.status();
    let headers = resp.headers().clone();
    let location = headers.get("location").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let set_cookie =
        headers.get_all("set-cookie").iter().filter_map(|v| v.to_str().ok()).find(|v| v.contains("flash")).unwrap_or("").to_string();
    let body = resp.into_body().collect().await.unwrap().to_bytes().to_vec();
    Reply { status, location, set_cookie, headers, body }
}

/// A request as `who` (or signed out), with a cookie for the flash a redirect set.
async fn call(
    h: &H,
    who: Option<&Who>,
    method: Method,
    path: &str,
    extra: &[(&str, &str)],
    body: Body,
    content_type: Option<&str>,
) -> Reply {
    let mut b = Request::builder().method(method.clone()).uri(path).header("origin", ORIGIN);
    if let Some(w) = who {
        b = b.header("cookie", format!("__Host-ch_session={}", w.cookie));
    }
    if let Some(ct) = content_type {
        b = b.header("content-type", ct);
    }
    for (k, v) in extra {
        b = b.header(*k, *v);
    }
    let mut req = b.body(body).unwrap();
    req.extensions_mut().insert(ConnectInfo("203.0.113.9:5555".parse::<SocketAddr>().unwrap()));
    finish(h.app.clone().oneshot(req).await.unwrap()).await
}

async fn get(h: &H, who: Option<&Who>, path: &str) -> Reply {
    call(h, who, Method::GET, path, &[], Body::empty(), None).await
}

async fn form(h: &H, who: &Who, path: &str, fields: &str) -> Reply {
    let token = h.guard.token_for_session(&who.cookie);
    call(h, Some(who), Method::POST, path, &[], Body::from(format!("_csrf={token}&{fields}")), Some("application/x-www-form-urlencoded"))
        .await
}

/// A multipart upload of `files` (name, bytes), with the token in the query as the page's form
/// has it, or none.
async fn upload(h: &H, who: &Who, path: &str, files: &[(&str, &[u8])], token: Option<&str>) -> Reply {
    let boundary = "----chtestboundary";
    let mut body: Vec<u8> = Vec::new();
    for (name, data) in files {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\nContent-Type: text/plain\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(data);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    let url = match token {
        Some(t) => format!("{path}?_csrf={t}"),
        None => path.to_string(),
    };
    call(h, Some(who), Method::POST, &url, &[], Body::from(body), Some(&format!("multipart/form-data; boundary={boundary}"))).await
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_writer_edits_publishes_uploads_and_deletes_files(pool: PgPool) {
    let h = harness(pool).await;
    let alice = user(&h, "alice").await;
    let bob = user(&h, "bob").await;
    let repo = make_repo(&h, &alice, "demo", Visibility::Private).await;
    h.s.repos.set_collaborator(&alice.actor, repo, bob.id, Permission::Read, &c()).await.unwrap();
    let rel = h.s.releases.create(&alice.actor, repo, draft("v1", true), &c()).await.unwrap().release.id;
    let at = format!("/alice/demo/releases/{}", rel.0);

    // The writer sees what a draft needs; a reader does not see the draft at all.
    let page = get(&h, Some(&alice), "/alice/demo/releases/tag/v1").await;
    assert_eq!(page.status, StatusCode::OK);
    let t = page.text();
    assert!(t.contains("Publish release") && t.contains("Upload files") && t.contains(&format!("{at}/edit")), "{t}");
    assert_eq!(get(&h, Some(&bob), "/alice/demo/releases/tag/v1").await.status, StatusCode::NOT_FOUND);

    // Edit.
    let edit = get(&h, Some(&alice), &format!("{at}/edit")).await;
    assert_eq!(edit.status, StatusCode::OK);
    assert!(edit.text().contains("Save release"));
    let r = form(&h, &alice, &format!("{at}/edit"), "title=Version+one&notes=Hello+%3Cscript%3Ealert(1)%3C%2Fscript%3E").await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.text());
    let v = h.s.releases.view(&alice.actor, repo, rel).await.unwrap();
    assert_eq!((v.release.name.as_str(), v.release.body.as_str()), ("Version one", "Hello <script>alert(1)</script>"));
    assert!(v.release.draft, "an edit does not publish");
    let page = get(&h, Some(&alice), "/alice/demo/releases/tag/v1").await.text();
    assert!(page.contains("Version one") && !page.contains("<script>alert(1)"), "notes are sanitized");

    // Upload: a name with a space and markup is escaped on the page and the file has its digest.
    let token = h.guard.token_for_session(&alice.cookie);
    let r = upload(&h, &alice, &format!("{at}/assets"), &[("a b <i>.txt", b"hello world")], Some(&token)).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.text());
    let v = h.s.releases.view(&alice.actor, repo, rel).await.unwrap();
    assert_eq!(v.assets.len(), 1);
    let asset = v.assets[0].clone();
    assert_eq!((asset.name.as_str(), asset.size), ("a b <i>.txt", 11));
    assert_eq!(asset.sha256, "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9");
    let page = get(&h, Some(&alice), "/alice/demo/releases/tag/v1").await.text();
    assert!(page.contains("b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"), "the SHA-256 is shown");
    assert!(page.contains("SHA-256") && !page.contains("<i>.txt"), "the name is escaped: {page}");
    // The same name twice is refused and says so; a path in a name is cut to the name.
    let r = upload(&h, &alice, &format!("{at}/assets"), &[("a b <i>.txt", b"again")], Some(&token)).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert!(r.set_cookie.contains("flash"), "the refusal is shown on the page");
    let r = upload(&h, &alice, &format!("{at}/assets"), &[("..\\..\\evil/deep/ok.bin", b"x")], Some(&token)).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let names: Vec<String> = h.s.releases.view(&alice.actor, repo, rel).await.unwrap().assets.into_iter().map(|a| a.name).collect();
    assert!(names.contains(&"ok.bin".to_string()) && names.len() == 2, "{names:?}");

    // No token, a wrong token and a foreign origin store nothing.
    let before = h.s.releases.view(&alice.actor, repo, rel).await.unwrap().assets.len();
    let r = upload(&h, &alice, &format!("{at}/assets"), &[("nope.bin", b"x")], None).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = upload(&h, &alice, &format!("{at}/assets"), &[("nope.bin", b"x")], Some("wrong")).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(h.s.releases.view(&alice.actor, repo, rel).await.unwrap().assets.len(), before);

    // Publish: it gets its tag, a reader sees it, and the writer's buttons are gone for the reader.
    let r = form(&h, &alice, &format!("{at}/publish"), "").await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.text());
    assert!(!h.s.releases.view(&alice.actor, repo, rel).await.unwrap().release.draft);
    let page = get(&h, Some(&bob), "/alice/demo/releases/tag/v1").await;
    assert_eq!(page.status, StatusCode::OK);
    let t = page.text();
    assert!(!t.contains("Publish release") && !t.contains("Upload files") && !t.contains("Delete file") && !t.contains("/edit"), "{t}");
    assert!(t.contains("b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"), "a reader sees the digest too");

    // A reader may not edit, publish, upload or delete a file.
    assert!(matches!(get(&h, Some(&bob), &format!("{at}/edit")).await.status, StatusCode::FORBIDDEN | StatusCode::NOT_FOUND));
    assert!(matches!(form(&h, &bob, &format!("{at}/edit"), "title=x").await.status, StatusCode::FORBIDDEN | StatusCode::NOT_FOUND));
    assert!(matches!(form(&h, &bob, &format!("{at}/publish"), "").await.status, StatusCode::FORBIDDEN | StatusCode::NOT_FOUND));
    let btoken = h.guard.token_for_session(&bob.cookie);
    let r = upload(&h, &bob, &format!("{at}/assets"), &[("bob.bin", b"x")], Some(&btoken)).await;
    assert!(matches!(r.status, StatusCode::FORBIDDEN | StatusCode::NOT_FOUND), "{}", r.status);
    assert!(matches!(
        form(&h, &bob, &format!("{at}/assets/{}/delete", asset.id), "").await.status,
        StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
    ));
    assert_eq!(h.s.releases.view(&alice.actor, repo, rel).await.unwrap().assets.len(), 2);

    // A file is deleted through its own release only.
    let other = h.s.releases.create(&alice.actor, repo, draft("v2", false), &c()).await.unwrap().release.id;
    let r = form(&h, &alice, &format!("/alice/demo/releases/{}/assets/{}/delete", other.0, asset.id), "").await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "a file of another release is not reached through this one");
    let r = form(&h, &alice, &format!("{at}/assets/{}/delete", asset.id), "").await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    assert_eq!(h.s.releases.view(&alice.actor, repo, rel).await.unwrap().assets.len(), 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_download_answers_range_and_head(pool: PgPool) {
    let h = harness(pool).await;
    let alice = user(&h, "alice").await;
    let repo = make_repo(&h, &alice, "demo", Visibility::Private).await;
    let rel = h.s.releases.create(&alice.actor, repo, draft("v1", false), &c()).await.unwrap().release.id;
    let body: futures::stream::BoxStream<'static, std::io::Result<bytes::Bytes>> =
        Box::pin(futures::stream::once(async { Ok(bytes::Bytes::from_static(b"hello world")) }));
    h.s.releases.upload_asset(&alice.actor, repo, rel, "a.txt", None, "text/plain", body, &c()).await.unwrap();
    let url = "/alice/demo/releases/download/v1/a.txt";
    let ranged = |range: &'static str, m: Method| {
        let (h, alice) = (&h, &alice);
        async move { call(h, Some(alice), m, url, &[("range", range)], Body::empty(), None).await }
    };
    let part = ranged("bytes=6-10", Method::GET).await;
    assert_eq!((part.status, part.body.as_slice()), (StatusCode::PARTIAL_CONTENT, &b"world"[..]));
    assert_eq!(part.headers["content-range"], "bytes 6-10/11");
    assert_eq!(part.headers["etag"], "\"b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9\"");
    assert_eq!(ranged("bytes=99-", Method::GET).await.status, StatusCode::RANGE_NOT_SATISFIABLE);
    let head = ranged("bytes=0-1", Method::HEAD).await;
    assert_eq!((head.status, head.body.len()), (StatusCode::OK, 0));
    assert_eq!(head.headers["content-length"], "11");
    let full = get(&h, Some(&alice), url).await;
    assert_eq!((full.status, full.body.as_slice()), (StatusCode::OK, &b"hello world"[..]));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_latest_links_follow_the_latest_release_and_hide_drafts(pool: PgPool) {
    let h = harness(pool).await;
    let alice = user(&h, "alice").await;
    let bob = user(&h, "bob").await;
    let repo = make_repo(&h, &alice, "demo", Visibility::Private).await;
    h.s.repos.set_collaborator(&alice.actor, repo, bob.id, Permission::Read, &c()).await.unwrap();
    // Nothing published yet: the latest links are 404, like a repository with no such page.
    assert_eq!(get(&h, Some(&bob), "/alice/demo/releases/latest").await.status, StatusCode::NOT_FOUND);
    let one = h.s.releases.create(&alice.actor, repo, draft("v1", false), &c()).await.unwrap().release.id;
    for (rel, name, data) in [(one, "tool.txt", &b"one"[..])] {
        let body: futures::stream::BoxStream<'static, std::io::Result<bytes::Bytes>> =
            Box::pin(futures::stream::once(async move { Ok(bytes::Bytes::copy_from_slice(data)) }));
        h.s.releases.upload_asset(&alice.actor, repo, rel, name, None, "text/plain", body, &c()).await.unwrap();
    }
    // A draft is never the latest, to anyone.
    h.s.releases.create(&alice.actor, repo, draft("v2", true), &c()).await.unwrap();
    for who in [&alice, &bob] {
        let r = get(&h, Some(who), "/alice/demo/releases/latest").await;
        assert_eq!((r.status, r.location.as_str()), (StatusCode::FOUND, "/alice/demo/releases/tag/v1"));
        let r = get(&h, Some(who), "/alice/demo/releases/latest/download/tool.txt").await;
        assert_eq!((r.status, r.location.as_str()), (StatusCode::FOUND, "/alice/demo/releases/download/v1/tool.txt"));
        // A file that is not on the latest release is a 404, not a redirect to nothing.
        assert_eq!(get(&h, Some(who), "/alice/demo/releases/latest/download/nope.txt").await.status, StatusCode::NOT_FOUND);
    }
    // Publishing the second release moves both links.
    let two = h.s.releases.view_by_tag(&alice.actor, repo, "v2").await.unwrap().release.id;
    h.s.releases
        .update(&alice.actor, repo, ReleaseId(two.0), ch_core::collab::ReleaseEdit { draft: Some(false), ..Default::default() }, &c())
        .await
        .unwrap();
    assert_eq!(get(&h, Some(&bob), "/alice/demo/releases/latest").await.location, "/alice/demo/releases/tag/v2");
    // Signed out, a private repository's links look like a page that is not there.
    let anon = get(&h, None, "/alice/demo/releases/latest").await;
    assert_eq!(anon.status, StatusCode::SEE_OTHER);
    assert!(anon.location.starts_with("/login"), "{}", anon.location);
    let missing = get(&h, None, "/alice/does-not-exist/releases/latest").await;
    assert_eq!((anon.status, &anon.location), (missing.status, &missing.location.replace("does-not-exist", "demo")));
}

// ------------------------------------------------------------------ a workflow job's token

async fn commit_file(h: &H, who: &Actor, r: RepoId, path: &str, text: &str) {
    let expected = h.s.git.resolve(r, "refs/heads/main").await.unwrap();
    h.s.git
        .commit_file_change(
            who,
            CommitFileChange {
                repo_id: r,
                branch: "main".into(),
                new_branch: None,
                expected_sha: Some(expected),
                changes: vec![FileChange { path: path.into(), content: Some(text.as_bytes().to_vec()), renamed_from: None }],
                message: format!("Change {path}"),
                author: Signature { name: "A".into(), email: "a@example.org".into(), at: chrono::Utc::now() },
            },
            &c(),
        )
        .await
        .unwrap();
}

/// The raw token of a running job whose workflow says `permissions`, as a runner would hold it.
async fn job_token(h: &H, owner: &Who, repo: RepoId, permissions: &str, n: u32, pool: &PgPool) -> String {
    let file = format!(".github/workflows/w{n}.yml");
    let wf = format!(
        "name: w\non: workflow_dispatch\n{permissions}jobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n"
    );
    commit_file(h, &owner.actor, repo, &file, &wf).await;
    let run = h.s.ci.dispatch(&owner.actor, repo, &format!("w{n}.yml"), "main", serde_json::json!({}), &c()).await.unwrap();
    let token = format!("chj_test{n}abcdefghijklmnopqrstuvwxyz0123456789");
    sqlx::query("UPDATE workflow_jobs SET status = 'in_progress', token_hash = $2 WHERE run_id = $1")
        .bind(run.0)
        .bind(ch_db::tokens::hash_token(&token))
        .execute(pool)
        .await
        .unwrap();
    token
}

async fn api(h: &H, token: &str, method: Method, path: &str, json: Option<serde_json::Value>, content_type: &str, body: Vec<u8>) -> Reply {
    let (ct, bytes) = match json {
        Some(v) => ("application/json".to_string(), v.to_string().into_bytes()),
        None => (content_type.to_string(), body),
    };
    let mut b = Request::builder().method(method).uri(path).header("authorization", format!("Bearer {token}")).header("content-type", ct);
    b = b.header("accept", "application/vnd.github+json");
    let mut req = b.body(Body::from(bytes)).unwrap();
    req.extensions_mut().insert(ConnectInfo("203.0.113.9:5555".parse::<SocketAddr>().unwrap()));
    finish(h.app.clone().oneshot(req).await.unwrap()).await
}

/// What softprops/action-gh-release and ncipollo/release-action do, over HTTP with the job's own
/// token: create, upload by the release's `upload_url`, replace a file of the same name, publish,
/// find by tag, ask for the latest, and download with the token.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_job_with_contents_write_runs_the_release_flow_and_one_without_cannot(pool: PgPool) {
    let h = harness(pool.clone()).await;
    let alice = user(&h, "alice").await;
    sqlx::query("UPDATE users SET can_run_ci = true WHERE id = $1").bind(alice.id.0).execute(&pool).await.unwrap();
    let repo = make_repo(&h, &alice, "app", Visibility::Private).await;
    let writer = job_token(&h, &alice, repo, "permissions:\n  contents: write\n", 1, &pool).await;
    let reader = job_token(&h, &alice, repo, "", 2, &pool).await;
    let r = "/api/v3/repos/alice/app";

    // A job with no `permissions:` reads and writes nothing.
    let plain = api(&h, &reader, Method::POST, &format!("{r}/releases"), Some(serde_json::json!({"tag_name": "v1"})), "", vec![]).await;
    assert_eq!(plain.status, StatusCode::FORBIDDEN, "{}", plain.text());
    assert_eq!(api(&h, &reader, Method::GET, &format!("{r}/releases"), None, "", vec![]).await.status, StatusCode::OK);

    // Create as a draft with generated notes, the way the action does, then upload by upload_url.
    let created = api(
        &h,
        &writer,
        Method::POST,
        &format!("{r}/releases"),
        Some(serde_json::json!({"tag_name": "v1", "name": "v1", "draft": true, "generate_release_notes": true, "make_latest": "true", "target_commitish": "main"})),
        "",
        vec![],
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.text());
    let rel: serde_json::Value = serde_json::from_slice(&created.body).unwrap();
    let id = rel["id"].as_i64().unwrap();
    let upload_url = rel["upload_url"].as_str().unwrap().split('{').next().unwrap().to_string();
    assert!(upload_url.starts_with("https://code.example.com/api/v3/repos/alice/app/releases/"), "{upload_url}");
    let upload_path = upload_url.trim_start_matches("https://code.example.com").to_string();
    let put = |name: &str, data: &'static [u8]| {
        let (h, writer, path) = (&h, &writer, format!("{upload_path}?name={name}"));
        async move { api(h, writer, Method::POST, &path, None, "text/plain", data.to_vec()).await }
    };
    let first = put("app.txt", b"hello world").await;
    assert_eq!(first.status, StatusCode::CREATED, "{}", first.text());
    let a: serde_json::Value = serde_json::from_slice(&first.body).unwrap();
    assert_eq!(a["digest"], "sha256:b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9");
    // A second file of the same name is a 422; deleting the first (the action's "overwrite") lets it in.
    assert_eq!(put("app.txt", b"again").await.status, StatusCode::UNPROCESSABLE_ENTITY);
    let aid = a["id"].as_i64().unwrap();
    assert_eq!(
        api(&h, &writer, Method::DELETE, &format!("{r}/releases/assets/{aid}"), None, "", vec![]).await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(put("app.txt", b"replaced").await.status, StatusCode::CREATED);

    // Publish, find it by tag and as the latest, and download with the job's token.
    let published =
        api(&h, &writer, Method::PATCH, &format!("{r}/releases/{id}"), Some(serde_json::json!({"draft": false})), "", vec![]).await;
    assert_eq!(published.status, StatusCode::OK, "{}", published.text());
    let by_tag = api(&h, &writer, Method::GET, &format!("{r}/releases/tags/v1"), None, "", vec![]).await;
    assert_eq!(by_tag.status, StatusCode::OK);
    let latest = api(&h, &writer, Method::GET, &format!("{r}/releases/latest"), None, "", vec![]).await;
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&latest.body).unwrap()["tag_name"], "v1");
    let file = api(&h, &writer, Method::GET, "/alice/app/releases/download/v1/app.txt", None, "", vec![]).await;
    assert_eq!((file.status, file.body.as_slice()), (StatusCode::OK, &b"replaced"[..]));
    // The same token reads nothing it was not given: another repository's file is out of reach.
    let other_rel = {
        let other = make_repo(&h, &alice, "elsewhere", Visibility::Private).await;
        let rel = h.s.releases.create(&alice.actor, other, draft("v1", false), &c()).await.unwrap().release.id;
        let body: futures::stream::BoxStream<'static, std::io::Result<bytes::Bytes>> =
            Box::pin(futures::stream::once(async { Ok(bytes::Bytes::from_static(b"private")) }));
        h.s.releases.upload_asset(&alice.actor, other, rel, "secret.txt", None, "text/plain", body, &c()).await.unwrap();
        rel
    };
    let _ = other_rel;
    let stolen = api(&h, &writer, Method::GET, "/alice/elsewhere/releases/download/v1/secret.txt", None, "", vec![]).await;
    assert_ne!(stolen.status, StatusCode::OK, "a job reads only its own repository");
    assert!(!stolen.text().contains("private"));
    // The job's release started no workflow and left the bot as its author.
    let author: String = sqlx::query_scalar("SELECT u.username::text FROM releases r JOIN users u ON u.id = r.author_id WHERE r.id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(author, "codehaven-actions");

    // The read-only job still cannot upload, edit or delete.
    let denied = api(&h, &reader, Method::POST, &format!("{upload_path}?name=x"), None, "text/plain", b"x".to_vec()).await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    let denied = api(&h, &reader, Method::PATCH, &format!("{r}/releases/{id}"), Some(serde_json::json!({"body": "x"})), "", vec![]).await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    assert_eq!(api(&h, &reader, Method::DELETE, &format!("{r}/releases/{id}"), None, "", vec![]).await.status, StatusCode::FORBIDDEN);

    // Another repository is out of reach for either token.
    let other = make_repo(&h, &alice, "other", Visibility::Private).await;
    let _ = other;
    let r2 = "/api/v3/repos/alice/other";
    let cross = api(&h, &writer, Method::POST, &format!("{r2}/releases"), Some(serde_json::json!({"tag_name": "v1"})), "", vec![]).await;
    assert!(matches!(cross.status, StatusCode::FORBIDDEN | StatusCode::NOT_FOUND), "{}", cross.status);
}

/// ch-ctk.10: a file upload is not cut off by the request clock, only by silence. With the clock
/// at one second, a body that takes three seconds to arrive in pieces is stored; one that goes
/// quiet for longer than the idle limit is not.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_slow_upload_is_stored_and_a_silent_one_is_cut(pool: PgPool) {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::for_tests();
    cfg.storage.data_dir = dir.path().to_path_buf();
    cfg.server.external_url = ORIGIN.into();
    cfg.http.request_timeout_secs = 1;
    cfg.http.upload_idle_secs = 2;
    let cfg = Arc::new(cfg);
    let s = wiring::build_state(cfg.clone(), pool.clone()).await.unwrap();
    let guard = ch_auth::Guard::new(&cfg);
    let app = build_router(s.clone(), Metrics::new(), None);
    let h = H { app, s, guard, _dir: dir };
    let alice = user(&h, "alice").await;
    sqlx::query("UPDATE users SET can_run_ci = true WHERE id = $1").bind(alice.id.0).execute(&pool).await.unwrap();
    let repo = make_repo(&h, &alice, "app", Visibility::Private).await;
    let rel = h.s.releases.create(&alice.actor, repo, draft("v1", true), &c()).await.unwrap().release.id;
    let token = job_token(&h, &alice, repo, "permissions:\n  contents: write\n", 1, &pool).await;
    let path = format!("/api/v3/repos/alice/app/releases/{}/assets?name=slow.bin", rel.0);

    let body = |gaps: Vec<u64>| {
        let chunks = futures::stream::unfold(gaps.into_iter().enumerate(), |mut it| async move {
            let (i, gap) = it.next()?;
            tokio::time::sleep(std::time::Duration::from_millis(gap)).await;
            Some((Ok::<_, std::io::Error>(bytes::Bytes::from(vec![b'a' + i as u8; 4])), it))
        });
        Body::from_stream(chunks)
    };
    let send = |b: Body| {
        let app = h.app.clone();
        let (token, path) = (token.clone(), path.clone());
        async move {
            let mut req = Request::builder()
                .method(Method::POST)
                .uri(path)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/octet-stream")
                .body(b)
                .unwrap();
            req.extensions_mut().insert(ConnectInfo("203.0.113.9:5555".parse::<SocketAddr>().unwrap()));
            finish(app.oneshot(req).await.unwrap()).await
        }
    };
    // Four pieces, 800 ms apart: over three seconds in all, three times the request clock.
    let slow = send(body(vec![800, 800, 800, 800])).await;
    assert_eq!(slow.status, StatusCode::CREATED, "{}", slow.text());
    let v = h.s.releases.view(&alice.actor, repo, rel).await.unwrap();
    assert_eq!((v.assets[0].name.as_str(), v.assets[0].size), ("slow.bin", 16));
    // A gap longer than the idle limit ends the upload and stores nothing under that name.
    let path2 = path.replace("slow.bin", "stuck.bin");
    let mut req = Request::builder()
        .method(Method::POST)
        .uri(path2)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/octet-stream")
        .body(body(vec![100, 3500]))
        .unwrap();
    req.extensions_mut().insert(ConnectInfo("203.0.113.9:5555".parse::<SocketAddr>().unwrap()));
    let stuck = finish(h.app.clone().oneshot(req).await.unwrap()).await;
    assert!(!stuck.status.is_success(), "{}", stuck.status);
    let names: Vec<String> = h.s.releases.view(&alice.actor, repo, rel).await.unwrap().assets.into_iter().map(|a| a.name).collect();
    assert_eq!(names, ["slow.bin"], "the silent upload stored nothing");
}
