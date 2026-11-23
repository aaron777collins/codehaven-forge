//! A queued job no runner can take does not wait for ever (ch-7rq).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;
use ch_core::RunId;
use sqlx::PgPool;
use support::*;

fn windows_workflow() -> String {
    "name: win
on: push
jobs:
  build:
    runs-on: windows-latest
    steps:
      - run: echo hi
"
    .to_string()
}

async fn backdate_queue(w: &World, job: i64, minutes: i32) {
    sqlx::query("UPDATE workflow_jobs SET queued_at = now() - make_interval(mins => $2) WHERE id = $1")
        .bind(job)
        .bind(minutes)
        .execute(w.pool())
        .await
        .unwrap();
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_job_no_runner_can_take_says_so_and_fails_after_a_while(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, &windows_workflow())]).await;
    let run = w.push(repo, &git, "main", None).await[0];
    let _r = w.runner("r1", &["ubuntu-latest", "linux"]).await;
    let job = w.job_id(run, "build").await;

    // Becoming queued stamps the time.
    let stamped: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT queued_at FROM workflow_jobs WHERE id = $1").bind(job).fetch_one(w.pool()).await.unwrap();
    assert!(stamped.is_some(), "queued_at is set when the job is queued");

    // The job page says why it waits, and which labels the runners here offer.
    let alice = ch_testkit::fixtures::user_actor(w.alice);
    let jobs = w.state.pipelines.jobs(&alice, repo, RunId(run)).await.unwrap();
    let note = jobs[0].queue_note.clone().expect("a note for a job no runner can take");
    assert!(note.contains("windows-latest"), "{note}");
    assert!(note.contains("linux, ubuntu-latest"), "{note}");

    // Fresh: nothing happens yet.
    assert_eq!(ch_ci::workers::reap_jobs(&w.ctx).await.unwrap(), 0);
    assert_eq!(w.run_status(run).await.0, "queued");

    // After the no-runner wait it fails, with a line in its log that says why.
    backdate_queue(&w, job, 16).await;
    assert_eq!(ch_ci::workers::reap_jobs(&w.ctx).await.unwrap(), 1);
    assert_eq!(w.run_status(run).await, ("completed".into(), Some("failure".into())));
    let log = String::from_utf8(ch_ci::logs::read_range(&w.ctx, run, job, 0, None).await.unwrap().to_vec()).unwrap();
    assert!(log.contains("No runner has the labels this job asks for: windows-latest"), "{log}");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_job_a_runner_could_take_waits_for_the_queue_timeout(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, &basic_workflow())]).await;
    let run = w.push(repo, &git, "main", None).await[0];
    let _r = w.runner("r1", &["ubuntu-latest"]).await;
    let job = w.job_id(run, "lint").await;

    // A matching runner exists, so no note and no early failure.
    let alice = ch_testkit::fixtures::user_actor(w.alice);
    let jobs = w.state.pipelines.jobs(&alice, repo, RunId(run)).await.unwrap();
    let lint = jobs.iter().find(|j| j.name == "lint").unwrap();
    assert!(lint.queue_note.as_deref().is_none_or(|n| !n.contains("No runner")), "{:?}", lint.queue_note);
    backdate_queue(&w, job, 60).await;
    assert_eq!(ch_ci::workers::reap_jobs(&w.ctx).await.unwrap(), 0, "a runner that is busy is not a reason to fail");

    // A day in the queue is the limit.
    backdate_queue(&w, job, 24 * 60 + 1).await;
    assert_eq!(ch_ci::workers::reap_jobs(&w.ctx).await.unwrap(), 1);
    let jobs = w.jobs(run).await;
    assert_eq!(jobs[0], ("lint".into(), "completed".into(), Some("timed_out".into())));
}
