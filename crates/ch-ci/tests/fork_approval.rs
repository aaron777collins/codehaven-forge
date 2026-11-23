//! ch-x12: a pull request from a fork runs nothing until someone with write access approves its
//! exact commit, unless both its author and the pusher have write access. Before that there is
//! no run, job or check row, no expression evaluated and no concurrency effect.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;
use axum::http::StatusCode;
use ch_core::events::DomainEvent;
use ch_core::{PullId, RepoId, UserId};
use serde_json::json;
use sqlx::PgPool;
use support::*;

/// A run-name and a concurrency group that read the repository's variables, cancelling
/// whatever else is in the group.
const GROUPED: &str = "name: ci
run-name: Build ${{ vars.SECRETISH }}
on: [push, pull_request]
concurrency:
  group: deploys
  cancel-in-progress: true
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: echo hi
";

const APPROVAL: &str = "/api/v3/repos/alice/demo/actions/pulls/1/approval";

async fn insert_pull(w: &World, repo: RepoId, head_repo: RepoId, author: UserId, sha: &str) -> i64 {
    let issue: i64 = sqlx::query_scalar(
        "INSERT INTO issues (repo_id, number, is_pull, author_id, title) VALUES ($1, 1, true, $2, 'Change things') RETURNING id",
    )
    .bind(repo.0)
    .bind(author.0)
    .fetch_one(w.pool())
    .await
    .unwrap();
    sqlx::query("INSERT INTO pull_requests (issue_id, repo_id, head_repo_id, head_ref, head_sha, base_ref, base_sha) VALUES ($1, $2, $3, 'feature', $4, 'main', $4)")
        .bind(issue)
        .bind(repo.0)
        .bind(head_repo.0)
        .bind(sha)
        .execute(w.pool())
        .await
        .unwrap();
    issue
}

async fn counts(w: &World) -> (i64, i64, i64) {
    sqlx::query_as("SELECT (SELECT count(*) FROM workflow_runs), (SELECT count(*) FROM workflow_jobs), (SELECT count(*) FROM check_runs)")
        .fetch_one(w.pool())
        .await
        .unwrap()
}

