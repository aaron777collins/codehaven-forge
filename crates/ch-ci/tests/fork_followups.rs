//! Follow-ups from the runner-sweep review (ch-3xn).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;
use axum::http::StatusCode;
use ch_core::events::DomainEvent;
use ch_core::{AppOrigin, GrantId, PullId, RepoId, UserId};
use serde_json::json;
use sqlx::PgPool;
use support::*;

const PR_AND_PUSH: &str = "name: ci
on: [push, pull_request]
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: echo hi
";

fn app() -> Option<AppOrigin> {
    Some(AppOrigin { grant_id: GrantId(9), ci_allowed: false })
}

async fn same_repo_pull(w: &World, repo: RepoId, author: UserId, sha: &str) -> i64 {
    let issue: i64 = sqlx::query_scalar(
        "INSERT INTO issues (repo_id, number, is_pull, author_id, title) VALUES ($1, 1, true, $2, 'Change') RETURNING id",
    )
    .bind(repo.0)
    .bind(author.0)
    .fetch_one(w.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO pull_requests (issue_id, repo_id, head_ref, head_sha, base_ref, base_sha) VALUES ($1, $2, 'feature', $3, 'main', $3)",
    )
    .bind(issue)
    .bind(repo.0)
    .bind(sha)
    .execute(w.pool())
    .await
    .unwrap();
    issue
}

async fn statuses(w: &World) -> Vec<String> {
    sqlx::query_scalar("SELECT status FROM workflow_runs ORDER BY id").fetch_all(w.pool()).await.unwrap()
}

/// Item 1: a fork run held the old way, then cancelled by migration 0123, cannot be re-run.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn an_old_fork_run_cannot_be_rerun_without_approval(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, &basic_workflow())]).await;
    let run = w.push(repo, &git, "main", None).await[0];
    // A fork run as the old scheme held it: waiting, no hold reason, no approver.
    sqlx::query(
        "UPDATE workflow_runs SET status = 'waiting', started_at = NULL,
            event_payload = jsonb_set(event_payload #- '{meta,hold_reason}', '{meta,fork}', 'true') WHERE id = $1",
    )
    .bind(run)
    .execute(w.pool())
    .await
    .unwrap();
    let old = include_str!("../../ch-db/migrations/0123_fork_run_approvals.sql");
    sqlx::raw_sql(&old[old.find("UPDATE check_runs").unwrap()..]).execute(w.pool()).await.unwrap();
    sqlx::raw_sql(include_str!("../../ch-db/migrations/0126_old_fork_runs_held.sql")).execute(w.pool()).await.unwrap();
    let (s, body) = w.post(&format!("/api/v3/repos/alice/demo/actions/runs/{run}/rerun"), &Who::User(w.alice), json!({})).await;
    assert_eq!(s, StatusCode::CONFLICT, "{body}");
    assert_eq!(w.run_ids().await.len(), 1);
    let _ = repo;
}

/// Item 2: a job token cannot list the repository's variables; it gets the ones it may use in
/// its payload.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_job_token_cannot_read_variables(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, &basic_workflow())]).await;
    let alice = Who::User(w.alice);
    assert_eq!(
        w.post("/api/v3/repos/alice/demo/actions/variables", &alice, json!({"name": "V", "value": "1"})).await.0,
        StatusCode::CREATED
    );
    w.push(repo, &git, "main", None).await;
    let r = w.runner("r", &["ubuntu-latest"]).await;
    let j = r.request(&w, 0).await.unwrap();
    let jt = Who::Bearer(j.token.clone());
    assert_eq!(w.get("/api/v3/repos/alice/demo/actions/variables", &jt).await.0, StatusCode::FORBIDDEN);
    assert_eq!(w.get("/api/v3/repos/alice/demo/actions/variables/V", &jt).await.0, StatusCode::FORBIDDEN);
    assert_eq!(w.get("/api/v3/repos/alice/demo/actions/variables", &alice).await.0, StatusCode::OK);
    // Values are settings: a reader is refused, as on the settings page (ch-uph).
    w.env.collaborator(repo, w.bob, "read").await;
    let bob = Who::User(w.bob);
    assert_eq!(w.get("/api/v3/repos/alice/demo/actions/variables", &bob).await.0, StatusCode::FORBIDDEN);
    assert_eq!(w.get("/api/v3/repos/alice/demo/actions/variables/V", &bob).await.0, StatusCode::FORBIDDEN);
}

