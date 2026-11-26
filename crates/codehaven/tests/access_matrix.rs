//! The access matrix (ch-4pn.17.1, A5): what every kind of credential reaches on every route, on a
//! repository at each of the three tiers, with the owner's contribution limit on and off.
//!
//! The signed-out column is `public_surface.rs`. This is everything else. A real database and the
//! real services, the world of `common/surface.rs`, and a request for every route the router has
//! registered with every method that writes or reads (GET, POST, PUT, PATCH, DELETE; HEAD follows
//! GET), once per credential, per tier, per contribution setting. Sixty tests, one per cell
//! group, each with a fresh world, so what one credential writes never reaches another.
//!
//! The principals have no grant on the repository, because that is where the tier decides:
//!
//! | credential  | what is sent                                                              |
//! |-------------|---------------------------------------------------------------------------|
//! | none        | nothing                                                                   |
//! | session     | carol's session cookie, with the Origin and CSRF token a browser sends    |
//! | basic       | carol's name and password                                                 |
//! | pat_read    | a personal access token of carol's with `repo:read`                       |
//! | pat_write   | a personal access token of carol's with every scope she may hold          |
//! | job_read    | the token of a running job of this repository, `contents: read`           |
//! | job_write   | the same with `contents: write`                                           |
//! | job_other   | the token of a running job of another private repository, with write     |
//! | runner      | a registered runner's credential                                          |
//! | app         | a connected app's access token for dave (two-factor on, read and write)   |
//!
//! Each answer is one of four things: refused (401, 403, 404, 405, or the redirect to sign in),
//! answered but not valid (the handler read the body and said no, which proves little about who
//! may; the routes that matter most are sent a valid body), answered and left every row and ref
//! alone, or answered and changed a row or a ref (the fingerprint below). The committed table says which, per route and credential and cell, and
//! the test fails on any difference in either direction. A route the router registers that is
//! not in `tests/access/matrix.routes.txt` fails too, so a new route cannot arrive unreviewed.
//!
//! Besides the table, rules that hold whatever the table says:
//! - a request never answers 5xx or 429;
//! - on a private repository, an account or job with no right to it gets, route by route, the
//!   answer for a repository that does not exist, and changes nothing;
//! - a GET never changes a row or a ref;
//! - with the contribution limit on, an account with no grant opens no issue, comment or pull
//!   request and reviews nothing;
//! - a read-only token and no credential at all never change anything.
//!
//! Regenerating the table (only a person does this, on purpose, and reads the diff):
//! `CH_MATRIX_OUT=/work/tests/access/.gen cargo nextest run -p codehaven --test access_matrix`, then
//! `python3 tests/access/merge_matrix.py tests/access/.gen`.

#![allow(clippy::unwrap_used, clippy::type_complexity)]

mod common;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode, header};
use ch_core::model::*;
use ch_core::{Actor, AuthVia, RepoId, UserId};
use codehaven::wiring;
use common::surface::*;
use http_body_util::BodyExt;
use sqlx::PgPool;
use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use tower::ServiceExt;

const PW: &str = "correct horse battery staple 42";
const ORIGIN: &str = "https://code.example.com";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Cred {
    None,
    Session,
    Basic,
    PatRead,
    PatWrite,
    JobRead,
    JobWrite,
    JobOther,
    Runner,
    App,
}

impl Cred {
    fn name(self) -> &'static str {
        match self {
            Cred::None => "none",
            Cred::Session => "session",
            Cred::Basic => "basic",
            Cred::PatRead => "pat_read",
            Cred::PatWrite => "pat_write",
            Cred::JobRead => "job_read",
            Cred::JobWrite => "job_write",
            Cred::JobOther => "job_other",
            Cred::Runner => "runner",
            Cred::App => "app",
        }
    }
    /// Has no right to the repository the sweep names, whatever its tier. Its answers on a
    /// private repository must be the answers for one that does not exist.
    fn without_rights(self) -> bool {
        !matches!(self, Cred::None | Cred::JobRead | Cred::JobWrite)
    }
    /// Never changes anything, wherever it is sent.
    fn read_only(self) -> bool {
        matches!(self, Cred::None | Cred::PatRead)
    }
    /// An account with no grant that could open an issue where the owner allows it.
    fn contributor(self) -> bool {
        matches!(self, Cred::Session | Cred::Basic | Cred::PatWrite)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tier {
    Private,
    Internal,
    Public,
}

impl Tier {
    fn name(self) -> &'static str {
        match self {
            Tier::Private => "private",
            Tier::Internal => "internal",
            Tier::Public => "public",
        }
    }
}