async fn sync(w: &World, repo: RepoId, pull: i64, sha: &str, pusher: Option<UserId>) {
    sqlx::query("UPDATE pull_requests SET head_sha = $2 WHERE issue_id = $1").bind(pull).bind(sha).execute(w.pool()).await.unwrap();
    w.emit_and_process(DomainEvent::PrSynchronized {
        repo_id: repo,
        pull_id: PullId(pull),
        number: 1,
        head_sha: sha.into(),
        app: None,
        pusher,
        origin_known: true,
        built_on: None,
    })
    .await;
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn nothing_exists_for_a_forks_commit_until_it_is_approved(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, GROUPED)]).await;
    let alice = Who::User(w.alice);
    let (s, _) = w.post("/api/v3/repos/alice/demo/actions/variables", &alice, json!({"name": "SECRETISH", "value": "owner-only"})).await;
    assert_eq!(s, StatusCode::CREATED);
    // A trusted run is going in the group the fork's workflow names.
    let first = w.push(repo, &git, "main", None).await[0];
    let r = w.runner("r1", &["ubuntu-latest"]).await;
    r.request(&w, 0).await.expect("the trusted run starts");
    let before = counts(&w).await;

    let fork = w.env.repo(w.bob, "demo").await;
    w.dirs_alias(fork, repo);
    let pull = insert_pull(&w, repo, fork, w.bob, &git.head()).await;
    w.emit_and_process(DomainEvent::PrOpened {
        repo_id: repo,
        pull_id: PullId(pull),
        number: 1,
        actor: w.bob,
        app: None,
        head_sha: None,
        pusher: Some(w.bob),
    })
    .await;
    assert_eq!(counts(&w).await, before, "no run, job or check row before approval");
    assert_eq!(w.run_status(first).await.0, "in_progress", "the fork cancelled nothing");
    let asked: Option<String> = sqlx::query_scalar("SELECT event_payload->'meta'->>'cancel_requested' FROM workflow_runs WHERE id = $1")
        .bind(first)
        .fetch_one(w.pool())
        .await
        .unwrap();
    assert_ne!(asked.as_deref(), Some("true"));
    let titles: Vec<String> = sqlx::query_scalar("SELECT title FROM workflow_runs").fetch_all(w.pool()).await.unwrap();
    assert_eq!(titles.len(), 1);

    // A new commit replaces the request: the old one can no longer be approved.
    let old = git.head();
    git.commit_file("b.txt", "b", "second");
    sync(&w, repo, pull, &git.head(), Some(w.bob)).await;
    assert_eq!(counts(&w).await, before);
    assert_eq!(w.get(APPROVAL, &alice).await.1["head_sha"], git.head());
    assert_eq!(w.post(APPROVAL, &alice, json!({"head_sha": old})).await.0, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(counts(&w).await, before);

    // Approved: now the run exists. Its run-name and group were worked out without the
    // repository's variables, and it takes its place in the group only now.
    let (s, made) = w.post(APPROVAL, &alice, json!({"head_sha": git.head()})).await;
    assert_eq!(s, StatusCode::CREATED, "{made}");
    let run = made["run_ids"][0].as_i64().unwrap();
    let title: String = sqlx::query_scalar("SELECT title FROM workflow_runs WHERE id = $1").bind(run).fetch_one(w.pool()).await.unwrap();
    assert!(!title.contains("owner-only"), "a fork's run-name never reads the repository's variables: {title}");
    let asked: Option<String> = sqlx::query_scalar("SELECT event_payload->'meta'->>'cancel_requested' FROM workflow_runs WHERE id = $1")
        .bind(first)
        .fetch_one(w.pool())
        .await
        .unwrap();
    assert_eq!(asked.as_deref(), Some("true"), "concurrency applies once approved");
    // A later commit needs its own approval.
    git.commit_file("c.txt", "c", "third");
    let n = w.run_ids().await.len();
    sync(&w, repo, pull, &git.head(), Some(w.bob)).await;
    assert_eq!(w.run_ids().await.len(), n, "a new commit waits again");
    assert_eq!(w.get(APPROVAL, &alice).await.1["pending"], true);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_author_and_the_pusher_both_need_write_access(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, GROUPED)]).await;
    let carol = w.env.insert_user("carol", false, false).await;
    // Bob may write here, but his pull request comes from a fork.
    w.env.collaborator(repo, w.bob, "write").await;
    let fork = w.env.repo(w.bob, "demo").await;
    w.dirs_alias(fork, repo);
    let pull = insert_pull(&w, repo, fork, w.bob, &git.head()).await;
    w.emit_and_process(DomainEvent::PrOpened {
        repo_id: repo,
        pull_id: PullId(pull),
        number: 1,
        actor: w.bob,
        app: None,
        head_sha: None,
        pusher: Some(w.bob),
    })
    .await;
    assert_eq!(w.run_ids().await.len(), 1, "author and opener with write access run at once");
    // Carol, who cannot write here, pushes to the fork's branch.
    git.commit_file("b.txt", "b", "carol's");
    sync(&w, repo, pull, &git.head(), Some(carol)).await;
    assert_eq!(w.run_ids().await.len(), 1, "a pusher without write access is held");
    assert_eq!(w.get(APPROVAL, &Who::User(w.alice)).await.1["pending"], true);
    // A push nobody can name is held too.
    git.commit_file("c.txt", "c", "unknown");
    sync(&w, repo, pull, &git.head(), None).await;
    assert_eq!(w.run_ids().await.len(), 1, "an unknown pusher is held");
    // Bob pushes again himself: both have write access.
    git.commit_file("d.txt", "d", "bob's");
    sync(&w, repo, pull, &git.head(), Some(w.bob)).await;
    assert_eq!(w.run_ids().await.len(), 2);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_run_cancelled_before_approval_cannot_be_rerun(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, &basic_workflow())]).await;
    let run = w.push(repo, &git, "main", None).await[0];
    // As a held run looks once it was cancelled while waiting.
    sqlx::query(
        "UPDATE workflow_runs SET status = 'completed', conclusion = 'cancelled',
            event_payload = jsonb_set(event_payload, '{meta,hold_reason}', '\"Started by a connected app\"') WHERE id = $1",
    )
    .bind(run)
    .execute(w.pool())
    .await
    .unwrap();
    let alice = Who::User(w.alice);
    let (s, body) = w.post(&format!("/api/v3/repos/alice/demo/actions/runs/{run}/rerun"), &alice, json!({})).await;
    assert_eq!(s, StatusCode::CONFLICT, "{body}");
    assert_eq!(w.run_ids().await.len(), 1);
    // Once a person approved it, a re-run is fine.
    sqlx::query(
        "UPDATE workflow_runs SET event_payload = jsonb_set(event_payload, '{meta,approved_by}', to_jsonb($2::bigint)) WHERE id = $1",
    )
    .bind(run)
    .bind(w.alice.0)
    .execute(w.pool())
    .await
    .unwrap();
    assert_eq!(w.post(&format!("/api/v3/repos/alice/demo/actions/runs/{run}/rerun"), &alice, json!({})).await.0, StatusCode::CREATED);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_migration_cancels_fork_runs_waiting_under_the_old_scheme(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, &basic_workflow())]).await;
    let run = w.push(repo, &git, "main", None).await[0];
    // A fork run held the old way, and a held run that is not a fork's.
    sqlx::query(
        "UPDATE workflow_runs SET status = 'waiting', event_payload = jsonb_set(event_payload, '{meta,fork}', 'true') WHERE id = $1",
    )
    .bind(run)
    .execute(w.pool())
    .await
    .unwrap();
    git.commit_file("b.txt", "b", "second");
    let other = *w.push(repo, &git, "main", None).await.iter().max().unwrap();
    sqlx::query("UPDATE workflow_runs SET status = 'waiting' WHERE id = $1").bind(other).execute(w.pool()).await.unwrap();
    let sql = include_str!("../../ch-db/migrations/0123_fork_run_approvals.sql");
    let cancel = &sql[sql.find("UPDATE check_runs").unwrap()..];
    sqlx::raw_sql(cancel).execute(w.pool()).await.unwrap();
    assert_eq!(w.run_status(run).await, ("completed".to_string(), Some("cancelled".to_string())));
    assert!(w.jobs(run).await.iter().all(|j| j.1 == "completed" && j.2.as_deref() == Some("cancelled")));
    let open: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM check_runs c JOIN workflow_jobs j ON j.id = c.job_id WHERE j.run_id = $1 AND c.status <> 'completed'",
    )
    .bind(run)
    .fetch_one(w.pool())
    .await
    .unwrap();
    assert_eq!(open, 0);
    assert_eq!(w.run_status(other).await.0, "waiting", "only fork runs are cancelled");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn an_app_pushing_to_a_fork_is_approved_by_commit_like_any_fork(pool: PgPool) {
    // Author and pusher may write here, but a connected app without the workflows permission
    // pushed the commit. A fork's run is approved before it exists, never as a waiting run.
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, GROUPED)]).await;
    w.env.collaborator(repo, w.bob, "write").await;
    let fork = w.env.repo(w.bob, "demo").await;
    w.dirs_alias(fork, repo);
    let pull = insert_pull(&w, repo, fork, w.bob, &git.head()).await;
    let app = Some(ch_core::AppOrigin { grant_id: ch_core::GrantId(9), ci_allowed: false });
    w.emit_and_process(DomainEvent::PrOpened {
        repo_id: repo,
        pull_id: PullId(pull),
        number: 1,
        actor: w.bob,
        app,
        head_sha: None,
        pusher: Some(w.bob),
    })
    .await;
    assert!(w.run_ids().await.is_empty(), "nothing before approval");
    let (s, made) = w.post(APPROVAL, &Who::User(w.alice), json!({"head_sha": git.head()})).await;
    assert_eq!(s, StatusCode::CREATED, "{made}");
    let run = made["run_ids"][0].as_i64().unwrap();
    assert_eq!(w.run_status(run).await.0, "queued", "approved by commit, so not held again");
}

