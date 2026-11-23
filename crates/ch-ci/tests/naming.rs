//! Names people see for runs, jobs and steps (ch-zwo).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;
use axum::http::StatusCode;
use serde_json::json;
use sqlx::PgPool;
use support::*;

const MATRIX: &str = "name: ci
on: push
jobs:
  test:
    name: Tests
    runs-on: ubuntu-latest
    strategy:
      matrix:
        go: ['1.21', '1.22']
    steps:
      - name: Install Go ${{ matrix.go }}
        run: echo go
      - name: Token ${{ secrets.DEPLOY_KEY }}
        run: echo key
  named:
    name: Build ${{ matrix.os }}
    runs-on: ubuntu-latest
    strategy:
      matrix:
        os: [linux]
    steps:
      - run: echo hi
";

async fn step_names(w: &World, job: i64) -> Vec<String> {
    sqlx::query_scalar("SELECT name FROM workflow_steps WHERE job_id = $1 ORDER BY number").bind(job).fetch_all(w.pool()).await.unwrap()
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn matrix_cells_steps_and_runs_have_names_people_can_tell_apart(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, MATRIX)]).await;
    let alice = ch_testkit::fixtures::user_actor(w.alice);
    ch_ci::secrets::put_secret(&w.ctx, ch_ci::secrets::Scope::Repo(repo.0), "DEPLOY_KEY", "hunter2", Some(w.alice)).await.unwrap();
    let run = w.push(repo, &git, "main", None).await[0];

    // A plain `name:` gets the matrix values, as on GitHub, so the cells and their checks differ.
    let names: Vec<String> = w.jobs(run).await.into_iter().map(|j| j.0).collect();
    assert_eq!(names, vec!["Tests (1.21)", "Tests (1.22)", "Build linux"]);
    let checks: Vec<String> = sqlx::query_scalar("SELECT name FROM check_runs ORDER BY id").fetch_all(w.pool()).await.unwrap();
    assert_eq!(checks, vec!["ci / Tests (1.21)", "ci / Tests (1.22)", "ci / Build linux"]);

    // Step names fill in their expressions, never with a secret.
    let steps = step_names(&w, w.job_id(run, "Tests (1.22)").await).await;
    assert_eq!(steps[0], "Install Go 1.22");
    assert!(!steps[1].contains("hunter2"), "{steps:?}");

    // The list says which workflow ran and what for, and a re-run replaces its row.
    let r = w.runner("r1", &["ubuntu-latest"]).await;
    r.drain(&w).await;
    let (s, v) = w.post(&format!("/api/v3/repos/alice/demo/actions/runs/{run}/rerun"), &Who::User(w.alice), json!({})).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let list = w.state.ci.runs(&alice, repo, 1, 50).await.unwrap();
    assert_eq!(list.len(), 1, "one row per run, showing its latest attempt");
    assert_eq!(list[0].attempt, 2);
    assert_eq!(list[0].workflow_name, "ci");
    assert_eq!(list[0].title, "Add .codehaven/workflows/ci.yml");
}