/// The columns of the committed table, in order.
const CELLS: [(Tier, bool); 6] = [
    (Tier::Private, false),
    (Tier::Private, true),
    (Tier::Internal, false),
    (Tier::Internal, true),
    (Tier::Public, false),
    (Tier::Public, true),
];

fn cell_index(tier: Tier, limited: bool) -> usize {
    CELLS.iter().position(|c| *c == (tier, limited)).unwrap()
}

// ------------------------------------------------------------------ the credentials

struct Auth {
    cred: Cred,
    headers: Vec<(String, String)>,
    /// Sent as `X-CSRF-Token` with unsafe methods, with `Origin`: a browser session only.
    csrf: Option<String>,
    /// What the gate reads, to see whether a request used it up.
    secret: String,
}

async fn db_id(w: &World, sql: &str, name: &str) -> i64 {
    sqlx::query_scalar(sql).bind(name).fetch_one(&w.state.db).await.unwrap()
}

/// A running job of `repo` whose token carries `perms`; returns the token.
async fn job_token(w: &World, repo: i64, tag: &str, perms: serde_json::Value) -> String {
    let wf: i64 = sqlx::query_scalar("INSERT INTO workflows (repo_id, path, name) VALUES ($1, $2, 'matrix') RETURNING id")
        .bind(repo)
        .bind(format!(".codehaven/workflows/matrix-{tag}.yml"))
        .fetch_one(&w.state.db)
        .await
        .unwrap();
    let run: i64 = sqlx::query_scalar(
        "INSERT INTO workflow_runs (repo_id, workflow_id, run_number, event, head_sha, head_branch, title, status, recorded_tier)
         VALUES ($1, $2, 90, 'push', $3, 'main', 'matrix', 'in_progress', 'private') RETURNING id",
    )
    .bind(repo)
    .bind(wf)
    .bind(&w.sha)
    .fetch_one(&w.state.db)
    .await
    .unwrap();
    let token = ch_db::tokens::new_token(ch_db::ci_auth::JOB_TOKEN_PREFIX, 40);
    sqlx::query(
        "INSERT INTO workflow_jobs (run_id, repo_id, job_key, name, status, token_hash, token_permissions)
         VALUES ($1, $2, 'build', 'matrix', 'in_progress', $3, $4)",
    )
    .bind(run)
    .bind(repo)
    .bind(ch_db::tokens::hash_token(&token))
    .bind(perms)
    .execute(&w.state.db)
    .await
    .unwrap();
    token
}

