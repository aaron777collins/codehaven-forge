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

mod common;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode, header};
use ch_core::Actor;
use ch_core::access::{self, Cost, PUBLIC_REPO_ROUTES, PublicTarget};
use codehaven::wiring;
use common::surface::*;
use http_body_util::BodyExt;
use sqlx::PgPool;
use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use tower::ServiceExt;

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
    let repos = expansion(&paths, &[PublicTarget::Repo, PublicTarget::Image]);
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
        "GET /{owner}/{repo}/actions",
        "GET /{owner}/{repo}/actions/runs/{run}",
        "GET /{owner}/{repo}/actions/runs/{run}/jobs/{job}",
        "GET /{owner}/{repo}/pulls/{number}/checks",
        "GET /api/v3/repos/{owner}/{repo}/actions/runs",
        "GET /api/v3/repos/{owner}/{repo}/actions/runs/{id}",
        "GET /api/v3/repos/{owner}/{repo}/actions/runs/{id}/jobs",
        "GET /api/v3/repos/{owner}/{repo}/commits/{reference}/check-runs",
        "GET /api/v3/repos/{owner}/{repo}/commits/{reference}/status",
        "GET /api/v3/repos/{owner}/{repo}/check-runs/{id}",
        "GET /api/v3/repos/{owner}/{repo}/actions/artifacts",
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

/// ch-4pn.12: signed out, a public repository's Actions history shows what was recorded public
/// and nothing recorded while it was private. A hidden run, job or log answers exactly as a run
/// that never existed, and the combined state counts only what shows.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn signed_out_actions_history_shows_only_what_was_recorded_public(pool: PgPool) {
    let w = seed(pool).await;
    enter(&w, Kind::Public).await;
    let json = |r: &Reply| serde_json::from_slice::<serde_json::Value>(&r.body).unwrap();

    let runs = send(&w.app, &Method::GET, "/api/v3/repos/alice/proj/actions/runs").await;
    assert_eq!(runs.status, StatusCode::OK);
    let v = json(&runs);
    assert_eq!(v["total_count"], 1, "{v}");
    assert_eq!(v["workflow_runs"][0]["id"], w.run);

    let missing = send(&w.app, &Method::GET, "/api/v3/repos/alice/proj/actions/runs/987654321").await;
    for p in [
        format!("/api/v3/repos/alice/proj/actions/runs/{}", w.hidden_run),
        format!("/api/v3/repos/alice/proj/actions/runs/{}/jobs", w.hidden_run),
        format!("/api/v3/repos/alice/proj/actions/runs/{}/artifacts", w.hidden_run),
    ] {
        let r = send(&w.app, &Method::GET, &p).await;
        assert_eq!((r.status, &r.body), (missing.status, &missing.body), "{p} differs from a run that never existed");
    }
    let missing_job = send(&w.app, &Method::GET, "/api/v3/repos/alice/proj/actions/jobs/987654321").await;
    for p in [
        format!("/api/v3/repos/alice/proj/actions/jobs/{}", w.hidden_job),
        format!("/api/v3/repos/alice/proj/actions/jobs/{}/logs", w.hidden_job),
    ] {
        let r = send(&w.app, &Method::GET, &p).await;
        assert_eq!(r.status, missing_job.status, "{p}");
    }
    for p in [
        format!("/alice/proj/actions/runs/{}", w.hidden_run),
        format!("/alice/proj/actions/runs/{}/jobs/{}", w.hidden_run, w.hidden_job),
        format!("/alice/proj/actions/runs/{}/jobs/{}/log.txt", w.hidden_run, w.hidden_job),
    ] {
        let r = send(&w.app, &Method::GET, &p).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{p}");
        assert!(!String::from_utf8_lossy(&r.body).contains("PLANTED"), "{p}");
    }

    let checks = json(&send(&w.app, &Method::GET, "/api/v3/repos/alice/proj/commits/main/check-runs").await);
    assert_eq!(checks["total_count"], 1, "{checks}");
    assert_eq!(checks["check_runs"][0]["id"], w.check);
    let status = json(&send(&w.app, &Method::GET, "/api/v3/repos/alice/proj/commits/main/status").await);
    assert_eq!(status["statuses"].as_array().unwrap().len(), 1, "{status}");
    let statuses = json(&send(&w.app, &Method::GET, "/api/v3/repos/alice/proj/commits/main/statuses").await);
    assert_eq!(statuses.as_array().unwrap().len(), 1, "{statuses}");
    let arts = json(&send(&w.app, &Method::GET, "/api/v3/repos/alice/proj/actions/artifacts").await);
    assert_eq!(arts["total_count"], 1, "{arts}");

    let page = send(&w.app, &Method::GET, "/alice/proj/actions").await;
    let text = String::from_utf8_lossy(&page.body);
    assert!(text.contains("public-RUN title"), "the public run is listed");
    assert!(!text.contains("PLANTED"), "the private run is not");

    // Listing workflows signed out writes nothing.
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM workflows").fetch_one(&w.state.db).await.unwrap();
    assert_eq!(send(&w.app, &Method::GET, "/api/v3/repos/alice/proj/actions/workflows").await.status, StatusCode::OK);
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM workflows").fetch_one(&w.state.db).await.unwrap();
    assert_eq!(before, after);

    // The closed routes stay closed: the run log zip, artifact downloads, variables, caches.
    for p in [
        format!("/api/v3/repos/alice/proj/actions/runs/{}/logs", w.run),
        format!("/api/v3/repos/alice/proj/actions/artifacts/{}/zip", w.artifact),
        "/api/v3/repos/alice/proj/actions/variables".to_string(),
        "/api/v3/repos/alice/proj/actions/caches".to_string(),
        "/api/v3/repos/alice/proj/actions/secrets".to_string(),
        "/api/v3/repos/alice/proj/actions/runners".to_string(),
    ] {
        assert_eq!(send(&w.app, &Method::GET, &p).await.status, StatusCode::UNAUTHORIZED, "{p}");
    }
}

