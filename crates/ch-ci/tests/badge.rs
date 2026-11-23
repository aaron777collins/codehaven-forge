//! Workflow badges show the latest finished run of the default branch (ch-c1d).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;
use sqlx::PgPool;
use support::*;

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_badge_shows_the_latest_finished_run_of_the_branch(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, &basic_workflow())]).await;
    let alice = ch_testkit::fixtures::user_actor(w.alice);
    let bob = ch_testkit::fixtures::user_actor(w.bob);
    let r = w.runner("r1", &["ubuntu-latest"]).await;

    w.push(repo, &git, "main", None).await;
    let b = w.state.ci.badge(&alice, repo, "ci.yml", None, None).await.unwrap();
    assert_eq!((b.workflow_name.as_str(), b.conclusion), ("ci", None), "nothing has finished yet");

    let lint = r.request(&w, 0).await.unwrap();
    r.complete(&w, lint.job_id, "failure", serde_json::json!({})).await;
    r.drain(&w).await;
    let b = w.state.ci.badge(&alice, repo, "ci.yml", None, None).await.unwrap();
    assert_eq!(b.conclusion.as_deref(), Some("failure"));

    // Another branch has its own badge, and the default branch's is unchanged by it.
    let before = git.head();
    git.branch("feature");
    git.commit_file("f.txt", "f\n", "Feature");
    w.push(repo, &git, "feature", Some(&before)).await;
    r.drain(&w).await;
    assert_eq!(w.state.ci.badge(&alice, repo, "ci.yml", Some("feature"), None).await.unwrap().conclusion.as_deref(), Some("success"));
    assert_eq!(w.state.ci.badge(&alice, repo, "ci.yml", None, None).await.unwrap().conclusion.as_deref(), Some("failure"));

    // An unknown file, and a person who cannot read the repository, get nothing.
    assert!(matches!(w.state.ci.badge(&alice, repo, "other.yml", None, None).await, Err(ch_core::Error::NotFound)));
    assert!(w.state.ci.badge(&bob, repo, "ci.yml", None, None).await.is_err());
}