/// Makes the credential, and its account when it needs one. Calling it again makes a new one,
/// which is how a request that ended the old one (a sign-out, a revoked token) is recovered from.
async fn make_auth(w: &World, cred: Cred) -> Auth {
    let state = &w.state;
    let bearer = |secret: String| Auth { cred, headers: vec![("authorization".into(), format!("Bearer {secret}"))], csrf: None, secret };
    let carol: UserId =
        match sqlx::query_scalar::<_, i64>("SELECT id FROM users WHERE username = 'carol'").fetch_optional(&state.db).await.unwrap() {
            Some(id) => UserId(id),
            None => {
                let id = user(state, "carol", "carol@example.org").await;
                state.accounts.set_password(&Actor::System, id, PW, &c()).await.unwrap();
                id
            }
        };
    let as_carol = Actor::User { id: carol, is_admin: false, has_2fa: false, via: AuthVia::Session };
    match cred {
        Cred::None => Auth { cred, headers: vec![], csrf: None, secret: String::new() },
        Cred::Session => {
            let s = state.credentials.sign_in("carol", PW, &c()).await.unwrap();
            let cookie = s.grant.cookie;
            let name = ch_core::access::session_cookie_name(state.config.is_https());
            let csrf = ch_auth::Guard::new(&state.config).token_for_session(&cookie);
            Auth { cred, headers: vec![("cookie".into(), format!("{name}={cookie}"))], csrf: Some(csrf), secret: cookie }
        }
        Cred::Basic => {
            use base64::Engine;
            let b = base64::engine::general_purpose::STANDARD.encode(format!("carol:{PW}"));
            Auth { cred, headers: vec![("authorization".into(), format!("Basic {b}"))], csrf: None, secret: String::new() }
        }
        Cred::PatRead => {
            let (_, secret) =
                state.accounts.create_token(&as_carol, carol, "matrix-read", vec!["repo:read".into()], 30, &c()).await.unwrap();
            bearer(secret)
        }
        Cred::PatWrite => {
            let scopes = ["repo", "workflow", "write:packages", "read:packages", "admin:repo_hook", "user", "notifications", "read:org"];
            let (_, secret) = state
                .accounts
                .create_token(&as_carol, carol, "matrix-write", scopes.iter().map(|s| s.to_string()).collect(), 30, &c())
                .await
                .unwrap();
            bearer(secret)
        }
        Cred::JobRead => bearer(job_token(w, w.proj.0, "read", serde_json::json!({"contents": "read"})).await),
        Cred::JobWrite => bearer(job_token(w, w.proj.0, "write", serde_json::json!({"contents": "write"})).await),
        Cred::JobOther => {
            let other = db_id(w, "SELECT id FROM repositories WHERE name = $1", "hidden-proj").await;
            bearer(job_token(w, other, "other", serde_json::json!({"contents": "write"})).await)
        }
        Cred::Runner => {
            let secret = ch_db::tokens::new_token(ch_db::ci_auth::RUNNER_CREDENTIAL_PREFIX, 40);
            sqlx::query("INSERT INTO runners (name, labels, credential_hash, version, os, arch, last_seen_at) VALUES ('matrix', '{}', $1, '1', 'linux', 'x64', now())")
                .bind(ch_db::tokens::hash_token(&secret))
                .execute(&state.db)
                .await
                .unwrap();
            bearer(secret)
        }
        Cred::App => {
            let dave =
                match sqlx::query_scalar::<_, i64>("SELECT id FROM users WHERE username = 'dave'").fetch_optional(&state.db).await.unwrap()
                {
                    Some(id) => id,
                    None => {
                        let id = user(state, "dave", "dave@example.org").await;
                        sqlx::query("UPDATE users SET totp_enabled_at = now() WHERE id = $1").bind(id.0).execute(&state.db).await.unwrap();
                        id.0
                    }
                };
            let grant: i64 = sqlx::query_scalar(
                "INSERT INTO oauth_grants (user_id, client_id, client_host, client_name, redirect_uri, resource, scopes, read_all, write_all, activated_at, expires_at)
                 VALUES ($1, 'matrix', 'matrix.example', 'Matrix', 'https://matrix.example/cb', $2, ARRAY['read','write'], true, true, now(), now() + interval '1 day') RETURNING id",
            )
            .bind(dave)
            .bind(state.config.mcp_resource())
            .fetch_one(&state.db)
            .await
            .unwrap();
            let secret = ch_db::tokens::new_token(ch_core::apps::ACCESS_PREFIX, 43);
            sqlx::query(
                "INSERT INTO oauth_tokens (grant_id, kind, token_hash, expires_at) VALUES ($1, 'access', $2, now() + interval '1 day')",
            )
            .bind(grant)
            .bind(ch_db::tokens::hash_token(&secret))
            .execute(&state.db)
            .await
            .unwrap();
            bearer(secret)
        }
    }
}

/// True while the service still takes the credential, asked the way the gate asks.
async fn alive(w: &World, a: &Auth) -> bool {
    let s = &w.state;
    match a.cred {
        Cred::None => true,
        Cred::Session => s.authn.from_session(&a.secret, &c()).await.is_ok_and(|x| !x.is_anonymous()),
        Cred::Basic => s.authn.from_basic("carol", PW, &c()).await.is_ok_and(|x| !x.is_anonymous()),
        Cred::App => s.authn.from_app_bearer(&a.secret, &c()).await.is_ok_and(|x| !x.is_anonymous()),
        _ => s.authn.from_bearer(&a.secret, &c()).await.is_ok_and(|x| !x.is_anonymous()),
    }
}

// ------------------------------------------------------------------ rows and refs

