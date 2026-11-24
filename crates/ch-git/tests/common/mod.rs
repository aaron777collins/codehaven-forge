//! Shared test setup: a real database, a real `GitCore`, the real hook binary, and stand-ins
//! for the identity stream's `Authn` and `Authz` (which do not exist yet in this stream).
#![allow(dead_code, clippy::unwrap_used)]

use async_trait::async_trait;
use axum::Router;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use ch_core::actor::{AuthVia, Scopes};
use ch_core::model::*;
use ch_core::services::{Authn, Authz, OrgRole};
use ch_core::state::AppState;
use ch_core::{Actor, Error, Install, OrgId, Permission, RepoId, Result, TokenId, UserId};
use ch_git::GitCore;
use ch_testkit::TestEnv;
use sqlx::PgPool;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

/// Permission model of ARCHITECTURE.md section 7.2, in SQL, for the tests of this stream.
pub struct FakeAuthz {
    pub pool: PgPool,
}

#[async_trait]
impl Authz for FakeAuthz {
    async fn repo_permission(&self, actor: &Actor, repo: RepoId) -> Result<Permission> {
        let (uid, cap) = match actor {
            Actor::User { id, .. } => (*id, Permission::Admin),
            Actor::Token { user_id, scopes, .. } => (*user_id, scopes.repo_cap()),
            Actor::System => return Ok(Permission::Admin),
            Actor::Job { repo_id, .. } => return Ok(if *repo_id == repo { Permission::Read } else { Permission::None }),
            _ => return Ok(Permission::None),
        };
        let row: Option<(i64, String, String)> =
            sqlx::query_as("SELECT r.owner_id, r.visibility, u.kind FROM repositories r JOIN users u ON u.id = r.owner_id WHERE r.id = $1")
                .bind(repo.0)
                .fetch_optional(&self.pool)
                .await?;
        let Some((owner, visibility, kind)) = row else { return Ok(Permission::None) };
        let mut perm = Permission::None;
        if owner == uid.0 {
            perm = Permission::Admin;
        }
        let collab: Option<(String,)> = sqlx::query_as("SELECT permission FROM repo_collaborators WHERE repo_id = $1 AND user_id = $2")
            .bind(repo.0)
            .bind(uid.0)
            .fetch_optional(&self.pool)
            .await?;
        if let Some((p,)) = collab {
            perm = perm.max(Permission::parse(&p).unwrap_or(Permission::None));
        }
        if kind == "org" {
            let role: Option<(String,)> = sqlx::query_as("SELECT role FROM org_members WHERE org_id = $1 AND user_id = $2")
                .bind(owner)
                .bind(uid.0)
                .fetch_optional(&self.pool)
                .await?;
            match role.as_ref().map(|r| r.0.as_str()) {
                Some("owner") => perm = Permission::Admin,
                Some(_) => perm = perm.max(Permission::Read),
                None => {}
            }
        }
        if visibility == "public" {
            perm = perm.max(Permission::Read);
        }
        Ok(perm.min(cap))
    }

    async fn org_role(&self, actor: &Actor, org: OrgId) -> Result<Option<OrgRole>> {
        let Some(uid) = actor.user_id() else { return Ok(None) };
        let row: Option<(String,)> = sqlx::query_as("SELECT role FROM org_members WHERE org_id = $1 AND user_id = $2")
            .bind(org.0)
            .bind(uid.0)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| if r.0 == "owner" { OrgRole::Owner } else { OrgRole::Member }))
    }

    async fn require(&self, actor: &Actor, repo: RepoId, needed: Permission) -> Result<()> {
        let p = self.repo_permission(actor, repo).await?;
        if p == Permission::None {
            return Err(Error::NotFound);
        }
        if !p.allows(needed) {
            return Err(Error::forbidden(format!("You need {} access to do that.", needed.as_str())));
        }
        Ok(())
    }
}

/// Passwords are `pw-<username>`, tokens `tok-<username>` (scope `repo`), `wf-<username>`
/// (`repo` and `workflow`) and `ro-<username>` (scope `repo:read`). SSH keys are registered by fingerprint.
pub struct FakeAuthn {
    pub pool: PgPool,
    pub keys: Mutex<HashMap<String, SshIdentity>>,
}

impl FakeAuthn {
    async fn user_by_name(&self, name: &str) -> Option<UserId> {
        let r: Option<(i64,)> =
            sqlx::query_as("SELECT id FROM users WHERE username = $1").bind(name).fetch_optional(&self.pool).await.ok()?;
        r.map(|r| UserId(r.0))
    }
}

