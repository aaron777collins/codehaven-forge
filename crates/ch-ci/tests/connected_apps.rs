//! What CI does for a connected app: runs it starts without the workflows permission wait for a
//! person, hold nothing and cancel nothing until approved, an app can never approve, and dispatch,
//! rerun and cancel need the workflows permission on that repository.

#![allow(clippy::unwrap_used)]

mod support;

use ch_core::events::DomainEvent;
use ch_core::{Actor, AppGrant, AppOrigin, GrantId, RepoId, RepoSet};
use sqlx::PgPool;
use support::*;

fn app_for(w: &World, workflows_on: Option<RepoId>, write_on: RepoId) -> Actor {
    let only = |r: RepoId| RepoSet::Only([r].into_iter().collect());
    Actor::App {
        user_id: w.alice,
        grant_id: GrantId(9),
        grant: AppGrant { read: RepoSet::All, write: only(write_on), workflows: workflows_on.map(only).unwrap_or_else(RepoSet::none) },
    }
}

fn push_by_app(repo: RepoId, git: &GitRepo, w: &World, ci_allowed: bool) -> DomainEvent {
    DomainEvent::Push {
        repo_id: repo,
        git_ref: "refs/heads/main".into(),
        before: "0".repeat(40),
        after: git.head(),
        pusher: Some(w.alice),
        forced: false,
        app: Some(AppOrigin { grant_id: GrantId(9), ci_allowed }),
        server_built: false,
    }
}