/// The pusher on an event describes the commit that event names. When the pull request's head
/// is a later commit by the time CI handles it, that later commit is not run on this event:
/// someone without write access may have pushed it. Its own event judges it.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_later_commit_does_not_ride_on_an_earlier_pushers_event(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, GROUPED)]).await;
    let carol = w.env.insert_user("carol", false, false).await;
    w.env.collaborator(repo, w.bob, "write").await;
    let fork = w.env.repo(w.bob, "demo").await;
    w.dirs_alias(fork, repo);
    let opened_at = git.head();
    let pull = insert_pull(&w, repo, fork, w.bob, &opened_at).await;

    // Bob opened it at A, but Carol pushed C before CI handled the open.
    let c = git.commit_file("c.txt", "c", "carol's");
    sqlx::query("UPDATE pull_requests SET head_sha = $2 WHERE issue_id = $1").bind(pull).bind(&c).execute(w.pool()).await.unwrap();
    w.emit_and_process(DomainEvent::PrOpened {
        repo_id: repo,
        pull_id: PullId(pull),
        number: 1,
        actor: w.bob,
        app: None,
        head_sha: Some(opened_at),
        pusher: Some(w.bob),
    })
    .await;
    assert!(w.run_ids().await.is_empty(), "Carol's commit ran on Bob's open");

    // Bob pushes B, Carol pushes D on top before CI handles Bob's push.
    let b = git.commit_file("b.txt", "b", "bob's");
    let d = git.commit_file("d.txt", "d", "carol's again");
    sqlx::query("UPDATE pull_requests SET head_sha = $2 WHERE issue_id = $1").bind(pull).bind(&d).execute(w.pool()).await.unwrap();
    w.emit_and_process(DomainEvent::PrSynchronized {
        repo_id: repo,
        pull_id: PullId(pull),
        number: 1,
        head_sha: b,
        app: None,
        pusher: Some(w.bob),
        origin_known: true,
        built_on: None,
    })
    .await;
    let shas: Vec<String> = sqlx::query_scalar("SELECT head_sha FROM workflow_runs").fetch_all(w.pool()).await.unwrap();
    assert!(!shas.contains(&d), "Carol's commit ran on Bob's push: {shas:?}");
    assert!(shas.is_empty(), "{shas:?}");

    // Carol's own event for D holds it for approval.
    sync(&w, repo, pull, &d, Some(carol)).await;
    assert!(w.run_ids().await.is_empty());
    assert_eq!(w.get(APPROVAL, &Who::User(w.alice)).await.1["head_sha"], d);
}

