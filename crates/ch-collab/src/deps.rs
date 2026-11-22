//! What the collab services need from the rest of the application.
//!
//! Collab reaches git and identity only through the traits in `ch_core::services`. It does read
//! plain rows from tables the other streams own when that is the honest shape of the question
//! (a username for an id, a repository's merge settings, the next issue number it owns), and it
//! never writes to them.

use ch_core::services::{Audit, Authz, BlobStore, Checks, EventBus, GitService, Repos};
use ch_core::state::{AppState, AppStateBuilder};
use ch_core::{Config, Error, Result};
use sqlx::PgPool;
use std::sync::{Arc, OnceLock};

/// The CI stream is built after this one (`wiring::install_services`: auth, notify, git, collab,
/// ci), so `b.checks` is still the fail-closed placeholder when `build` runs. `workers`, which
/// receives the finished `AppState`, binds the real one here. Until then, and in any process
/// that never calls `workers`, the placeholder answers and required checks count as not
/// satisfied, so a merge can only be blocked, never wrongly allowed.
static LATE_CHECKS: OnceLock<Arc<dyn Checks>> = OnceLock::new();

/// Called once at startup by `ch_collab::workers`.
pub fn bind_checks(checks: Arc<dyn Checks>) {
    let _ = LATE_CHECKS.set(checks);
}

#[derive(Clone)]
pub struct Deps {
    pub db: PgPool,
    pub audit: Arc<dyn Audit>,
    pub events: Arc<dyn EventBus>,
    pub blobs: Arc<dyn BlobStore>,
    pub authz: Arc<dyn Authz>,
    pub git: Arc<dyn GitService>,
    pub repos: Arc<dyn Repos>,
    pub config: Arc<Config>,
    checks: Arc<dyn Checks>,
    /// Tests give an explicit `Checks` and never touch the process-wide binding.
    use_late_checks: bool,
}

impl Deps {
    pub fn from_builder(b: &AppStateBuilder) -> Self {
        Self {
            db: b.db.clone(),
            audit: b.audit.clone(),
            events: b.events.clone(),
            blobs: b.blobs.clone(),
            authz: b.authz.clone(),
            git: b.git.clone(),
            repos: b.repos.clone(),
            config: b.config.clone(),
            checks: b.checks.clone(),
            use_late_checks: true,
        }
    }

    pub fn from_state(s: &AppState) -> Self {
        Self {
            db: s.db.clone(),
            audit: s.audit.clone(),
            events: s.events.clone(),
            blobs: s.blobs.clone(),
            authz: s.authz.clone(),
            git: s.git.clone(),
            repos: s.repos.clone(),
            config: s.config.clone(),
            checks: s.checks.clone(),
            use_late_checks: false,
        }
    }

    /// For tests: the same as `from_builder` but with a fixed `Checks`.
    pub fn with_checks(mut self, checks: Arc<dyn Checks>) -> Self {
        self.checks = checks;
        self.use_late_checks = false;
        self
    }

    pub fn checks(&self) -> Arc<dyn Checks> {
        if self.use_late_checks
            && let Some(c) = LATE_CHECKS.get()
        {
            return c.clone();
        }
        self.checks.clone()
    }

    /// `https://host`, no trailing slash.
    pub fn external_url(&self) -> String {
        self.config.server.external_url.trim_end_matches('/').to_string()
    }
}

/// A short error for something the caller cannot fix.
pub fn internal(msg: &str) -> Error {
    Error::internal(anyhow::anyhow!(msg.to_string()))
}

pub type Res<T> = Result<T>;
