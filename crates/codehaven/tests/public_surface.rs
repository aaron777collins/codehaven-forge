//! The signed-out surface per tier (epic ch-4pn, spec section 5): from "nothing" to exact.
//!
//! A real database and the real services, one seeded repository with an issue, a pull request,
//! a comment, a label, a milestone and a release with a file. Every route the router has
//! registered is sent every method signed out, in five worlds, and whatever was not rejected is
//! written to `target/` and compared with both the route table and a committed file:
//!
//! | world    | switch | the repository the paths name | must equal                     |
//! |----------|--------|-------------------------------|--------------------------------|
//! | off      | off    | public before the switch went off | `anonymous.off.txt`        |
//! | missing  | on     | does not exist                | `anonymous.missing.txt`        |
//! | private  | on     | private                       | the missing file, byte for byte|
//! | internal | on     | internal                      | the missing file, byte for byte|
//! | public   | on     | public                        | `anonymous.public.txt`         |
//!
//! The test never writes the committed files. A change to what strangers can reach is a change to
//! `ch_core::access::PUBLIC_REPO_ROUTES` and to those files, made on purpose and reviewed.

#![allow(clippy::unwrap_used, clippy::type_complexity)]

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode, header};
use ch_core::access::{self, Cost, PUBLIC_REPO_ROUTES, PublicTarget};
use ch_core::collab::{LabelDraft, MilestoneDraft, ReleaseDraft};
use ch_core::model::*;
use ch_core::state::AppState;
use ch_core::{Actor, AuthVia, Config, RepoId, UserId};
use codehaven::app::build_router;
use codehaven::metrics::Metrics;
use codehaven::wiring;
use http_body_util::BodyExt;
use sqlx::PgPool;
use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use tower::ServiceExt;

const PLANTED: &[&str] = &["alice-planted@example.org", "PLANTED-SECRET-ISSUE", "hidden-proj", "bobplanted", "PLANTED-PRIVATE-BODY"];

struct World {
    app: Router,
    state: AppState,
    _dir: tempfile::TempDir,
    proj: RepoId,
    sha: String,
    blob: String,
    comment: i64,
    release: i64,
    asset: i64,
}

fn alice_actor(id: UserId) -> Actor {
    Actor::User { id, is_admin: false, has_2fa: false, via: AuthVia::Session }
}

fn c() -> ClientInfo {
    ClientInfo::local()
}

async fn user(state: &AppState, name: &str, email: &str) -> UserId {
    state
        .accounts
        .create_user(
            &Actor::System,
            NewUser {
                username: name.into(),
                email: email.into(),
                display_name: Some(format!("{name} display")),
                is_admin: false,
                can_run_ci: false,
            },
            &c(),
        )
        .await
        .unwrap()
        .id
}