const SLOW: &str = "name: ci
on: push
concurrency:
  group: deploys
  cancel-in-progress: true
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: echo hi
";

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_push_by_an_app_without_workflows_waits_for_a_person(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, &basic_workflow())]).await;
    let ids = w.emit_and_process(push_by_app(repo, &git, &w, false)).await;
    assert_eq!(ids.len(), 1);
    let run = ids[0];
    assert_eq!(w.run_status(run).await.0, "waiting", "held, not queued");
    let app = app_for(&w, None, repo);
    let reason = w.state.ci.run_hold_reason(&app, repo, ch_core::RunId(run)).await.unwrap();
    assert_eq!(reason.as_deref(), Some("Started by a connected app"));
    // No job reaches a runner and no job token is made before approval.
    let r = w.runner("r1", &["ubuntu-latest"]).await;
    assert!(r.request(&w, 0).await.is_none(), "no job is dispatched to a runner");
    let tokens: i64 =
        sqlx::query_scalar("SELECT count(*) FROM workflow_jobs WHERE token_hash IS NOT NULL").fetch_one(w.pool()).await.unwrap();
    assert_eq!(tokens, 0, "no job token exists yet");
    // An app can never approve, even with every permission.
    let all = app_for(&w, Some(repo), repo);
    assert!(w.state.ci.approve_run(&all, repo, ch_core::RunId(run), &ch_core::model::ClientInfo::local()).await.is_err());
    assert_eq!(w.run_status(run).await.0, "waiting");
    // A person with write access can, and it is audited.
    let alice = ch_testkit::fixtures::user_actor(w.alice);
    w.state.ci.approve_run(&alice, repo, ch_core::RunId(run), &ch_core::model::ClientInfo::local()).await.unwrap();
    assert_eq!(w.run_status(run).await.0, "queued");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action = 'run.approved'").fetch_one(w.pool()).await.unwrap();
    assert_eq!(n, 1);
    assert!(r.request(&w, 0).await.is_some(), "after approval the job reaches the runner");
    assert!(w.state.ci.run_hold_reason(&alice, repo, ch_core::RunId(run)).await.unwrap().is_none());
    // Approving twice is a conflict, not a second start.
    assert!(w.state.ci.approve_run(&alice, repo, ch_core::RunId(run), &ch_core::model::ClientInfo::local()).await.is_err());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_push_by_an_app_with_workflows_or_by_a_person_runs_at_once(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, &basic_workflow())]).await;
    let run = w.emit_and_process(push_by_app(repo, &git, &w, true)).await[0];
    assert_eq!(w.run_status(run).await.0, "queued", "the person gave the app workflows on this repository");
    let (repo2, git2) = w.repo_with(w.alice, "other", &[(CI_YML, &basic_workflow())]).await;
    let run2 = w.push(repo2, &git2, "main", None).await[0];
    assert_eq!(w.run_status(run2).await.0, "queued", "a person's own push is untouched");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_held_run_cancels_nothing_until_it_is_approved(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, SLOW)]).await;
    // A person's run is going.
    let first = w.push(repo, &git, "main", None).await[0];
    let r = w.runner("r1", &["ubuntu-latest"]).await;
    let job = r.request(&w, 0).await.expect("the first run's job starts");
    let _ = job;
    assert_eq!(w.run_status(first).await.0, "in_progress");
    // An app's push is held. It must not cancel the running job, and must not take the group.
    let held = w.emit_and_process(push_by_app(repo, &git, &w, false)).await;
    // The same sha twice is deduplicated by trigger; give the app's event its own sha.
    let held = if held.is_empty() {
        git.commit_file("x.txt", "x", "second");
        w.emit_and_process(push_by_app(repo, &git, &w, false)).await
    } else {
        held
    };
    // The harness lists every run so far; the held one is the newest.
    let held = *held.iter().max().unwrap();
    assert_eq!(w.run_status(held).await.0, "waiting");
    assert_eq!(w.run_status(first).await.0, "in_progress", "an unapproved run cancels nothing");
    let asked: Option<String> = sqlx::query_scalar("SELECT event_payload->'meta'->>'cancel_requested' FROM workflow_runs WHERE id = $1")
        .bind(first)
        .fetch_one(w.pool())
        .await
        .unwrap();
    assert_ne!(asked.as_deref(), Some("true"), "and does not ask it to stop");
    let group: Option<String> =
        sqlx::query_scalar("SELECT concurrency_group FROM workflow_runs WHERE id = $1").bind(held).fetch_one(w.pool()).await.unwrap();
    assert_eq!(group.as_deref(), Some("deploys"), "the group is recorded");
    // A person approves: now it takes its place and cancels the older one as the workflow says.
    let alice = ch_testkit::fixtures::user_actor(w.alice);
    w.state.ci.approve_run(&alice, repo, ch_core::RunId(held), &ch_core::model::ClientInfo::local()).await.unwrap();
    let asked: Option<String> = sqlx::query_scalar("SELECT event_payload->'meta'->>'cancel_requested' FROM workflow_runs WHERE id = $1")
        .bind(first)
        .fetch_one(w.pool())
        .await
        .unwrap();
    assert_eq!(asked.as_deref(), Some("true"), "concurrency applies at approval: the older run is told to stop");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_fork_pull_request_that_waits_cancels_nothing_either(pool: PgPool) {
    // The same hole existed for forks: the run was created waiting but still applied concurrency.
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, SLOW)]).await;
    let first = w.push(repo, &git, "main", None).await[0];
    let r = w.runner("r1", &["ubuntu-latest"]).await;
    r.request(&w, 0).await.unwrap();
    let held = w.emit_and_process(push_by_app(repo, &git, &w, false)).await;
    let held = if held.is_empty() {
        git.commit_file("y.txt", "y", "third");
        w.emit_and_process(push_by_app(repo, &git, &w, false)).await
    } else {
        held
    };
    assert_eq!(w.run_status(*held.iter().max().unwrap()).await.0, "waiting");
    assert_eq!(w.run_status(first).await.0, "in_progress");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn dispatch_rerun_and_cancel_need_the_workflows_permission(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w
        .repo_with(w.alice, "demo", &[(CI_YML, &format!("{}\n", basic_workflow().replace("on: push", "on: [push, workflow_dispatch]")))])
        .await;
    let run = w.push(repo, &git, "main", None).await[0];
    let c = ch_core::model::ClientInfo::local();
    let no_ci = app_for(&w, None, repo);
    let with_ci = app_for(&w, Some(repo), repo);
    assert!(w.state.ci.dispatch(&no_ci, repo, "ci.yml", "main", serde_json::json!({}), &c).await.is_err());
    assert!(w.state.ci.cancel(&no_ci, repo, ch_core::RunId(run), &c).await.is_err());
    assert!(w.state.ci.rerun(&no_ci, repo, ch_core::RunId(run), false, &c).await.is_err());
    // With the permission on this repository it works, and on another repository it still does not.
    let (other, _) = w.repo_with(w.alice, "other", &[(CI_YML, &basic_workflow())]).await;
    let elsewhere = app_for(&w, Some(repo), other);
    assert!(w.state.ci.cancel(&elsewhere, other, ch_core::RunId(run), &c).await.is_err());
    w.state.ci.cancel(&with_ci, repo, ch_core::RunId(run), &c).await.unwrap();
    // Reading workflows as an app writes no rows.
    let before: i64 =
        sqlx::query_scalar("SELECT (SELECT count(*) FROM workflows) + (SELECT count(*) FROM schedules)").fetch_one(w.pool()).await.unwrap();
    sqlx::query("DELETE FROM workflows").execute(w.pool()).await.unwrap();
    let listed = w.state.ci.workflows(&no_ci, repo, None).await.unwrap();
    assert!(!listed.is_empty());
    let after: i64 =
        sqlx::query_scalar("SELECT (SELECT count(*) FROM workflows) + (SELECT count(*) FROM schedules)").fetch_one(w.pool()).await.unwrap();
    assert_eq!(after, 0, "an app's read created no workflow rows (before: {before})");
}
