//! Everything a handler or worker needs, gathered once.

use crate::shared::{Shared, shared_for};
use crate::source::{GitSource, WorkflowSource};
use ch_core::services::{Audit, Authz, BlobStore, EventBus, GitService, Repos};
use ch_core::state::{AppState, AppStateBuilder};
use ch_core::{Config, Result};
use ch_db::sealing::MasterKey;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use std::sync::Arc;

#[derive(Clone)]
pub struct Ctx {
    pub db: PgPool,
    pub config: Arc<Config>,
    pub audit: Arc<dyn Audit>,
    pub events: Arc<dyn EventBus>,
    pub blobs: Arc<dyn BlobStore>,
    pub authz: Arc<dyn Authz>,
    pub repos: Arc<dyn Repos>,
    pub source: Arc<dyn WorkflowSource>,
    pub git: Arc<dyn GitService>,
    pub shared: Arc<Shared>,
}

impl Ctx {
    pub fn from_state(s: &AppState) -> Self {
        let shared = shared_for(&s.config);
        Self {
            db: s.db.clone(),
            config: s.config.clone(),
            audit: s.audit.clone(),
            events: s.events.clone(),
            blobs: s.blobs.clone(),
            authz: s.authz.clone(),
            repos: s.repos.clone(),
            source: shared.source_override().unwrap_or_else(|| Arc::new(GitSource::new(s.git.clone()))),
            git: s.git.clone(),
            shared,
        }
    }

    pub fn from_builder(b: &AppStateBuilder) -> Self {
        let shared = shared_for(&b.config);
        Self {
            db: b.db.clone(),
            config: b.config.clone(),
            audit: b.audit.clone(),
            events: b.events.clone(),
            blobs: b.blobs.clone(),
            authz: b.authz.clone(),
            repos: b.repos.clone(),
            source: shared.source_override().unwrap_or_else(|| Arc::new(GitSource::new(b.git.clone()))),
            git: b.git.clone(),
            shared,
        }
    }

    /// Replaces where workflow files are read from. Tests use this to read a temporary repository.
    pub fn with_source(mut self, source: Arc<dyn WorkflowSource>) -> Self {
        self.source = source;
        self
    }

    pub fn key(&self) -> Result<&MasterKey> {
        self.shared.key(&self.config)
    }

    pub fn now(&self) -> DateTime<Utc> {
        self.shared.clock().now()
    }

    /// The one public origin, without a trailing slash.
    pub fn base_url(&self) -> String {
        self.config.server.external_url.trim_end_matches('/').to_string()
    }
}
