//! A small world to run the CI crate in: a real Postgres, real git repositories made with the
//! `git` binary, the real audit log, event bus and blob store, and stand-ins for the services
//! other streams own (who may do what, and where repositories live).
//!
//! The runner side is played by [`FakeRunner`], which speaks the real HTTP protocol through the
//! real router, so every test of the runner API is also a test of the wire format.

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(dead_code)]

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use bytes::Bytes;
use ch_ci::clock::ManualClock;
use ch_ci::ctx::Ctx;
use ch_ci::shared::shared_for;
use ch_ci::source::{CommitSummary, WorkflowSource};
use ch_core::actor::{Actor, AuthVia, Permission};
use ch_core::events::{DomainEvent, StoredEvent};
use ch_core::live::Credential;
use ch_core::model::*;
use ch_core::services::{Authn, Authz, OrgRole, Repos};
use ch_core::state::AppState;
use ch_core::{Error, OrgId, RepoId, Result, UserId};
use ch_testkit::fixtures::{TestEnv, admin_actor, user_actor};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

// ---------------------------------------------------------------- real git repositories

/// A throwaway repository with a fixed identity and stepping dates, so shas are stable. It is
/// `Send` and `Sync`, unlike the testkit's `TempRepo`, because tests share a `World` across tasks.
pub struct GitRepo {
    _dir: tempfile::TempDir,
    pub work: PathBuf,
    clock: std::sync::atomic::AtomicU64,
}

impl GitRepo {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let r = GitRepo { _dir: dir, work, clock: std::sync::atomic::AtomicU64::new(1_700_000_000) };
        r.git(&["init", "--initial-branch=main", "--quiet"]);
        r
    }

    pub fn git(&self, args: &[&str]) -> String {
        let t = self.clock.fetch_add(60, std::sync::atomic::Ordering::SeqCst);
        let date = format!("{t} +0000");
        let out = Command::new("git")
            .current_dir(&self.work)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", &self.work)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Alice Example")
            .env("GIT_AUTHOR_EMAIL", "alice@test.example")
            .env("GIT_COMMITTER_NAME", "Alice Example")
            .env("GIT_COMMITTER_EMAIL", "alice@test.example")
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    pub fn commit_file(&self, path: &str, content: &str, message: &str) -> String {
        let full = self.work.join(path);
        if let Some(p) = full.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(&full, content).unwrap();
        self.git(&["add", "--", path]);
        self.git(&["commit", "--quiet", "-m", message]);
        self.head()
    }

    pub fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"])
    }

    pub fn branch(&self, name: &str) {
        self.git(&["checkout", "--quiet", "-b", name]);
    }
}

// ---------------------------------------------------------------- source over real git

#[derive(Default)]
pub struct GitDirs(Mutex<HashMap<i64, PathBuf>>);

impl GitDirs {
    fn dir(&self, repo: RepoId) -> Option<PathBuf> {
        self.0.lock().unwrap().get(&repo.0).cloned()
    }
}

fn git(dir: &PathBuf, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .current_dir(dir)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args(args)
        .output()
        .ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).to_string())
}

