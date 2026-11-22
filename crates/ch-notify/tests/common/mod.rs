//! Shared setup for the ch-notify integration tests.
#![allow(clippy::unwrap_used)]
#![allow(dead_code)]

use async_trait::async_trait;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use ch_core::error::{Error, Result};
use ch_core::ids::*;
use ch_core::model::ClientInfo;
use ch_core::services::{Authz, OrgRole};
use ch_core::state::AppState;
use ch_core::{Actor, Permission};
use ch_db::sealing::MasterKey;
use ch_notify::NotifyOptions;
use ch_testkit::TestEnv;
use sqlx::PgPool;
use std::sync::{Arc, Mutex};

/// A small stand-in for the identity crate's `Authz`, reading the same tables: owner, org
/// owner, collaborator, team, org base permission and public repositories.
pub struct DbAuthz(pub PgPool);

#[async_trait]
impl Authz for DbAuthz {
    async fn repo_permission(&self, actor: &Actor, repo: RepoId) -> Result<Permission> {
        let (user, site_admin) = match actor {
            Actor::User { id, .. } => (*id, actor.is_site_admin()),
            Actor::Token { user_id, scopes, .. } => {
                let p = self
                    .repo_permission(&Actor::User { id: *user_id, is_admin: false, has_2fa: false, via: ch_core::AuthVia::Session }, repo)
                    .await?;
                return Ok(p.min(scopes.repo_cap()));
            }
            Actor::System => return Ok(Permission::Admin),
            _ => return Ok(Permission::None),
        };
        let row: Option<(i64, String, String, String, Option<String>, Option<String>, Option<Vec<String>>)> = sqlx::query_as(
            "SELECT r.owner_id, o.kind, r.visibility, o.org_base_permission,
                    (SELECT m.role FROM org_members m WHERE m.org_id = r.owner_id AND m.user_id = $2),
                    (SELECT c.permission FROM repo_collaborators c WHERE c.repo_id = r.id AND c.user_id = $2),
                    (SELECT array_agg(tr.permission) FROM team_repos tr JOIN team_members tm ON tm.team_id = tr.team_id WHERE tr.repo_id = r.id AND tm.user_id = $2)
             FROM repositories r JOIN users o ON o.id = r.owner_id WHERE r.id = $1",
        )
        .bind(repo.0)
        .bind(user.0)
        .fetch_optional(&self.0)
        .await?;
        let Some((owner, kind, vis, base, role, collab, teams)) = row else { return Ok(Permission::None) };
        let mut p = Permission::None;
        if site_admin || (kind == "user" && owner == user.0) || (kind == "org" && role.as_deref() == Some("owner")) {
            p = Permission::Admin;
        }
        if let Some(c) = collab.as_deref().and_then(Permission::parse) {
            p = p.max(c);
        }
        for t in teams.unwrap_or_default() {
            p = p.max(Permission::parse(&t).unwrap_or(Permission::None));
        }
        if kind == "org" && role.is_some() {
            p = p.max(Permission::parse(&base).unwrap_or(Permission::None));
        }
        if vis == "public" {
            p = p.max(Permission::Read);
        }
        Ok(p)
    }

    async fn org_role(&self, actor: &Actor, org: OrgId) -> Result<Option<OrgRole>> {
        if actor.is_site_admin() {
            return Ok(Some(OrgRole::Owner));
        }
        let Some(user) = actor.user_id() else { return Ok(None) };
        let role: Option<String> = sqlx::query_scalar("SELECT role FROM org_members WHERE org_id = $1 AND user_id = $2")
            .bind(org.0)
            .bind(user.0)
            .fetch_optional(&self.0)
            .await?;
        Ok(role.map(|r| if r == "owner" { OrgRole::Owner } else { OrgRole::Member }))
    }

    async fn require(&self, actor: &Actor, repo: RepoId, needed: Permission) -> Result<()> {
        let have = self.repo_permission(actor, repo).await?;
        if have < Permission::Read {
            return Err(Error::NotFound);
        }
        if have < needed {
            return Err(Error::forbidden("You do not have permission to do that."));
        }
        Ok(())
    }
}

pub struct Ctx {
    pub env: TestEnv,
    pub state: AppState,
    pub pool: PgPool,
    pub key: MasterKey,
}

