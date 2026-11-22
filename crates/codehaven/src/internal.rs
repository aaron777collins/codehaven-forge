//! The internal listener: `/readyz` and `/metrics` on `127.0.0.1:9300` inside the container.
//! Nothing outside the container can reach it, and the public router has no such routes, so
//! through Caddy the same paths get the gate's redirect like any other unknown page.

use crate::metrics::Metrics;
use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use ch_core::state::AppState;
use std::sync::Arc;

#[derive(Clone)]
pub struct InternalState {
    pub app: AppState,
    pub metrics: Arc<Metrics>,
}

pub fn router(app: AppState, metrics: Arc<Metrics>) -> Router {
    Router::new().route("/readyz", get(readyz)).route("/metrics", get(metrics_page)).with_state(InternalState { app, metrics })
}

/// Ready when the database answers and the logs directory is writable.
pub async fn readyz(State(s): State<InternalState>) -> Response {
    let db_ok = sqlx::query_scalar::<_, i32>("SELECT 1").fetch_one(&s.app.db).await.is_ok();
    // Probe a directory the server owns. The data root itself may be read-only to it.
    let probe = s.app.config.logs_dir().join(".readyz");
    let disk_ok = tokio::fs::write(&probe, b"ok").await.is_ok();
    let _ = tokio::fs::remove_file(&probe).await;
    if db_ok && disk_ok {
        (StatusCode::OK, "ok\n").into_response()
    } else {
        let msg =
            format!("not ready: database {} data dir {}\n", if db_ok { "ok" } else { "down" }, if disk_ok { "ok" } else { "not writable" });
        (StatusCode::SERVICE_UNAVAILABLE, msg).into_response()
    }
}

async fn metrics_page(State(s): State<InternalState>) -> Response {
    let mut r = s.metrics.render(env!("CARGO_PKG_VERSION")).into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, "text/plain; version=0.0.4".parse().expect("static header"));
    r
}
