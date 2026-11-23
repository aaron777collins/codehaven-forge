//! A pushed workflow that cannot run shows a failed run that says why (ch-xy5), instead of
//! starting nothing.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;
use ch_core::RunId;
use sqlx::PgPool;
use support::*;

const VOLUMES: &str = "name: vols
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    container:
      image: alpine
      volumes:
        - /:/host
    steps:
      - run: echo hi
";

const PRIVILEGED_LATER: &str = "name: priv
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    container:
      image: alpine
      options: ${{ github.event_name == 'push' && '--privileged' || '' }}
    steps:
      - run: echo hi
";

async fn problem(w: &World, repo: ch_core::RepoId, run: i64) -> String {
    let alice = ch_testkit::fixtures::user_actor(w.alice);
    w.state.ci.run_problem(&alice, repo, RunId(run)).await.unwrap().expect("the run says what is wrong")
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_workflow_that_does_not_parse_shows_a_failed_run(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, VOLUMES)]).await;
    let runs = w.push(repo, &git, "main", None).await;
    assert_eq!(runs.len(), 1, "one failed run for the broken file");
    assert_eq!(w.run_status(runs[0]).await, ("completed".into(), Some("failure".into())));
    assert!(w.jobs(runs[0]).await.is_empty(), "nothing runs");
    assert!(problem(&w, repo, runs[0]).await.contains("container.volumes"));

    // The same event delivered twice does not add a second row.
    let mut failures = Default::default();
    ch_ci::triggers::process_batch(&w.ctx, &mut failures).await.unwrap();
    assert_eq!(w.run_ids().await.len(), 1);

    // A later push that leaves the broken file alone does not repeat it.
    let before = git.head();
    git.commit_file("README.md", "more\n", "Docs");
    assert_eq!(w.push(repo, &git, "main", Some(&before)).await.len(), 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_workflow_refused_while_planning_shows_a_failed_run(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, PRIVILEGED_LATER)]).await;
    let runs = w.push(repo, &git, "main", None).await;
    assert_eq!(runs.len(), 1);
    assert_eq!(w.run_status(runs[0]).await, ("completed".into(), Some("failure".into())));
    assert!(problem(&w, repo, runs[0]).await.contains("--privileged"));
}

const UNPARSEABLE: &str = "name: fbroken
on: push
jobs:
  a:
    steps: []
";

/// Only a branch push records a problem run. The server writes `refs/pull/N/head` (and its
/// merge ref) into the base repository when a fork opens a pull request and fires a push for
/// it, so a problem run there would put a fork commit into the base repository before anyone
/// approved it (ch-x12). A tag push of a commit is not a new place to report it either.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_broken_file_on_a_pull_request_or_tag_ref_records_no_run(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, UNPARSEABLE)]).await;
    let sha = git.head();
    for git_ref in ["refs/pull/1/head", "refs/pull/1/merge", "refs/tags/v1"] {
        let ev = ch_core::events::DomainEvent::Push {
            repo_id: repo,
            git_ref: git_ref.into(),
            before: "0".repeat(40),
            after: sha.clone(),
            pusher: None,
            forced: false,
            app: None,
            server_built: false,
        };
        assert!(w.emit_and_process(ev).await.is_empty(), "{git_ref} records no run");
    }
    // The same commit pushed to a branch does (so the check above is not vacuous).
    assert_eq!(w.push(repo, &git, "main", None).await.len(), 1);
}
