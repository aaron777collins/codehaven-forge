//! What a job's own credentials may not do over Actions (C8 review). A job with `contents:
//! write` still never starts, reruns, cancels or switches workflows, or deletes artifacts and
//! caches, and the runtime token an action receives reaches the toolkit and nothing else.

#![allow(clippy::unwrap_used)]

mod support;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode, header};
use serde_json::json;
use sqlx::PgPool;
use support::*;

const WRITER: &str = "name: ci\non:\n  push:\n  workflow_dispatch:\npermissions:\n  contents: write\n  statuses: write\n  issues: write\njobs:\n  b:\n    runs-on: x\n    steps:\n      - run: echo\n";
const BASE: &str = "/api/v3/repos/alice/demo/actions";

fn runtime_token(job_token: &str, run: i64, job: i64, exp: i64) -> String {
    use base64::Engine;
    use hmac::Mac;
    use sha2::Digest;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let claims = json!({"sub": format!("job:{job}"), "scp": format!("Actions.Results:{run}:{job} Actions.UploadArtifacts"), "exp": exp});
    let input = format!("{}.{}", b64.encode(br#"{"alg":"HS256","typ":"JWT"}"#), b64.encode(claims.to_string()));
    let key = sha2::Sha256::digest(job_token.as_bytes());
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(&key).unwrap();
    mac.update(input.as_bytes());
    format!("{input}.{}", b64.encode(mac.finalize().into_bytes()))
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_job_with_contents_write_cannot_run_or_change_actions(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, WRITER)]).await;
    w.push(repo, &git, "main", None).await;
    let r = w.runner("r", &["x"]).await;
    let job = r.request(&w, 0).await.unwrap();
    let jt = Who::Bearer(job.token.clone());
    let run = job.run_id;

    // The token is a writer: it can do what its `permissions:` say.
    let sha = git.head();
    let (s, v) = w.post(&format!("/api/v3/repos/alice/demo/statuses/{sha}"), &jt, json!({"state": "success", "context": "x"})).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");

    // And never starts, stops or switches a workflow, or removes what a run saved.
    let forbidden = |s: StatusCode| assert_eq!(s, StatusCode::FORBIDDEN);
    forbidden(w.post(&format!("{BASE}/workflows/ci.yml/dispatches"), &jt, json!({"ref": "main"})).await.0);
    forbidden(w.post(&format!("{BASE}/runs/{run}/rerun"), &jt, json!({})).await.0);
    forbidden(w.post(&format!("{BASE}/runs/{run}/rerun-failed-jobs"), &jt, json!({})).await.0);
    forbidden(w.post(&format!("{BASE}/runs/{run}/cancel"), &jt, json!({})).await.0);
    forbidden(w.send(Method::PUT, &format!("{BASE}/workflows/ci.yml/disable"), &jt, None).await.0);
    forbidden(w.send(Method::PUT, &format!("{BASE}/workflows/ci.yml/enable"), &jt, None).await.0);
    forbidden(w.send(Method::DELETE, &format!("{BASE}/artifacts/1"), &jt, None).await.0);
    forbidden(w.send(Method::DELETE, &format!("{BASE}/caches/1"), &jt, None).await.0);
    // Nothing started: the one push run is all there is.
    assert_eq!(w.run_ids().await.len(), 1);
    // A person with the same repository still can.
    assert_eq!(w.send(Method::PUT, &format!("{BASE}/workflows/ci.yml/disable"), &Who::User(w.alice), None).await.0, StatusCode::NO_CONTENT);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_runtime_token_reaches_the_toolkit_and_nothing_else(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, WRITER)]).await;
    w.push(repo, &git, "main", None).await;
    let r = w.runner("r", &["x"]).await;
    let job = r.request(&w, 0).await.unwrap();
    let rt = runtime_token(&job.token, job.run_id, job.job_id, chrono::Utc::now().timestamp() + 600);
    let who = Who::Bearer(rt.clone());

    // It does not read the repository, write a status, or open an issue.
    let sha = git.head();
    let (s, _) = w.post(&format!("/api/v3/repos/alice/demo/statuses/{sha}"), &who, json!({"state": "success", "context": "x"})).await;
    assert!(s == StatusCode::FORBIDDEN || s == StatusCode::NOT_FOUND, "status write: {s}");
    let (s, _) = w.post("/api/v3/repos/alice/demo/issues", &who, json!({"title": "t"})).await;
    assert!(s == StatusCode::FORBIDDEN || s == StatusCode::NOT_FOUND, "issue: {s}");
    let (s, _) = w.get("/api/v3/repos/alice/demo", &who).await;
    assert!(s == StatusCode::FORBIDDEN || s == StatusCode::NOT_FOUND, "repo read: {s}");

    // The cache service still answers it.
    let req = Request::builder()
        .method(Method::GET)
        .uri("/api/runner/v1/toolkit/_apis/artifactcache/cache?keys=k&version=v")
        .header(header::AUTHORIZATION, format!("Bearer {rt}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(w.raw(req).await.0, StatusCode::NO_CONTENT);
    // The job token itself keeps its reach.
    let (s, v) = w
        .post(
            &format!("/api/v3/repos/alice/demo/statuses/{sha}"),
            &Who::Bearer(job.token.clone()),
            json!({"state": "success", "context": "x"}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
}