impl Ctx {
    pub fn deliverer(&self, allow_private: bool) -> ch_notify::deliver::Deliverer {
        ch_notify::deliver::Deliverer::new(self.pool.clone(), self.key.clone(), allow_private)
    }
}

pub async fn ctx(pool: PgPool) -> Ctx {
    ctx_with(pool, |_| {}).await
}

pub async fn ctx_with(pool: PgPool, tweak: impl FnOnce(&mut ch_core::Config)) -> Ctx {
    let mut env = TestEnv::new(pool.clone()).await;
    let mut cfg = (*env.config).clone();
    tweak(&mut cfg);
    env.config = Arc::new(cfg);
    let mut b = env.builder().await;
    b.authz = Arc::new(DbAuthz(pool.clone()));
    let key = MasterKey::random();
    let services = ch_notify::build_with(&b, NotifyOptions { master_key: Some(key.clone()) }).unwrap();
    ch_core::Install::install(services, &mut b);
    let state = b.build();
    Ctx { env, state, pool, key }
}

pub fn client() -> ClientInfo {
    ClientInfo { ip: "203.0.113.10".parse().unwrap(), user_agent: Some("test".into()) }
}

pub fn user(id: UserId) -> Actor {
    ch_testkit::fixtures::user_actor(id)
}

pub fn admin_session(id: UserId) -> Actor {
    ch_testkit::fixtures::admin_actor(id)
}

#[derive(Debug, Clone)]
pub struct Received {
    pub headers: HeaderMap,
    pub body: String,
    pub path: String,
}

/// A throwaway HTTP receiver on a local port. `status` is what it answers.
pub struct Sink {
    pub url: String,
    pub received: Arc<Mutex<Vec<Received>>>,
    pub status: Arc<Mutex<u16>>,
    pub reply: Arc<Mutex<String>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Sink {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Clone)]
struct SinkState {
    received: Arc<Mutex<Vec<Received>>>,
    status: Arc<Mutex<u16>>,
    reply: Arc<Mutex<String>>,
}

async fn sink_handler(
    State(s): State<SinkState>,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: String,
) -> (StatusCode, HeaderMap, String) {
    s.received.lock().unwrap().push(Received { headers, body, path: uri.path().to_string() });
    let code = *s.status.lock().unwrap();
    let mut h = HeaderMap::new();
    if code == 302 {
        h.insert("location", "/elsewhere".parse().unwrap());
    }
    (StatusCode::from_u16(code).unwrap(), h, s.reply.lock().unwrap().clone())
}

pub async fn sink() -> Sink {
    let received = Arc::new(Mutex::new(Vec::new()));
    let status = Arc::new(Mutex::new(200u16));
    let reply = Arc::new(Mutex::new("ok".to_string()));
    let st = SinkState { received: received.clone(), status: status.clone(), reply: reply.clone() };
    let app = Router::new().route("/hook", post(sink_handler)).route("/elsewhere", post(sink_handler)).with_state(st);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Sink { url: format!("http://{addr}/hook"), received, status, reply, task }
}

impl Sink {
    pub fn count(&self) -> usize {
        self.received.lock().unwrap().len()
    }
    pub fn set_status(&self, s: u16) {
        *self.status.lock().unwrap() = s;
    }
    pub fn last(&self) -> Received {
        self.received.lock().unwrap().last().cloned().expect("nothing was received")
    }
}

/// Makes every pending delivery due now, so a test does not wait out the retry delays.
pub async fn make_due(pool: &PgPool) {
    sqlx::query("UPDATE webhook_deliveries SET next_attempt_at = now() - interval '1 second' WHERE delivered_at IS NULL AND next_attempt_at IS NOT NULL").execute(pool).await.unwrap();
}

/// An issue row, written directly.
pub async fn issue(pool: &PgPool, repo: RepoId, number: i64, author: UserId, title: &str, body: &str, is_pull: bool) -> i64 {
    sqlx::query_scalar("INSERT INTO issues (repo_id, number, author_id, title, body, is_pull) VALUES ($1, $2, $3, $4, $5, $6) RETURNING id")
        .bind(repo.0)
        .bind(number)
        .bind(author.0)
        .bind(title)
        .bind(body)
        .bind(is_pull)
        .fetch_one(pool)
        .await
        .unwrap()
}
