//! `GET /runs/{id}/logs` is a zip with one text file per job, as GitHub's is (ch-1bp).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;
use axum::http::{Method, StatusCode};
use serde_json::json;
use sqlx::PgPool;
use std::io::Read;
use support::*;

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn run_logs_download_as_a_zip_with_a_file_per_job(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, &basic_workflow())]).await;
    let run = w.push(repo, &git, "main", None).await[0];
    let r = w.runner("r1", &["ubuntu-latest"]).await;
    let lint = r.request(&w, 0).await.unwrap();
    let (s, v) = r.logs(&w, lint.job_id, 1, 0, &["lint says hello"]).await;
    assert!(s.is_success(), "{v}");
    r.complete(&w, lint.job_id, "success", json!({})).await;
    r.drain(&w).await;

    let (s, body) = w.send_raw(Method::GET, &format!("/api/v3/repos/alice/demo/actions/runs/{run}/logs"), &Who::User(w.alice), None).await;
    assert_eq!(s, StatusCode::OK);
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(body.to_vec())).expect("a zip");
    let names: Vec<String> = (0..zip.len()).map(|i| zip.by_index(i).unwrap().name().to_string()).collect();
    assert_eq!(names, vec!["0_lint.txt", "1_test (a).txt", "2_test (b).txt"]);
    let mut text = String::new();
    zip.by_name("0_lint.txt").unwrap().read_to_string(&mut text).unwrap();
    assert!(text.contains("lint says hello"), "{text}");

    // Someone who cannot read the repository gets nothing.
    let (s, _) = w.send_raw(Method::GET, &format!("/api/v3/repos/alice/demo/actions/runs/{run}/logs"), &Who::User(w.bob), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}
