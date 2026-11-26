//! Deployment environments (ch-bj5): reviewers, a wait timer, a branch filter, and secrets that
//! go to a job only after its gate and never to a fork's pull request.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;
use ch_core::model::ClientInfo;
use ch_core::web_contract::EnvironmentDraft;
use ch_core::{Actor, Error, RepoId, UserId};
use ch_testkit::fixtures::user_actor;
use sqlx::PgPool;
use std::collections::BTreeSet;
use support::*;

const DEPLOY: &str = "name: ci
on: push
jobs:
  deploy:
    runs-on: x
    environment: prod
    steps:
      - run: echo ${{ secrets.PROD_KEY }} ${{ secrets.OTHER }}
";

fn c() -> ClientInfo {
    ClientInfo::local()
}

struct Setup {
    w: World,
    repo: RepoId,
    git: std::sync::Arc<GitRepo>,
    alice: Actor,
    bob: Actor,
    bob_id: UserId,
}

async fn setup(pool: PgPool, draft: EnvironmentDraft) -> Setup {
    setup_with(pool, draft, DEPLOY).await
}

async fn setup_with(pool: PgPool, draft: EnvironmentDraft, workflow: &str) -> Setup {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, workflow)]).await;
    w.env.collaborator(repo, w.bob, "write").await;
    let alice = user_actor(w.alice);
    let bob = user_actor(w.bob);
    let p = &w.state.pipelines;
    let mut draft = draft;
    if draft.reviewers == vec![UserId(-1)] {
        draft.reviewers = vec![w.bob];
    }
    p.save_environment(&alice, repo, "prod", draft, &c()).await.unwrap();
    p.set_environment_secret(&alice, repo, "prod", "PROD_KEY", "prod-value-1", &c()).await.unwrap();
    ch_ci::secrets::put_secret(&w.ctx, ch_ci::secrets::Scope::Repo(repo.0), "OTHER", "repo-value", Some(w.alice)).await.unwrap();
    ch_ci::secrets::put_secret(&w.ctx, ch_ci::secrets::Scope::Repo(repo.0), "PROD_KEY", "repo-prod-value", Some(w.alice)).await.unwrap();
    let bob_id = w.bob;
    Setup { w, repo, git, alice, bob, bob_id }
}