#[async_trait]
impl WorkflowSource for GitDirs {
    async fn resolve(&self, repo: RepoId, rev: &str) -> Result<String> {
        let dir = self.dir(repo).ok_or(Error::NotFound)?;
        git(&dir, &["rev-parse", "--verify", "--quiet", &format!("{rev}^{{commit}}")]).map(|s| s.trim().to_string()).ok_or(Error::NotFound)
    }
    async fn exact_ref(&self, repo: RepoId, name: &str) -> Result<Option<String>> {
        let dir = self.dir(repo).ok_or(Error::NotFound)?;
        let Some(out) = git(&dir, &["for-each-ref", "--format=%(refname) %(objectname)", name]) else { return Ok(None) };
        Ok(out.lines().filter_map(|l| l.split_once(' ')).find(|(n, _)| *n == name).map(|(_, sha)| sha.to_string()))
    }
    async fn list_dir(&self, repo: RepoId, sha: &str, d: &str) -> Result<Vec<String>> {
        let dir = self.dir(repo).ok_or(Error::NotFound)?;
        let Some(out) = git(&dir, &["ls-tree", &format!("{sha}:{d}")]) else { return Ok(vec![]) };
        Ok(out.lines().filter(|l| l.contains(" blob ")).filter_map(|l| l.split('\t').nth(1)).map(String::from).collect())
    }
    async fn read(&self, repo: RepoId, sha: &str, path: &str, _max: u64) -> Result<Option<String>> {
        let dir = self.dir(repo).ok_or(Error::NotFound)?;
        Ok(git(&dir, &["show", &format!("{sha}:{path}")]))
    }
    async fn changed_paths(&self, repo: RepoId, base: &str, head: &str) -> Result<Option<Vec<String>>> {
        let dir = self.dir(repo).ok_or(Error::NotFound)?;
        if base.bytes().all(|b| b == b'0') {
            return Ok(None);
        }
        Ok(git(&dir, &["diff", "--name-only", base, head]).map(|s| s.lines().map(String::from).collect()))
    }
    async fn commit(&self, repo: RepoId, sha: &str) -> Result<Option<CommitSummary>> {
        let dir = self.dir(repo).ok_or(Error::NotFound)?;
        Ok(git(&dir, &["log", "-1", "--format=%H%n%an%n%ae%n%B", sha]).map(|s| {
            let mut l = s.lines();
            CommitSummary {
                id: l.next().unwrap_or("").into(),
                author_name: l.next().unwrap_or("").into(),
                author_email: l.next().unwrap_or("").into(),
                message: l.collect::<Vec<_>>().join("\n"),
            }
        }))
    }
}

// ---------------------------------------------------------------- stand-ins for other streams

/// Permissions read straight from the tables, following ARCHITECTURE.md 7.2 closely enough for CI.
pub struct DbAuthz(pub PgPool);

#[async_trait]
impl Authz for DbAuthz {
    async fn repo_permission(&self, actor: &Actor, repo: RepoId) -> Result<Permission> {
        // A connected app: what the person can do, capped by what they gave it here.
        if let Actor::App { user_id, grant, .. } = actor {
            let cap = if grant.write.has(repo) {
                Permission::Write
            } else if grant.read.has(repo) {
                Permission::Read
            } else {
                Permission::None
            };
            let person = Actor::User { id: *user_id, is_admin: false, has_2fa: false, via: AuthVia::Session };
            return Ok(Box::pin(self.repo_permission(&person, repo)).await?.min(cap));
        }
        let row: Option<(i64, String)> =
            sqlx::query_as("SELECT owner_id, visibility FROM repositories WHERE id = $1").bind(repo.0).fetch_optional(&self.0).await?;
        let Some((owner, vis)) = row else { return Ok(Permission::None) };
        let uid = match actor {
            Actor::Job { repo_id, perms, .. } => {
                // As the real service: a job reads its own repository, and writes it with `contents: write`.
                return Ok(match (*repo_id == repo, perms.contents >= Permission::Write) {
                    (true, true) => Permission::Write,
                    (true, false) => Permission::Read,
                    _ => Permission::None,
                });
            }
            Actor::System => return Ok(Permission::Admin),
            a => match a.user_id() {
                Some(u) => u,
                None => return Ok(Permission::None),
            },
        };
        if actor.is_site_admin() || uid.0 == owner {
            return Ok(Permission::Admin);
        }
        let collab: Option<String> = sqlx::query_scalar("SELECT permission FROM repo_collaborators WHERE repo_id = $1 AND user_id = $2")
            .bind(repo.0)
            .bind(uid.0)
            .fetch_optional(&self.0)
            .await?;
        let mut p = match collab.as_deref() {
            Some("admin") => Permission::Admin,
            Some("maintain") => Permission::Maintain,
            Some("write") => Permission::Write,
            Some("triage") => Permission::Triage,
            Some("read") => Permission::Read,
            _ => Permission::None,
        };
        if vis == "public" && p < Permission::Read {
            p = Permission::Read;
        }
        Ok(p)
    }
    /// Owner, site admin or a collaborator row: never the tier alone.
    async fn has_grant(&self, actor: &Actor, repo: RepoId) -> Result<bool> {
        let uid = match actor {
            Actor::System => return Ok(true),
            Actor::User { id, .. } | Actor::Token { user_id: id, .. } | Actor::App { user_id: id, .. } => *id,
            _ => return Ok(false),
        };
        if actor.is_site_admin() {
            return Ok(true);
        }
        let granted: Option<bool> = sqlx::query_scalar(
            "SELECT r.owner_id = $2 OR EXISTS (SELECT 1 FROM repo_collaborators c WHERE c.repo_id = r.id AND c.user_id = $2) \
             FROM repositories r WHERE r.id = $1",
        )
        .bind(repo.0)
        .bind(uid.0)
        .fetch_optional(&self.0)
        .await?;
        Ok(granted.unwrap_or(false))
    }
    async fn org_role(&self, actor: &Actor, org: OrgId) -> Result<Option<OrgRole>> {
        Ok(actor.user_id().filter(|u| u.0 == org.0).map(|_| OrgRole::Owner))
    }
    async fn require(&self, actor: &Actor, repo: RepoId, needed: Permission) -> Result<()> {
        let have = self.repo_permission(actor, repo).await?;
        if have == Permission::None {
            Err(Error::NotFound)
        } else if have < needed {
            Err(Error::forbidden("Must have more access"))
        } else {
            Ok(())
        }
    }
}