async fn seed(pool: PgPool) -> World {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::for_tests();
    cfg.storage.data_dir = dir.path().to_path_buf();
    cfg.server.external_url = "https://code.example.com".into();
    // The sweep covers the connector's routes too, so they must be registered.
    cfg.mcp.enabled = true;
    let cfg = Arc::new(cfg);
    let state = wiring::build_state(cfg.clone(), pool.clone()).await.unwrap();
    let app = build_router(state.clone(), Metrics::new(), None);

    let alice = user(&state, "alice", "alice-planted@example.org").await;
    let bob = user(&state, "bobplanted", "bob@example.org").await;
    let a = alice_actor(alice);
    let new = |name: &str| NewRepo {
        owner_id: alice,
        name: name.into(),
        description: format!("{name} description"),
        visibility: Visibility::Private,
        default_branch: None,
        auto_init: true,
    };
    let proj = state.repos.create(&a, new("proj"), &c()).await.unwrap();
    let hidden = state.repos.create(&a, new("hidden-proj"), &c()).await.unwrap();
    let sig = Signature { name: "Alice".into(), email: "alice@users.noreply.code.example.com".into(), at: chrono::Utc::now() };
    state
        .git
        .commit_file_change(
            &a,
            CommitFileChange {
                repo_id: proj.id,
                branch: "main".into(),
                new_branch: Some("feature".into()),
                expected_sha: None,
                changes: vec![FileChange { path: "src/lib.rs".into(), content: Some(b"pub fn x() {}\n".to_vec()), renamed_from: None }],
                message: "Add lib".into(),
                author: sig.clone(),
            },
            &c(),
        )
        .await
        .unwrap();
    let sha = state.git.resolve(proj.id, "refs/heads/main").await.unwrap();
    let blob = state.git.blob(proj.id, "main", "README.md", 1 << 20).await.unwrap().sha;
    let label = LabelDraft { name: "bug".into(), color: Some("d73a4a".into()), description: None };
    state.issues.create_label(&a, proj.id, label, &c()).await.unwrap();
    let ms = MilestoneDraft { title: "v1".into(), description: None, due_on: None, open: Some(true) };
    state.issues.create_milestone(&a, proj.id, ms, &c()).await.unwrap();
    let issue = NewIssue {
        repo_id: proj.id,
        title: "A public issue".into(),
        body: "hello".into(),
        labels: vec![],
        assignees: vec![],
        milestone_id: None,
    };
    state.issues.create(&a, issue, &c()).await.unwrap();
    let comment = state.issues.add_comment(&a, proj.id, 1, "a comment", &c()).await.unwrap().id.0;
    let pull = NewPull {
        repo_id: proj.id,
        title: "A pull request".into(),
        body: "please".into(),
        head_repo_id: proj.id,
        head_ref: "feature".into(),
        base_ref: "main".into(),
        draft: false,
    };
    state.pulls.create(&a, pull, &c()).await.unwrap();
    let draft = ReleaseDraft {
        tag_name: "v1".into(),
        target_commitish: Some("main".into()),
        name: Some("First".into()),
        body: Some("notes".into()),
        draft: false,
        prerelease: false,
        generate_notes: false,
    };
    let release = state.releases.create(&a, proj.id, draft, &c()).await.unwrap().release.id;
    let body: futures::stream::BoxStream<'static, std::io::Result<bytes::Bytes>> =
        Box::pin(futures::stream::once(async { Ok(bytes::Bytes::from_static(b"file contents\n")) }));
    let asset = state.releases.upload_asset(&a, proj.id, release, "notes.txt", None, "text/plain", body, &c()).await.unwrap().id;

    // What a stranger must never see: a private repository and its issue, an email, a member.
    let secret = NewIssue {
        repo_id: hidden.id,
        title: "PLANTED-SECRET-ISSUE".into(),
        body: "PLANTED-PRIVATE-BODY".into(),
        labels: vec![],
        assignees: vec![],
        milestone_id: None,
    };
    state.issues.create(&a, secret, &c()).await.unwrap();
    let org = state.people.create_org(&a, "harbor", "Harbor Works", &c()).await.unwrap();
    sqlx::query("INSERT INTO org_members (org_id, user_id, role) VALUES ($1, $2, 'member') ON CONFLICT DO NOTHING")
        .bind(org.id.0)
        .bind(bob.0)
        .execute(&pool)
        .await
        .unwrap();
    let tools = NewRepo { owner_id: org.id, ..new("tools") };
    let tools = state.repos.create(&a, tools, &c()).await.unwrap();
    sqlx::query("UPDATE repositories SET access_tier = 'internal' WHERE id = $1").bind(tools.id.0).execute(&pool).await.unwrap();

    World { app, state, _dir: dir, proj: proj.id, sha, blob, comment, release: release.0, asset }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Off,
    Missing,
    Private,
    Internal,
    Public,
}

