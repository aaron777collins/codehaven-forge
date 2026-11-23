//! A run started by hand, or started again, is for a branch or a tag that exists under that
//! exact name. Anything else could be code that has not passed the pull request rules.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;
use axum::http::StatusCode;
use serde_json::json;
use sqlx::PgPool;
use std::sync::Arc;
use support::*;

const MANUAL: &str = "name: manual
on: workflow_dispatch
jobs:
  b:
    runs-on: x
    steps:
      - run: echo ${{ secrets.DEPLOY_KEY }}
";
const DISPATCH: &str = "/api/v3/repos/alice/demo/actions/workflows/manual.yml/dispatches";

/// alice/demo with main, two tags, and a commit that only non-branch refs point at.
async fn setup(w: &World) -> (Arc<GitRepo>, String) {
    let (_, git) = w.repo_with(w.alice, "demo", &[(".codehaven/workflows/manual.yml", MANUAL)]).await;
    git.git(&["tag", "v1"]);
    git.git(&["tag", "-a", "v2", "-m", "two"]);
    git.branch("contributor");
    let outside = git.commit_file("change.txt", "not reviewed\n", "A change from a fork");
    for r in ["refs/pull/1/head", "refs/pull/1/merge", "refs/ch/x", "refs/notes/x", "refs/remotes/fork/x"] {
        git.git(&["update-ref", r, &outside]);
    }
    git.git(&["checkout", "--quiet", "main"]);
    git.git(&["branch", "-D", "contributor"]);
    (git, outside)
}

async fn dispatch(w: &World, r: &str) -> (StatusCode, serde_json::Value) {
    w.post(DISPATCH, &Who::User(w.alice), json!({"ref": r})).await
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn refs_that_are_not_branches_or_tags_are_refused(pool: PgPool) {
    let w = World::new(pool).await;
    setup(&w).await;
    for r in [
        "refs/pull/1/head",
        "refs/pull/1/merge",
        " refs/pull/1/head ",
        "refs/ch/x",
        "refs/notes/x",
        "refs/remotes/fork/x",
        "refs/heads",
        "refs/",
    ] {
        let (status, body) = dispatch(&w, r).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{r}: {body}");
        assert!(body.to_string().contains("Choose a branch or tag."), "{r}: {body}");
        assert!(w.run_ids().await.is_empty(), "{r} started a run");
    }
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_name_is_never_read_as_a_revision(pool: PgPool) {
    let w = World::new(pool).await;
    let (_, outside) = setup(&w).await;
    // `git rev-parse` gives a commit for several of these although no such branch or tag exists.
    let described = format!("none-1-g{outside}");
    let names = [
        described.clone(),
        format!("refs/heads/{described}"),
        format!("refs/tags/{described}"),
        format!("main-1-g{}", &outside[..12]),
        outside.clone(),
        format!("refs/heads/{outside}"),
        "main~0".into(),
        "refs/heads/main~0".into(),
        "refs/tags/v2^{}".into(),
        "main^{/Add}".into(),
        "main@{0}".into(),
        "../pull/1/head".into(),
        "refs/heads/../pull/1/head".into(),
        "refs/tags/../pull/1/head".into(),
        "pull/1/head".into(),
        "refs/heads/".into(),
        "refs/heads/mai".into(),
        "HEAD".into(),
    ];
    for r in &names {
        let (status, body) = dispatch(&w, r).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{r}: {body}");
        assert!(w.run_ids().await.is_empty(), "{r} started a run");
    }
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn branches_and_tags_still_start_runs(pool: PgPool) {
    let w = World::new(pool).await;
    let (git, _) = setup(&w).await;
    let main = git.head();
    git.git(&["branch", "feature/one"]);
    git.git(&["tag", "feature/one"]);
    for (given, full) in [
        ("", "refs/heads/main"),
        ("main", "refs/heads/main"),
        ("refs/heads/main", "refs/heads/main"),
        ("v1", "refs/tags/v1"),
        ("refs/tags/v1", "refs/tags/v1"),
        ("v2", "refs/tags/v2"),
        ("feature/one", "refs/heads/feature/one"),
        ("refs/tags/feature/one", "refs/tags/feature/one"),
    ] {
        let (status, body) = dispatch(&w, given).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{given}: {body}");
        let got: (Option<String>, String) =
            sqlx::query_as("SELECT git_ref, head_sha FROM workflow_runs ORDER BY id DESC LIMIT 1").fetch_one(w.pool()).await.unwrap();
        assert_eq!(got, (Some(full.to_string()), main.clone()), "{given}");
    }
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn running_again_follows_the_same_rule(pool: PgPool) {
    let w = World::new(pool).await;
    let (_, outside) = setup(&w).await;
    assert_eq!(dispatch(&w, "main").await.0, StatusCode::NO_CONTENT);
    let run = w.run_ids().await[0];
    let r = w.runner("r", &["x"]).await;
    r.run_one(&w, "success").await.unwrap();
    let alice = Who::User(w.alice);

    // Runs saved before the rule can name refs it would now refuse.
    for saved in [Some("refs/pull/1/head".to_string()), Some(format!("refs/heads/none-1-g{outside}")), None] {
        sqlx::query("UPDATE workflow_runs SET git_ref = $2 WHERE id = $1").bind(run).bind(&saved).execute(w.pool()).await.unwrap();
        for action in ["rerun", "rerun-failed-jobs"] {
            let (status, body) = w.post(&format!("/api/v3/repos/alice/demo/actions/runs/{run}/{action}"), &alice, json!({})).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{saved:?} {action}: {body}");
            assert_eq!(w.run_ids().await, vec![run], "{saved:?} {action} started a run");
        }
    }

    sqlx::query("UPDATE workflow_runs SET git_ref = 'refs/heads/main' WHERE id = $1").bind(run).execute(w.pool()).await.unwrap();
    assert_eq!(w.post(&format!("/api/v3/repos/alice/demo/actions/runs/{run}/rerun"), &alice, json!({})).await.0, StatusCode::CREATED);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_rerun_records_who_started_it(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, _) = w.repo_with(w.alice, "demo", &[(".codehaven/workflows/manual.yml", MANUAL)]).await;
    assert_eq!(dispatch(&w, "main").await.0, StatusCode::NO_CONTENT);
    let run = w.run_ids().await[0];
    let r = w.runner("r", &["x"]).await;
    r.run_one(&w, "success").await.unwrap();
    // Bob did not start the run, but he has write access, so his re-run is his.
    w.env.collaborator(repo, w.bob, "write").await;
    let (status, body) = w.post(&format!("/api/v3/repos/alice/demo/actions/runs/{run}/rerun"), &Who::User(w.bob), json!({})).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let started_by = sqlx::query_scalar::<_, Option<i64>>("SELECT actor_id FROM workflow_runs ORDER BY id DESC LIMIT 1")
        .fetch_one(w.pool())
        .await
        .unwrap();
    assert_eq!(started_by, Some(w.bob.0));
}