pub struct DbRepos(pub PgPool, pub Arc<DbAuthz>);

#[async_trait]
impl Repos for DbRepos {
    async fn create(&self, _: &Actor, _: NewRepo, _: &ClientInfo) -> Result<Repo> {
        Err(Error::NotFound)
    }
    async fn get_by_name(&self, actor: &Actor, owner: &str, name: &str) -> Result<Repo> {
        let id: Option<i64> =
            sqlx::query_scalar("SELECT r.id FROM repositories r JOIN users u ON u.id = r.owner_id WHERE u.username = $1 AND r.name = $2")
                .bind(owner)
                .bind(name)
                .fetch_optional(&self.0)
                .await?;
        self.get(actor, RepoId(id.ok_or(Error::NotFound)?)).await
    }
    async fn get(&self, actor: &Actor, id: RepoId) -> Result<Repo> {
        if self.1.repo_permission(actor, id).await? == Permission::None {
            return Err(Error::NotFound);
        }
        let r: (i64, i64, String, String, String, String, String, bool, bool, chrono::DateTime<chrono::Utc>) = sqlx::query_as(
            "SELECT r.id, r.owner_id, u.username::text, r.name::text, r.description, r.access_tier, r.default_branch, r.archived, r.is_empty, r.created_at FROM repositories r JOIN users u ON u.id = r.owner_id WHERE r.id = $1",
        )
        .bind(id.0)
        .fetch_one(&self.0)
        .await?;
        Ok(Repo {
            id: RepoId(r.0),
            owner_id: UserId(r.1),
            owner_name: r.2,
            name: r.3,
            description: r.4,
            visibility: Visibility::parse(&r.5).unwrap_or(Visibility::Private),
            default_branch: r.6,
            archived: r.7,
            is_empty: r.8,
            fork_parent_id: None,
            topics: vec![],
            pushed_at: None,
            created_at: r.9,
            contribution_policy: Default::default(),
        })
    }
    async fn list_for_owner(&self, _: &Actor, _: UserId) -> Result<Vec<Repo>> {
        Ok(vec![])
    }
    async fn rename(&self, _: &Actor, _: RepoId, _: &str, _: &ClientInfo) -> Result<Repo> {
        Err(Error::NotFound)
    }
    async fn transfer(&self, _: &Actor, _: RepoId, _: UserId, _: &ClientInfo) -> Result<Repo> {
        Err(Error::NotFound)
    }
    async fn fork(&self, _: &Actor, _: RepoId, _: UserId, _: &ClientInfo) -> Result<Repo> {
        Err(Error::NotFound)
    }
    async fn delete(&self, _: &Actor, _: RepoId, _: &ClientInfo) -> Result<()> {
        Err(Error::NotFound)
    }
    async fn collaborators(&self, _: &Actor, _: RepoId) -> Result<Vec<Collaborator>> {
        Ok(vec![])
    }
    async fn set_collaborator(&self, _: &Actor, _: RepoId, _: UserId, _: Permission, _: &ClientInfo) -> Result<()> {
        Ok(())
    }
    async fn remove_collaborator(&self, _: &Actor, _: RepoId, _: UserId, _: &ClientInfo) -> Result<()> {
        Ok(())
    }
    async fn protections(&self, _: &Actor, _: RepoId) -> Result<Vec<BranchProtection>> {
        Ok(vec![])
    }
    async fn set_protection(&self, _: &Actor, _: RepoId, _: BranchProtection, _: &ClientInfo) -> Result<()> {
        Ok(())
    }
    async fn remove_protection(&self, _: &Actor, _: RepoId, _: &str, _: &ClientInfo) -> Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------- the world

#[derive(Clone)]
pub enum Who {
    Anon,
    User(UserId),
    Admin(UserId),
    /// A bearer credential: a runner's or a job's.
    Bearer(String),
}

pub struct World {
    pub env: TestEnv,
    pub state: AppState,
    pub ctx: Ctx,
    pub dirs: Arc<GitDirs>,
    pub clock: Arc<ManualClock>,
    pub repos: Mutex<Vec<Arc<GitRepo>>>,
    router: Router,
    pub alice: UserId,
    pub bob: UserId,
    pub root: UserId,
}

/// Plays the part of the gate: turns a test header or a bearer credential into an `Actor`, the
/// way the real gate does, using the same helpers the identity stream will call.
async fn fake_gate(State(pool): State<PgPool>, mut req: Request, next: Next) -> Response {
    let h = req.headers().clone();
    let actor = if let Some(v) = h.get("x-test-actor").and_then(|v| v.to_str().ok()) {
        let (kind, id) = v.split_once(':').unwrap_or(("", "0"));
        let id = UserId(id.parse().unwrap_or(0));
        match kind {
            "admin" => admin_actor(id),
            "user" => user_actor(id),
            "basic" => Actor::User { id, is_admin: false, has_2fa: false, via: AuthVia::Basic },
            _ => Actor::Anonymous,
        }
    } else if let Some(tok) = h.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")) {
        if let Some(r) = ch_db::ci_auth::runner_from_credential(&pool, tok).await.unwrap() {
            Actor::Runner { runner_id: r }
        } else if let Some((job_id, repo_id, perms)) = ch_db::ci_auth::job_from_token(&pool, tok).await.unwrap() {
            Actor::Job { job_id, repo_id, perms }
        } else if let Some((job_id, repo_id, perms)) = ch_db::ci_auth::job_from_runtime_token(&pool, tok).await.unwrap() {
            Actor::Job { job_id, repo_id, perms }
        } else {
            Actor::Anonymous
        }
    } else {
        Actor::Anonymous
    };
    // The real gate rejects a signed-out request before it reaches a handler, except on the
    // public routes. Register is the only one CI has.
    let is_public = ch_core::access::is_public(req.method(), req.uri().path());
    if actor.is_anonymous() && !is_public {
        return Response::builder().status(StatusCode::UNAUTHORIZED).body(Body::from("{\"message\":\"Requires authentication\"}")).unwrap();
    }
    let credential = match &actor {
        Actor::User { id, .. } => Some(Credential::Basic { user_id: *id, fingerprint: vec![] }),
        Actor::Runner { runner_id } => Some(Credential::Runner { runner_id: *runner_id }),
        Actor::Job { job_id, .. } => Some(Credential::Job { job_id: *job_id }),
        _ => None,
    };
    if let Some(c) = credential {
        req.extensions_mut().insert(c);
    }
    req.extensions_mut().insert(actor);
    req.extensions_mut().insert(ClientInfo { ip: "10.1.2.3".parse().unwrap(), user_agent: Some("test".into()) });
    next.run(req).await
}