#[async_trait]
impl Authn for FakeAuthn {
    async fn from_session(&self, _c: &str, _client: &ClientInfo) -> Result<Actor> {
        Ok(Actor::Anonymous)
    }
    async fn from_basic(&self, username: &str, secret: &str, _client: &ClientInfo) -> Result<Actor> {
        let Some(id) = self.user_by_name(username).await else { return Err(Error::unauthorized()) };
        if secret == format!("pw-{username}") {
            return Ok(Actor::User { id, is_admin: false, has_2fa: false, via: AuthVia::Basic });
        }
        let scopes: &[&str] = if secret == format!("tok-{username}") {
            &["repo"]
        } else if secret == format!("wf-{username}") {
            &["repo", "workflow"]
        } else if secret == format!("ro-{username}") {
            &["repo:read"]
        } else {
            return Err(Error::unauthorized());
        };
        Ok(Actor::Token { user_id: id, token_id: TokenId(1), scopes: Scopes::new(scopes.iter().copied()) })
    }
    async fn from_bearer(&self, _t: &str, _c: &ClientInfo) -> Result<Actor> {
        Err(Error::unauthorized())
    }
    async fn from_ssh_key(&self, fingerprint: &str) -> Result<Option<SshIdentity>> {
        Ok(self.keys.lock().unwrap().get(fingerprint).cloned())
    }
}

/// What the real gate does for git and API routes, in a few lines: Basic credentials in, a 401
/// with a challenge out.
async fn fake_gate(State(authn): State<Arc<FakeAuthn>>, mut req: Request, next: Next) -> Response {
    use base64::Engine;
    let creds = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
        .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b.trim()).ok())
        .and_then(|b| String::from_utf8(b).ok())
        .and_then(|s| s.split_once(':').map(|(u, p)| (u.to_string(), p.to_string())));
    let client = ClientInfo { ip: "10.1.2.3".parse().unwrap(), user_agent: Some("tests".into()) };
    let actor = match creds {
        Some((u, p)) => authn.from_basic(&u, &p, &client).await.ok(),
        None => None,
    };
    let Some(actor) = actor else {
        let mut r = (StatusCode::UNAUTHORIZED, "Authentication required\n").into_response();
        r.headers_mut().insert(header::WWW_AUTHENTICATE, "Basic realm=\"Codehaven\"".parse().unwrap());
        return r;
    };
    req.extensions_mut().insert(actor);
    req.extensions_mut().insert(client);
    next.run(req).await
}