/// Item 3: reopening a pull request names nobody for its head, so a commit an app pushed
/// before it was closed waits for a person. A commit CI already let run runs at once.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn reopening_does_not_run_an_apps_commit_unheld(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, PR_AND_PUSH)]).await;
    let pull = same_repo_pull(&w, repo, w.alice, &git.head()).await;
    let reopen = || DomainEvent::PrOpened {
        repo_id: repo,
        pull_id: PullId(pull),
        number: 1,
        actor: w.alice,
        app: None,
        head_sha: None,
        pusher: None,
    };
    w.emit_and_process(reopen()).await;
    assert_eq!(statuses(&w).await, vec!["waiting"], "a commit CI never let run waits");

    sqlx::query("INSERT INTO pull_cleared_commits (pull_id, head_sha) VALUES ($1, $2)")
        .bind(pull)
        .bind(git.head())
        .execute(w.pool())
        .await
        .unwrap();
    w.emit_and_process(reopen()).await;
    assert_eq!(statuses(&w).await, vec!["waiting", "queued"], "a cleared commit runs at once");
}

/// Item 6: a commit the server writes on top of an app's held push (a web edit) is held too.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_web_edit_on_a_held_push_is_held(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, PR_AND_PUSH)]).await;
    let base = git.head();
    let push = |before: String, after: String, app: Option<AppOrigin>, server_built: bool| DomainEvent::Push {
        repo_id: repo,
        git_ref: "refs/heads/main".into(),
        before,
        after,
        pusher: Some(w.alice),
        forced: false,
        app,
        server_built,
    };
    let a = git.commit_file("app.txt", "from the app\n", "App change");
    w.emit_and_process(push(base.clone(), a.clone(), app(), false)).await;
    assert_eq!(statuses(&w).await, vec!["waiting"]);
    let b = git.commit_file("web.txt", "edit\n", "Web edit");
    w.emit_and_process(push(a.clone(), b.clone(), None, true)).await;
    assert_eq!(statuses(&w).await, vec!["waiting", "waiting"], "the edit holds the app's code too");
    let held: Option<String> =
        sqlx::query_scalar("SELECT event_payload->'meta'->>'hold_reason' FROM workflow_runs ORDER BY id DESC LIMIT 1")
            .fetch_one(w.pool())
            .await
            .unwrap();
    assert_eq!(held.as_deref(), Some("It builds on a commit that waits for approval"));

    // A web edit on a commit that ran unheld runs at once.
    let c = git.commit_file("c.txt", "c\n", "Person's push");
    w.emit_and_process(push(b.clone(), c.clone(), None, false)).await;
    let d = git.commit_file("d.txt", "d\n", "Web edit two");
    w.emit_and_process(push(c, d, None, true)).await;
    assert_eq!(statuses(&w).await, vec!["waiting", "waiting", "queued", "queued"]);
}

/// Item 7: approving a held run of a same-repository pull request clears its commit, so a later
/// Update branch or web edit on top does not wait for approval again.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn approving_a_pull_requests_run_clears_its_commit(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, PR_AND_PUSH)]).await;
    let pull = same_repo_pull(&w, repo, w.alice, &git.head()).await;
    w.emit_and_process(DomainEvent::PrSynchronized {
        repo_id: repo,
        pull_id: PullId(pull),
        number: 1,
        head_sha: git.head(),
        app: app(),
        pusher: Some(w.alice),
        origin_known: true,
        built_on: None,
    })
    .await;
    let run = w.run_ids().await[0];
    assert_eq!(statuses(&w).await, vec!["waiting"]);
    let alice = ch_testkit::fixtures::user_actor(w.alice);
    w.state.ci.approve_run(&alice, repo, ch_core::RunId(run), &ch_core::model::ClientInfo::local()).await.unwrap();
    let cleared: i64 = sqlx::query_scalar("SELECT count(*) FROM pull_cleared_commits WHERE pull_id = $1 AND head_sha = $2")
        .bind(pull)
        .bind(git.head())
        .fetch_one(w.pool())
        .await
        .unwrap();
    assert_eq!(cleared, 1);
}
