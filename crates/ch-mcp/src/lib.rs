//! The Claude connector: an MCP endpoint at `/mcp`, behind OAuth sign-in.
//!
//! The endpoint speaks three protocol versions: `2026-07-28` (stateless, every request carries its
//! own `_meta`), and `2025-11-25` and `2025-06-18` (an `initialize` handshake, but no session).
//! One JSON-RPC message per POST, a JSON response, no SSE and no batches. Tools call the same
//! services as the web pages, as the connected app's `Actor`, so every permission lives in a
//! service. This crate has no SQL, no loopback HTTP call and no shell.

pub mod limits;
pub mod registry;
pub mod rpc;
pub mod shape;
pub mod tools;

use axum::Router;
use axum::routing::post;
use ch_core::state::AppState;

pub use tools::ToolServices;

/// `POST /mcp`. Registered only while `mcp.enabled` is true, so with MCP off the path is an
/// ordinary unknown one. Any other method on `/mcp` is a plain 405 from the router.
pub fn routes(config: &ch_core::Config) -> Router<AppState> {
    if !config.mcp.enabled {
        return Router::new();
    }
    Router::new().route("/mcp", post(rpc::handle))
}