pub struct Fixture {
    pub env: TestEnv,
    pub state: AppState,
    pub core: Arc<GitCore>,
    pub authn: Arc<FakeAuthn>,
    pub cancel: CancellationToken,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl Fixture {
    pub async fn new(pool: PgPool) -> Fixture {
        let env = TestEnv::new(pool.clone()).await;
        let mut b = env.builder().await;
        b.authz = Arc::new(FakeAuthz { pool: pool.clone() });
        let authn = Arc::new(FakeAuthn { pool: pool.clone(), keys: Mutex::new(HashMap::new()) });
        b.authn = authn.clone();
        let services = ch_git::build(&b).expect("build git");
        // The hook scripts must run the real hook client, not this test binary.
        services.core.store.install_hooks(Path::new(env!("CARGO_BIN_EXE_ch-hook"))).expect("hooks");
        let core = services.core.clone();
        services.install(&mut b);
        let state = b.build();
        let cancel = CancellationToken::new();
        for w in ch_git::workers(&state) {
            w.spawn(cancel.clone());
        }
        Fixture { env, state, core, authn, cancel }
    }

    pub fn actor(&self, user: UserId) -> Actor {
        Actor::User { id: user, is_admin: false, has_2fa: false, via: AuthVia::Session }
    }

    pub fn client(&self) -> ClientInfo {
        ClientInfo { ip: "10.9.9.9".parse().unwrap(), user_agent: Some("tests".into()) }
    }

    /// Creates a repository the way a person would, through the service.
    pub async fn repo(&self, owner: UserId, name: &str, auto_init: bool) -> Repo {
        self.state
            .repos
            .create(
                &self.actor(owner),
                NewRepo {
                    owner_id: owner,
                    name: name.into(),
                    description: String::new(),
                    visibility: Visibility::Private,
                    default_branch: None,
                    auto_init,
                },
                &self.client(),
            )
            .await
            .expect("create repo")
    }

    pub async fn public_repo(&self, owner: UserId, name: &str) -> Repo {
        self.state
            .repos
            .create(
                &self.actor(owner),
                NewRepo {
                    owner_id: owner,
                    name: name.into(),
                    description: "a repo".into(),
                    visibility: Visibility::Public,
                    default_branch: None,
                    auto_init: true,
                },
                &self.client(),
            )
            .await
            .expect("create repo")
    }

    /// Commits a file through the service and returns the new commit sha.
    pub async fn commit(&self, repo: &Repo, user: UserId, branch: &str, path: &str, content: &str, message: &str) -> String {
        let req = CommitFileChange {
            repo_id: repo.id,
            branch: branch.into(),
            new_branch: None,
            expected_sha: None,
            changes: vec![FileChange { path: path.into(), content: Some(content.as_bytes().to_vec()), renamed_from: None }],
            message: message.into(),
            author: Signature {
                name: "Alice Example".into(),
                email: "alice@test.example".into(),
                at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            },
        };
        self.state.git.commit_file_change(&self.actor(user), req, &self.client()).await.expect("commit file")
    }

    pub async fn branch_sha(&self, repo: &Repo, branch: &str) -> String {
        self.state.git.resolve(repo.id, branch).await.expect("resolve")
    }

    /// Serves the git and REST routes behind the stand-in gate. Returns the address.
    pub async fn serve_http(&self) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new()
            .merge(ch_git::http_routes())
            .nest("/api/v3", ch_git::api_routes())
            .with_state(self.state.clone())
            .layer(from_fn_with_state(self.authn.clone(), fake_gate));
        let cancel = self.cancel.clone();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
                .with_graceful_shutdown(async move { cancel.cancelled().await })
                .await;
        });
        addr
    }

    pub async fn user(&self, name: &str) -> UserId {
        self.env.user(name).await
    }

    /// The repository's events of one kind, oldest first.
    pub async fn events(&self, kind: &str) -> Vec<serde_json::Value> {
        let rows: Vec<(serde_json::Value,)> = sqlx::query_as("SELECT payload FROM outbox_events WHERE kind = $1 ORDER BY id")
            .bind(kind)
            .fetch_all(&self.env.pool)
            .await
            .unwrap();
        rows.into_iter().map(|r| r.0).collect()
    }

    pub async fn audit_actions(&self) -> Vec<String> {
        let rows: Vec<(String,)> = sqlx::query_as("SELECT action FROM audit_events ORDER BY id").fetch_all(&self.env.pool).await.unwrap();
        rows.into_iter().map(|r| r.0).collect()
    }
}

/// A scratch directory and a clean `git` for acting as a client.
pub struct Client {
    pub dir: tempfile::TempDir,
}

pub struct GitOut {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

impl Client {
    pub fn new() -> Client {
        Client { dir: tempfile::tempdir().unwrap() }
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    pub async fn git_in(&self, cwd: &Path, args: &[&str]) -> GitOut {
        self.git_env(cwd, args, &[]).await
    }

    /// Runs the `git` client without blocking the test's runtime. The server under test runs
    /// on the same single-threaded runtime, so a blocking call here would starve it.
    pub async fn git_env(&self, cwd: &Path, args: &[&str], envs: &[(&str, &str)]) -> GitOut {
        let mut c = tokio::process::Command::new("git");
        c.current_dir(cwd)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", self.dir.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_AUTHOR_NAME", "Alice Example")
            .env("GIT_AUTHOR_EMAIL", "alice@test.example")
            .env("GIT_COMMITTER_NAME", "Alice Example")
            .env("GIT_COMMITTER_EMAIL", "alice@test.example")
            .kill_on_drop(true);
        for (k, v) in envs {
            c.env(k, v);
        }
        let out = tokio::time::timeout(std::time::Duration::from_secs(120), c.args(args).output())
            .await
            .expect("git client timed out")
            .expect("run git");
        GitOut {
            ok: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).trim().to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        }
    }

    /// Runs git and panics with its stderr on failure.
    pub async fn git(&self, cwd: &Path, args: &[&str]) -> String {
        let o = self.git_in(cwd, args).await;
        assert!(o.ok, "git {args:?} failed: {}", o.stderr);
        o.stdout
    }

    pub fn write(&self, repo_dir: &Path, rel: &str, content: &str) {
        let p = repo_dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    pub async fn commit_all(&self, repo_dir: &Path, message: &str) -> String {
        self.git(repo_dir, &["add", "-A"]).await;
        self.git(repo_dir, &["commit", "-q", "-m", message]).await;
        self.git(repo_dir, &["rev-parse", "HEAD"]).await
    }
}

/// `http://user:pw@127.0.0.1:port/owner/repo.git`
pub fn http_url(addr: SocketAddr, user: &str, secret: &str, owner: &str, repo: &str) -> String {
    format!("http://{user}:{secret}@{addr}/{owner}/{repo}.git")
}