/// A commit nobody can be named for is held: a fork's waits for approval of the commit, and a
/// branch of the repository gets a run that waits for a person, since an app may have pushed it.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_commit_nobody_can_be_named_for_waits_for_a_person(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, GROUPED)]).await;
    w.env.collaborator(repo, w.bob, "write").await;
    let fork = w.env.repo(w.bob, "demo").await;
    w.dirs_alias(fork, repo);
    let pull = insert_pull(&w, repo, fork, w.bob, &git.head()).await;
    let b = git.commit_file("b.txt", "b", "unknown");
    sqlx::query("UPDATE pull_requests SET head_sha = $2 WHERE issue_id = $1").bind(pull).bind(&b).execute(w.pool()).await.unwrap();
    w.emit_and_process(DomainEvent::PrSynchronized {
        repo_id: repo,
        pull_id: PullId(pull),
        number: 1,
        head_sha: b.clone(),
        app: None,
        pusher: None,
        origin_known: false,
        built_on: None,
    })
    .await;
    assert!(w.run_ids().await.is_empty(), "a fork's commit nobody pushed by name waits for approval");
    assert_eq!(w.get(APPROVAL, &Who::User(w.alice)).await.1["head_sha"], b);
    // Reopening names nobody either, so a fork's commit is held on reopen too.
    w.emit_and_process(DomainEvent::PrOpened {
        repo_id: repo,
        pull_id: PullId(pull),
        number: 1,
        actor: w.bob,
        app: None,
        head_sha: None,
        pusher: None,
    })
    .await;
    assert!(w.run_ids().await.is_empty(), "a reopen by someone with write access is not an approval");

    // The same on a branch of this repository: the run is made, and it waits.
    let issue: i64 = sqlx::query_scalar(
        "INSERT INTO issues (repo_id, number, is_pull, author_id, title) VALUES ($1, 2, true, $2, 'Same repo') RETURNING id",
    )
    .bind(repo.0)
    .bind(w.bob.0)
    .fetch_one(w.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO pull_requests (issue_id, repo_id, head_ref, head_sha, base_ref, base_sha) VALUES ($1, $2, 'feature', $3, 'main', $3)",
    )
    .bind(issue)
    .bind(repo.0)
    .bind(&b)
    .execute(w.pool())
    .await
    .unwrap();
    let runs = w
        .emit_and_process(DomainEvent::PrSynchronized {
            repo_id: repo,
            pull_id: PullId(issue),
            number: 2,
            head_sha: b.clone(),
            app: None,
            pusher: Some(w.bob),
            origin_known: false,
            built_on: None,
        })
        .await;
    assert_eq!(runs.len(), 1);
    assert_eq!(w.run_status(runs[0]).await.0, "waiting");
    let reason: Option<String> = sqlx::query_scalar("SELECT event_payload->'meta'->>'hold_reason' FROM workflow_runs WHERE id = $1")
        .bind(runs[0])
        .fetch_one(w.pool())
        .await
        .unwrap();
    assert_eq!(reason.as_deref(), Some("It is not known who pushed this commit"));
}