/// Every table that holds a repository, what is on it, or who may reach it, plus the settings
/// only an administrator changes. Not the audit log, the queues, sessions or tokens: those move
/// on every request.
const TABLES: &[&str] = &[
    "repositories",
    "repo_collaborators",
    "repo_redirects",
    "repo_access_holds",
    "issues",
    "issue_comments",
    "issue_assignees",
    "issue_labels",
    "issue_subscriptions",
    "labels",
    "milestones",
    "pull_requests",
    "pull_cleared_commits",
    "reviews",
    "review_comments",
    "review_threads",
    "review_requests",
    "reactions",
    "timeline_events",
    "releases",
    "release_assets",
    "branch_protections",
    "deploy_keys",
    "webhooks",
    "stars",
    "watches",
    "secrets",
    "variables",
    "environments",
    "environment_secrets",
    "environment_reviewers",
    "deployment_gates",
    "workflows",
    "workflow_runs",
    "workflow_jobs",
    "workflow_steps",
    "check_runs",
    "commit_statuses",
    "caches",
    "artifacts",
    "toolkit_uploads",
    "merge_queue_entries",
    "schedules",
    "fork_run_approvals",
    "packages",
    "oci_blobs",
    "oci_manifests",
    "oci_tags",
    "oci_uploads",
    "nuget_versions",
    "pages_sites",
    "pages_settings",
    "pages_deployments",
    "pages_files",
    "teams",
    "team_members",
    "team_repos",
    "org_members",
    "invites",
    "instance_settings",
    "visibility_settings",
    "actions_policy",
    "runner_registration_tokens",
];

/// What an account with no grant on a repository may change on it, where the tier lets it read: say
/// something, and mark something. Not code, not settings, not who has access.
const STRANGER_MAY_CHANGE: &[&str] = &[
    "issues",
    "issue_comments",
    "issue_assignees",
    "issue_labels",
    "issue_subscriptions",
    "pull_requests",
    "pull_cleared_commits",
    "reviews",
    "review_comments",
    "review_threads",
    "review_requests",
    "reactions",
    "timeline_events",
    "stars",
    "watches",
];

/// What the owner's contribution limit closes to an account with no grant.
const OPENED: &[&str] = &["issues", "issue_comments", "pull_requests", "reviews", "review_comments", "review_threads"];

/// What is in those tables and in the refs of both private repositories, as one hash per table.
async fn fingerprint(w: &World) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for t in TABLES {
        // A download counts itself, and an issue or pull request takes the next number: the
        // counters are not what is being watched.
        let row = match *t {
            "release_assets" => "(to_jsonb(x) - 'download_count')::text",
            "repositories" => "(to_jsonb(x) - 'next_number' - 'updated_at')::text",
            _ => "x::text",
        };
        let h: String = sqlx::query_scalar(&format!("SELECT md5(coalesce(string_agg({row}, '|' ORDER BY {row}), '')) FROM {t} x"))
            .fetch_one(&w.state.db)
            .await
            .unwrap_or_else(|e| panic!("fingerprint of {t}: {e}"));
        out.insert((*t).to_string(), h);
    }
    let repos: Vec<i64> = sqlx::query_scalar("SELECT id FROM repositories ORDER BY id").fetch_all(&w.state.db).await.unwrap();
    for id in repos {
        let refs = w.state.git.refs(RepoId(id), None).await.map(|r| format!("{r:?}")).unwrap_or_default();
        out.insert(format!("refs of repository {id}"), ch_db::tokens::sha256_hex(refs.as_bytes()));
    }
    out
}

fn changed(before: &BTreeMap<String, String>, after: &BTreeMap<String, String>) -> Vec<String> {
    after.iter().filter(|(k, v)| before.get(*k) != Some(*v)).map(|(k, _)| k.clone()).collect()
}

// ------------------------------------------------------------------ one request

const METHODS: &[Method] = &[Method::GET, Method::POST, Method::PUT, Method::PATCH, Method::DELETE];
static PEER: AtomicU32 = AtomicU32::new(0);

struct Seen {
    status: StatusCode,
    content_type: Option<String>,
    location: Option<String>,
    www: Option<String>,
    body: String,
}

