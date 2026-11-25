//! REST run and job objects carry what GitHub's do (ch-fm9): `head_branch` and `pull_requests`
//! for a pull request's run, `head_commit`, and `null` rather than a missing key while queued.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;
use ch_core::PullId;
use ch_core::events::DomainEvent;
use sqlx::PgPool;
use support::*;

const PR: &str = "name: ci
on: pull_request
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: echo hi
";

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_pull_requests_run_names_its_branch_pull_and_commit(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, PR)]).await;
    let base = git.head();
    git.branch("feature");
    let head = git.commit_file("f.txt", "f\n", "Add the feature\n\nWith a body.");
    let issue: i64 = sqlx::query_scalar(
        "INSERT INTO issues (repo_id, number, is_pull, author_id, title) VALUES ($1, 7, true, $2, 'Feature') RETURNING id",
    )
    .bind(repo.0)
    .bind(w.alice.0)
    .fetch_one(w.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO pull_requests (issue_id, repo_id, head_ref, head_sha, base_ref, base_sha) VALUES ($1, $2, 'feature', $3, 'main', $4)",
    )
    .bind(issue)
    .bind(repo.0)
    .bind(&head)
    .bind(&base)
    .execute(w.pool())
    .await
    .unwrap();
    w.emit_and_process(DomainEvent::PrOpened {
        repo_id: repo,
        pull_id: PullId(issue),
        number: 7,
        actor: w.alice,
        app: None,
        head_sha: Some(head.clone()),
        pusher: Some(w.alice),
    })
    .await;
    let run = w.run_ids().await[0];
    let (s, v) = w.get(&format!("/api/v3/repos/alice/demo/actions/runs/{run}"), &Who::User(w.alice)).await;
    assert!(s.is_success(), "{v}");
    assert_eq!(v["head_branch"], "feature");
    assert_eq!(v["head_commit"]["id"], head);
    assert!(v["head_commit"]["message"].as_str().unwrap().starts_with("Add the feature\n\nWith a body."), "{v}");
    let pr = &v["pull_requests"][0];
    assert_eq!(pr["number"], 7);
    assert_eq!(pr["id"], issue);
    assert_eq!(pr["head"]["ref"], "feature");
    assert_eq!(pr["head"]["sha"], head);
    assert_eq!(pr["base"]["ref"], "main");
    assert_eq!(pr["base"]["repo"]["name"], "demo");
    assert!(pr["url"].as_str().unwrap().ends_with("/api/v3/repos/alice/demo/pulls/7"), "{pr}");
    // Still queued: the keys are there, as null.
    assert!(v.as_object().unwrap().contains_key("conclusion") && v["conclusion"].is_null(), "{v}");
    let (_, jobs) = w.get(&format!("/api/v3/repos/alice/demo/actions/runs/{run}/jobs"), &Who::User(w.alice)).await;
    let job = jobs["jobs"][0].as_object().unwrap();
    for key in ["conclusion", "started_at", "completed_at", "runner_id", "runner_name"] {
        assert!(job.contains_key(key) && job[key].is_null(), "{key}: {jobs}");
    }
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_push_run_has_no_pull_requests_and_a_head_commit(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, &basic_workflow())]).await;
    let run = w.push(repo, &git, "main", None).await[0];
    let (_, v) = w.get(&format!("/api/v3/repos/alice/demo/actions/runs/{run}"), &Who::User(w.alice)).await;
    assert_eq!(v["pull_requests"], serde_json::json!([]));
    assert_eq!(v["head_commit"]["id"], git.head());
    assert_eq!(v["head_commit"]["author"]["name"].as_str().map(|s| !s.is_empty()), Some(true));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_push_event_carries_the_compare_link_and_its_commit(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, &basic_workflow())]).await;
    w.push(repo, &git, "main", None).await;
    let run = w.run_ids().await[0];
    let payload: serde_json::Value =
        sqlx::query_scalar("SELECT event_payload FROM workflow_runs WHERE id = $1").bind(run).fetch_one(w.pool()).await.unwrap();
    let event = ch_ci::spec::split_payload(&payload).0;
    assert!(event["compare"].as_str().unwrap().contains("/alice/demo/compare/"), "{event}");
    assert_eq!(event["commits"][0]["id"], git.head());
}