/// A commit the server built on top of the head for someone (Update branch, an applied
/// suggestion, a web edit) holds that head's code as well as theirs. The person vouches for
/// their own change only, so it runs at once only when the commit it was built on was itself let
/// run or approved. Otherwise a writer's click would run a non-writer's waiting commit.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_commit_built_on_a_waiting_commit_waits_too(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, GROUPED)]).await;
    let carol = w.env.insert_user("carol", false, false).await;
    let dave = w.env.insert_user("dave", false, false).await;
    w.env.collaborator(repo, carol, "write").await;
    w.env.collaborator(repo, dave, "read").await;
    let fork = w.env.repo(carol, "demo").await;
    w.dirs_alias(fork, repo);
    let opened = git.head();
    let pull = insert_pull(&w, repo, fork, carol, &opened).await;
    w.emit_and_process(DomainEvent::PrOpened {
        repo_id: repo,
        pull_id: PullId(pull),
        number: 1,
        actor: carol,
        app: None,
        head_sha: Some(opened.clone()),
        pusher: Some(carol),
    })
    .await;
    assert_eq!(w.run_ids().await.len(), 1, "carol can write, so her pull request runs");

    // Built on a commit that ran: it runs at once.
    let s1 = git.commit_file("s1.txt", "s1", "Apply suggestion");
    built(&w, repo, pull, &s1, carol, &opened).await;
    assert_eq!(w.run_ids().await.len(), 2, "built on a commit that was let run");

    // Dave, who cannot write here, pushes to carol's branch. It waits.
    let d = git.commit_file("dave.txt", "dave", "dave's");
    sync(&w, repo, pull, &d, Some(dave)).await;
    assert_eq!(w.run_ids().await.len(), 2);
    assert_eq!(w.get(APPROVAL, &Who::User(w.alice)).await.1["head_sha"], d);

    // Carol clicks Update branch: the merge commit holds dave's commit.
    let m = git.commit_file("merge.txt", "m", "Merge branch 'main' into ub");
    built(&w, repo, pull, &m, carol, &d).await;
    assert_eq!(w.run_ids().await.len(), 2, "dave's waiting commit ran under carol's click");
    let (_, body) = w.get(APPROVAL, &Who::User(w.alice)).await;
    assert_eq!((body["pending"].as_bool(), body["head_sha"].as_str()), (Some(true), Some(m.as_str())), "{body}");

    // Approved, it runs, and a commit built on it runs at once.
    let (s, made) = w.post(APPROVAL, &Who::User(w.alice), json!({"head_sha": m})).await;
    assert_eq!(s, StatusCode::CREATED, "{made}");
    assert_eq!(w.run_ids().await.len(), 3);
    let s2 = git.commit_file("s2.txt", "s2", "Apply suggestion");
    built(&w, repo, pull, &s2, carol, &m).await;
    assert_eq!(w.run_ids().await.len(), 4, "built on an approved commit");

    // Built on a commit CI never judged (its event was passed over): it waits.
    let gap = git.commit_file("gap.txt", "gap", "never judged");
    let s3 = git.commit_file("s3.txt", "s3", "Apply suggestion");
    built(&w, repo, pull, &s3, carol, &gap).await;
    assert_eq!(w.run_ids().await.len(), 4, "built on a commit nobody let run");
}

