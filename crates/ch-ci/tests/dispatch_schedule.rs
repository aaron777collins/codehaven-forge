//! Manual runs and scheduled runs.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;
use axum::http::StatusCode;
use chrono::{Duration, TimeZone, Utc};
use serde_json::json;
use sqlx::PgPool;
use support::*;

const DISPATCH: &str = "name: manual
on:
  workflow_dispatch:
    inputs:
      level:
        type: choice
        options: [quick, full]
        default: quick
      reason:
        required: true
        description: why
jobs:
  b:
    runs-on: x
    steps:
      - run: echo ${{ inputs.level }} ${{ github.event.inputs.reason }}
";

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_manual_run_takes_inputs_and_starts_a_run(pool: PgPool) {
    let w = World::new(pool).await;
    let (_, _) = w.repo_with(w.alice, "demo", &[(".codehaven/workflows/manual.yml", DISPATCH)]).await;
    let alice = Who::User(w.alice);
    let uri = "/api/v3/repos/alice/demo/actions/workflows/manual.yml/dispatches";
    let (s, v) = w.post(uri, &alice, json!({"ref": "main", "inputs": {"reason": "testing"}})).await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{v}");
    let run = w.run_ids().await[0];
    let (event, ref_, title): (String, Option<String>, String) =
        sqlx::query_as("SELECT event, git_ref, title FROM workflow_runs").fetch_one(w.pool()).await.unwrap();
    assert_eq!((event.as_str(), ref_.as_deref(), title.as_str()), ("workflow_dispatch", Some("refs/heads/main"), "manual"));
    let payload: serde_json::Value = sqlx::query_scalar("SELECT event_payload FROM workflow_runs").fetch_one(w.pool()).await.unwrap();
    assert_eq!(payload["event"]["inputs"], json!({"level": "quick", "reason": "testing"}), "defaults are filled in");
    assert_eq!(payload["meta"]["inputs"]["level"], "quick");
    let r = w.runner("r", &["x"]).await;
    let job = r.request(&w, 0).await.unwrap();
    assert_eq!(job.run_id, run);
    assert_eq!(job.context.event_name, "workflow_dispatch");
    assert_eq!(job.context.event["inputs"]["reason"], "testing");

    // Each press of the button is a new run.
    assert_eq!(w.post(uri, &alice, json!({"ref": "main", "inputs": {"reason": "again"}})).await.0, StatusCode::NO_CONTENT);
    assert_eq!(w.run_ids().await.len(), 2);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_manual_run_refuses_what_it_cannot_do(pool: PgPool) {
    let w = World::new(pool).await;
    let plain = "name: plain\non: push\njobs:\n  b:\n    runs-on: x\n    steps:\n      - run: echo\n";
    let (repo, _) =
        w.repo_with(w.alice, "demo", &[(".codehaven/workflows/manual.yml", DISPATCH), (".codehaven/workflows/plain.yml", plain)]).await;
    let alice = Who::User(w.alice);
    let base = "/api/v3/repos/alice/demo/actions/workflows";
    // Wrong inputs: all problems at once.
    let (s, v) =
        w.post(&format!("{base}/manual.yml/dispatches"), &alice, json!({"ref": "main", "inputs": {"level": "huge", "extra": 1}})).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(v["errors"].as_array().unwrap().len() >= 3, "{v}");
    // No such ref, no such workflow, no dispatch trigger.
    assert_eq!(
        w.post(&format!("{base}/manual.yml/dispatches"), &alice, json!({"ref": "nope", "inputs": {"reason": "x"}})).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(w.post(&format!("{base}/ghost.yml/dispatches"), &alice, json!({"ref": "main"})).await.0, StatusCode::NOT_FOUND);
    assert_eq!(w.post(&format!("{base}/plain.yml/dispatches"), &alice, json!({"ref": "main"})).await.0, StatusCode::UNPROCESSABLE_ENTITY);
    // Read-only collaborators and strangers cannot.
    w.env.collaborator(repo, w.bob, "read").await;
    assert_eq!(
        w.post(&format!("{base}/manual.yml/dispatches"), &Who::User(w.bob), json!({"ref": "main", "inputs": {"reason": "x"}})).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(w.post(&format!("{base}/manual.yml/dispatches"), &Who::Anon, json!({"ref": "main"})).await.0, StatusCode::UNAUTHORIZED);
    // A disabled workflow does not run, and enabling it again does.
    w.state.ci.workflows(&ch_testkit::fixtures::user_actor(w.alice), repo, None).await.unwrap();
    assert_eq!(w.send(axum::http::Method::PUT, &format!("{base}/manual.yml/disable"), &alice, None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(
        w.post(&format!("{base}/manual.yml/dispatches"), &alice, json!({"ref": "main", "inputs": {"reason": "x"}})).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(w.send(axum::http::Method::PUT, &format!("{base}/manual.yml/enable"), &alice, None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(
        w.post(&format!("{base}/manual.yml/dispatches"), &alice, json!({"ref": "main", "inputs": {"reason": "x"}})).await.0,
        StatusCode::NO_CONTENT
    );
    // And not at all when the owner may not run workflows.
    w.set_ci_allowed(w.alice, false).await;
    assert_eq!(
        w.post(&format!("{base}/manual.yml/dispatches"), &alice, json!({"ref": "main", "inputs": {"reason": "x"}})).await.0,
        StatusCode::FORBIDDEN
    );
}

const NIGHTLY: &str = "name: nightly
on:
  schedule:
    - cron: '*/15 * * * *'
jobs:
  b:
    runs-on: x
    steps:
      - run: echo
";

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn schedules_fire_once_per_slot_on_the_default_branch(pool: PgPool) {
    let w = World::new(pool).await;
    let start = Utc.with_ymd_and_hms(2026, 3, 1, 10, 7, 0).unwrap();
    w.clock.set(start);
    let (repo, git) = w.repo_with(w.alice, "demo", &[(".codehaven/workflows/nightly.yml", NIGHTLY)]).await;
    // A push to the default branch records the schedule.
    w.push(repo, &git, "main", None).await;
    assert_eq!(w.run_ids().await.len(), 0, "the workflow only listens to the schedule");
    let (next,): (chrono::DateTime<Utc>,) = sqlx::query_as("SELECT next_run_at FROM schedules").fetch_one(w.pool()).await.unwrap();
    assert_eq!(next, Utc.with_ymd_and_hms(2026, 3, 1, 10, 15, 0).unwrap());

    assert_eq!(ch_ci::workers::fire_due_schedules(&w.ctx).await.unwrap(), 0, "not due yet");
    w.clock.advance(Duration::minutes(9));
    assert_eq!(ch_ci::workers::fire_due_schedules(&w.ctx).await.unwrap(), 1);
    assert_eq!(ch_ci::workers::fire_due_schedules(&w.ctx).await.unwrap(), 0, "once per slot");
    let (event, git_ref, branch): (String, Option<String>, Option<String>) =
        sqlx::query_as("SELECT event, git_ref, head_branch FROM workflow_runs").fetch_one(w.pool()).await.unwrap();
    assert_eq!((event.as_str(), git_ref.as_deref(), branch.as_deref()), ("schedule", Some("refs/heads/main"), Some("main")));
    let (next,): (chrono::DateTime<Utc>,) = sqlx::query_as("SELECT next_run_at FROM schedules").fetch_one(w.pool()).await.unwrap();
    assert_eq!(next, Utc.with_ymd_and_hms(2026, 3, 1, 10, 30, 0).unwrap());

    // A server that was down for hours fires once, not once per missed slot.
    w.clock.advance(Duration::hours(5));
    assert_eq!(ch_ci::workers::fire_due_schedules(&w.ctx).await.unwrap(), 1);
    assert_eq!(w.run_ids().await.len(), 2);
    let (next,): (chrono::DateTime<Utc>,) = sqlx::query_as("SELECT next_run_at FROM schedules").fetch_one(w.pool()).await.unwrap();
    assert!(next > w.clock.now_for_test());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn schedules_follow_the_workflow_file_and_the_owners_permission(pool: PgPool) {
    let w = World::new(pool).await;
    w.clock.set(Utc.with_ymd_and_hms(2026, 3, 1, 10, 7, 0).unwrap());
    let (repo, git) = w.repo_with(w.alice, "demo", &[(".codehaven/workflows/nightly.yml", NIGHTLY)]).await;
    w.push(repo, &git, "main", None).await;
    assert_eq!(w.env.count("schedules").await, 1);
    // Changing the cron replaces the schedule.
    git.commit_file(".codehaven/workflows/nightly.yml", &NIGHTLY.replace("*/15 * * * *", "0 3 * * *"), "move the schedule");
    let before = git.git(&["rev-parse", "HEAD~1"]);
    w.push(repo, &git, "main", Some(&before)).await;
    let crons: Vec<String> = sqlx::query_scalar("SELECT cron FROM schedules").fetch_all(w.pool()).await.unwrap();
    assert_eq!(crons, vec!["0 3 * * *"]);
    // Taking the permission away stops the runs, the schedule stays for when it comes back.
    w.set_ci_allowed(w.alice, false).await;
    w.clock.advance(Duration::days(2));
    assert_eq!(ch_ci::workers::fire_due_schedules(&w.ctx).await.unwrap(), 0);
    assert_eq!(w.run_ids().await.len(), 0);
    assert_eq!(w.env.count("schedules").await, 1);
    // Deleting the file from the default branch removes the schedule.
    w.set_ci_allowed(w.alice, true).await;
    git.git(&["rm", "-q", ".codehaven/workflows/nightly.yml"]);
    git.git(&["commit", "-q", "-m", "drop it"]);
    let before = git.git(&["rev-parse", "HEAD~1"]);
    w.push(repo, &git, "main", Some(&before)).await;
    w.state.ci.workflows(&ch_testkit::fixtures::user_actor(w.alice), repo, None).await.unwrap();
    // The row of a deleted file stays (it has history) but nothing schedules it.
    sqlx::query("DELETE FROM schedules WHERE next_run_at < now() - interval '1 year'").execute(w.pool()).await.unwrap();
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn run_name_titles_the_run(pool: PgPool) {
    let yml = "name: ci\nrun-name: Deploy by ${{ github.actor }} to ${{ github.ref_name }}\non: push\njobs:\n  b:\n    runs-on: x\n    steps:\n      - run: echo\n";
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, yml)]).await;
    w.push(repo, &git, "main", None).await;
    let title: String = sqlx::query_scalar("SELECT title FROM workflow_runs").fetch_one(w.pool()).await.unwrap();
    assert_eq!(title, "Deploy by alice to main");
}
