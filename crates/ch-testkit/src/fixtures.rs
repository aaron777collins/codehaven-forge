//! Rows and actors for tests. These write straight to the tables, so a stream can test its
//! own crate before the crate that normally creates these rows exists.

use ch_core::actor::{Actor, AuthVia};
use ch_core::{AppStateBuilder, Config, RepoId, UserId};
use ch_db::{FsBlobStore, PgAudit, PgEventBus};
use sqlx::PgPool;
use std::sync::Arc;

pub struct TestEnv {
    pub pool: PgPool,
    pub config: Arc<Config>,
    /// Holds the data directory. Dropped with the env.
    pub dir: tempfile::TempDir,
}

impl TestEnv {
    pub async fn new(pool: PgPool) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = Config::for_tests();
        config.storage.data_dir = dir.path().to_path_buf();
        config.server.external_url = "https://test.example".into();
        Self { pool, config: Arc::new(config), dir }
    }

    /// An `AppStateBuilder` with the real audit log, event bus and blob store, and every
    /// service `NotBuilt`. Install the services under test on it.
    pub async fn builder(&self) -> AppStateBuilder {
        let blobs = FsBlobStore::new(self.config.blobs_dir());
        blobs.init().await.expect("blob store");
        AppStateBuilder::new(
            self.config.clone(),
            self.pool.clone(),
            Arc::new(PgAudit::new(self.pool.clone())),
            Arc::new(PgEventBus::new(self.pool.clone())),
            Arc::new(blobs),
        )
    }

    pub async fn user(&self, username: &str) -> UserId {
        self.insert_user(username, false, false).await
    }

    pub async fn admin(&self, username: &str) -> UserId {
        self.insert_user(username, true, false).await
    }

    pub async fn insert_user(&self, username: &str, is_admin: bool, can_run_ci: bool) -> UserId {
        let id: i64 = sqlx::query_scalar("INSERT INTO users (username, is_admin, can_run_ci) VALUES ($1, $2, $3) RETURNING id")
            .bind(username)
            .bind(is_admin)
            .bind(can_run_ci)
            .fetch_one(&self.pool)
            .await
            .expect("insert user");
        sqlx::query("INSERT INTO user_emails (user_id, email, is_primary, verified_at) VALUES ($1, $2, true, now())")
            .bind(id)
            .bind(format!("{username}@test.example"))
            .execute(&self.pool)
            .await
            .expect("insert email");
        UserId(id)
    }

    pub async fn org(&self, name: &str) -> UserId {
        let id: i64 = sqlx::query_scalar("INSERT INTO users (kind, username) VALUES ('org', $1) RETURNING id")
            .bind(name)
            .fetch_one(&self.pool)
            .await
            .expect("insert org");
        UserId(id)
    }

    pub async fn repo(&self, owner: UserId, name: &str) -> RepoId {
        self.insert_repo(owner, name, "private").await
    }

    pub async fn public_repo(&self, owner: UserId, name: &str) -> RepoId {
        self.insert_repo(owner, name, "public").await
    }

    pub async fn insert_repo(&self, owner: UserId, name: &str, visibility: &str) -> RepoId {
        let id: i64 = sqlx::query_scalar("INSERT INTO repositories (owner_id, name, visibility) VALUES ($1, $2, $3) RETURNING id")
            .bind(owner.0)
            .bind(name)
            .bind(visibility)
            .fetch_one(&self.pool)
            .await
            .expect("insert repo");
        RepoId(id)
    }

    pub async fn collaborator(&self, repo: RepoId, user: UserId, permission: &str) {
        sqlx::query("INSERT INTO repo_collaborators (repo_id, user_id, permission) VALUES ($1, $2, $3)")
            .bind(repo.0)
            .bind(user.0)
            .bind(permission)
            .execute(&self.pool)
            .await
            .expect("insert collaborator");
    }

    pub async fn count(&self, table: &str) -> i64 {
        // Test-only: the table name comes from the test, never from input.
        sqlx::query_scalar(&format!("SELECT count(*) FROM {table}")).fetch_one(&self.pool).await.expect("count")
    }
}

/// A signed-in browser session with two-factor, the only kind that counts as site admin.
pub fn admin_actor(id: UserId) -> Actor {
    Actor::User { id, is_admin: true, has_2fa: true, via: AuthVia::Session }
}

pub fn user_actor(id: UserId) -> Actor {
    Actor::User { id, is_admin: false, has_2fa: false, via: AuthVia::Session }
}

pub fn token_actor(user: UserId, scopes: &[&str]) -> Actor {
    Actor::Token { user_id: user, token_id: ch_core::TokenId(1), scopes: ch_core::Scopes::new(scopes.iter().copied()) }
}

pub fn client() -> ch_core::model::ClientInfo {
    ch_core::model::ClientInfo { ip: "10.9.9.9".parse().expect("ip"), user_agent: Some("ch-testkit".into()) }
}