fn reviewed() -> EnvironmentDraft {
    EnvironmentDraft { wait_minutes: 0, branches: vec![], reviewers: vec![UserId(-1)] }
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_job_waits_for_a_required_reviewer_and_then_gets_the_environments_secrets(pool: PgPool) {
    let s = setup(pool, reviewed()).await;
    let w = &s.w;
    let run = w.push(s.repo, &s.git, "main", None).await[0];
    assert_eq!(w.jobs(run).await, vec![("deploy".to_string(), "waiting".to_string(), None)], "held, not queued");
    let r = w.runner("r", &["x"]).await;
    assert!(r.request(w, 0).await.is_none(), "nothing for a runner to take");
    let job = w.job_id(run, "deploy").await;
    let gates = w.state.pipelines.run_gates(&s.alice, s.repo, ch_core::RunId(run)).await.unwrap();
    assert_eq!((gates.len(), gates[0].state.as_str(), gates[0].can_review), (1, "pending", false), "alice is not a reviewer");
    // Alice owns the repository, but the list decides who may approve.
    let no = w.state.pipelines.review_deployment(&s.alice, s.repo, ch_core::RunId(run), job, true, "", &c()).await;
    assert!(matches!(no, Err(Error::Forbidden(_))), "{no:?}");
    // Still held.
    assert!(r.request(w, 0).await.is_none());
    assert!(w.state.pipelines.run_gates(&s.bob, s.repo, ch_core::RunId(run)).await.unwrap()[0].can_review);
    w.state.pipelines.review_deployment(&s.bob, s.repo, ch_core::RunId(run), job, true, "ship it", &c()).await.unwrap();
    let taken = r.request(w, 0).await.expect("approved, so it can be taken");
    // The environment's value beats the repository's, and the repository's other secret still comes.
    assert_eq!(taken.secrets["PROD_KEY"], "prod-value-1");
    assert_eq!(taken.secrets["OTHER"], "repo-value");
    // A second decision is refused.
    let again = w.state.pipelines.review_deployment(&s.bob, s.repo, ch_core::RunId(run), job, false, "", &c()).await;
    assert!(matches!(again, Err(Error::Conflict(_))), "{again:?}");
    let g = w.state.pipelines.run_gates(&s.bob, s.repo, ch_core::RunId(run)).await.unwrap();
    assert_eq!((g[0].state.as_str(), g[0].decided_by.as_deref(), g[0].comment.as_str()), ("approved", Some("bob"), "ship it"));
    assert_eq!(s.bob_id, w.bob);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_rejected_deployment_fails_the_job_and_no_secret_leaves(pool: PgPool) {
    let s = setup(pool, reviewed()).await;
    let w = &s.w;
    let run = w.push(s.repo, &s.git, "main", None).await[0];
    let job = w.job_id(run, "deploy").await;
    w.state.pipelines.review_deployment(&s.bob, s.repo, ch_core::RunId(run), job, false, "not now", &c()).await.unwrap();
    let (status, conclusion) = w.run_status(run).await;
    assert_eq!((status.as_str(), conclusion.as_deref()), ("completed", Some("failure")));
    let r = w.runner("r", &["x"]).await;
    assert!(r.request(w, 0).await.is_none());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_branch_filter_fails_a_job_from_another_branch_without_asking_anyone(pool: PgPool) {
    let s = setup(pool, EnvironmentDraft { wait_minutes: 0, branches: vec!["release/*".into()], reviewers: vec![UserId(-1)] }).await;
    let w = &s.w;
    // main does not match release/*.
    let run = w.push(s.repo, &s.git, "main", None).await[0];
    assert_eq!(w.run_status(run).await.1.as_deref(), Some("failure"));
    let gates: i64 = sqlx::query_scalar("SELECT count(*) FROM deployment_gates").fetch_one(w.pool()).await.unwrap();
    assert_eq!(gates, 0, "a branch that may not deploy is not even asked about");
    // A release branch is held for review instead.
    s.git.branch("release/1");
    let run2 = *w.push(s.repo, &s.git, "release/1", None).await.last().unwrap();
    assert_eq!(w.jobs(run2).await[0].1, "waiting");
}

/// ch-b30: a tag named like the filtered branch is not that branch. A writer who cannot push to a
/// protected `main` must not get a deploy by pushing a tag `main` at some other commit.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_tag_named_like_the_filtered_branch_does_not_pass_the_branch_filter(pool: PgPool) {
    let s = setup(pool, EnvironmentDraft { wait_minutes: 0, branches: vec!["main".into()], reviewers: vec![] }).await;
    let w = &s.w;
    s.git.git(&["tag", "main", "HEAD"]);
    let run = *w.push_ref(s.repo, &s.git, "refs/tags/main", "main").await.last().unwrap();
    assert_eq!(w.run_status(run).await.1.as_deref(), Some("failure"), "the tag main is not the branch main");
    let (secrets_seen, gates): (i64, i64) = (
        sqlx::query_scalar("SELECT count(*) FROM workflow_jobs WHERE run_id = $1 AND status <> 'completed'")
            .bind(run)
            .fetch_one(w.pool())
            .await
            .unwrap(),
        sqlx::query_scalar("SELECT count(*) FROM deployment_gates").fetch_one(w.pool()).await.unwrap(),
    );
    assert_eq!((secrets_seen, gates), (0, 0), "no job was left to be claimed and no gate was made");
    let r = w.runner("r", &["x"]).await;
    assert!(r.request(w, 0).await.is_none(), "nothing for a runner to take, so no secret leaves");
    // The branch itself still deploys.
    let run2 = *w.push(s.repo, &s.git, "main", None).await.last().unwrap();
    assert_ne!(w.run_status(run2).await.1.as_deref(), Some("failure"));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_tags_pattern_lets_a_matching_tag_deploy_and_nothing_else_by_that_name(pool: PgPool) {
    let s = setup(pool, EnvironmentDraft { wait_minutes: 0, branches: vec!["tags/v*".into()], reviewers: vec![UserId(-1)] }).await;
    let w = &s.w;
    s.git.git(&["tag", "v1", "HEAD"]);
    s.git.git(&["tag", "x1", "HEAD"]);
    let held = *w.push_ref(s.repo, &s.git, "refs/tags/v1", "v1").await.last().unwrap();
    assert_eq!(w.jobs(held).await[0].1, "waiting", "v1 matches tags/v*, so it waits for its reviewer");
    let refused = *w.push_ref(s.repo, &s.git, "refs/tags/x1", "x1").await.last().unwrap();
    assert_eq!(w.run_status(refused).await.1.as_deref(), Some("failure"));
    // The branch main is not a tag, so the same filter refuses it.
    let branch = *w.push(s.repo, &s.git, "main", None).await.last().unwrap();
    assert_eq!(w.run_status(branch).await.1.as_deref(), Some("failure"));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_wait_timer_holds_the_job_until_it_runs_out(pool: PgPool) {
    let s = setup(pool, EnvironmentDraft { wait_minutes: 10, branches: vec![], reviewers: vec![] }).await;
    let w = &s.w;
    let run = w.push(s.repo, &s.git, "main", None).await[0];
    assert_eq!(w.jobs(run).await[0].1, "waiting", "no reviewers, but the timer is running");
    let r = w.runner("r", &["x"]).await;
    assert!(r.request(w, 0).await.is_none());
    assert_eq!(ch_ci::environments::release_due(&w.ctx).await.unwrap(), 0, "not yet");
    sqlx::query("UPDATE deployment_gates SET release_at = now() - interval '1 minute'").execute(w.pool()).await.unwrap();
    assert_eq!(ch_ci::environments::release_due(&w.ctx).await.unwrap(), 1);
    let job = r.request(w, 0).await.expect("the timer ran out");
    assert_eq!(job.secrets["PROD_KEY"], "prod-value-1");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn secrets_stay_back_for_a_fork_and_for_a_job_not_past_its_gate(pool: PgPool) {
    let s = setup(pool, reviewed()).await;
    let w = &s.w;
    let run = w.push(s.repo, &s.git, "main", None).await[0];
    let id = w.job_id(run, "deploy").await;
    let job = ch_ci::runs::fetch_jobs(&w.ctx, run).await.unwrap().into_iter().find(|j| j.id == id).unwrap();
    let names: BTreeSet<String> = ["PROD_KEY".to_string()].into();
    let get = |fork: bool| ch_ci::environments::secrets_for_job(&w.ctx, &job, "prod", &names, false, fork);
    assert!(get(false).await.unwrap().is_empty(), "the gate is pending");
    w.state.pipelines.review_deployment(&s.bob, s.repo, ch_core::RunId(run), id, true, "", &c()).await.unwrap();
    assert_eq!(get(false).await.unwrap()["PROD_KEY"], "prod-value-1");
    assert!(get(true).await.unwrap().is_empty(), "never for a fork's pull request");
    // Sealed at rest, and tied to its environment and name.
    let sealed: Vec<u8> = sqlx::query_scalar("SELECT value_sealed FROM environment_secrets").fetch_one(w.pool()).await.unwrap();
    assert!(!String::from_utf8_lossy(&sealed).contains("prod-value"));
    let list = w.state.pipelines.environment(&s.alice, s.repo, "prod").await.unwrap();
    assert_eq!(list.secrets.len(), 1);
    assert!(!format!("{list:?}").contains("prod-value"), "no value is ever read back");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn only_admins_manage_environments_and_a_reviewer_needs_write(pool: PgPool) {
    let s = setup(pool, reviewed()).await;
    let w = &s.w;
    let p = &w.state.pipelines;
    // Bob can write but not administer.
    let d = EnvironmentDraft::default();
    assert!(matches!(p.save_environment(&s.bob, s.repo, "staging", d.clone(), &c()).await, Err(Error::Forbidden(_))));
    assert!(matches!(p.environments(&s.bob, s.repo).await, Err(Error::Forbidden(_))));
    assert!(matches!(p.set_environment_secret(&s.bob, s.repo, "prod", "X", "v", &c()).await, Err(Error::Forbidden(_))));
    // Someone with read only cannot be a reviewer.
    let carol = w.env.insert_user("carol", false, false).await;
    w.env.collaborator(s.repo, carol, "read").await;
    let bad = EnvironmentDraft { wait_minutes: 0, branches: vec![], reviewers: vec![carol] };
    assert!(matches!(p.save_environment(&s.alice, s.repo, "staging", bad, &c()).await, Err(Error::Validation(_))));
    // Odd rules are refused with a reason.
    for bad in [
        EnvironmentDraft { wait_minutes: -1, ..Default::default() },
        EnvironmentDraft { wait_minutes: 43_201, ..Default::default() },
        EnvironmentDraft { branches: vec!["a[".into()], ..Default::default() },
    ] {
        assert!(matches!(p.save_environment(&s.alice, s.repo, "staging", bad, &c()).await, Err(Error::Validation(_))));
    }
    assert!(matches!(p.save_environment(&s.alice, s.repo, "-bad name", d, &c()).await, Err(Error::Validation(_))));
    let candidates = p.reviewer_candidates(&s.alice, s.repo).await.unwrap();
    let names: Vec<&str> = candidates.iter().map(|c| c.login.as_str()).collect();
    assert!(names.contains(&"alice") && names.contains(&"bob") && !names.contains(&"carol"), "{names:?}");
    // Deleting the environment removes its secrets and rules.
    p.delete_environment(&s.alice, s.repo, "prod", &c()).await.unwrap();
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM environment_secrets").fetch_one(w.pool()).await.unwrap();
    assert_eq!(left, 0);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn creating_an_environment_by_the_name_of_an_existing_one_is_refused_and_leaves_its_rules(pool: PgPool) {
    let draft = EnvironmentDraft { wait_minutes: 5, branches: vec!["main".into()], reviewers: vec![UserId(-1)] };
    let s = setup(pool, draft).await;
    let p = &s.w.state.pipelines;
    for name in ["prod", "PROD", "Prod"] {
        let r = p.create_environment(&s.alice, s.repo, name, &c()).await;
        assert!(matches!(r, Err(Error::Validation(_))), "{name}: {r:?}");
    }
    let e = p.environment(&s.alice, s.repo, "prod").await.unwrap();
    assert_eq!(e.wait_minutes, 5);
    assert_eq!(e.branches, vec!["main".to_string()]);
    assert_eq!(e.reviewers.len(), 1, "the reviewer is still there");
    assert_eq!(e.secrets.len(), 1, "and so is its secret");
    // A new name is created with no rules, and only an admin may.
    assert!(matches!(p.create_environment(&s.bob, s.repo, "staging", &c()).await, Err(Error::Forbidden(_))));
    p.create_environment(&s.alice, s.repo, "staging", &c()).await.unwrap();
    let e = p.environment(&s.alice, s.repo, "staging").await.unwrap();
    assert_eq!((e.wait_minutes, e.branches.len(), e.reviewers.len()), (0, 0, 0));
}

/// ch-e7b: the name column ignores case, so the lookup must too. A job that spells the environment
/// another way is held for the same reviewers, the same timer and the same branch filter, and
/// gets the same secrets only after approval.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_job_naming_the_environment_in_another_case_is_held_like_the_exact_name(pool: PgPool) {
    let spelling = "PROD";
    let yml = DEPLOY.replace("environment: prod", &format!("environment: {spelling}"));
    let s = setup_with(pool, reviewed(), &yml).await;
    let w = &s.w;
    let run = w.push(s.repo, &s.git, "main", None).await[0];
    assert_eq!(w.jobs(run).await[0].1, "waiting", "held for the reviewer");
    let r = w.runner("r", &["x"]).await;
    assert!(r.request(w, 0).await.is_none(), "nothing to take before approval");
    let job = w.job_id(run, "deploy").await;
    let gates = w.state.pipelines.run_gates(&s.alice, s.repo, ch_core::RunId(run)).await.unwrap();
    assert_eq!((gates.len(), gates[0].state.as_str()), (1, "pending"));
    let no = w.state.pipelines.review_deployment(&s.alice, s.repo, ch_core::RunId(run), job, true, "", &c()).await;
    assert!(matches!(no, Err(Error::Forbidden(_))), "{no:?}");
    w.state.pipelines.review_deployment(&s.bob, s.repo, ch_core::RunId(run), job, true, "", &c()).await.unwrap();
    let taken = r.request(w, 0).await.expect("approved, so it can be taken");
    assert_eq!(taken.secrets["PROD_KEY"], "prod-value-1", "the environment's secret, only after approval");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_job_naming_the_environment_in_another_case_honours_its_wait_timer(pool: PgPool) {
    let yml = DEPLOY.replace("environment: prod", "environment: Prod");
    let s = setup_with(pool, EnvironmentDraft { wait_minutes: 10, branches: vec![], reviewers: vec![] }, &yml).await;
    let w = &s.w;
    let run = w.push(s.repo, &s.git, "main", None).await[0];
    assert_eq!(w.jobs(run).await[0].1, "waiting");
    let r = w.runner("r", &["x"]).await;
    assert!(r.request(w, 0).await.is_none());
    sqlx::query("UPDATE deployment_gates SET release_at = now() - interval '1 minute'").execute(w.pool()).await.unwrap();
    assert_eq!(ch_ci::environments::release_due(&w.ctx).await.unwrap(), 1);
    assert_eq!(r.request(w, 0).await.expect("the timer ran out").secrets["PROD_KEY"], "prod-value-1");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_job_naming_the_environment_in_another_case_honours_its_branch_filter(pool: PgPool) {
    let yml = DEPLOY.replace("environment: prod", "environment: PROD");
    let s =
        setup_with(pool, EnvironmentDraft { wait_minutes: 0, branches: vec!["release/*".into()], reviewers: vec![UserId(-1)] }, &yml).await;
    let w = &s.w;
    let run = w.push(s.repo, &s.git, "main", None).await[0];
    assert_eq!(w.run_status(run).await.1.as_deref(), Some("failure"), "main may not deploy, whatever the spelling");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn secrets_for_job_and_the_settings_calls_find_the_environment_in_any_case(pool: PgPool) {
    let s = setup(pool, EnvironmentDraft { wait_minutes: 0, branches: vec![], reviewers: vec![] }).await;
    let w = &s.w;
    let p = &w.state.pipelines;
    let run = w.push(s.repo, &s.git, "main", None).await[0];
    let id = w.job_id(run, "deploy").await;
    let job = ch_ci::runs::fetch_jobs(&w.ctx, run).await.unwrap().into_iter().find(|j| j.id == id).unwrap();
    let names: BTreeSet<String> = ["PROD_KEY".to_string()].into();
    for spelling in ["PROD", "Prod"] {
        let got = ch_ci::environments::secrets_for_job(&w.ctx, &job, spelling, &names, false, false).await.unwrap();
        assert_eq!(got["PROD_KEY"], "prod-value-1", "{spelling}");
        assert_eq!(p.environment(&s.alice, s.repo, spelling).await.unwrap().name, "prod", "{spelling}: the stored spelling");
    }
    // Changing the rules under another spelling changes the one environment, not a second one.
    p.save_environment(&s.alice, s.repo, "PROD", EnvironmentDraft { wait_minutes: 3, branches: vec![], reviewers: vec![] }, &c())
        .await
        .unwrap();
    let e = p.environment(&s.alice, s.repo, "prod").await.unwrap();
    assert_eq!(e.wait_minutes, 3);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM environments").fetch_one(w.pool()).await.unwrap();
    assert_eq!(n, 1);
    // A secret set and deleted under another spelling lands in, and leaves, the same environment.
    p.set_environment_secret(&s.alice, s.repo, "Prod", "EXTRA", "x", &c()).await.unwrap();
    assert_eq!(p.environment(&s.alice, s.repo, "prod").await.unwrap().secrets.len(), 2);
    p.delete_environment_secret(&s.alice, s.repo, "PROD", "EXTRA", &c()).await.unwrap();
    assert_eq!(p.environment(&s.alice, s.repo, "prod").await.unwrap().secrets.len(), 1);
    // Deleting under another spelling deletes it.
    p.delete_environment(&s.alice, s.repo, "pROD", &c()).await.unwrap();
    assert!(matches!(p.environment(&s.alice, s.repo, "prod").await, Err(Error::NotFound)));
}