/// Plays the identity service for the live checks of a stream: a person's credential lasts
/// while the account is not suspended, a job's while the job is not completed.
struct TestAuthn(PgPool);

#[async_trait]
impl Authn for TestAuthn {
    async fn from_session(&self, _c: &str, _client: &ClientInfo) -> Result<Actor> {
        Ok(Actor::Anonymous)
    }
    async fn from_basic(&self, _u: &str, _s: &str, _client: &ClientInfo) -> Result<Actor> {
        Err(Error::unauthorized())
    }
    async fn from_bearer(&self, _t: &str, _client: &ClientInfo) -> Result<Actor> {
        Err(Error::unauthorized())
    }
    async fn from_ssh_key(&self, _f: &str) -> Result<Option<SshIdentity>> {
        Ok(None)
    }
    async fn recheck(&self, credential: &Credential) -> Result<Option<Actor>> {
        match credential {
            Credential::Basic { user_id, .. } => {
                let admin: Option<bool> = sqlx::query_scalar("SELECT is_admin FROM users WHERE id = $1 AND suspended_at IS NULL")
                    .bind(user_id.0)
                    .fetch_optional(&self.0)
                    .await?;
                Ok(admin.map(|a| if a { admin_actor(*user_id) } else { user_actor(*user_id) }))
            }
            // The real lookup, so a job's `permissions:` decide here as they do in the server.
            Credential::Job { job_id } => Ok(ch_db::ci_auth::job_by_id(&self.0, *job_id).await?.map(|(repo_id, perms)| Actor::Job {
                job_id: *job_id,
                repo_id,
                perms,
            })),
            _ => Ok(None),
        }
    }
}