async fn send_auth(app: &Router, method: &Method, uri: &str, auth: Option<&str>) -> Reply {
    let mut b = Request::builder().method(method.clone()).uri(uri);
    if let Some(a) = auth {
        b = b.header(header::AUTHORIZATION, a);
    }
    let mut req = b.body(Body::empty()).unwrap();
    req.extensions_mut().insert(ConnectInfo("198.51.100.77:4000".parse::<SocketAddr>().unwrap()));
    let resp = app.clone().oneshot(req).await.unwrap();
    let (parts, body) = resp.into_parts();
    Reply { status: parts.status, headers: parts.headers, body: body.collect().await.unwrap().to_bytes().to_vec() }
}

/// ch-4pn.13: a signed-out `docker pull` of an image linked to a public repository works with
/// the literal token `anonymous`; an image with no repository, a private one and a missing one
/// all get the same challenge; the catalog lists only what a stranger may pull; nothing writes.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn anonymous_registry_pulls_follow_the_repository(pool: PgPool) {
    let w = seed(pool).await;
    enter(&w, Kind::Public).await;
    let anon = Some("Bearer anonymous");

    // `/v2/` still challenges, so `docker login` works, and the token is the literal one.
    let root = send_auth(&w.app, &Method::GET, "/v2/", None).await;
    assert_eq!(root.status, StatusCode::UNAUTHORIZED);
    assert!(root.headers[header::WWW_AUTHENTICATE].to_str().unwrap().contains("/v2/token"));
    let t = send_auth(&w.app, &Method::GET, "/v2/token?service=x&scope=repository:alice/hidden-proj:pull,push", None).await;
    assert_eq!(t.status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&t.body).unwrap();
    assert_eq!(v["token"], "anonymous");

    let tags = send_auth(&w.app, &Method::GET, "/v2/alice/proj/tags/list", anon).await;
    assert_eq!(tags.status, StatusCode::OK, "{}", String::from_utf8_lossy(&tags.body));
    let unlinked = send_auth(&w.app, &Method::GET, "/v2/alice/hidden-proj/tags/list", anon).await;
    let missing = send_auth(&w.app, &Method::GET, "/v2/nobody/nothing/tags/list", anon).await;
    assert_eq!(unlinked.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        (unlinked.status, &unlinked.body, unlinked.headers.get(header::WWW_AUTHENTICATE)),
        (missing.status, &missing.body, missing.headers.get(header::WWW_AUTHENTICATE))
    );

    let cat = send_auth(&w.app, &Method::GET, "/v2/_catalog", anon).await;
    assert_eq!(cat.status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&cat.body).unwrap();
    assert_eq!(v["repositories"], serde_json::json!(["alice/proj"]), "only the linked public image");

    // Never a push, whatever the method.
    for (m, p) in [
        (Method::POST, "/v2/alice/proj/blobs/uploads/"),
        (Method::PUT, "/v2/alice/proj/manifests/latest"),
        (Method::DELETE, "/v2/alice/proj/manifests/latest"),
    ] {
        assert_eq!(send_auth(&w.app, &m, p, anon).await.status, StatusCode::UNAUTHORIZED, "{m} {p}");
    }
    // The literal token means nothing anywhere else.
    assert_eq!(send_auth(&w.app, &Method::GET, "/api/v3/repos/alice/proj", anon).await.status, StatusCode::UNAUTHORIZED);

    // Private looks exactly like missing.
    enter(&w, Kind::Private).await;
    let private = send_auth(&w.app, &Method::GET, "/v2/alice/proj/tags/list", anon).await;
    assert_eq!((private.status, &private.body), (missing.status, &missing.body));
    let cat = send_auth(&w.app, &Method::GET, "/v2/_catalog", anon).await;
    let v: serde_json::Value = serde_json::from_slice(&cat.body).unwrap();
    assert_eq!(v["repositories"], serde_json::json!([]));
    // With the switch off the token route is closed too.
    enter(&w, Kind::Off).await;
    assert_eq!(send_auth(&w.app, &Method::GET, "/v2/token", None).await.status, StatusCode::UNAUTHORIZED);
}

