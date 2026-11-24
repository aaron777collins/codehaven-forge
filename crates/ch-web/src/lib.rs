//! The web UI: server-rendered pages, htmx partials, SSE and static files.
//!
//! Handlers call the service traits in `AppState` and shape the answers into view structs
//! for Askama templates (ARCHITECTURE.md section 12). Nothing here talks to the database.
//!
//! Every route registered here is private to signed-out visitors unless it is listed in
//! `ch_core::access::PUBLIC_ROUTES`. The gate in the `codehaven` binary enforces that before
//! a handler runs, so a handler that forgets to check still never serves a visitor.
//!
//! Every POST handler reads its body through `forms::FormData`, which checks the Origin and
//! the CSRF token first, so none can forget.

pub mod assets;
pub mod chrome;
pub mod csrf;
pub mod ctx;
pub mod error;
pub mod fmt;
pub mod forms;
pub mod qr;
pub mod routes;
pub mod views;

#[cfg(feature = "demo")]
pub mod demo;

use axum::Router;
use ch_core::state::AppState;

/// The mark: a rounded arch (a harbour entrance) with a short beam of light across it.
pub const FAVICON_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32"><path d="M6 28V15a10 10 0 0 1 20 0v13" fill="none" stroke="#0f766e" stroke-width="3" stroke-linecap="round"/><path d="M11 20h10" stroke="#0f766e" stroke-width="3" stroke-linecap="round"/></svg>"##;

/// Merged at the root.
pub fn web_routes(config: &ch_core::Config) -> Router<AppState> {
    routes::build_for(config.mcp.enabled).router
}