impl World {
    pub async fn new(pool: PgPool) -> World {
        let _ = tracing_subscriber::fmt().with_test_writer().with_env_filter("ch_ci=debug,warn").try_init();
        let env = TestEnv::new(pool.clone()).await;
        let alice = env.insert_user("alice", false, true).await;
        let bob = env.insert_user("bob", false, false).await;
        let root = env.insert_user("root", true, true).await;
        let dirs = Arc::new(GitDirs::default());
        let shared = shared_for(&env.config);
        shared.set_source(dirs.clone());
        let clock = Arc::new(ManualClock::new(chrono::Utc::now()));
        shared.set_clock(clock.clone());
        let mut b = env.builder().await;
        let authz = Arc::new(DbAuthz(pool.clone()));
        b.authz = authz.clone();
        b.authn = Arc::new(TestAuthn(pool.clone()));
        b.repos = Arc::new(DbRepos(pool.clone(), authz));
        use ch_core::state::Install;
        ch_ci::build(&b).unwrap().install(&mut b);
        let state = b.build();
        let router = Router::new()
            .nest("/api/v3", ch_ci::api_routes())
            .nest("/api/runner/v1", ch_ci::runner_routes())
            .with_state(state.clone())
            .layer(middleware::from_fn_with_state(pool, fake_gate));
        let ctx = Ctx::from_state(&state);
        World { env, state, ctx, dirs, clock, repos: Mutex::new(vec![]), router, alice, bob, root }
    }

    pub fn pool(&self) -> &PgPool {
        &self.env.pool
    }

    /// A repository with a git history: each `(path, content)` is committed to `main`.
    pub async fn repo_with(&self, owner: UserId, name: &str, files: &[(&str, &str)]) -> (RepoId, Arc<GitRepo>) {
        let id = self.env.repo(owner, name).await;
        let t = Arc::new(GitRepo::new());
        if files.is_empty() {
            t.commit_file("README.md", "# demo\n", "Initial commit");
        }
        for (p, c) in files {
            t.commit_file(p, c, &format!("Add {p}"));
        }
        self.dirs.0.lock().unwrap().insert(id.0, t.work.clone());
        self.repos.lock().unwrap().push(t.clone());
        (id, t)
    }

    /// Makes `alias` read the same git history as `repo`, the way a fork shares its parent's objects.
    pub fn dirs_alias(&self, alias: RepoId, repo: RepoId) {
        let dir = self.dirs.0.lock().unwrap().get(&repo.0).cloned().unwrap();
        self.dirs.0.lock().unwrap().insert(alias.0, dir);
    }

