//! `codehaven serve`: one process runs HTTP, SSH and every worker (ARCHITECTURE.md 14).

use crate::app::build_router;
use crate::internal;
use crate::metrics::Metrics;
use crate::wiring;
use anyhow::{Context, Result};
use ch_core::Config;
use ch_db::sealing::MasterKey;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub fn init_tracing(config: &Config) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.log.level));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    if config.log.format == "pretty" {
        builder.pretty().init();
    } else {
        builder.json().init();
    }
}

/// Opens the database pool from the config.
pub async fn connect(config: &Config) -> Result<sqlx::PgPool> {
    let url = config.database_url()?;
    Ok(ch_db::connect(&url, config.database.max_connections).await?)
}

pub async fn migrate(config: &Config) -> Result<()> {
    let pool = connect(config).await?;
    ch_db::migrate(&pool).await?;
    let (applied, known) = ch_db::migration_status(&pool).await?;
    tracing::info!(applied, known, "migrations applied");
    Ok(())
}

pub async fn serve(config: Config, migrate_first: bool) -> Result<()> {
    let config = Arc::new(config);
    for d in [config.git_dir(), config.blobs_dir(), config.logs_dir()] {
        std::fs::create_dir_all(&d).with_context(|| format!("creating {}", d.display()))?;
    }
    crate::doctor::create_secrets_dir(&config.secrets_dir()).with_context(|| format!("creating {}", config.secrets_dir().display()))?;
    // Creates the master key with mode 0600 on first start, and refuses a bad one.
    MasterKey::load_or_create(&config.master_key_path())?;

    let pool = connect(&config).await?;
    if migrate_first {
        ch_db::migrate(&pool).await?;
    }
    let state = wiring::build_state(config.clone(), pool).await?;
    let metrics = Metrics::new();
    let cancel = CancellationToken::new();

    let workers: Vec<_> = wiring::workers(&state).into_iter().map(|w| w.spawn(cancel.clone())).collect();

    let public: SocketAddr = config.server.bind.parse().context("server.bind")?;
    let internal_addr: SocketAddr = config.server.internal_bind.parse().context("server.internal_bind")?;
    let app = build_router(state.clone(), metrics.clone(), None);
    let internal_app = internal::router(state.clone(), metrics);

    let public_listener = tokio::net::TcpListener::bind(public).await.with_context(|| format!("binding {public}"))?;
    let internal_listener = tokio::net::TcpListener::bind(internal_addr).await.with_context(|| format!("binding {internal_addr}"))?;
    tracing::info!(%public, internal = %internal_addr, external_url = %config.server.external_url, "listening");

    let c1 = cancel.clone();
    let public_task = tokio::spawn(async move {
        axum::serve(public_listener, app.into_make_service_with_connect_info::<SocketAddr>())
            .with_graceful_shutdown(async move { c1.cancelled().await })
            .await
    });
    let c2 = cancel.clone();
    let internal_task = tokio::spawn(async move {
        axum::serve(internal_listener, internal_app).with_graceful_shutdown(async move { c2.cancelled().await }).await
    });

    shutdown_signal().await;
    tracing::info!("shutting down");
    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(20), async {
        let _ = public_task.await;
        let _ = internal_task.await;
        for w in workers {
            let _ = w.await;
        }
    })
    .await;
    Ok(())
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}