/// A client-chosen `skip` used to be an offset over rows the caller cannot see, so two values of
/// `last` around a hidden name answered differently and the difference said how many hidden names
/// sit between them. Signed out, `skip` is refused unless the server issued it, and hidden rows
/// are never read at all.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_signed_out_catalog_cannot_count_hidden_names(pool: PgPool) {
    let w = seed(pool).await;
    enter(&w, Kind::Public).await;
    let anon = Some("Bearer anonymous");

    // The hidden name sorts between "alice/h" and "alice/i", so a skip that reaches past it makes
    // the two `last` values disagree about what is on the page.
    for skip in ["1", "2"] {
        let before = send_auth(&w.app, &Method::GET, &format!("/v2/_catalog?n=1&last=alice/h&skip={skip}"), anon).await;
        let after = send_auth(&w.app, &Method::GET, &format!("/v2/_catalog?n=1&last=alice/i&skip={skip}"), anon).await;
        assert_eq!((before.status, &before.body), (after.status, &after.body), "last values that bracket a hidden name must answer alike");
        assert_eq!(before.status, StatusCode::BAD_REQUEST, "a signed-out caller may not choose a skip");
    }

    // Without skip both sides of the hidden name show the same one readable image, with no next
    // page to follow.
    for last in ["alice/h", "alice/i"] {
        let p = send_auth(&w.app, &Method::GET, &format!("/v2/_catalog?n=1&last={last}"), anon).await;
        assert_eq!(p.status, StatusCode::OK, "{}", String::from_utf8_lossy(&p.body));
        let v: serde_json::Value = serde_json::from_slice(&p.body).unwrap();
        assert_eq!(v["repositories"], serde_json::json!(["alice/proj"]));
        assert!(!p.headers.contains_key(header::LINK));
    }

    // Hidden rows are never even read signed out, so they cannot cost the scan budget either. With
    // a budget of two rows, reading alice/hidden-proj first would spend it and stop at alice/proj
    // with a next link; reading only alice/proj leaves room to see that nothing follows.
    let (names, next) = ch_registry::catalog::page(
        &w.state,
        &Actor::Anonymous,
        10,
        &ch_registry::catalog::Cursor::default(),
        ch_registry::catalog::Limits { batch: 1, max_scan: 2 },
    )
    .await
    .unwrap();
    assert_eq!((names, next), (vec!["alice/proj".to_string()], None));
}