/// The same on a branch of this repository: a commit built on a connected app's waiting commit
/// is made as a run that waits for a person.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_same_repository_commit_built_on_a_held_commit_is_held(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, GROUPED)]).await;
    w.env.collaborator(repo, w.bob, "write").await;
    let issue: i64 = sqlx::query_scalar(
        "INSERT INTO issues (repo_id, number, is_pull, author_id, title) VALUES ($1, 1, true, $2, 'Same repo') RETURNING id",
    )
    .bind(repo.0)
    .bind(w.bob.0)
    .fetch_one(w.pool())
    .await
    .unwrap();
    let a = git.commit_file("app.txt", "app", "the app's");
    sqlx::query(
        "INSERT INTO pull_requests (issue_id, repo_id, head_ref, head_sha, base_ref, base_sha) VALUES ($1, $2, 'feature', $3, 'main', $3)",
    )
    .bind(issue)
    .bind(repo.0)
    .bind(&a)
    .execute(w.pool())
    .await
    .unwrap();
    let runs = w
        .emit_and_process(DomainEvent::PrSynchronized {
            repo_id: repo,
            pull_id: PullId(issue),
            number: 1,
            head_sha: a.clone(),
            app: Some(ch_core::AppOrigin { grant_id: ch_core::GrantId(9), ci_allowed: false }),
            pusher: Some(w.bob),
            origin_known: true,
            built_on: None,
        })
        .await;
    assert_eq!(w.run_status(runs[0]).await.0, "waiting", "the app's commit waits");
    let m = git.commit_file("m.txt", "m", "Merge branch 'main' into feature");
    sqlx::query("UPDATE pull_requests SET head_sha = $2 WHERE issue_id = $1").bind(issue).bind(&m).execute(w.pool()).await.unwrap();
    let runs = w
        .emit_and_process(DomainEvent::PrSynchronized {
            repo_id: repo,
            pull_id: PullId(issue),
            number: 1,
            head_sha: m.clone(),
            app: None,
            pusher: Some(w.bob),
            origin_known: true,
            built_on: Some(a),
        })
        .await;
    assert_eq!(runs.len(), 2, "a run is made for the merge commit");
    assert_eq!(w.run_status(runs[1]).await.0, "waiting", "the app's code ran under bob's click");
    let reason: Option<String> = sqlx::query_scalar("SELECT event_payload->'meta'->>'hold_reason' FROM workflow_runs WHERE id = $1")
        .bind(runs[1])
        .fetch_one(w.pool())
        .await
        .unwrap();
    assert_eq!(reason.as_deref(), Some("It builds on a commit that waits for approval"));
}

/// A commit the server wrote for `pusher` on top of `parent`, as Update branch or a suggestion
/// reports it.
async fn built(w: &World, repo: RepoId, pull: i64, sha: &str, pusher: UserId, parent: &str) {
    sqlx::query("UPDATE pull_requests SET head_sha = $2 WHERE issue_id = $1").bind(pull).bind(sha).execute(w.pool()).await.unwrap();
    w.emit_and_process(DomainEvent::PrSynchronized {
        repo_id: repo,
        pull_id: PullId(pull),
        number: 1,
        head_sha: sha.into(),
        app: None,
        pusher: Some(pusher),
        origin_known: true,
        built_on: Some(parent.into()),
    })
    .await;
}
