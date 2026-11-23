//! Builds the concrete services and routes. Written once in M0: each stream fills in the
//! bodies behind these calls and never touches this file.
//!
//! Build order matters. Identity goes first because everything calls `Authn` and `Authz`,
//! git next, then collab (which reads `b.authz` and `b.git`), then CI.

use axum::Router;
use ch_core::state::{AppState, AppStateBuilder, Install, Worker};
use ch_core::{Config, Result};
use ch_db::{FsBlobStore, PgAudit, PgEventBus};
use sqlx::PgPool;
use std::sync::Arc;

/// Builds the state: the real database-backed pieces, then each crate's services in order.
pub async fn build_state(config: Arc<Config>, pool: PgPool) -> Result<AppState> {
    let blobs = FsBlobStore::new(config.blobs_dir());
    blobs.init().await?;
    let mut b =
        AppStateBuilder::new(config, pool.clone(), Arc::new(PgAudit::new(pool.clone())), Arc::new(PgEventBus::new(pool)), Arc::new(blobs));
    install_services(&mut b)?;
    Ok(b.build())
}

/// The service crates, in dependency order. Tests call this with their own builder.
pub fn install_services(b: &mut AppStateBuilder) -> Result<()> {
    ch_auth::build(b)?.install(b);
    ch_notify::build(b)?.install(b);
    ch_git::build(b)?.install(b);
    ch_collab::build(b)?.install(b);
    ch_ci::build(b)?.install(b);
    Ok(())
}

/// Every route in the application, before the gate and the layers wrap them.
pub fn routes(config: &Config) -> Router<AppState> {
    let api = Router::new()
        .merge(crate::meta::api_routes())
        .merge(ch_auth::api_routes())
        .merge(ch_notify::api_routes())
        .merge(ch_git::api_routes())
        .merge(ch_collab::api_routes())
        .merge(ch_ci::api_routes())
        .fallback(crate::api_errors::not_found)
        .layer(axum::middleware::map_response(crate::api_errors::json_errors));
    Router::new()
        .merge(crate::meta::public_routes())
        .nest("/api/v3", api)
        .nest("/api/runner/v1", ch_ci::runner_routes())
        .merge(ch_registry::registry_routes())
        .merge(ch_git::http_routes())
        .merge(ch_auth::oauth_routes(config))
        .merge(ch_mcp::routes(config))
        .merge(ch_web::web_routes(config))
}

/// Every background loop.
pub fn workers(state: &AppState) -> Vec<Worker> {
    let mut w = Vec::new();
    w.extend(ch_auth::workers(state));
    w.extend(ch_notify::workers(state));
    w.extend(ch_git::workers(state));
    w.extend(ch_ssh::workers(state));
    w.extend(ch_registry::workers(state));
    w.extend(ch_collab::workers(state));
    w.extend(ch_ci::workers(state));
    w
}