async fn enter(w: &World, k: Kind) {
    let db = &w.state.db;
    let on = !matches!(k, Kind::Off);
    sqlx::query("UPDATE visibility_settings SET allow_public_repos = true").execute(db).await.unwrap();
    let tier = match k {
        Kind::Private | Kind::Missing => "private",
        Kind::Internal => "internal",
        // `off` starts from public, so turning the switch off is what lowers it.
        Kind::Public | Kind::Off => "public",
    };
    sqlx::query("UPDATE repositories SET access_tier = $2, public_since = CASE WHEN $2 = 'public' THEN now() END WHERE id = $1")
        .bind(w.proj.0)
        .bind(tier)
        .execute(db)
        .await
        .unwrap();
    // The organization's repository follows the world too, so in the public world the org
    // routes have an owner to name and in the others the org looks like any missing name.
    sqlx::query("UPDATE repositories SET access_tier = $1, public_since = CASE WHEN $1 = 'public' THEN now() END WHERE name = 'tools'")
        .bind(if k == Kind::Public { "public" } else { "internal" })
        .execute(db)
        .await
        .unwrap();
    if !on {
        sqlx::query("UPDATE visibility_settings SET allow_public_repos = false").execute(db).await.unwrap();
    }
}

/// A concrete path for a registered pattern, from seeded values. Every parameter name used by
/// any route must be known here: an unknown one fails the test with the route.
fn concrete(pattern: &str, w: &World, owner: &str, repo: &str) -> String {
    let p = pattern;
    // The missing world names an organization that does not exist either.
    let org = if owner == "nobody" { "nobody" } else { "harbor" };
    pattern
        .split('/')
        .map(|s| -> String {
            match s {
                "{username}" => owner.into(),
                "{owner}" | "{login}" | "{user}" => {
                    if p.starts_with("/api/v3/orgs/") || p.starts_with("/orgs/") {
                        org.into()
                    } else {
                        owner.into()
                    }
                }
                "{org}" => org.into(),
                "{repo}" => repo.into(),
                "{*rest}" => {
                    if p.contains("/archive/") {
                        "main.zip".into()
                    } else if p.contains("/commits/") || p.contains("/find/") || p.contains("/tree/") || p.contains("/branches/") {
                        "main".into()
                    } else {
                        "main/README.md".into()
                    }
                }
                "{*spec}" | "{basehead}" => "main...feature".into(),
                "{*path}" => {
                    if p.starts_with("/static/") {
                        "app.css".into()
                    } else {
                        "README.md".into()
                    }
                }
                "{*prefix}" | "{*name}" => "heads/main".into(),
                "{*file}" => "workspace.js".into(),
                "{*tag}" | "{tag}" => "v1".into(),
                "{sha}" => {
                    if p.ends_with("/git/blobs/{sha}") {
                        w.blob.clone()
                    } else {
                        w.sha.clone()
                    }
                }
                "{ref}" | "{reference}" => "main".into(),
                "{number}" => {
                    if p.contains("/pulls/") {
                        "2".into()
                    } else {
                        "1".into()
                    }
                }
                "{id}" => {
                    if p.contains("/issues/comments/") {
                        w.comment.to_string()
                    } else if p.contains("/releases/assets/") {
                        w.asset.to_string()
                    } else if p.contains("/releases/") {
                        w.release.to_string()
                    } else {
                        "1".into()
                    }
                }
                "{name}" => {
                    if p.contains("/labels/") {
                        "bug".into()
                    } else if p.contains("/releases/download/") {
                        "notes.txt".into()
                    } else if p.starts_with("/avatar/") {
                        "alice".into()
                    } else {
                        "x".into()
                    }
                }
                "{run}" | "{job}" | "{rid}" | "{delivery}" | "{thread}" | "{workflow}" | "{token}" | "{provider}" | "{user_id}"
                | "{repo_id}" | "{slug}" | "{team}" | "{key}" | "{run_id}" | "{jid}" | "{wid}" | "{did}" | "{digest}" | "{kind}"
                | "{n}" | "{layer}" | "{issue_number}" | "{build}" => "1".into(),
                other if other.starts_with('{') => panic!("route {pattern}: no fixture for parameter {other}"),
                other => other.into(),
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Pulls every registered path out of axum's `Debug` output for the router, as the surface test
/// next door does.
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

const METHODS: &[Method] = &[Method::GET, Method::HEAD, Method::POST, Method::PUT, Method::PATCH, Method::DELETE];

struct Reply {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Vec<u8>,
}

static PEER: AtomicU32 = AtomicU32::new(0);

async fn send(app: &Router, method: &Method, uri: &str) -> Reply {
    // Each request from its own documentation address, so the per-address caps never decide
    // what the sweep sees.
    let n = PEER.fetch_add(1, Ordering::Relaxed);
    let peer: SocketAddr = format!("198.51.{}.{}:40000", (n / 250) % 250, n % 250 + 1).parse().unwrap();
    let mut req = Request::builder().method(method.clone()).uri(uri).body(Body::empty()).unwrap();
    req.extensions_mut().insert(ConnectInfo(peer));
    let resp = app.clone().oneshot(req).await.unwrap();
    let (parts, body) = resp.into_parts();
    Reply { status: parts.status, headers: parts.headers, body: body.collect().await.unwrap().to_bytes().to_vec() }
}

/// The request a pattern and method stand for: info/refs carries the one service it is open for.
fn uri_of(path: &str) -> String {
    if path.ends_with("/info/refs") { format!("{path}?service=git-upload-pack") } else { path.to_string() }
}

/// True when the answer is exactly the rejection the gate gives for this kind of path.
fn is_rejection(w: &World, method: &Method, path: &str, r: &Reply) -> bool {
    let kind = access::classify_for(path, true);
    let q = path.ends_with("/info/refs").then_some("service=git-upload-pack");
    let want = codehaven::gate::reject(&w.state, kind, method, path, q);
    if kind == access::RouteKind::OAuth && r.status == StatusCode::METHOD_NOT_ALLOWED {
        return true;
    }
    r.status == want.status()
        && r.headers.get(header::LOCATION) == want.headers().get(header::LOCATION)
        && r.headers.get(header::WWW_AUTHENTICATE) == want.headers().get(header::WWW_AUTHENTICATE)
        // A HEAD answer carries no body.
        && (r.status == StatusCode::SEE_OTHER || *method == Method::HEAD || !r.body.is_empty())
}

struct Sweep {
    /// `METHOD pattern` for every request that was not rejected.
    admitted: BTreeSet<String>,
    /// What each admitted request answered, for the debug file and the checks below.
    answers: BTreeMap<String, (StatusCode, Vec<u8>)>,
    /// Every rejection, by `METHOD pattern`, for comparing worlds.
    rejected: BTreeMap<String, (StatusCode, Vec<u8>, Option<String>, Option<String>)>,
}

async fn sweep(w: &World, paths: &[String], owner: &str, repo: &str) -> Sweep {
    let mut s = Sweep { admitted: BTreeSet::new(), answers: BTreeMap::new(), rejected: BTreeMap::new() };
    for pattern in paths {
        let path = concrete(pattern, w, owner, repo);
        for m in METHODS {
            let r = send(&w.app, m, &uri_of(&path)).await;
            let key = format!("{m} {pattern}");
            if is_rejection(w, m, &path, &r) {
                let org = if owner == "nobody" { "nobody" } else { "harbor" };
                let loc = r.headers.get(header::LOCATION).map(|v| {
                    v.to_str()
                        .unwrap()
                        .replace(&format!("/orgs/{org}"), "/orgs/ORG")
                        .replace(&format!("/{owner}"), "/OWNER")
                        .replace(&format!("/{repo}"), "/REPO")
                });
                let www = r.headers.get(header::WWW_AUTHENTICATE).map(|v| v.to_str().unwrap().to_string());
                s.rejected.insert(key, (r.status, r.body, loc, www));
            } else {
                s.answers.insert(key.clone(), (r.status, r.body));
                s.admitted.insert(key);
            }
        }
    }
    s
}

fn committed(name: &str) -> String {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/access").join(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn write_target(name: &str, s: &Sweep) -> String {
    let text: String = s.admitted.iter().map(|l| format!("{l}\n")).collect();
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("access");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(name), &text).unwrap();
    let detail: String = s.answers.iter().map(|(k, (st, _))| format!("{k} -> {}\n", st.as_u16())).collect();
    std::fs::write(dir.join(format!("{name}.status")), detail).unwrap();
    text
}

/// What the route table says a world adds to the baseline. HEAD follows GET except on git and
/// archive routes. Lines use the registered pattern, so names match the router's.
fn expansion(paths: &[String], only: &[PublicTarget]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for rule in PUBLIC_REPO_ROUTES.iter().filter(|r| only.contains(&r.target)) {
        let registered = paths.iter().find(|p| access::same_route(rule, p)).unwrap_or_else(|| panic!("{} is not registered", rule.pattern));
        for m in rule.methods {
            out.insert(format!("{m} {registered}"));
            if *m == "GET" && !matches!(rule.cost, Cost::Git | Cost::Archive) {
                out.insert(format!("HEAD {registered}"));
            }
        }
    }
    out
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_signed_out_surface_per_tier_is_exactly_the_table(pool: PgPool) {
    let w = seed(pool).await;
    let paths = registered_paths(&wiring::routes(&w.state.config));
    assert!(paths.len() > 200, "the router listing found only {} paths", paths.len());

    enter(&w, Kind::Off).await;
    let off = sweep(&w, &paths, "alice", "proj").await;
    let off_text = write_target("anonymous.off.txt", &off);
    enter(&w, Kind::Missing).await;
    let missing = sweep(&w, &paths, "nobody", "nope").await;
    let missing_text = write_target("anonymous.missing.txt", &missing);
    enter(&w, Kind::Private).await;
    let private = sweep(&w, &paths, "alice", "proj").await;
    let private_text = write_target("anonymous.private.txt", &private);
    enter(&w, Kind::Internal).await;
    let internal = sweep(&w, &paths, "alice", "proj").await;
    let internal_text = write_target("anonymous.internal.txt", &internal);
    enter(&w, Kind::Public).await;
    let public = sweep(&w, &paths, "alice", "proj").await;
    let public_text = write_target("anonymous.public.txt", &public);

    // The table's own expansion.
    let discovery = expansion(&paths, &[PublicTarget::Discovery]);
    let owners = expansion(&paths, &[PublicTarget::Owner]);
    let repos = expansion(&paths, &[PublicTarget::Repo]);
    let leaked: Vec<&String> =
        off.admitted.iter().filter(|k| discovery.contains(*k) || repos.contains(*k) || owners.contains(*k)).collect();
    assert!(leaked.is_empty(), "nothing public while the switch is off: {leaked:?}");
    let diff = |got: &BTreeSet<String>, want: &BTreeSet<String>| {
        (got.difference(want).cloned().collect::<Vec<_>>(), want.difference(got).cloned().collect::<Vec<_>>())
    };
    let want_missing: BTreeSet<String> = off.admitted.union(&discovery).cloned().collect();
    assert_eq!(diff(&missing.admitted, &want_missing), (vec![], vec![]), "missing names add discovery and nothing else (extra, absent)");
    let want_private = want_missing.clone();
    assert_eq!(diff(&private.admitted, &want_private), (vec![], vec![]), "private adds nothing for its own routes (extra, absent)");
    assert_eq!(diff(&internal.admitted, &private.admitted), (vec![], vec![]), "internal looks exactly like private signed out");
    let want_public: BTreeSet<String> = want_private.union(&repos).chain(owners.iter()).cloned().collect();
    assert_eq!(diff(&public.admitted, &want_public), (vec![], vec![]), "public adds exactly the table (extra, absent)");

    // Private, internal and missing refuse with the same answer, route by route.
    for (k, v) in &private.rejected {
        assert_eq!(Some(v), missing.rejected.get(k), "{k}: private and missing differ");
        assert_eq!(Some(v), internal.rejected.get(k), "{k}: private and internal differ");
    }

    // The committed files, which only a person changes.
    assert_eq!(off_text, committed("anonymous.off.txt"), "see target/tmp/access/anonymous.off.txt");
    assert_eq!(missing_text, committed("anonymous.missing.txt"));
    assert_eq!(private_text, committed("anonymous.private.txt"));
    assert_eq!(internal_text, committed("anonymous.internal.txt"));
    assert_eq!(public_text, committed("anonymous.public.txt"));

    // Every admitted public read works on its fixture, and nothing planted leaks.
    let mut forms = Vec::new();
    for (k, (status, body)) in &public.answers {
        assert!(!status.is_server_error(), "{k} answered {status}");
        let text = String::from_utf8_lossy(body);
        for p in PLANTED {
            assert!(!text.contains(p), "{k} shows {p}");
        }
        // A signed-out page of the public list renders no form that changes anything and no
        // CSRF token. The sign-in pages of the baseline have their own forms.
        let lower = text.to_ascii_lowercase();
        let has_form = lower.contains("method=\"post\"") || lower.contains("name=\"_csrf\"") || lower.contains("name=\"csrf-token\"");
        if has_form && !off.admitted.contains(k) {
            forms.push(k.clone());
        }
    }
    assert!(forms.is_empty(), "signed-out pages with forms or a CSRF token: {forms:?}");
    let home = String::from_utf8_lossy(&public.answers["GET /{owner}/{repo}"].1).to_string();
    assert!(home.contains("You can read this repository. Sign in to open an issue, comment or send a pull request."), "the sign-in prompt");
    for k in [
        "GET /{owner}/{repo}",
        "GET /{owner}/{repo}/tree/{*rest}",
        "GET /{owner}/{repo}/commit/{sha}",
        "GET /{owner}/{repo}/blame/{*rest}",
        "GET /{owner}/{repo}/archive/{*rest}",
        "GET /{owner}/{repo}/releases/download/{tag}/{name}",
        "GET /api/v3/repos/{owner}/{repo}/contents/{*path}",
        "GET /api/v3/repos/{owner}/{repo}/zipball",
        "GET /{owner}/{repo}/blob/{*rest}",
        "GET /{owner}/{repo}/raw/{*rest}",
        "GET /{owner}/{repo}/commits",
        "GET /{owner}/{repo}/issues",
        "GET /{owner}/{repo}/issues/{number}",
        "GET /{owner}/{repo}/pulls",
        "GET /{owner}/{repo}/pulls/{number}",
        "GET /{owner}/{repo}/releases",
        "GET /{owner}/{repo}/info/refs",
        "GET /api/v3/repos/{owner}/{repo}",
        "GET /api/v3/repos/{owner}/{repo}/issues",
        "GET /explore",
        "GET /{owner}",
    ] {
        let (status, body) = public.answers.get(k).unwrap_or_else(|| panic!("{k} was not admitted"));
        let text = String::from_utf8_lossy(body);
        let main = text.find("<main").map(|i| &text[i..]).unwrap_or(&text);
        assert_eq!(*status, StatusCode::OK, "{k}: {}", &main[..main.len().min(800)]);
    }
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn every_public_answer_is_uncached_and_sets_no_cookie(pool: PgPool) {
    let w = seed(pool).await;
    enter(&w, Kind::Public).await;
    for p in ["/alice/proj", "/alice/proj/raw/main/README.md", "/api/v3/repos/alice/proj", "/explore", "/alice"] {
        let r = send(&w.app, &Method::GET, p).await;
        assert_eq!(r.status, StatusCode::OK, "{p}");
        assert_eq!(r.headers[header::CACHE_CONTROL], "private, no-store", "{p}");
        assert!(r.headers.get(header::SET_COOKIE).is_none(), "{p} set a cookie");
        assert!(r.headers.get("access-control-allow-origin").is_none(), "{p}");
        assert_eq!(r.headers["x-robots-tag"], "noindex, nofollow", "{p}");
    }
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn predicates_and_credentials_keep_the_door_shut(pool: PgPool) {
    let w = seed(pool).await;
    enter(&w, Kind::Public).await;
    let refused = |r: &Reply| r.status == StatusCode::UNAUTHORIZED || r.status == StatusCode::SEE_OTHER;
    // Push never opens, and info/refs only for exactly upload-pack.
    for uri in [
        "/alice/proj/info/refs?service=git-receive-pack",
        "/alice/proj/info/refs",
        "/alice/proj/info/refs?service=git-upload-pack&x=1",
        "/alice/proj.git/info/refs?service=git-receive-pack",
    ] {
        assert!(refused(&send(&w.app, &Method::GET, uri).await), "{uri}");
    }
    assert!(refused(&send(&w.app, &Method::POST, "/alice/proj/git-receive-pack").await));
    assert!(refused(&send(&w.app, &Method::GET, "/api/v3/repos/alice/proj/branches/main/protection").await));
    assert!(refused(&send(&w.app, &Method::HEAD, "/alice/proj/archive/main.zip").await), "HEAD stays closed on archives");
    // `.git` names the same public repository.
    assert_eq!(send(&w.app, &Method::GET, "/alice/proj.git/info/refs?service=git-upload-pack").await.status, StatusCode::OK);
    // A bad credential never turns into a public read.
    for auth in ["Bearer nope", "Basic YWxpY2U6d3Jvbmc=", "token x"] {
        let mut req = Request::builder().uri("/api/v3/repos/alice/proj").header(header::AUTHORIZATION, auth).body(Body::empty()).unwrap();
        req.extensions_mut().insert(ConnectInfo("198.51.100.1:4000".parse::<SocketAddr>().unwrap()));
        let r = w.app.clone().oneshot(req).await.unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "{auth}");
    }
    // Writes stay closed whatever the method.
    for (m, p) in [
        (Method::POST, "/api/v3/repos/alice/proj/issues"),
        (Method::POST, "/alice/proj/issues/1/comment"),
        (Method::PATCH, "/api/v3/repos/alice/proj"),
        (Method::DELETE, "/api/v3/repos/alice/proj"),
        (Method::PUT, "/api/v3/repos/alice/proj/contents/README.md"),
    ] {
        assert!(refused(&send(&w.app, &m, p).await), "{m} {p}");
    }
    // An old name follows only if it was recorded while public.
    sqlx::query("INSERT INTO repo_redirects (old_owner, old_name, repo_id) VALUES ('alice', 'renamed-while-public', $1)")
        .bind(w.proj.0)
        .execute(&w.state.db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO repo_redirects (old_owner, old_name, repo_id, audience) VALUES ('alice', 'secret-old-name', $1, 'private')")
        .bind(w.proj.0)
        .execute(&w.state.db)
        .await
        .unwrap();
    // The trigger records the audience from the repository, so set the private one by hand.
    sqlx::query("ALTER TABLE repo_redirects DISABLE TRIGGER repo_redirects_audience").execute(&w.state.db).await.unwrap();
    sqlx::query("UPDATE repo_redirects SET audience = 'private' WHERE old_name = 'secret-old-name'").execute(&w.state.db).await.unwrap();
    sqlx::query("ALTER TABLE repo_redirects ENABLE TRIGGER repo_redirects_audience").execute(&w.state.db).await.unwrap();
    assert_eq!(send(&w.app, &Method::GET, "/api/v3/repos/alice/renamed-while-public").await.status, StatusCode::OK);
    let old = send(&w.app, &Method::GET, "/api/v3/repos/alice/secret-old-name").await;
    let missing = send(&w.app, &Method::GET, "/api/v3/repos/alice/never-existed").await;
    assert_eq!((old.status, old.body), (missing.status, missing.body), "an old private name is the same as no name");
}