/// ch-g7n: every `docker push` and `docker pull` starts with requests that are challenged
/// before the client signs in. Those challenges look nothing up, so they never use up the
/// signed-out budget; a page that is refused still does.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn registry_challenges_do_not_use_up_the_signed_out_budget(pool: PgPool) {
    let w = seed(pool).await;
    enter(&w, Kind::Private).await;
    for _ in 0..400 {
        let r = send_auth(&w.app, &Method::POST, "/v2/alice/proj/blobs/uploads/", None).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "a challenge, never a 429");
    }
    let mut limited = false;
    for _ in 0..400 {
        let mut req = Request::builder().uri("/alice/proj").body(Body::empty()).unwrap();
        req.extensions_mut().insert(ConnectInfo("198.51.100.78:4000".parse::<SocketAddr>().unwrap()));
        if w.app.clone().oneshot(req).await.unwrap().status() == StatusCode::TOO_MANY_REQUESTS {
            limited = true;
            break;
        }
    }
    assert!(limited, "refused pages still count against the budget");
}

/// ch-4pn.17 (A20), through the real router and gate: a flood of signed-out archive requests from
/// many addresses is held to the measured caps, and nothing answers 5xx. While the instance's own
/// rate has room (600 a minute for all addresses, 18 requests from each here), each address gets
/// exactly the archives a minute the table says (`public_gate.rs`, module `flood`, pins the same
/// numbers on the limiter alone) and the rest are told to come back. Once the instance rate is
/// used up, every further address is refused, whatever its own share.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_flood_of_signed_out_archive_requests_is_held_to_the_caps(pool: PgPool) {
    let w = seed(pool).await;
    enter(&w, Kind::Public).await;
    let cap = w.state.config.public_access.archive_per_address_per_minute as usize;
    let instance = w.state.config.public_access.instance_requests_per_minute as usize;
    assert_eq!((cap, instance), (6, 600), "the shipped starting values");
    let asks = cap * 3;
    // One address asks `asks` times; every download is read to its end, so nothing stays in flight.
    let flood = |addr: u32| {
        let app = w.app.clone();
        async move {
            let peer: SocketAddr = format!("203.0.{}.{}:5000", addr / 250, addr % 250 + 1).parse().unwrap();
            let (mut ok, mut limited) = (0usize, 0usize);
            for _ in 0..asks {
                let mut req = Request::builder().uri("/alice/proj/archive/main.zip").body(Body::empty()).unwrap();
                req.extensions_mut().insert(ConnectInfo(peer));
                let r = app.clone().oneshot(req).await.unwrap();
                let (status, retry) = (r.status(), r.headers().get(header::RETRY_AFTER).cloned());
                let _ = r.into_body().collect().await;
                match status {
                    StatusCode::OK => ok += 1,
                    StatusCode::TOO_MANY_REQUESTS => {
                        assert!(retry.is_some(), "a refusal says when to come back");
                        limited += 1;
                    }
                    other => panic!("address {addr}: {other}"),
                }
            }
            (ok, limited)
        }
    };
    let rooms = instance / asks;
    for addr in 0..rooms as u32 - 2 {
        assert_eq!(flood(addr).await, (cap, asks - cap), "address {addr} gets {cap} archives a minute and the rest are refused");
    }
    // Past the instance's own rate, no address gets anything.
    let mut last = (0, 0);
    for addr in 0..rooms as u32 + 6 {
        last = flood(1000 + addr).await;
    }
    assert_eq!(last, (0, asks), "the instance rate is used up, so a new address is refused too");
}