async fn send(app: &Router, auth: &Auth, method: &Method, uri: &str, body: &str, repo: &str) -> Seen {
    let n = PEER.fetch_add(1, Ordering::Relaxed);
    let peer: SocketAddr = format!("198.18.{}.{}:40000", (n / 250) % 250, n % 250 + 1).parse().unwrap();
    let mut b = Request::builder().method(method.clone()).uri(uri);
    for (k, v) in &auth.headers {
        b = b.header(k.as_str(), v.as_str());
    }
    let unsafe_method = !matches!(*method, Method::GET | Method::HEAD);
    if unsafe_method && let Some(t) = &auth.csrf {
        b = b.header(header::ORIGIN, ORIGIN).header("x-csrf-token", t.as_str());
    }
    let body = if matches!(*method, Method::POST | Method::PUT | Method::PATCH) {
        b = b.header(header::CONTENT_TYPE, "application/json");
        Body::from(body.to_string())
    } else {
        Body::empty()
    };
    let mut req = b.body(body).unwrap();
    req.extensions_mut().insert(ConnectInfo(peer));
    let resp = app.clone().oneshot(req).await.unwrap();
    let (parts, resp_body) = resp.into_parts();
    // A stream (a live log) never ends: what arrives in a moment is what there is to see.
    let bytes = match tokio::time::timeout(std::time::Duration::from_secs(3), resp_body.collect()).await {
        Ok(Ok(c)) => c.to_bytes().to_vec(),
        _ => Vec::new(),
    };
    let norm = |s: &str| s.replace(&format!("alice/{repo}"), "alice/REPO").replace(&format!("/{repo}"), "/REPO");
    let h = |name: header::HeaderName| parts.headers.get(name).map(|v| norm(v.to_str().unwrap_or("?")));
    Seen {
        status: parts.status,
        content_type: h(header::CONTENT_TYPE),
        location: h(header::LOCATION),
        www: h(header::WWW_AUTHENTICATE),
        body: norm(&String::from_utf8_lossy(&bytes)),
    }
}

impl Seen {
    fn same_as(&self, o: &Seen) -> bool {
        self.status == o.status
            && self.content_type == o.content_type
            && self.location == o.location
            && self.www == o.www
            && self.body == o.body
    }
    /// Answered, but the request was not valid: the handler looked at the body and said no. Such an
    /// answer does not show that the caller was allowed, only that nothing came first.
    fn invalid(&self) -> bool {
        matches!(self.status.as_u16(), 400 | 413 | 415 | 422)
    }
    fn refused(&self) -> bool {
        matches!(self.status.as_u16(), 401 | 403 | 404 | 405)
            || (self.status == StatusCode::SEE_OTHER && self.location.as_deref().is_some_and(|l| l.starts_with("/login")))
    }
}

/// A body that is valid for the routes where a stranger might get something done, so that the
/// answer is the authorisation's and not the validator's. Everything else gets `{}`.
fn body_for(w: &World, method: &Method, pattern: &str) -> String {
    let Some(tail) = pattern.strip_prefix("/api/v3/repos/{owner}/{repo}") else { return "{}".into() };
    let v = match (method.as_str(), tail) {
        ("POST", "/issues") => serde_json::json!({"title": "matrix issue", "body": "b"}),
        ("PATCH", "/issues/{number}") => serde_json::json!({"title": "renamed", "state": "closed"}),
        ("POST", "/issues/{number}/comments") => serde_json::json!({"body": "matrix comment"}),
        ("POST", "/pulls") => serde_json::json!({"title": "matrix pull request", "head": "matrix-pr", "base": "main"}),
        ("PATCH", "/pulls/{number}") => serde_json::json!({"title": "renamed"}),
        ("PUT", "/pulls/{number}/merge") => serde_json::json!({"merge_method": "merge"}),
        ("POST", "/pulls/{number}/reviews") => serde_json::json!({"event": "COMMENT", "body": "matrix review"}),
        ("PUT", "/contents/{*path}") => serde_json::json!({"message": "matrix", "content": "aGk=", "branch": "main"}),
        ("POST", "/git/refs") => serde_json::json!({"ref": "refs/heads/matrix", "sha": w.sha}),
        ("PATCH", "") => serde_json::json!({"description": "changed by the matrix"}),
        ("POST", "/releases") => serde_json::json!({"tag_name": "v9", "target_commitish": "main"}),
        ("POST", "/labels") => serde_json::json!({"name": "matrix", "color": "ffffff"}),
        ("POST", "/milestones") => serde_json::json!({"title": "matrix"}),
        ("PUT", "/collaborators/{user}") => serde_json::json!({"permission": "admin"}),
        ("POST", "/hooks") => {
            serde_json::json!({"config": {"url": "https://hooks.example.org/m", "content_type": "json"}, "events": ["push"]})
        }
        ("POST", "/actions/workflows/{workflow}/dispatches") => serde_json::json!({"ref": "main"}),
        ("POST", "/statuses/{sha}") => serde_json::json!({"state": "success", "context": "matrix"}),
        ("POST", "/actions/variables") => serde_json::json!({"name": "MATRIX", "value": "v"}),
        ("PUT", "/actions/secrets/{name}") => serde_json::json!({"encrypted_value": "x"}),
        ("POST", "/transfer") => serde_json::json!({"new_owner": "carol"}),
        _ => return "{}".into(),
    };
    v.to_string()
}

