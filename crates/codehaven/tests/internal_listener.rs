//! The internal listener (`/readyz`, `/metrics`): what it answers, and that the public router
//! has neither.

#![allow(clippy::unwrap_used)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use codehaven::internal;
use codehaven::metrics::Metrics;
use http_body_util::BodyExt;
use sqlx::PgPool;
use tower::ServiceExt;

async fn get(app: &axum::Router, path: &str) -> (StatusCode, String) {
    let resp = app.clone().oneshot(Request::builder().uri(path).body(Body::empty()).unwrap()).await.unwrap();
    let status = resp.status();
    (status, String::from_utf8_lossy(&resp.into_body().collect().await.unwrap().to_bytes()).to_string())
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn ready_when_database_and_logs_dir_work(pool: PgPool) {
    let env = ch_testkit::TestEnv::new(pool).await;
    std::fs::create_dir_all(env.config.logs_dir()).unwrap();
    let state = env.builder().await.build();
    let app = internal::router(state, Metrics::new());
    let (status, body) = get(&app, "/readyz").await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, "ok\n"));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn not_ready_when_the_logs_dir_is_missing(pool: PgPool) {
    let env = ch_testkit::TestEnv::new(pool).await;
    // No logs directory was created.
    let state = env.builder().await.build();
    let app = internal::router(state, Metrics::new());
    let (status, body) = get(&app, "/readyz").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body.contains("not writable"), "{body}");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn metrics_render_prometheus_text(pool: PgPool) {
    let env = ch_testkit::TestEnv::new(pool).await;
    let state = env.builder().await.build();
    let app = internal::router(state, Metrics::new());
    let (status, body) = get(&app, "/metrics").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("codehaven_build_info"));
}

#[tokio::test]
async fn healthcheck_fails_when_nothing_listens() {
    let mut cfg = ch_core::Config::for_tests();
    cfg.server.internal_bind = "127.0.0.1:1".into();
    assert!(!codehaven::doctor::healthcheck(&cfg).await);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn healthcheck_passes_against_a_ready_server(pool: PgPool) {
    let env = ch_testkit::TestEnv::new(pool).await;
    std::fs::create_dir_all(env.config.logs_dir()).unwrap();
    let state = env.builder().await.build();
    let app = internal::router(state, Metrics::new());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut cfg = ch_core::Config::for_tests();
    cfg.server.internal_bind = addr.to_string();
    assert!(codehaven::doctor::healthcheck(&cfg).await);
}