    pub async fn set_ci_allowed(&self, user: UserId, allowed: bool) {
        sqlx::query("UPDATE users SET can_run_ci = $2 WHERE id = $1").bind(user.0).bind(allowed).execute(self.pool()).await.unwrap();
    }

    /// Records a push to `branch` in the outbox and runs the consumer over it.
    pub async fn push(&self, repo: RepoId, git: &GitRepo, branch: &str, before: Option<&str>) -> Vec<i64> {
        let after = git.git(&["rev-parse", branch]);
        let ev = DomainEvent::Push {
            repo_id: repo,
            git_ref: format!("refs/heads/{branch}"),
            before: before.map(String::from).unwrap_or_else(|| "0".repeat(40)),
            after,
            pusher: Some(self.alice),
            forced: false,
            app: None,
            server_built: false,
        };
        self.emit_and_process(ev).await
    }

    pub async fn emit_and_process(&self, ev: DomainEvent) -> Vec<i64> {
        let mut tx = self.pool().begin().await.unwrap();
        let id = self.state.events.emit(&mut tx, ev).await.unwrap();
        tx.commit().await.unwrap();
        let mut failures = Default::default();
        ch_ci::triggers::process_batch(&self.ctx, &mut failures).await.unwrap();
        let _ = id;
        self.run_ids().await
    }

    pub async fn run_ids(&self) -> Vec<i64> {
        sqlx::query_scalar("SELECT id FROM workflow_runs ORDER BY id").fetch_all(self.pool()).await.unwrap()
    }

    pub async fn run_status(&self, run: i64) -> (String, Option<String>) {
        sqlx::query_as("SELECT status, conclusion FROM workflow_runs WHERE id = $1").bind(run).fetch_one(self.pool()).await.unwrap()
    }

    pub async fn jobs(&self, run: i64) -> Vec<(String, String, Option<String>)> {
        sqlx::query_as("SELECT name, status, conclusion FROM workflow_jobs WHERE run_id = $1 ORDER BY id")
            .bind(run)
            .fetch_all(self.pool())
            .await
            .unwrap()
    }

    pub async fn job_id(&self, run: i64, name: &str) -> i64 {
        sqlx::query_scalar("SELECT id FROM workflow_jobs WHERE run_id = $1 AND name = $2")
            .bind(run)
            .bind(name)
            .fetch_one(self.pool())
            .await
            .unwrap()
    }

    // ---------------------------------------------------------- HTTP

    pub async fn send(&self, method: Method, uri: &str, who: &Who, body: Option<Value>) -> (StatusCode, Value) {
        let (s, bytes) =
            self.send_raw(method, uri, who, body.map(|b| (Bytes::from(serde_json::to_vec(&b).unwrap()), "application/json"))).await;
        let v = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).to_string()))
        };
        (s, v)
    }

    pub async fn send_raw(&self, method: Method, uri: &str, who: &Who, body: Option<(Bytes, &str)>) -> (StatusCode, Bytes) {
        let mut b = Request::builder().method(method).uri(uri);
        match who {
            Who::Anon => {}
            Who::User(u) => b = b.header("x-test-actor", format!("user:{}", u.0)),
            Who::Admin(u) => b = b.header("x-test-actor", format!("admin:{}", u.0)),
            Who::Bearer(t) => b = b.header(header::AUTHORIZATION, format!("Bearer {t}")),
        }
        let req = match body {
            Some((bytes, ct)) => b.header(header::CONTENT_TYPE, ct).body(Body::from(bytes)).unwrap(),
            None => b.body(Body::empty()).unwrap(),
        };
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        (status, resp.into_body().collect().await.unwrap().to_bytes())
    }

    /// Any request, headers and all, through the router and the fake gate.
    pub async fn raw(&self, req: Request) -> (StatusCode, Bytes) {
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        (status, resp.into_body().collect().await.unwrap().to_bytes())
    }

    pub async fn get(&self, uri: &str, who: &Who) -> (StatusCode, Value) {
        self.send(Method::GET, uri, who, None).await
    }
    pub async fn post(&self, uri: &str, who: &Who, body: Value) -> (StatusCode, Value) {
        self.send(Method::POST, uri, who, Some(body)).await
    }

    /// An admin-issued registration token, as a site admin would get from the API.
    pub async fn registration_token(&self) -> String {
        let (s, v) = self.post("/api/v3/admin/actions/runners/registration-token", &Who::Admin(self.root), json!({})).await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        v["token"].as_str().unwrap().to_string()
    }

    pub async fn runner(&self, name: &str, labels: &[&str]) -> FakeRunner {
        let token = self.registration_token().await;
        let (s, v) = self.post("/api/runner/v1/register", &Who::Anon, json!({"token": token, "name": name, "labels": labels})).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        FakeRunner { cred: v["credential"].as_str().unwrap().to_string(), id: v["runner_id"].as_i64().unwrap() }
    }

    pub async fn events(&self) -> Vec<StoredEvent> {
        self.state.events.poll("test-reader", 1000).await.unwrap()
    }
}