// ------------------------------------------------------------------ the committed table

fn tests_access(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/access").join(name)
}

/// `cells METHOD pattern` per line, `#` for notes. Returns key (`METHOD pattern`) -> the six cells.
fn read_table(name: &str) -> BTreeMap<String, Vec<char>> {
    let p = tests_access(name);
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| {
        panic!("{}: {e}. Generate it: CH_MATRIX_OUT=<dir> cargo nextest run -p codehaven --test access_matrix, then python3 tests/access/merge_matrix.py <dir>", p.display())
    });
    let mut out = BTreeMap::new();
    for line in text.lines().filter(|l| !l.trim().is_empty() && !l.starts_with('#')) {
        let (cells, key) = line.split_once(' ').unwrap_or_else(|| panic!("{name}: bad line {line:?}"));
        assert_eq!(cells.chars().count(), 6, "{name}: {line:?} should start with six cells");
        out.insert(key.to_string(), cells.chars().collect());
    }
    out
}

fn routes_file() -> Vec<String> {
    let p = tests_access("matrix.routes.txt");
    std::fs::read_to_string(&p)
        .unwrap_or_else(|e| {
            panic!("{}: {e}. Generate it with CH_MATRIX_OUT and merge_matrix.py (see the top of access_matrix.rs)", p.display())
        })
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}

/// The order of rows: by pattern, then GET before the writes.
fn order(key: &str) -> (String, usize) {
    let (m, p) = key.split_once(' ').unwrap();
    (p.to_string(), METHODS.iter().position(|x| x.as_str() == m).unwrap_or(9))
}

// ------------------------------------------------------------------ one cell group

async fn set_cell(w: &World, tier: Tier, limited: bool) {
    let db = &w.state.db;
    sqlx::query("UPDATE visibility_settings SET allow_public_repos = true").execute(db).await.unwrap();
    // Repositories that look at the job-token ceiling would otherwise cap every job at read.
    sqlx::query("UPDATE repositories SET actions_token_ceiling = 'write'").execute(db).await.unwrap();
    let policy = if limited { "grants" } else { "everyone" };
    sqlx::query(
        "UPDATE repositories SET access_tier = $2, public_since = CASE WHEN $2 = 'public' THEN now() END, contribution_policy = $3
         WHERE id = $1 OR name = 'tools'",
    )
    .bind(w.proj.0)
    .bind(tier.name())
    .bind(policy)
    .execute(db)
    .await
    .unwrap();
}

