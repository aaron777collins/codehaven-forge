//! Runs: from a push to a finished run, the way a person would see it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;
use ch_api_types::runner::StepKind;
use serde_json::json;
use sqlx::PgPool;
use support::*;

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_push_starts_a_run_and_the_jobs_follow_their_needs(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, &basic_workflow())]).await;
    let runs = w.push(repo, &git, "main", None).await;
    assert_eq!(runs.len(), 1);
    let run = runs[0];

    // lint is queued, the two test cells wait for it.
    let jobs = w.jobs(run).await;
    assert_eq!(
        jobs.iter().map(|j| (j.0.as_str(), j.1.as_str())).collect::<Vec<_>>(),
        vec![("lint", "queued"), ("test (a)", "waiting"), ("test (b)", "waiting")]
    );
    // Each job has a check run named after the workflow.
    let checks: Vec<(String, String)> =
        sqlx::query_as("SELECT name, status FROM check_runs ORDER BY id").fetch_all(w.pool()).await.unwrap();
    assert_eq!(checks[0], ("ci / lint".into(), "queued".into()));
    assert_eq!(checks[1].0, "ci / test (a)");

    let r = w.runner("r1", &["ubuntu-latest"]).await;
    // Only lint is on offer.
    let first = r.request(&w, 0).await.expect("lint");
    assert_eq!(first.name, "lint");
    assert!(r.request(&w, 0).await.is_none(), "the test cells wait for lint");
    assert_eq!(w.run_status(run).await.0, "in_progress");

    r.complete(&w, first.job_id, "success", json!({})).await;
    let mut names = r.drain(&w).await;
    names.sort();
    assert_eq!(names, vec!["test (a)", "test (b)"]);
    assert_eq!(w.run_status(run).await, ("completed".into(), Some("success".into())));
    let checks: Vec<(String, Option<String>)> = sqlx::query_as("SELECT status, conclusion FROM check_runs")
        .fetch_all(w.pool())
        .await
        .map(|v: Vec<(String, Option<String>)>| v)
        .unwrap();
    assert!(checks.iter().all(|c| c.0 == "completed" && c.1.as_deref() == Some("success")), "{checks:?}");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_payload_carries_what_the_runner_needs(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, &basic_workflow())]).await;
    w.push(repo, &git, "main", None).await;
    let r = w.runner("r1", &["ubuntu-latest"]).await;
    let lint = r.request(&w, 0).await.unwrap();
    r.complete(&w, lint.job_id, "success", json!({})).await;
    let j = r.request(&w, 0).await.unwrap();
    assert_eq!(j.job_key, "test");
    assert_eq!(j.matrix, json!({"os": "a"}));
    assert_eq!(j.needs["lint"].result, "success");
    assert_eq!(j.repo.full_name, "alice/demo");
    assert_eq!(j.repo.clone_url, "https://test.example/alice/demo.git");
    assert_eq!(j.context.event_name, "push");
    assert_eq!(j.context.git_ref, "refs/heads/main");
    assert_eq!(j.context.sha, git.head());
    assert_eq!(j.context.actor, "alice");
    assert!(j.token.starts_with("chj_"));
    assert_eq!(j.lease_seconds, 120);
    assert_eq!(j.timeout_minutes, 360);
    assert!(matches!(&j.steps[0].kind, StepKind::Uses { uses, .. } if uses == "actions/checkout@v4"));
    assert!(matches!(&j.steps[1].kind, StepKind::Run { run, .. } if run == "echo hello"));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn an_owner_without_permission_gets_no_runs(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.bob, "demo", &[(CI_YML, &basic_workflow())]).await;
    assert!(w.push(repo, &git, "main", None).await.is_empty(), "bob has not been allowed to run workflows");
    assert_eq!(w.state.ci.availability(repo).await.unwrap(), ch_core::model::CiAvailability::OwnerNotAllowed);
    w.set_ci_allowed(w.bob, true).await;
    let ev = ch_core::events::DomainEvent::Push {
        repo_id: repo,
        git_ref: "refs/heads/main".into(),
        before: "0".repeat(40),
        after: git.head(),
        pusher: Some(w.bob),
        forced: false,
        app: None,
        server_built: false,
    };
    assert_eq!(w.emit_and_process(ev).await.len(), 1);
    sqlx::query("UPDATE repositories SET has_actions = false WHERE id = $1").bind(repo.0).execute(w.pool()).await.unwrap();
    assert_eq!(w.state.ci.availability(repo).await.unwrap(), ch_core::model::CiAvailability::TurnedOff);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_failing_job_skips_what_needs_it_and_fails_the_run(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, &basic_workflow())]).await;
    let run = w.push(repo, &git, "main", None).await[0];
    let r = w.runner("r1", &["ubuntu-latest"]).await;
    r.run_one(&w, "failure").await.unwrap();
    assert!(r.request(&w, 0).await.is_none());
    let jobs = w.jobs(run).await;
    assert_eq!(jobs[0].2.as_deref(), Some("failure"));
    assert_eq!(jobs[1].2.as_deref(), Some("skipped"));
    assert_eq!(jobs[2].2.as_deref(), Some("skipped"));
    assert_eq!(w.run_status(run).await, ("completed".into(), Some("failure".into())));
    let c: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT name, conclusion FROM check_runs ORDER BY id").fetch_all(w.pool()).await.unwrap();
    assert_eq!(c[0].1.as_deref(), Some("failure"));
    assert_eq!(c[1].1.as_deref(), Some("skipped"));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn if_conditions_decide_whether_a_job_runs(pool: PgPool) {
    let yml = "name: ci
on: push
jobs:
  build:
    runs-on: x
    steps:
      - run: echo
  on-pr:
    runs-on: x
    if: github.event_name == 'pull_request'
    steps:
      - run: echo
  cleanup:
    needs: [build]
    if: always()
    runs-on: x
    steps:
      - run: echo
  deploy:
    needs: [build]
    if: github.ref == 'refs/heads/main'
    runs-on: x
    steps:
      - run: echo
  on-failure:
    needs: [build]
    if: failure()
    runs-on: x
    steps:
      - run: echo
";
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, yml)]).await;
    let run = w.push(repo, &git, "main", None).await[0];
    let r = w.runner("r1", &["x"]).await;
    let mut names = r.drain(&w).await;
    names.sort();
    assert_eq!(names, vec!["build", "cleanup", "deploy"], "on-pr and on-failure are skipped");
    let jobs = w.jobs(run).await;
    let get = |n: &str| jobs.iter().find(|j| j.0 == n).unwrap().2.clone();
    assert_eq!(get("on-pr").as_deref(), Some("skipped"));
    assert_eq!(get("on-failure").as_deref(), Some("skipped"));
    assert_eq!(get("deploy").as_deref(), Some("success"));
    assert_eq!(w.run_status(run).await.1.as_deref(), Some("success"));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn failure_and_always_conditions_after_a_failed_job(pool: PgPool) {
    let yml = "name: ci
on: push
jobs:
  build:
    runs-on: x
    steps:
      - run: echo
  notify:
    needs: build
    if: failure()
    runs-on: x
    steps:
      - run: echo
  cleanup:
    needs: build
    if: always()
    runs-on: x
    steps:
      - run: echo
  deploy:
    needs: build
    runs-on: x
    steps:
      - run: echo
";
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, yml)]).await;
    let run = w.push(repo, &git, "main", None).await[0];
    let r = w.runner("r1", &["x"]).await;
    r.run_one(&w, "failure").await.unwrap();
    let mut names = r.drain(&w).await;
    names.sort();
    assert_eq!(names, vec!["cleanup", "notify"]);
    let jobs = w.jobs(run).await;
    assert_eq!(jobs.iter().find(|j| j.0 == "deploy").unwrap().2.as_deref(), Some("skipped"));
    assert_eq!(w.run_status(run).await.1.as_deref(), Some("failure"));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn fail_fast_stops_the_other_cells(pool: PgPool) {
    let yml = "name: ci
on: push
jobs:
  test:
    runs-on: x
    strategy:
      matrix:
        n: [1, 2, 3]
    steps:
      - run: echo
";
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, yml)]).await;
    let run = w.push(repo, &git, "main", None).await[0];
    let r = w.runner("r1", &["x"]).await;
    let first = r.request(&w, 0).await.unwrap();
    let second = r.request(&w, 0).await.unwrap();
    r.complete(&w, first.job_id, "failure", json!({})).await;
    // The running sibling is told to stop, the queued one never starts.
    let (_, hb) = r.heartbeat(&w, second.job_id).await;
    assert_eq!(hb["cancel"], true);
    assert!(r.request(&w, 0).await.is_none());
    r.complete(&w, second.job_id, "cancelled", json!({})).await;
    let jobs = w.jobs(run).await;
    assert_eq!(jobs.iter().filter(|j| j.2.as_deref() == Some("cancelled")).count(), 2);
    assert_eq!(w.run_status(run).await.1.as_deref(), Some("failure"));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn without_fail_fast_every_cell_runs(pool: PgPool) {
    let yml = "name: ci
on: push
jobs:
  test:
    runs-on: x
    strategy:
      fail-fast: false
      matrix:
        n: [1, 2, 3]
    steps:
      - run: echo
";
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, yml)]).await;
    let run = w.push(repo, &git, "main", None).await[0];
    let r = w.runner("r1", &["x"]).await;
    r.run_one(&w, "failure").await.unwrap();
    assert_eq!(r.drain(&w).await.len(), 2);
    assert_eq!(w.run_status(run).await.1.as_deref(), Some("failure"));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn max_parallel_limits_cells_in_flight(pool: PgPool) {
    let yml = "name: ci
on: push
jobs:
  test:
    runs-on: x
    strategy:
      max-parallel: 2
      matrix:
        n: [1, 2, 3, 4]
    steps:
      - run: echo
";
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, yml)]).await;
    w.push(repo, &git, "main", None).await;
    let r = w.runner("r1", &["x"]).await;
    let a = r.request(&w, 0).await.unwrap();
    let _b = r.request(&w, 0).await.unwrap();
    assert!(r.request(&w, 0).await.is_none(), "two at a time");
    r.complete(&w, a.job_id, "success", json!({})).await;
    assert!(r.request(&w, 0).await.is_some(), "finishing one lets the next start");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_job_allowed_to_fail_does_not_fail_the_run(pool: PgPool) {
    let yml = "name: ci
on: push
jobs:
  experiment:
    runs-on: x
    continue-on-error: true
    steps:
      - run: echo
  after:
    needs: experiment
    runs-on: x
    steps:
      - run: echo
";
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, yml)]).await;
    let run = w.push(repo, &git, "main", None).await[0];
    let r = w.runner("r1", &["x"]).await;
    r.run_one(&w, "failure").await.unwrap();
    assert_eq!(r.drain(&w).await, vec!["after"], "a job allowed to fail still lets its dependents run");
    assert_eq!(w.run_status(run).await.1.as_deref(), Some("success"));
    let c: Option<String> =
        sqlx::query_scalar("SELECT conclusion FROM check_runs WHERE name = 'ci / experiment'").fetch_one(w.pool()).await.unwrap();
    assert_eq!(c.as_deref(), Some("neutral"), "the commit does not turn red");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_matrix_built_from_a_needed_jobs_output(pool: PgPool) {
    let yml = "name: ci
on: push
jobs:
  plan:
    runs-on: x
    outputs:
      m: ${{ steps.p.outputs.m }}
    steps:
      - id: p
        run: echo
  test:
    needs: plan
    runs-on: x
    strategy:
      matrix: ${{ fromJSON(needs.plan.outputs.m) }}
    steps:
      - run: echo ${{ matrix.os }}
";
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, yml)]).await;
    let run = w.push(repo, &git, "main", None).await[0];
    let r = w.runner("r1", &["x"]).await;
    let plan = r.request(&w, 0).await.unwrap();
    assert_eq!(w.jobs(run).await.len(), 2, "one placeholder for the matrix until its needs finish");
    r.complete(&w, plan.job_id, "success", json!({"m": "{\"os\": [\"a\", \"b\", \"c\"]}"})).await;
    let jobs = w.jobs(run).await;
    assert_eq!(jobs.len(), 4, "{jobs:?}");
    let mut names: Vec<String> = jobs.iter().map(|j| j.0.clone()).collect();
    names.sort();
    assert_eq!(names, vec!["plan", "test (a)", "test (b)", "test (c)"]);
    let mut got = r.drain(&w).await;
    got.sort();
    assert_eq!(got, vec!["test (a)", "test (b)", "test (c)"]);
    assert_eq!(w.run_status(run).await.1.as_deref(), Some("success"));
    let checks: Vec<String> = sqlx::query_scalar("SELECT name FROM check_runs ORDER BY name").fetch_all(w.pool()).await.unwrap();
    assert_eq!(checks, vec!["ci / plan", "ci / test (a)", "ci / test (b)", "ci / test (c)"]);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn cancelling_a_run(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, &basic_workflow())]).await;
    let run = w.push(repo, &git, "main", None).await[0];
    let r = w.runner("r1", &["ubuntu-latest"]).await;
    let lint = r.request(&w, 0).await.unwrap();
    let who = Who::User(w.alice);
    let (s, _) = w.post(&format!("/api/v3/repos/alice/demo/actions/runs/{run}/cancel"), &who, json!({})).await;
    assert_eq!(s, 202);
    // What had not started is cancelled at once, the running job learns at its next heartbeat.
    let jobs = w.jobs(run).await;
    assert_eq!(jobs[1].2.as_deref(), Some("cancelled"));
    assert_eq!(jobs[0].1, "in_progress");
    assert_eq!(r.heartbeat(&w, lint.job_id).await.1["cancel"], true);
    r.complete(&w, lint.job_id, "cancelled", json!({})).await;
    assert_eq!(w.run_status(run).await, ("completed".into(), Some("cancelled".into())));
    // Cancelling again is a conflict, and bob cannot cancel at all.
    let (s, _) = w.post(&format!("/api/v3/repos/alice/demo/actions/runs/{run}/cancel"), &who, json!({})).await;
    assert_eq!(s, 409);
    let (s, _) = w.post(&format!("/api/v3/repos/alice/demo/actions/runs/{run}/cancel"), &Who::User(w.bob), json!({})).await;
    assert_eq!(s, 404, "a private repo bob cannot see does not exist for him");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn re_running_all_jobs_and_only_the_failed_ones(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, &basic_workflow())]).await;
    let run = w.push(repo, &git, "main", None).await[0];
    let r = w.runner("r1", &["ubuntu-latest"]).await;
    // lint passes, test (a) passes, test (b) fails.
    r.run_one(&w, "success").await.unwrap();
    let a = r.request(&w, 0).await.unwrap();
    let b = r.request(&w, 0).await.unwrap();
    r.complete(&w, a.job_id, "success", json!({})).await;
    r.complete(&w, b.job_id, "failure", json!({})).await;
    assert_eq!(w.run_status(run).await.1.as_deref(), Some("failure"));
    let who = Who::User(w.alice);

    let (s, _) = w.post(&format!("/api/v3/repos/alice/demo/actions/runs/{run}/rerun-failed-jobs"), &who, json!({})).await;
    assert_eq!(s, 201);
    let runs = w.run_ids().await;
    assert_eq!(runs.len(), 2);
    let second = runs[1];
    let attempt: i32 =
        sqlx::query_scalar("SELECT attempt FROM workflow_runs WHERE id = $1").bind(second).fetch_one(w.pool()).await.unwrap();
    assert_eq!(attempt, 2);
    let jobs = w.jobs(second).await;
    assert_eq!(jobs[0], ("lint".into(), "completed".into(), Some("success".into())), "kept");
    assert_eq!(jobs[1], ("test (a)".into(), "completed".into(), Some("success".into())), "kept");
    assert_eq!(jobs[2].1, "queued", "only the failed one runs again");
    assert_eq!(r.drain(&w).await, vec!["test (b)"]);
    assert_eq!(w.run_status(second).await.1.as_deref(), Some("success"));
    // The commit shows the newest attempt of each check.
    let checks = w.state.checks.check_runs_for_sha(repo, &git.head()).await.unwrap();
    assert_eq!(checks.len(), 3);
    assert!(checks.iter().all(|c| c.conclusion.as_deref() == Some("success")), "{checks:?}");

    // A full re-run starts everything again.
    let (s, _) = w.post(&format!("/api/v3/repos/alice/demo/actions/runs/{second}/rerun"), &who, json!({})).await;
    assert_eq!(s, 201);
    let third = *w.run_ids().await.last().unwrap();
    assert_eq!(w.jobs(third).await[0].1, "queued");
    // A run still going cannot be re-run.
    let (s, _) = w.post(&format!("/api/v3/repos/alice/demo/actions/runs/{third}/rerun"), &who, json!({})).await;
    assert_eq!(s, 409);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn concurrency_groups_cancel_or_queue(pool: PgPool) {
    let yml = "name: ci
on: push
concurrency:
  group: deploy-${{ github.ref }}
  cancel-in-progress: true
jobs:
  deploy:
    runs-on: x
    steps:
      - run: echo
";
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, yml)]).await;
    let first = w.push(repo, &git, "main", None).await[0];
    let r = w.runner("r1", &["x"]).await;
    let j1 = r.request(&w, 0).await.unwrap();
    git.commit_file("a.txt", "x", "second");
    let before = git.git(&["rev-parse", "HEAD~1"]);
    let second = *w.push(repo, &git, "main", Some(&before)).await.last().unwrap();
    assert_ne!(first, second);
    assert_eq!(r.heartbeat(&w, j1.job_id).await.1["cancel"], true, "the older run is told to stop");
    r.complete(&w, j1.job_id, "cancelled", json!({})).await;
    assert_eq!(w.run_status(first).await.1.as_deref(), Some("cancelled"));
    assert_eq!(r.request(&w, 0).await.unwrap().run_id, second);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn concurrency_without_cancel_holds_the_newer_run_and_replaces_a_pending_one(pool: PgPool) {
    let yml = "name: ci
on: push
concurrency: build
jobs:
  b:
    runs-on: x
    steps:
      - run: echo
";
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, yml)]).await;
    let one = w.push(repo, &git, "main", None).await[0];
    let r = w.runner("r1", &["x"]).await;
    let j1 = r.request(&w, 0).await.unwrap();
    // Two more pushes while the first runs. Only the newest waits.
    let mut prev = git.head();
    git.commit_file("a.txt", "1", "two");
    let two = *w.push(repo, &git, "main", Some(&prev)).await.last().unwrap();
    prev = git.head();
    git.commit_file("a.txt", "2", "three");
    let three = *w.push(repo, &git, "main", Some(&prev)).await.last().unwrap();
    assert!(r.request(&w, 0).await.is_none(), "held behind the running one");
    assert_eq!(w.run_status(two).await.1.as_deref(), Some("cancelled"), "a pending run is replaced by a newer one");
    assert_eq!(w.run_status(one).await.0, "in_progress", "the running one is left alone");
    r.complete(&w, j1.job_id, "success", json!({})).await;
    assert_eq!(r.request(&w, 0).await.unwrap().run_id, three, "the newest goes next");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_same_event_never_starts_the_same_workflow_twice(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, &basic_workflow())]).await;
    w.push(repo, &git, "main", None).await;
    // The consumer crashed before acknowledging: the event is delivered again.
    sqlx::query("UPDATE event_cursors SET last_id = 0 WHERE consumer = 'ci-triggers'").execute(w.pool()).await.unwrap();
    let mut failures = Default::default();
    ch_ci::triggers::process_batch(&w.ctx, &mut failures).await.unwrap();
    assert_eq!(w.run_ids().await.len(), 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn branch_and_path_filters_decide_which_pushes_run(pool: PgPool) {
    let yml = "name: ci
on:
  push:
    branches: [main]
    paths: ['src/**']
jobs:
  b:
    runs-on: x
    steps:
      - run: echo
";
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, yml)]).await;
    // A new branch with unknown changes runs (nothing says it should not).
    assert_eq!(w.push(repo, &git, "main", None).await.len(), 1);
    // A docs-only change on main does not.
    let before = git.head();
    git.commit_file("docs/a.md", "x", "docs");
    assert_eq!(w.push(repo, &git, "main", Some(&before)).await.len(), 1, "docs only");
    // A source change does.
    let before = git.head();
    git.commit_file("src/a.rs", "x", "code");
    assert_eq!(w.push(repo, &git, "main", Some(&before)).await.len(), 2);
    // Another branch does not.
    git.branch("dev");
    let before = git.head();
    git.commit_file("src/b.rs", "x", "code on dev");
    assert_eq!(w.push(repo, &git, "dev", Some(&before)).await.len(), 2);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_broken_workflow_file_does_not_stop_the_others(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w
        .repo_with(
            w.alice,
            "demo",
            &[(".codehaven/workflows/broken.yml", "on: push\njobs:\n  a:\n    steps: []\n"), (CI_YML, &basic_workflow())],
        )
        .await;
    // The good workflow runs, and the broken one shows as a failed run that says why (ch-xy5).
    let runs = w.push(repo, &git, "main", None).await;
    assert_eq!(runs.len(), 2);
    let statuses: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT status, conclusion FROM workflow_runs ORDER BY event_payload->'meta'->>'workflow_path'")
            .fetch_all(w.pool())
            .await
            .unwrap();
    assert_eq!(statuses, vec![("completed".into(), Some("failure".into())), ("queued".into(), None)]);
    let files = w.state.ci.workflows(&ch_testkit::fixtures::user_actor(w.alice), repo, None).await.unwrap();
    let broken = files.iter().find(|f| f.path.ends_with("broken.yml")).unwrap();
    assert!(!broken.errors.is_empty());
    assert!(broken.errors[0].starts_with("line "), "{:?}", broken.errors);
    let ci = files.iter().find(|f| f.path == CI_YML).unwrap();
    assert!(ci.errors.is_empty());
    assert_eq!(ci.events, vec!["push"]);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_codehaven_workflow_wins_over_a_github_one_of_the_same_name(pool: PgPool) {
    let gh = "name: from github\non: push\njobs:\n  g:\n    runs-on: x\n    steps:\n      - run: echo\n";
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(".github/workflows/ci.yml", gh), (CI_YML, &basic_workflow())]).await;
    let runs = w.push(repo, &git, "main", None).await;
    assert_eq!(runs.len(), 1);
    let name: String = sqlx::query_scalar("SELECT job_key FROM workflow_jobs WHERE run_id = $1 ORDER BY id LIMIT 1")
        .bind(runs[0])
        .fetch_one(w.pool())
        .await
        .unwrap();
    assert_eq!(name, "lint");
    // Turn the .github directory off and a differently named file there is ignored.
    sqlx::query("UPDATE repositories SET read_github_workflows = false WHERE id = $1").bind(repo.0).execute(w.pool()).await.unwrap();
    let files = w.state.ci.workflows(&ch_testkit::fixtures::user_actor(w.alice), repo, None).await.unwrap();
    assert_eq!(files.len(), 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn events_are_emitted_for_every_change(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, &basic_workflow())]).await;
    w.push(repo, &git, "main", None).await;
    let r = w.runner("r1", &["ubuntu-latest"]).await;
    r.run_one(&w, "success").await.unwrap();
    r.drain(&w).await;
    let kinds: Vec<&'static str> = w.events().await.iter().map(|e| e.event.kind()).collect();
    for k in ["push", "workflow_run_updated", "workflow_job_updated", "check_run_updated"] {
        assert!(kinds.contains(&k), "{k} missing from {kinds:?}");
    }
    // The run's last event says it completed.
    let last =
        w.events().await.into_iter().rev().find(|e| matches!(e.event, ch_core::events::DomainEvent::WorkflowRunUpdated { .. })).unwrap();
    assert!(
        matches!(last.event, ch_core::events::DomainEvent::WorkflowRunUpdated { ref status, ref conclusion, .. } if status == "completed" && conclusion.as_deref() == Some("success"))
    );
}

