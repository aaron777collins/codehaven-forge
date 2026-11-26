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
    build_state_with(config, pool, ch_auth::AuthOptions::default()).await
}

/// The same with the identity options a test may change (cheap password hashing, a fixed key).
pub async fn build_state_with(config: Arc<Config>, pool: PgPool, auth: ch_auth::AuthOptions) -> Result<AppState> {
    let blobs = FsBlobStore::new(config.blobs_dir()).with_min_free(config.storage.min_free_gib.saturating_mul(1 << 30));
    blobs.init().await?;
    let mut b =
        AppStateBuilder::new(config, pool.clone(), Arc::new(PgAudit::new(pool.clone())), Arc::new(PgEventBus::new(pool)), Arc::new(blobs));
    install_services_with(&mut b, auth)?;
    let state = b.build();
    // The packages pages' service needs the whole state, so it goes in after it is built.
    ch_registry::packages::install(&state);
    Ok(state)
}

/// The service crates, in dependency order. Tests call this with their own builder.
pub fn install_services(b: &mut AppStateBuilder) -> Result<()> {
    install_services_with(b, ch_auth::AuthOptions::default())
}

/// The same with the identity options a test may change.
pub fn install_services_with(b: &mut AppStateBuilder, auth: ch_auth::AuthOptions) -> Result<()> {
    b.ext.insert(Arc::new(ch_core::net::ProxyHosts::default()));
    ch_auth::build_with(b, auth)?.install(b);
    ch_notify::build(b)?.install(b);
    ch_git::build(b)?.install(b);
    ch_collab::build(b)?.install(b);
    ch_ci::build(b)?.install(b);
    ch_pages::build(b)?.install(b);
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
        .merge(ch_ci::twirp_routes())
        .merge(ch_registry::registry_routes())
        .merge(ch_git::http_routes())
        .merge(ch_auth::oauth_routes(config))
        .merge(ch_mcp::routes(config))
        .merge(ch_web::web_routes(config))
}

/// Every background loop.
pub fn workers(state: &AppState) -> Vec<Worker> {
    let mut w = Vec::new();
    // The trusted proxy names, looked up now and every 30 seconds (ch-dc1).
    if let Some(hosts) = state.ext.get::<ch_core::net::ProxyHosts>()
        && !state.config.http.trusted_proxy_hosts.is_empty()
    {
        let names = state.config.http.trusted_proxy_hosts.clone();
        w.push(Worker::new("proxy-hosts", move |token| async move {
            loop {
                hosts.refresh(&names).await;
                tokio::select! {
                    _ = token.cancelled() => return,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {}
                }
            }
        }));
    }
    w.extend(ch_auth::workers(state));
    w.extend(ch_notify::workers(state));
    w.extend(ch_git::workers(state));
    w.extend(ch_ssh::workers(state));
    w.extend(ch_registry::workers(state));
    w.extend(ch_collab::workers(state));
    w.extend(ch_ci::workers(state));
    w.extend(ch_pages::workers(state));
    w
}