async fn run_cell(pool: PgPool, cred: Cred, tier: Tier, limited: bool) {
    let w = seed(pool).await;
    let paths = registered_paths(&wiring::routes(&w.state.config));
    assert!(paths.len() > 200, "the router listing found only {} paths", paths.len());
    // A branch with a change on it that no pull request names yet, for the one a stranger opens.
    let alice = UserId(sqlx::query_scalar("SELECT id FROM users WHERE username = 'alice'").fetch_one(&w.state.db).await.unwrap());
    let sig = Signature { name: "Alice".into(), email: "alice@users.noreply.code.example.com".into(), at: chrono::Utc::now() };
    let change = CommitFileChange {
        repo_id: w.proj,
        branch: "main".into(),
        new_branch: Some("matrix-pr".into()),
        expected_sha: None,
        changes: vec![FileChange { path: "matrix.txt".into(), content: Some(b"matrix\n".to_vec()), renamed_from: None }],
        message: "A change for the matrix".into(),
        author: sig,
    };
    w.state.git.commit_file_change(&alice_actor(alice), change, &c()).await.unwrap();
    set_cell(&w, tier, limited).await;
    let mut auth = make_auth(&w, cred).await;
    assert!(alive(&w, &auth).await, "{}: the credential made for this run does not work", cred.name());
    let mut fp = fingerprint(&w).await;

    let mut got: BTreeMap<String, char> = BTreeMap::new();
    let mut problems: Vec<String> = Vec::new();
    let mut admitted_writes = 0;
    for pattern in &paths {
        let path = concrete(pattern, &w, "alice", "proj");
        for m in METHODS {
            let key = format!("{m} {pattern}");
            let body = body_for(&w, m, pattern);
            let r = send(&w.app, &auth, m, &path, &body, "proj").await;
            if r.status.is_server_error() || r.status == StatusCode::TOO_MANY_REQUESTS {
                problems.push(format!("{key} answered {}: {}", r.status, r.body.chars().take(200).collect::<String>()));
            }
            let code = if r.refused() {
                '.'
            } else {
                let now = fingerprint(&w).await;
                let diff = changed(&fp, &now);
                fp = now;
                if diff.is_empty() {
                    if r.invalid() { 'v' } else { 'r' }
                } else {
                    let route_names_repo = pattern.contains("{repo}");
                    if matches!(*m, Method::GET | Method::HEAD) {
                        problems.push(format!("{key} is a GET and changed {diff:?}"));
                    }
                    // A job's own repository is the one its runner calls act on, so only a route that
                    // names a repository can be a write to this one.
                    if tier == Tier::Private && cred.without_rights() && route_names_repo {
                        problems.push(format!("{key} changed {diff:?} on a private repository the credential has no right to"));
                    }
                    if cred.read_only() {
                        problems.push(format!("{key} changed {diff:?} with a credential that only reads"));
                    }
                    // Whatever the tier, a stranger's account may open and discuss and star and fork,
                    // and nothing else: not a ref, not a setting, not a release, not a collaborator.
                    if cred.without_rights() && route_names_repo && !pattern.ends_with("/forks") {
                        let extra: Vec<&String> = diff.iter().filter(|t| !STRANGER_MAY_CHANGE.contains(&t.as_str())).collect();
                        if !extra.is_empty() {
                            problems.push(format!("{key} changed {extra:?}, which an account with no grant may never change"));
                        }
                    }
                    if limited
                        && cred.contributor()
                        && let Some(t) = diff.iter().find(|t| OPENED.contains(&t.as_str()))
                    {
                        problems.push(format!("{key} changed {t} though the owner limits contribution to people with access"));
                    }
                    'w'
                }
            };
            if code != '.' && !matches!(*m, Method::GET | Method::HEAD) {
                admitted_writes += 1;
            }
            got.insert(key.clone(), code);

            // On a private repository, a credential with no right to it learns nothing: the
            // answer is the one for a repository that does not exist.
            if tier == Tier::Private && cred.without_rights() && pattern.contains("{owner}") && pattern.contains("{repo}") {
                let missing = send(&w.app, &auth, m, &concrete(pattern, &w, "alice", "nope"), &body, "nope").await;
                if !r.same_as(&missing) {
                    problems.push(format!(
                        "{key}: the private repository answers {} {:?}, a missing one {} {:?}",
                        r.status,
                        r.body.chars().take(120).collect::<String>(),
                        missing.status,
                        missing.body.chars().take(120).collect::<String>()
                    ));
                }
            }
            // A request that ended the credential (a sign-out, a revoked token) must not turn the
            // rest of the sweep into refusals: make another and go on.
            if code != '.' && !matches!(*m, Method::GET | Method::HEAD) && !alive(&w, &auth).await {
                auth = make_auth(&w, cred).await;
                assert!(alive(&w, &auth).await, "{key}: could not make a new {} credential", cred.name());
            }
        }
    }
    if cred == Cred::Session {
        assert!(admitted_writes > 0, "no write was answered for a session: the Origin and CSRF token are not being accepted");
    }
    assert!(problems.is_empty(), "{} {} {}:\n{}", cred.name(), tier.name(), if limited { "limited" } else { "open" }, problems.join("\n"));

    // The cell, for the table.
    let col = cell_index(tier, limited);
    let mut lines: Vec<(String, char)> = got.iter().filter(|(_, c)| **c != '.').map(|(k, c)| (k.clone(), *c)).collect();
    lines.sort_by_key(|(k, _)| order(k));
    if let Ok(dir) = std::env::var("CH_MATRIX_OUT") {
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).unwrap();
        let text: String = lines.iter().map(|(k, c)| format!("{c} {k}\n")).collect();
        std::fs::write(dir.join(format!("{}.{}.{}.txt", cred.name(), col, tier.name())), text).unwrap();
        std::fs::write(dir.join("routes.txt"), paths.iter().map(|p| format!("{p}\n")).collect::<String>()).unwrap();
        return;
    }
    let table = read_table(&format!("matrix.{}.txt", cred.name()));
    let mut diffs = Vec::new();
    let keys: BTreeSet<&String> = table.keys().chain(got.keys()).collect();
    for k in keys {
        let want = table.get(k).map(|c| c[col]).unwrap_or('.');
        let now = got.get(k).copied().unwrap_or('.');
        if want != now {
            diffs.push(format!("{k}: the table says {want:?}, now {now:?}"));
        }
    }
    assert!(
        diffs.is_empty(),
        "{} {} {} differs from tests/access/matrix.{}.txt in {} rows ('.' refused, 'v' answered but not valid, 'r' answered, 'w' answered and changed rows or refs):\n{}",
        cred.name(),
        tier.name(),
        if limited { "limited" } else { "open" },
        cred.name(),
        diffs.len(),
        diffs.join("\n")
    );
}

