//! Issues, labels, milestones, pull requests, reviews, the merge gate, releases and search.
//! It reaches git and identity only through the traits in `ch_core::services`.
//!
//! `Core` is the engine, `services` puts it behind the five traits, `rest` is the `/api/v3`
//! edge and `sync` holds the event consumers. `docs/collab.md` has the map and the rules.

pub mod codeowners;
pub mod comments;
pub mod core;
pub mod deps;
pub mod filter;
pub mod gate;
pub mod issues;
pub mod labels;
pub mod merge;
pub mod protect;
pub mod pulls;
pub mod query;
pub mod queue;
pub mod releases;
pub mod rest;
pub mod reviews;
pub mod search;
pub mod services;
pub mod suggest;
pub mod sync;
pub mod util;
pub mod web;

use axum::Router;
use ch_core::Result;
use ch_core::services::{Issues, Pulls, Releases, Reviews, Search};
use ch_core::state::{AppState, AppStateBuilder, Install, Worker};
use ch_core::web_contract::Tracker;
use std::sync::Arc;

pub use core::Core;
pub use deps::Deps;

/// Mounted under `/api/v3`: issues, labels, milestones, pulls, reviews, search, releases.
pub fn api_routes() -> Router<AppState> {
    rest::routes()
}

/// Event consumers `pr-sync` (branch tracking, merge detection, closing keywords, auto-merge)
/// and `collab-defaults` (starting labels for new repositories).
///
/// This is also where the real CI service reaches the merge gate: `build` runs before the CI
/// crate is built, so it could only see the placeholder. See `deps::bind_checks`.
pub fn workers(state: &AppState) -> Vec<Worker> {
    deps::bind_checks(state.checks.clone());
    sync::workers(Core::new(Deps::from_state(state)))
}

pub struct CollabServices {
    pub issues: Arc<dyn Issues>,
    pub pulls: Arc<dyn Pulls>,
    pub reviews: Arc<dyn Reviews>,
    pub releases: Arc<dyn Releases>,
    pub search: Arc<dyn Search>,
    /// What the web pages call (`ch_core::web_contract`).
    pub tracker: Arc<dyn Tracker>,
}

impl Install for CollabServices {
    fn install(self, b: &mut AppStateBuilder) {
        b.issues = self.issues;
        b.pulls = self.pulls;
        b.reviews = self.reviews;
        b.releases = self.releases;
        b.search = self.search;
        b.tracker = self.tracker;
    }
}

/// Built after identity and git: it reads `b.authz`, `b.git` and `b.repos`.
pub fn build(b: &AppStateBuilder) -> Result<CollabServices> {
    let core = Core::new(Deps::from_builder(b));
    let issues: Arc<dyn Issues> = Arc::new(services::IssuesSvc(core.clone()));
    let reviews: Arc<dyn Reviews> = Arc::new(services::ReviewsSvc(core.clone()));
    let releases: Arc<dyn Releases> = Arc::new(services::ReleasesSvc(core.clone()));
    Ok(CollabServices {
        tracker: Arc::new(web::TrackerSvc { issues: issues.clone(), reviews: reviews.clone(), releases: releases.clone() }),
        issues,
        pulls: Arc::new(services::PullsSvc(core.clone())),
        reviews,
        releases,
        search: Arc::new(services::SearchSvc(core)),
    })
}