/// ch-5ke and ch-4pn.12: a re-run is recorded as started by whoever re-ran it, not by the
/// original actor, and it keeps the tier the run was recorded at, so what strangers may see of it
/// does not widen because the repository did.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_rerun_records_who_reran_it_and_keeps_the_recorded_tier(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, &basic_workflow())]).await;
    let run = w.push(repo, &git, "main", None).await[0];
    let r = w.runner("r1", &["ubuntu-latest"]).await;
    r.drain(&w).await;
    assert_eq!(w.run_status(run).await.0, "completed");
    w.env.collaborator(repo, w.bob, "write").await;
    w.set_ci_allowed(w.bob, true).await;
    // Recorded while private; the repository has since been widened to internal.
    sqlx::query("UPDATE workflow_runs SET recorded_tier = 'private' WHERE id = $1").bind(run).execute(w.pool()).await.unwrap();
    sqlx::query("UPDATE repositories SET access_tier = 'internal' WHERE id = $1").bind(repo.0).execute(w.pool()).await.unwrap();

    let (s, _) = w.post(&format!("/api/v3/repos/alice/demo/actions/runs/{run}/rerun"), &Who::User(w.bob), json!({})).await;
    assert_eq!(s, 201);
    let second = *w.run_ids().await.last().unwrap();
    let (actor, tier): (Option<i64>, Option<String>) =
        sqlx::query_as("SELECT actor_id, recorded_tier FROM workflow_runs WHERE id = $1").bind(second).fetch_one(w.pool()).await.unwrap();
    assert_eq!(actor, Some(w.bob.0), "the person who re-ran it");
    assert_eq!(tier.as_deref(), Some("private"), "the re-run keeps the old recorded tier");
}