macro_rules! cells {
    ($($name:ident: $cred:expr, $tier:expr, $limited:expr;)*) => {
        $(
            #[sqlx::test(migrator = "ch_db::MIGRATOR")]
            async fn $name(pool: PgPool) {
                run_cell(pool, $cred, $tier, $limited).await;
            }
        )*
    };
}

macro_rules! per_credential {
    ($($cred:ident => $variant:expr;)*) => {
        $(
            mod $cred {
                use super::*;
                cells! {
                    private_everyone: $variant, Tier::Private, false;
                    private_limited: $variant, Tier::Private, true;
                    internal_everyone: $variant, Tier::Internal, false;
                    internal_limited: $variant, Tier::Internal, true;
                    public_everyone: $variant, Tier::Public, false;
                    public_limited: $variant, Tier::Public, true;
                }
            }
        )*
    };
}

per_credential! {
    none => Cred::None;
    session => Cred::Session;
    basic => Cred::Basic;
    pat_read => Cred::PatRead;
    pat_write => Cred::PatWrite;
    job_read => Cred::JobRead;
    job_write => Cred::JobWrite;
    job_other => Cred::JobOther;
    runner => Cred::Runner;
    app => Cred::App;
}

/// A new route cannot arrive unreviewed: the router's routes are the committed list, and every
/// row of the table names one of them. Needs no database.
#[test]
fn every_registered_route_has_a_row_in_the_matrix() {
    let mut cfg = ch_core::Config::for_tests();
    cfg.server.external_url = ORIGIN.into();
    cfg.mcp.enabled = true;
    let now: Vec<String> = registered_paths(&wiring::routes(&cfg));
    let committed = routes_file();
    let now_set: BTreeSet<&String> = now.iter().collect();
    let committed_set: BTreeSet<&String> = committed.iter().collect();
    let new: Vec<&&String> = now_set.difference(&committed_set).collect();
    let gone: Vec<&&String> = committed_set.difference(&now_set).collect();
    assert!(
        new.is_empty() && gone.is_empty(),
        "the router and tests/access/matrix.routes.txt disagree. New routes (read what each credential reaches, then regenerate the table, see the top of access_matrix.rs): {new:?}. Gone: {gone:?}"
    );
    for cred in [
        Cred::None,
        Cred::Session,
        Cred::Basic,
        Cred::PatRead,
        Cred::PatWrite,
        Cred::JobRead,
        Cred::JobWrite,
        Cred::JobOther,
        Cred::Runner,
        Cred::App,
    ] {
        for key in read_table(&format!("matrix.{}.txt", cred.name())).keys() {
            let pattern = key.split_once(' ').unwrap().1;
            assert!(
                committed_set.contains(&pattern.to_string()),
                "matrix.{}.txt has a row for {pattern}, which is not a route",
                cred.name()
            );
        }
    }
}
