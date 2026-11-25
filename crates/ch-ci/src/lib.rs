//! CI: workflow triggers, the scheduler, the job queue, the runner API, logs, artifacts, cache,
//! secrets, check runs and commit statuses.
//!
//! Workflows run only for repos whose owner has `can_run_ci`. See ARCHITECTURE.md section 9.
//!
//! How the pieces fit:
//! - [`triggers`] turns outbox events (push, pull request, release) into runs, and [`workers`]
//!   adds schedules and manual runs.
//! - [`runs`] owns the run and job state machine. Everything that changes a run goes through
//!   `advance_run`.
//! - [`runner_api`] is what `haven-runner` calls. [`rest`] is the REST v3 surface.
//! - [`logs`] and [`live`] keep and stream job output, [`store`] keeps artifacts and caches,
//!   [`secrets`] keeps sealed secrets, [`checks`] keeps check runs and statuses.

pub mod action_fetch;
pub mod checks;
pub mod clock;
pub mod ctx;
pub mod discovery;
pub mod history;
pub mod live;
pub mod logs;
pub mod payload;
pub mod rest;
pub mod reusable;
pub mod runner_api;
pub mod runs;
pub mod secrets;
pub mod service;
pub mod shared;
pub mod source;
pub mod spec;
pub mod store;
pub mod toolkit;
pub mod toolkit_artifacts;
pub mod triggers;
pub mod web;
pub mod workers;

use axum::Router;
use ch_core::Result;
use ch_core::services::{Checks, Ci};
use ch_core::state::{AppState, AppStateBuilder, Install, Worker};
use ch_core::web_contract::Pipelines;
use std::sync::Arc;

/// Mounted under `/api/v3`: statuses, check runs, actions, secrets, variables, runners.
pub fn api_routes() -> Router<AppState> {
    rest::routes()
}

/// Mounted under `/api/runner/v1`: register, jobs, logs, artifacts, cache. The gate admits
/// a signed-out request only to `POST /register`.
pub fn runner_routes() -> Router<AppState> {
    runner_api::routes().merge(toolkit::routes())
}

/// Mounted at the root: the artifact service the toolkit finds at the origin of
/// `ACTIONS_RESULTS_URL` (`/twirp/...`). It needs a job's runtime token or job token, like every
/// runner route, so the gate treats the path as one.
pub fn twirp_routes() -> Router<AppState> {
    toolkit_artifacts::routes()
}

/// Event consumer `ci-triggers`, the scheduler, lease reaper, log and artifact retention.
pub fn workers(state: &AppState) -> Vec<Worker> {
    workers::all(state)
}

pub struct CiServices {
    pub checks: Arc<dyn Checks>,
    pub ci: Arc<dyn Ci>,
    /// What the Actions pages call (`ch_core::web_contract`).
    pub pipelines: Arc<dyn Pipelines>,
}

impl Install for CiServices {
    fn install(self, b: &mut AppStateBuilder) {
        b.checks = self.checks;
        b.ci = self.ci;
        b.pipelines = self.pipelines;
    }
}

pub fn build(b: &AppStateBuilder) -> Result<CiServices> {
    let ctx = ctx::Ctx::from_builder(b);
    // Reads (or creates) the master key now, so a broken key fails the start, not the first secret.
    ctx.key()?;
    let ci = Arc::new(service::CiImpl { ctx: ctx.clone() });
    Ok(CiServices { checks: Arc::new(checks::ChecksImpl { ctx }), ci: ci.clone(), pipelines: ci })
}