// ---------------------------------------------------------------- the runner

pub struct FakeRunner {
    pub cred: String,
    pub id: i64,
}

impl FakeRunner {
    pub fn who(&self) -> Who {
        Who::Bearer(self.cred.clone())
    }

    /// Asks for a job, waiting `wait` seconds at most.
    pub async fn request(&self, w: &World, wait: u32) -> Option<ch_api_types::runner::JobPayload> {
        let (s, v) = w.post("/api/runner/v1/jobs/request", &self.who(), json!({"wait_seconds": wait})).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        serde_json::from_value::<ch_api_types::runner::JobResponse>(v).unwrap().job
    }

    pub async fn heartbeat(&self, w: &World, job: i64) -> (StatusCode, Value) {
        w.post(&format!("/api/runner/v1/jobs/{job}/heartbeat"), &self.who(), json!({})).await
    }

    pub async fn logs(&self, w: &World, job: i64, step: i32, seq: u64, lines: &[&str]) -> (StatusCode, Value) {
        w.post(&format!("/api/runner/v1/jobs/{job}/logs"), &self.who(), json!({"step": step, "seq": seq, "lines": lines})).await
    }

    pub async fn step(&self, w: &World, job: i64, n: i32, status: &str, conclusion: Option<&str>) -> (StatusCode, Value) {
        w.post(&format!("/api/runner/v1/jobs/{job}/steps/{n}"), &self.who(), json!({"status": status, "conclusion": conclusion})).await
    }

    pub async fn complete(&self, w: &World, job: i64, conclusion: &str, outputs: Value) -> (StatusCode, Value) {
        w.post(&format!("/api/runner/v1/jobs/{job}/complete"), &self.who(), json!({"conclusion": conclusion, "outputs": outputs})).await
    }

    /// Takes the next job and finishes it with `conclusion`.
    pub async fn run_one(&self, w: &World, conclusion: &str) -> Option<ch_api_types::runner::JobPayload> {
        let j = self.request(w, 0).await?;
        let (s, v) = self.complete(w, j.job_id, conclusion, json!({})).await;
        assert!(s.is_success(), "{s} {v}");
        Some(j)
    }

    /// Takes jobs and finishes each with success until nothing is queued. Returns their names.
    pub async fn drain(&self, w: &World) -> Vec<String> {
        let mut names = vec![];
        while let Some(j) = self.run_one(w, "success").await {
            names.push(j.name);
        }
        names
    }
}

pub const CI_YML: &str = ".codehaven/workflows/ci.yml";

/// A workflow file that runs `lint` then a two-cell `test` matrix.
pub fn basic_workflow() -> String {
    "name: ci
on: push
jobs:
  lint:
    runs-on: ubuntu-latest
    steps:
      - run: echo lint
  test:
    needs: lint
    runs-on: ubuntu-latest
    strategy:
      fail-fast: false
      matrix:
        os: [a, b]
    steps:
      - uses: actions/checkout@v4
      - run: echo hello
"
    .to_string()
}
