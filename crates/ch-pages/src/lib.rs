//! Codehaven Pages: static sites published from a repository (`docs/design/pages.md`).
//!
//! `build` makes the service the settings pages call, `host_router` answers every request for
//! the pages host (the binary picks it by `Host` before any app layer runs), and `workers`
//! publishes copies after pushes.

pub mod core;
pub mod deploy;
pub mod host;
pub mod inject;
pub mod path;
pub mod service;
pub mod token;

use axum::Router;
use ch_core::Result;
use ch_core::state::{AppState, AppStateBuilder, Install, Worker};
use std::sync::Arc;

pub use crate::core::Core;

pub struct PagesServices {
    core: Arc<Core>,
}

impl Install for PagesServices {
    fn install(self, b: &mut AppStateBuilder) {
        b.ext.insert(self.core.clone());
        b.pages = Arc::new(service::PagesService { core: self.core });
    }
}

/// Needs git, authz and repos in the builder already: call it after `ch_ci::build`.
pub fn build(b: &AppStateBuilder) -> Result<PagesServices> {
    Ok(PagesServices { core: Arc::new(Core::from_builder(b)) })
}

/// The router for the pages host, or `None` when Pages is off.
pub fn host_router(state: &AppState) -> Option<Router> {
    let core = state.ext.get::<Core>()?;
    core.host.as_ref()?;
    Some(host::router(core))
}

/// The publisher, when Pages is on.
pub fn workers(state: &AppState) -> Vec<Worker> {
    match state.ext.get::<Core>() {
        Some(core) if core.host.is_some() => vec![Worker::new("pages-deploy", move |stop| deploy::run(core, stop))],
        _ => Vec::new(),
    }
}
