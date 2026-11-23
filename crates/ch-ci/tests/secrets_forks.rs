//! Secrets and variables, fork pull requests, and the safe list for container options.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;
use axum::http::{Method, StatusCode};
use ch_core::events::DomainEvent;
use serde_json::json;
use sqlx::PgPool;
use support::*;

const NEEDS_SECRET: &str = "name: ci
on: [push, pull_request]
jobs:
  deploy:
    runs-on: x
    env:
      PLAIN: ${{ vars.GREETING }}
    steps:
      - run: echo ${{ secrets.DEPLOY_KEY }}
";

async fn put_secret(w: &World, uri: &str, who: &Who, value: &str) -> StatusCode {
    w.send(Method::PUT, uri, who, Some(json!({"value": value}))).await.0
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_job_gets_only_the_secrets_it_names_and_they_are_sealed_at_rest(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, NEEDS_SECRET)]).await;
    let alice = Who::User(w.alice);
    assert_eq!(put_secret(&w, "/api/v3/repos/alice/demo/actions/secrets/DEPLOY_KEY", &alice, "hunter2-the-key").await, StatusCode::CREATED);
    assert_eq!(put_secret(&w, "/api/v3/repos/alice/demo/actions/secrets/UNUSED", &alice, "never sent").await, StatusCode::CREATED);
    assert_eq!(
        put_secret(&w, "/api/v3/repos/alice/demo/actions/secrets/DEPLOY_KEY", &alice, "hunter2-the-key").await,
        StatusCode::NO_CONTENT,
        "updating is a 204"
    );
    let (s, _) = w.post("/api/v3/repos/alice/demo/actions/variables", &alice, json!({"name": "GREETING", "value": "hello"})).await;
    assert_eq!(s, StatusCode::CREATED);

    // At rest the value is not readable.
    let sealed: Vec<u8> =
        sqlx::query_scalar("SELECT value_sealed FROM secrets WHERE name = 'DEPLOY_KEY'").fetch_one(w.pool()).await.unwrap();
    assert!(!String::from_utf8_lossy(&sealed).contains("hunter2"));
    // And no API response ever carries it.
    let (_, list) = w.get("/api/v3/repos/alice/demo/actions/secrets", &alice).await;
    assert_eq!(list["total_count"], 2);
    assert!(!list.to_string().contains("hunter2"));
    let (_, one) = w.get("/api/v3/repos/alice/demo/actions/secrets/DEPLOY_KEY", &alice).await;
    assert!(one.get("value").is_none());

    w.push(repo, &git, "main", None).await;
    let r = w.runner("r", &["x"]).await;
    let job = r.request(&w, 0).await.unwrap();
    assert_eq!(job.secrets.len(), 1, "UNUSED is not sent");
    assert_eq!(job.secrets["DEPLOY_KEY"], "hunter2-the-key");
    assert_eq!(job.vars["GREETING"], "hello");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_repo_secret_beats_the_owners_and_a_copied_value_does_not_open(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, NEEDS_SECRET)]).await;
    let alice = Who::User(w.alice);
    // alice is a user account here, so "org" scope is her own account-wide secret.
    assert_eq!(put_secret(&w, "/api/v3/orgs/alice/actions/secrets/DEPLOY_KEY", &alice, "from the account").await, StatusCode::CREATED);
    w.push(repo, &git, "main", None).await;
    let r = w.runner("r", &["x"]).await;
    assert_eq!(r.request(&w, 0).await.unwrap().secrets["DEPLOY_KEY"], "from the account");
    assert_eq!(put_secret(&w, "/api/v3/repos/alice/demo/actions/secrets/DEPLOY_KEY", &alice, "from the repo").await, StatusCode::CREATED);
    git.commit_file("x.txt", "1", "again");
    let before = git.git(&["rev-parse", "HEAD~1"]);
    w.push(repo, &git, "main", Some(&before)).await;
    assert_eq!(r.request(&w, 0).await.unwrap().secrets["DEPLOY_KEY"], "from the repo");

    // A sealed value moved to a different secret name is refused: it is tied to its name.
    assert_eq!(put_secret(&w, "/api/v3/repos/alice/demo/actions/secrets/OTHER", &alice, "another").await, StatusCode::CREATED);
    sqlx::query("UPDATE secrets SET value_sealed = (SELECT value_sealed FROM secrets WHERE name = 'OTHER' AND scope = 'repo') WHERE name = 'DEPLOY_KEY' AND scope = 'repo'").execute(w.pool()).await.unwrap();
    git.commit_file("y.txt", "1", "third");
    let before = git.git(&["rev-parse", "HEAD~1"]);
    w.push(repo, &git, "main", Some(&before)).await;
    // The job cannot be built, so it fails instead of running with a wrong secret.
    assert!(r.request(&w, 0).await.is_none());
    let failed: i64 =
        sqlx::query_scalar("SELECT count(*) FROM workflow_jobs WHERE conclusion = 'failure'").fetch_one(w.pool()).await.unwrap();
    assert_eq!(failed, 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn who_may_manage_secrets_and_how_they_are_audited(pool: PgPool) {
    let w = World::new(pool).await;
    w.repo_with(w.alice, "demo", &[(CI_YML, NEEDS_SECRET)]).await;
    let repo = w.env.count("repositories").await;
    assert_eq!(repo, 1);
    let uri = "/api/v3/repos/alice/demo/actions/secrets/TOKEN";
    // bob cannot see the private repo at all.
    assert_eq!(put_secret(&w, uri, &Who::User(w.bob), "x").await, StatusCode::NOT_FOUND);
    // A collaborator with write access can see it but not manage secrets.
    let id: i64 = sqlx::query_scalar("SELECT id FROM repositories").fetch_one(w.pool()).await.unwrap();
    w.env.collaborator(ch_core::RepoId(id), w.bob, "write").await;
    assert_eq!(put_secret(&w, uri, &Who::User(w.bob), "x").await, StatusCode::FORBIDDEN);
    assert_eq!(w.get("/api/v3/repos/alice/demo/actions/secrets", &Who::User(w.bob)).await.0, StatusCode::FORBIDDEN);
    assert_eq!(put_secret(&w, uri, &Who::Anon, "x").await, StatusCode::UNAUTHORIZED);
    // Bad names and oversized values are refused.
    let alice = Who::User(w.alice);
    for bad in ["lower", "1ABC", "GITHUB_TOKEN", "A-B"] {
        assert_eq!(
            put_secret(&w, &format!("/api/v3/repos/alice/demo/actions/secrets/{bad}"), &alice, "x").await,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{bad}"
        );
    }
    assert_eq!(put_secret(&w, uri, &alice, &"x".repeat(70_000)).await, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(put_secret(&w, uri, &alice, "s3cret-value").await, StatusCode::CREATED);
    let (s, _) = w.send(Method::DELETE, uri, &alice, None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(w.send(Method::DELETE, uri, &alice, None).await.0, StatusCode::NOT_FOUND);
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT action, details::text FROM audit_events ORDER BY id").fetch_all(w.pool()).await.unwrap();
    assert_eq!(rows.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(), vec!["secret.changed", "secret.deleted"]);
    assert!(!rows.iter().any(|r| r.1.contains("s3cret-value")), "the value never reaches the audit log");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn variables_can_be_listed_updated_and_deleted(pool: PgPool) {
    let w = World::new(pool).await;
    w.repo_with(w.alice, "demo", &[(CI_YML, NEEDS_SECRET)]).await;
    let alice = Who::User(w.alice);
    let base = "/api/v3/repos/alice/demo/actions/variables";
    assert_eq!(w.post(base, &alice, json!({"name": "A", "value": "1"})).await.0, StatusCode::CREATED);
    assert_eq!(
        w.post(base, &alice, json!({"name": "A", "value": "2"})).await.0,
        StatusCode::UNPROCESSABLE_ENTITY,
        "creating twice is refused"
    );
    assert_eq!(w.send(Method::PATCH, &format!("{base}/A"), &alice, Some(json!({"value": "3"}))).await.0, StatusCode::NO_CONTENT);
    let (_, v) = w.get(&format!("{base}/A"), &alice).await;
    assert_eq!(v["value"], "3");
    assert_eq!(w.send(Method::PATCH, &format!("{base}/A"), &alice, Some(json!({"name": "B"}))).await.0, StatusCode::NO_CONTENT);
    let (_, list) = w.get(base, &alice).await;
    assert_eq!(list["variables"][0]["name"], "B");
    assert_eq!(list["variables"][0]["value"], "3");
    assert_eq!(w.send(Method::DELETE, &format!("{base}/B"), &alice, None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(w.send(Method::PATCH, &format!("{base}/B"), &alice, Some(json!({"value": "x"}))).await.0, StatusCode::NOT_FOUND);
}

// ------------------------------------------------------------------ fork pull requests

/// A pull request from `head_repo` into `repo`, authored by `author`. `None` is a branch of
/// `repo` itself, stored the way ch-collab stores it: with no head repository.
async fn open_pull(w: &World, repo: ch_core::RepoId, head_repo: Option<ch_core::RepoId>, author: ch_core::UserId, sha: &str) -> i64 {
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
        .bind(head_repo.map(|h| h.0))
        .bind(sha)
        .execute(w.pool())
        .await
        .unwrap();
    w.emit_and_process(DomainEvent::PrOpened {
        repo_id: repo,
        pull_id: ch_core::PullId(issue),
        number: 1,
        actor: author,
        app: None,
        head_sha: None,
        pusher: Some(author),
    })
    .await;
    issue
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_pull_request_from_a_fork_waits_for_approval_and_gets_no_secrets(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, NEEDS_SECRET)]).await;
    let fork = w.env.repo(w.bob, "demo").await;
    w.dirs_alias(fork, repo);
    let alice = Who::User(w.alice);
    put_secret(&w, "/api/v3/repos/alice/demo/actions/secrets/DEPLOY_KEY", &alice, "hunter2").await;
    let (s, _) = w.post("/api/v3/repos/alice/demo/actions/variables", &alice, json!({"name": "GREETING", "value": "hello"})).await;
    assert_eq!(s, StatusCode::CREATED);
    open_pull(&w, repo, Some(fork), w.bob, &git.head()).await;
    // Held: nothing exists until the commit is approved, and no job is offered.
    assert!(w.run_ids().await.is_empty(), "no run before approval");
    let r = w.runner("r", &["x"]).await;
    assert!(r.request(&w, 0).await.is_none());
    let uri = "/api/v3/repos/alice/demo/actions/pulls/1/approval";
    let (s, pending) = w.get(uri, &alice).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(pending["pending"], true);
    assert_eq!(pending["head_sha"], git.head());
    // Bob cannot approve his own pull request. Alice can, once.
    let body = json!({"head_sha": git.head()});
    assert_eq!(w.post(uri, &Who::User(w.bob), body.clone()).await.0, StatusCode::NOT_FOUND);
    assert!(w.run_ids().await.is_empty());
    let (s, made) = w.post(uri, &alice, body.clone()).await;
    assert_eq!(s, StatusCode::CREATED, "{made}");
    assert_eq!(made["run_ids"].as_array().unwrap().len(), 1);
    assert_eq!(w.post(uri, &alice, body).await.0, StatusCode::CONFLICT, "approving twice is refused");
    let run = w.run_ids().await[0];
    assert_eq!(w.run_status(run).await.0, "queued");
    let approved: Option<i64> =
        sqlx::query_scalar("SELECT (event_payload->'meta'->>'approved_by')::bigint FROM workflow_runs WHERE id = $1")
            .bind(run)
            .fetch_one(w.pool())
            .await
            .unwrap();
    assert_eq!(approved, Some(w.alice.0), "the run records who approved it");
    assert_eq!(w.get(uri, &alice).await.1["pending"], false);
    let job = r.request(&w, 0).await.expect("approved");
    assert!(job.secrets.is_empty(), "a fork's code never sees secrets: {:?}", job.secrets);
    assert!(job.vars.is_empty(), "nor the repository's or owner's variables: {:?}", job.vars);
    assert_eq!(job.context.event_name, "pull_request");
    assert_eq!(job.context.git_ref, "refs/pull/1/merge");
    assert_eq!(job.context.event["pull_request"]["head"]["repo"]["full_name"], "bob/demo");
    // Its token cannot write artifacts or caches.
    let jt = Who::Bearer(job.token.clone());
    let (s, _) = w
        .send_raw(
            Method::PUT,
            &format!("/api/runner/v1/jobs/{}/artifacts/x", job.job_id),
            &jt,
            Some((bytes::Bytes::from_static(b"x"), "application/zip")),
        )
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) =
        w.send_raw(Method::PUT, "/api/runner/v1/cache/k", &jt, Some((bytes::Bytes::from_static(b"x"), "application/octet-stream"))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // And it cannot post commit statuses.
    let sha = git.head();
    let (s, _) = w.post(&format!("/api/v3/repos/alice/demo/statuses/{sha}"), &jt, json!({"state": "success", "context": "evil"})).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "a fork job gets read access only");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_pull_request_from_a_collaborator_runs_at_once_with_secrets(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, NEEDS_SECRET)]).await;
    w.env.collaborator(repo, w.bob, "write").await;
    put_secret(&w, "/api/v3/repos/alice/demo/actions/secrets/DEPLOY_KEY", &Who::User(w.alice), "hunter2").await;
    open_pull(&w, repo, None, w.bob, &git.head()).await;
    let run = w.run_ids().await[0];
    assert_eq!(w.run_status(run).await.0, "queued");
    let r = w.runner("r", &["x"]).await;
    let job = r.request(&w, 0).await.unwrap();
    assert_eq!(job.secrets["DEPLOY_KEY"], "hunter2");
    // A fork whose author has write access is trusted the same way.
    let fork = w.env.repo(w.bob, "demo2").await;
    sqlx::query("UPDATE pull_requests SET head_repo_id = $1").bind(fork.0).execute(w.pool()).await.unwrap();
}

/// The fork flag the run was stored with.
async fn run_is_fork(w: &World, run: i64) -> bool {
    let payload: serde_json::Value =
        sqlx::query_scalar("SELECT event_payload FROM workflow_runs WHERE id = $1").bind(run).fetch_one(w.pool()).await.unwrap();
    ch_ci::spec::split_payload(&payload).1.fork
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_same_repo_pull_request_gets_a_job_token_that_can_write(pool: PgPool) {
    // ch-x12 regression: ch-collab stores a branch of the same repository with no head
    // repository (NULL), and that is not a fork. bob has write access and opens a pull request
    // from a branch of alice/demo: its job uploads artifacts, saves caches and posts statuses.
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, NEEDS_SECRET)]).await;
    w.env.collaborator(repo, w.bob, "write").await;
    put_secret(&w, "/api/v3/repos/alice/demo/actions/secrets/DEPLOY_KEY", &Who::User(w.alice), "hunter2").await;
    open_pull(&w, repo, None, w.bob, &git.head()).await;
    let run = w.run_ids().await[0];
    assert!(!run_is_fork(&w, run).await, "a branch of the same repository is not a fork");
    assert_eq!(w.run_status(run).await.0, "queued");
    let r = w.runner("r", &["x"]).await;
    let job = r.request(&w, 0).await.unwrap();
    assert_eq!(job.secrets["DEPLOY_KEY"], "hunter2");
    let jt = Who::Bearer(job.token.clone());
    let (s, _) = w
        .send_raw(
            Method::PUT,
            &format!("/api/runner/v1/jobs/{}/artifacts/x", job.job_id),
            &jt,
            Some((bytes::Bytes::from_static(b"PK\x03\x04 pretend"), "application/zip")),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "upload-artifact");
    let (s, _) = w
        .send_raw(
            Method::PUT,
            "/api/runner/v1/cache/k?version=v1",
            &jt,
            Some((bytes::Bytes::from_static(b"x"), "application/octet-stream")),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "cache save");
    let sha = git.head();
    let (s, _) = w.post(&format!("/api/v3/repos/alice/demo/statuses/{sha}"), &jt, json!({"state": "success", "context": "ci"})).await;
    assert_eq!(s, StatusCode::CREATED, "commit status");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_pull_request_whose_fork_was_deleted_stays_a_fork(pool: PgPool) {
    // Deleting the fork sets head_repo_id to NULL (ON DELETE SET NULL), which on its own reads
    // like a branch of the base repository. The pull request was opened from a fork, so its
    // runs stay fork runs: held for approval, no secrets, a read-only token.
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, NEEDS_SECRET)]).await;
    let fork = w.env.repo(w.bob, "demo").await;
    w.dirs_alias(fork, repo);
    put_secret(&w, "/api/v3/repos/alice/demo/actions/secrets/DEPLOY_KEY", &Who::User(w.alice), "hunter2").await;
    let pull = open_pull(&w, repo, Some(fork), w.bob, &git.head()).await;
    sqlx::query("DELETE FROM repositories WHERE id = $1").bind(fork.0).execute(w.pool()).await.unwrap();
    let head: Option<i64> =
        sqlx::query_scalar("SELECT head_repo_id FROM pull_requests WHERE issue_id = $1").bind(pull).fetch_one(w.pool()).await.unwrap();
    assert_eq!(head, None, "the fork is gone");
    w.emit_and_process(DomainEvent::PrSynchronized {
        repo_id: repo,
        pull_id: ch_core::PullId(pull),
        number: 1,
        head_sha: git.head(),
        app: None,
        pusher: Some(w.bob),
        origin_known: true,
        built_on: None,
    })
    .await;
    assert!(w.run_ids().await.is_empty(), "still held after the fork was deleted");
    let uri = "/api/v3/repos/alice/demo/actions/pulls/1/approval";
    assert_eq!(w.post(uri, &Who::User(w.alice), json!({"head_sha": git.head()})).await.0, StatusCode::CREATED);
    let run = w.run_ids().await[0];
    assert!(run_is_fork(&w, run).await, "still a fork after the fork was deleted");
    let r = w.runner("r", &["x"]).await;
    let job = r.request(&w, 0).await.expect("approved");
    assert!(job.secrets.is_empty(), "{:?}", job.secrets);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn container_options_outside_the_safe_list_never_start_a_run(pool: PgPool) {
    let yml = "name: ci
on: push
jobs:
  b:
    runs-on: x
    container:
      image: alpine
      options: --privileged
    steps:
      - run: echo
";
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, yml)]).await;
    assert!(w.push(repo, &git, "main", None).await.is_empty());
    let files = w.state.ci.workflows(&ch_testkit::fixtures::user_actor(w.alice), repo, None).await.unwrap();
    assert!(files[0].errors[0].contains("--privileged") && files[0].errors[0].starts_with("line 8"), "{:?}", files[0].errors);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_job_token_lookup_reads_the_fork_flag_join_payload_writes(pool: PgPool) {
    // ch-x12: the one lookup authn uses (ch_db::ci_auth::job_from_token), against the exact
    // payload shape create_run stores. A fork's job token cannot write checks, artifacts or caches.
    let w = World::new(pool).await;
    let repo = w.env.repo(w.alice, "demo").await;
    for (fork, secret) in [(true, "chj_forkpayload0000000000000000000000000"), (false, "chj_samepayload0000000000000000000000000")] {
        let meta = ch_ci::spec::RunMeta { workflow_path: ".codehaven/workflows/ci.yml".into(), fork, ..Default::default() };
        let payload = ch_ci::spec::join_payload(&json!({"action": "opened"}), &meta);
        let run: i64 = sqlx::query_scalar(
            "INSERT INTO workflow_runs (repo_id, run_number, event, head_sha, event_payload) VALUES ($1, $2, 'pull_request', 'abc', $3) RETURNING id",
        )
        .bind(repo.0)
        .bind(if fork { 1_i64 } else { 2 })
        .bind(&payload)
        .fetch_one(&w.env.pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO workflow_jobs (run_id, repo_id, job_key, name, token_hash, status) VALUES ($1, $2, 'build', 'build', $3, 'in_progress')")
            .bind(run)
            .bind(repo.0)
            .bind(ch_db::tokens::hash_token(secret))
            .execute(&w.env.pool)
            .await
            .unwrap();
        let (_, _, perms) = ch_db::ci_auth::job_from_token(&w.env.pool, secret).await.unwrap().unwrap();
        assert!(perms.contents_read);
        assert_eq!(perms.write_checks, !fork, "fork={fork}");
    }
}
