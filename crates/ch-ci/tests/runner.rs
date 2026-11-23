//! The runner API: registration, claiming, leases, logs, artifacts and the cache.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;
use axum::http::{Method, StatusCode};
use bytes::Bytes;
use serde_json::json;
use sqlx::PgPool;
use std::sync::Arc;
use support::*;

async fn one_job(w: &World, yml: &str) -> (ch_core::RepoId, Arc<GitRepo>, i64) {
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, yml)]).await;
    let run = w.push(repo, &git, "main", None).await[0];
    (repo, git, run)
}

const ONE: &str = "name: ci\non: push\njobs:\n  b:\n    runs-on: x\n    steps:\n      - run: echo hi\n      - run: echo there\n";

// ------------------------------------------------------------------ registration

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_registration_token_works_once(pool: PgPool) {
    let w = World::new(pool).await;
    let token = w.registration_token().await;
    assert!(token.starts_with("chrt_"));
    let body = json!({"token": token, "name": "runner-1", "labels": ["Ubuntu-Latest", "linux", "linux", " x64 "]});
    let (s, v) = w.post("/api/runner/v1/register", &Who::Anon, body.clone()).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(v["credential"].as_str().unwrap().starts_with("chr_"));
    let (labels,): (Vec<String>,) = sqlx::query_as("SELECT labels FROM runners WHERE id = $1")
        .bind(v["runner_id"].as_i64().unwrap())
        .fetch_one(w.pool())
        .await
        .unwrap();
    assert_eq!(labels, vec!["ubuntu-latest", "linux", "x64"], "lower-cased, trimmed, no duplicates");
    // The credential is stored hashed.
    let stored: Vec<u8> = sqlx::query_scalar("SELECT credential_hash FROM runners").fetch_one(w.pool()).await.unwrap();
    assert_eq!(stored.len(), 32);
    // The same token again, and a made-up one, fail the same way.
    let (s1, v1) = w.post("/api/runner/v1/register", &Who::Anon, body).await;
    let (s2, v2) = w
        .post(
            "/api/runner/v1/register",
            &Who::Anon,
            json!({"token": "chrt_madeupmadeupmadeupmadeupmadeupmadeupma", "name": "x", "labels": ["a"]}),
        )
        .await;
    assert_eq!((s1, s2), (StatusCode::UNAUTHORIZED, StatusCode::UNAUTHORIZED));
    assert_eq!(v1, v2, "used and made-up tokens look the same");
    assert_eq!(w.env.count("runners").await, 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn guessing_registration_tokens_is_throttled_per_address(pool: PgPool) {
    let w = World::new(pool).await;
    for i in 0..10 {
        let (s, _) =
            w.post("/api/runner/v1/register", &Who::Anon, json!({"token": format!("guess{i}"), "name": "x", "labels": ["a"]})).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
    }
    // Ten failures from one address and it is told to wait, even with a good token.
    let token = w.registration_token().await;
    let (s, _) = w.post("/api/runner/v1/register", &Who::Anon, json!({"token": token, "name": "x", "labels": ["a"]})).await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
    // The audit log holds the ten failures and not an eleventh.
    let failures: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action = 'runner.registered' AND outcome = 'failure'")
        .fetch_one(w.pool())
        .await
        .unwrap();
    assert_eq!(failures, 10);
    assert_eq!(w.env.count("runners").await, 0);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn an_expired_token_does_not_register(pool: PgPool) {
    let w = World::new(pool).await;
    let token = w.registration_token().await;
    sqlx::query("UPDATE runner_registration_tokens SET expires_at = now() - interval '1 minute'").execute(w.pool()).await.unwrap();
    let (s, _) = w.post("/api/runner/v1/register", &Who::Anon, json!({"token": token, "name": "x", "labels": ["a"]})).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(w.env.count("runners").await, 0);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn twenty_runners_racing_for_one_token_make_one_runner(pool: PgPool) {
    let w = Arc::new(World::new(pool).await);
    let token = w.registration_token().await;
    let mut tasks = vec![];
    for i in 0..20 {
        let (w, token) = (w.clone(), token.clone());
        tasks.push(tokio::spawn(async move {
            w.post("/api/runner/v1/register", &Who::Anon, json!({"token": token, "name": format!("r{i}"), "labels": ["a"]})).await.0
        }));
    }
    let mut ok = 0;
    for t in tasks {
        if t.await.unwrap() == StatusCode::OK {
            ok += 1;
        }
    }
    assert_eq!(ok, 1);
    assert_eq!(w.env.count("runners").await, 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn bad_runner_details_are_refused(pool: PgPool) {
    let w = World::new(pool).await;
    for (name, labels) in
        [("", json!(["a"])), ("ok", json!([])), ("ok", json!(["bad label!"])), ("ok", json!(["a\nb"])), ("ok", json!(["x".repeat(65)]))]
    {
        let token = w.registration_token().await;
        let (s, _) = w.post("/api/runner/v1/register", &Who::Anon, json!({"token": token, "name": name, "labels": labels})).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{name:?} {labels}");
    }
    // A refused registration does not use the token up: the 422 comes after the checks that matter.
    assert_eq!(w.env.count("runners").await, 0);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn only_a_site_admin_issues_tokens_and_it_is_audited(pool: PgPool) {
    let w = World::new(pool).await;
    for who in [Who::User(w.alice), Who::User(w.bob)] {
        let (s, _) = w.post("/api/v3/admin/actions/runners/registration-token", &who, json!({})).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
    }
    let (s, _) = w.post("/api/v3/admin/actions/runners/registration-token", &Who::Anon, json!({})).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    w.registration_token().await;
    let token = w.registration_token().await;
    let (s, v) = w.post("/api/runner/v1/register", &Who::Anon, json!({"token": token, "name": "n", "labels": ["a"]})).await;
    assert_eq!(s, StatusCode::OK);
    // A failed attempt is recorded too.
    w.post("/api/runner/v1/register", &Who::Anon, json!({"token": "nope", "name": "n", "labels": ["a"]})).await;
    let rows: Vec<(String, String, Option<String>, String)> =
        sqlx::query_as("SELECT action, outcome, host(client_ip), details::text FROM audit_events ORDER BY id")
            .fetch_all(w.pool())
            .await
            .unwrap();
    let acts: Vec<(&str, &str)> = rows.iter().map(|r| (r.0.as_str(), r.1.as_str())).collect();
    assert_eq!(
        acts,
        vec![
            ("runner.token_issued", "success"),
            ("runner.token_issued", "success"),
            ("runner.registered", "success"),
            ("runner.registered", "failure")
        ]
    );
    assert_eq!(rows[2].2.as_deref(), Some("10.1.2.3"));
    let all = rows.iter().map(|r| r.3.clone()).collect::<String>();
    assert!(!all.contains("chrt_") && !all.contains(v["credential"].as_str().unwrap()), "no secret reaches the audit log: {all}");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_revoked_runner_cannot_call_in(pool: PgPool) {
    let w = World::new(pool).await;
    let r = w.runner("r", &["x"]).await;
    assert!(r.request(&w, 0).await.is_none());
    let (s, _) = w.send(Method::DELETE, &format!("/api/v3/admin/actions/runners/{}", r.id), &Who::Admin(w.root), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = w.post("/api/runner/v1/jobs/request", &r.who(), json!({"wait_seconds": 0})).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, v) = w.get("/api/v3/admin/actions/runners", &Who::Admin(w.root)).await;
    assert_eq!((s, v["total_count"].as_i64()), (StatusCode::OK, Some(0)));
}

// ------------------------------------------------------------------ claiming

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_job_goes_only_to_a_runner_with_all_its_labels(pool: PgPool) {
    let yml = "name: ci\non: push\njobs:\n  b:\n    runs-on: [self-hosted, gpu]\n    steps:\n      - run: echo\n";
    let w = World::new(pool).await;
    one_job(&w, yml).await;
    let small = w.runner("small", &["self-hosted"]).await;
    let big = w.runner("big", &["self-hosted", "gpu", "linux"]).await;
    assert!(small.request(&w, 0).await.is_none(), "missing the gpu label");
    assert!(big.request(&w, 0).await.is_some());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_repo_scoped_runner_only_serves_its_repo(pool: PgPool) {
    let w = World::new(pool).await;
    let (a, ga) = w.repo_with(w.alice, "a", &[(CI_YML, ONE)]).await;
    let (b, gb) = w.repo_with(w.alice, "b", &[(CI_YML, ONE)]).await;
    w.push(a, &ga, "main", None).await;
    w.push(b, &gb, "main", None).await;
    let (s, v) = w.post("/api/v3/repos/alice/b/actions/runners/registration-token", &Who::Admin(w.root), json!({})).await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, reg) = w.post("/api/runner/v1/register", &Who::Anon, json!({"token": v["token"], "name": "only-b", "labels": ["x"]})).await;
    assert_eq!(s, StatusCode::OK);
    let r = FakeRunner { cred: reg["credential"].as_str().unwrap().into(), id: reg["runner_id"].as_i64().unwrap() };
    let j = r.request(&w, 0).await.unwrap();
    assert_eq!(j.repo.full_name, "alice/b");
    assert!(r.request(&w, 0).await.is_none(), "repo a's job is not for it");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn ten_runners_racing_for_one_job_give_it_to_one(pool: PgPool) {
    let w = Arc::new(World::new(pool).await);
    one_job(&w, ONE).await;
    let mut rs = vec![];
    for i in 0..10 {
        rs.push(Arc::new(w.runner(&format!("r{i}"), &["x"]).await));
    }
    let mut tasks = vec![];
    for r in rs {
        let w = w.clone();
        tasks.push(tokio::spawn(async move { r.request(&w, 0).await.is_some() }));
    }
    let mut got = 0;
    for t in tasks {
        if t.await.unwrap() {
            got += 1;
        }
    }
    assert_eq!(got, 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_waiting_runner_is_woken_when_a_job_is_queued(pool: PgPool) {
    let w = Arc::new(World::new(pool).await);
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, ONE)]).await;
    let r = Arc::new(w.runner("r", &["x"]).await);
    let (w2, r2) = (w.clone(), r.clone());
    let waiting = tokio::spawn(async move {
        let t = std::time::Instant::now();
        let j = r2.request(&w2, 20).await;
        (j.is_some(), t.elapsed())
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    w.push(repo, &git, "main", None).await;
    let (got, took) = tokio::time::timeout(std::time::Duration::from_secs(10), waiting).await.expect("woken").unwrap();
    assert!(got);
    assert!(took < std::time::Duration::from_secs(8), "woken at once, not at the end of the wait: {took:?}");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_long_poll_with_nothing_to_do_ends_with_no_job(pool: PgPool) {
    let w = World::new(pool).await;
    let r = w.runner("r", &["x"]).await;
    let t = std::time::Instant::now();
    assert!(r.request(&w, 1).await.is_none());
    assert!(t.elapsed() >= std::time::Duration::from_millis(900));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_runner_cannot_touch_a_job_it_does_not_hold(pool: PgPool) {
    let w = World::new(pool).await;
    one_job(&w, ONE).await;
    let mine = w.runner("mine", &["x"]).await;
    let other = w.runner("other", &["x"]).await;
    let j = mine.request(&w, 0).await.unwrap();
    assert_eq!(other.heartbeat(&w, j.job_id).await.0, StatusCode::NOT_FOUND);
    assert_eq!(other.logs(&w, j.job_id, 1, 0, &["x"]).await.0, StatusCode::NOT_FOUND);
    assert_eq!(other.step(&w, j.job_id, 1, "in_progress", None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(other.complete(&w, j.job_id, "success", json!({})).await.0, StatusCode::NOT_FOUND);
    // A job token is not a runner credential, and a runner credential is not a job token.
    let jt = Who::Bearer(j.token.clone());
    let (s, _) = w.post(&format!("/api/runner/v1/jobs/{}/complete", j.job_id), &jt, json!({"conclusion": "success"})).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = w.get(&format!("/api/runner/v1/jobs/{}/artifacts", j.job_id), &mine.who()).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

// ------------------------------------------------------------------ leases

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_runner_that_goes_quiet_fails_its_job(pool: PgPool) {
    let w = World::new(pool).await;
    let (_, _, run) = one_job(&w, ONE).await;
    let r = w.runner("r", &["x"]).await;
    let j = r.request(&w, 0).await.unwrap();
    assert_eq!(ch_ci::workers::reap_jobs(&w.ctx).await.unwrap(), 0, "the lease is fresh");
    // A heartbeat pushes the lease out.
    sqlx::query("UPDATE workflow_jobs SET lease_expires_at = now() + interval '1 second'").execute(w.pool()).await.unwrap();
    r.heartbeat(&w, j.job_id).await;
    let lease: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT lease_expires_at FROM workflow_jobs").fetch_one(w.pool()).await.unwrap();
    assert!(lease > chrono::Utc::now() + chrono::Duration::seconds(100));
    // Silence for two minutes.
    sqlx::query("UPDATE workflow_jobs SET lease_expires_at = now() - interval '1 second'").execute(w.pool()).await.unwrap();
    assert_eq!(ch_ci::workers::reap_jobs(&w.ctx).await.unwrap(), 1);
    assert_eq!(w.run_status(run).await, ("completed".into(), Some("failure".into())));
    let log = w.get(&format!("/api/v3/repos/alice/demo/actions/jobs/{}/logs", j.job_id), &Who::User(w.alice)).await.1;
    assert!(log.as_str().unwrap().contains("The runner stopped responding."), "{log}");
    // The runner, if it comes back, is told to stop and cannot report the job.
    assert_eq!(r.heartbeat(&w, j.job_id).await.1["cancel"], true);
    assert_eq!(
        r.complete(&w, j.job_id, "success", json!({})).await.0,
        StatusCode::NO_CONTENT,
        "a late report is accepted and changes nothing"
    );
    assert_eq!(w.run_status(run).await.1.as_deref(), Some("failure"));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_job_far_past_its_time_limit_is_timed_out(pool: PgPool) {
    let yml = "name: ci\non: push\njobs:\n  b:\n    runs-on: x\n    timeout-minutes: 5\n    steps:\n      - run: echo\n";
    let w = World::new(pool).await;
    let (_, _, run) = one_job(&w, yml).await;
    let r = w.runner("r", &["x"]).await;
    r.request(&w, 0).await.unwrap();
    sqlx::query("UPDATE workflow_jobs SET started_at = now() - interval '11 minutes'").execute(w.pool()).await.unwrap();
    assert_eq!(ch_ci::workers::reap_jobs(&w.ctx).await.unwrap(), 1);
    let c: Option<String> = sqlx::query_scalar("SELECT conclusion FROM workflow_jobs").fetch_one(w.pool()).await.unwrap();
    assert_eq!(c.as_deref(), Some("timed_out"));
    assert_eq!(w.run_status(run).await.1.as_deref(), Some("failure"));
}

// ------------------------------------------------------------------ logs

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn log_batches_are_idempotent_and_ordered(pool: PgPool) {
    let w = World::new(pool).await;
    one_job(&w, ONE).await;
    let r = w.runner("r", &["x"]).await;
    let j = r.request(&w, 0).await.unwrap();
    let id = j.job_id;
    assert_eq!(r.step(&w, id, 1, "in_progress", None).await.0, StatusCode::NO_CONTENT);
    let (s, v) = r.logs(&w, id, 1, 0, &["first", "second"]).await;
    assert_eq!((s, v["next_seq"].as_u64()), (StatusCode::OK, Some(1)));
    // The same batch again, as after a lost response, is acknowledged and dropped.
    let (s, v) = r.logs(&w, id, 1, 0, &["first", "second"]).await;
    assert_eq!((s, v["next_seq"].as_u64()), (StatusCode::OK, Some(1)));
    // A batch from the future is refused and says what is expected.
    let (s, v) = r.logs(&w, id, 1, 5, &["skipped ahead"]).await;
    assert_eq!((s, v["next_seq"].as_u64()), (StatusCode::CONFLICT, Some(1)));
    let (s, _) = r.logs(&w, id, 1, 1, &["third\nwith newline"]).await;
    assert_eq!(s, StatusCode::OK);
    r.step(&w, id, 1, "completed", Some("success")).await;
    let (_, text) = w.get(&format!("/api/v3/repos/alice/demo/actions/jobs/{id}/logs"), &Who::User(w.alice)).await;
    // The runner writes its own group lines, so the file holds exactly what it sent.
    assert_eq!(text.as_str().unwrap(), "first\nsecond\nthird with newline\n");
    // The step's range covers its own output.
    let (start, end): (i64, i64) = sqlx::query_as("SELECT log_start, log_end FROM workflow_steps WHERE job_id = $1 AND number = 1")
        .bind(id)
        .fetch_one(w.pool())
        .await
        .unwrap();
    let body = text.as_str().unwrap().as_bytes();
    assert_eq!(&body[start as usize..end as usize], b"first\nsecond\nthird with newline\n");
}

const THREE: &str =
    "name: ci\non: push\njobs:\n  b:\n    runs-on: x\n    steps:\n      - run: echo one\n      - run: echo two\n      - run: exit 3\n";

/// A step's own output stays in its own range even when a log batch arrives after the next
/// step has already started (the runner sends logs and step updates on separate calls).
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_late_log_batch_stays_in_its_own_step(pool: PgPool) {
    let w = World::new(pool).await;
    one_job(&w, THREE).await;
    let r = w.runner("r", &["x"]).await;
    let j = r.request(&w, 0).await.unwrap();
    let id = j.job_id;
    // Step 1 starts, prints, and a second batch of its output is delayed.
    r.step(&w, id, 1, "in_progress", None).await;
    r.logs(&w, id, 1, 0, &["step one"]).await;
    r.step(&w, id, 1, "completed", Some("success")).await;
    // Step 2 starts and completes before step 1's delayed batch reaches the server.
    r.step(&w, id, 2, "in_progress", None).await;
    r.step(&w, id, 2, "completed", Some("success")).await;
    let (s, _) = r.logs(&w, id, 1, 1, &["step one, late"]).await;
    assert_eq!(s, StatusCode::OK);
    r.logs(&w, id, 2, 2, &["step two"]).await;
    // Step 3 fails: its output ends with the error line.
    r.step(&w, id, 3, "in_progress", None).await;
    r.logs(&w, id, 3, 3, &["about to fail", "##[error]Process completed with exit code 3."]).await;
    r.step(&w, id, 3, "completed", Some("failure")).await;
    r.complete(&w, id, "failure", json!({})).await;

    let (_, text) = w.get(&format!("/api/v3/repos/alice/demo/actions/jobs/{id}/logs"), &Who::User(w.alice)).await;
    let text = text.as_str().unwrap();
    assert_eq!(
        text, "step one\nstep one, late\nstep two\nabout to fail\n##[error]Process completed with exit code 3.\n",
        "one line per line, in the order the runner sent them"
    );
    let body = text.as_bytes();
    for (n, want) in
        [(1, "step one\nstep one, late\n"), (2, "step two\n"), (3, "about to fail\n##[error]Process completed with exit code 3.\n")]
    {
        let (start, end): (i64, i64) = sqlx::query_as("SELECT log_start, log_end FROM workflow_steps WHERE job_id = $1 AND number = $2")
            .bind(id)
            .bind(n)
            .fetch_one(w.pool())
            .await
            .unwrap();
        assert_eq!(std::str::from_utf8(&body[start as usize..end as usize]).unwrap(), want, "step {n}");
    }
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn logs_can_be_read_by_byte_range(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, _, _) = one_job(&w, ONE).await;
    let r = w.runner("r", &["x"]).await;
    let j = r.request(&w, 0).await.unwrap();
    r.logs(&w, j.job_id, 0, 0, &["hello", "world"]).await;
    let alice = ch_testkit::fixtures::user_actor(w.alice);
    let got =
        w.state.ci.log_range(&alice, repo, ch_core::JobId(j.job_id), ch_core::model::LogRange { from: 6, to: Some(11) }).await.unwrap();
    assert_eq!(&got[..], b"world");
    let all = w.state.ci.log_range(&alice, repo, ch_core::JobId(j.job_id), ch_core::model::LogRange { from: 0, to: None }).await.unwrap();
    assert_eq!(&all[..], b"hello\nworld\n");
    let past =
        w.state.ci.log_range(&alice, repo, ch_core::JobId(j.job_id), ch_core::model::LogRange { from: 500, to: None }).await.unwrap();
    assert!(past.is_empty());
    // A stranger gets nothing, and a job of another repo is not found through this one.
    let bob = ch_testkit::fixtures::user_actor(w.bob);
    assert!(w.state.ci.log_range(&bob, repo, ch_core::JobId(j.job_id), ch_core::model::LogRange { from: 0, to: None }).await.is_err());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_huge_log_is_cut_off_with_a_note(pool: PgPool) {
    let w = World::new(pool).await;
    let (_, _, _) = one_job(&w, ONE).await;
    let r = w.runner("r", &["x"]).await;
    let j = r.request(&w, 0).await.unwrap();
    // Pretend the log is already at its limit.
    let run: i64 = sqlx::query_scalar("SELECT run_id FROM workflow_jobs").fetch_one(w.pool()).await.unwrap();
    let path = ch_ci::logs::log_path(&w.ctx, run, j.job_id);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let f = std::fs::File::create(&path).unwrap();
    f.set_len(ch_ci::logs::MAX_LOG_BYTES - 10).unwrap();
    let (s, _) = r.logs(&w, j.job_id, 0, 0, &["this will not fit in ten bytes at all"]).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = r.logs(&w, j.job_id, 0, 1, &["nor will this"]).await;
    assert_eq!(s, StatusCode::OK, "the runner is not told to stop, the lines are just not kept");
    let len = std::fs::metadata(&path).unwrap().len();
    assert!(len < ch_ci::logs::MAX_LOG_BYTES + 200, "{len}");
    let tail = Bytes::from(std::fs::read(&path).unwrap());
    assert!(String::from_utf8_lossy(&tail[tail.len() - 120..]).contains("log limit"));
    assert!(!String::from_utf8_lossy(&tail).contains("nor will this"));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn live_logs_stream_to_a_viewer_and_resume_after_a_drop(pool: PgPool) {
    let w = Arc::new(World::new(pool).await);
    one_job(&w, ONE).await;
    let r = w.runner("r", &["x"]).await;
    let j = r.request(&w, 0).await.unwrap();
    r.logs(&w, j.job_id, 0, 0, &["before the viewer"]).await;
    let uri = format!("/api/v3/repos/alice/demo/actions/jobs/{}/logs/stream", j.job_id);
    let (w2, uri2) = (w.clone(), uri.clone());
    let viewer = tokio::spawn(async move { w2.send_raw(Method::GET, &uri2, &Who::User(w2.alice), None).await });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    r.logs(&w, j.job_id, 1, 1, &["hello from codehaven"]).await;
    r.complete(&w, j.job_id, "success", json!({})).await;
    let (s, body) =
        tokio::time::timeout(std::time::Duration::from_secs(10), viewer).await.expect("the stream ends when the job does").unwrap();
    assert_eq!(s, StatusCode::OK);
    let text = String::from_utf8(body.to_vec()).unwrap();
    let pos = |needle: &str| text.find(needle).unwrap_or_else(|| panic!("{needle:?} not in {text}"));
    assert!(pos("before the viewer") < pos("hello from codehaven"), "a late viewer gets what was already written first, in order");
    assert!(pos("hello from codehaven") < pos("event: done"));

    // Reconnecting with the last event id resumes: only what came after is sent.
    let first_offset = "before the viewer\n".len();
    let req_uri = format!("{uri}?from={first_offset}");
    let (s, body) = w.send_raw(Method::GET, &req_uri, &Who::User(w.alice), None).await;
    assert_eq!(s, StatusCode::OK);
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(!text.contains("before the viewer"));
    assert!(text.contains("hello from codehaven") && text.contains("event: done"));
    // A stranger cannot watch.
    let (s, _) = w.send_raw(Method::GET, &uri, &Who::User(w.bob), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_live_log_closes_within_two_seconds_of_losing_access(pool: PgPool) {
    let w = Arc::new(World::new(pool).await);
    one_job(&w, ONE).await;
    let r = w.runner("r", &["x"]).await;
    let j = r.request(&w, 0).await.unwrap();
    r.logs(&w, j.job_id, 0, 0, &["still running"]).await;
    let uri = format!("/api/v3/repos/alice/demo/actions/jobs/{}/logs/stream", j.job_id);
    let (w2, uri2) = (w.clone(), uri.clone());
    // The job keeps running, so only the live check can end this stream.
    let viewer = tokio::spawn(async move { w2.send_raw(Method::GET, &uri2, &Who::User(w2.alice), None).await });
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    sqlx::query("UPDATE users SET suspended_at = now() WHERE id = $1").bind(w.alice.0).execute(w.pool()).await.unwrap();
    let start = std::time::Instant::now();
    let (s, body) =
        tokio::time::timeout(std::time::Duration::from_secs(5), viewer).await.expect("the stream ends when access does").unwrap();
    assert!(start.elapsed() <= std::time::Duration::from_millis(2500), "{:?}", start.elapsed());
    assert_eq!(s, StatusCode::OK);
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("still running"));
    assert!(!text.contains("event: done"), "it closed, it did not finish: {text}");
}

// ------------------------------------------------------------------ artifacts

fn zip_bytes() -> Bytes {
    // Not a real zip: the server stores bytes and never opens them.
    Bytes::from_static(b"PK\x03\x04 pretend archive contents")
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn artifacts_upload_download_and_list(pool: PgPool) {
    let w = World::new(pool).await;
    let (_, _, run) = one_job(&w, ONE).await;
    let r = w.runner("r", &["x"]).await;
    let j = r.request(&w, 0).await.unwrap();
    let jt = Who::Bearer(j.token.clone());
    let uri = format!("/api/runner/v1/jobs/{}/artifacts/report", j.job_id);
    let (s, v) = w.send_raw(Method::PUT, &uri, &jt, Some((zip_bytes(), "application/zip"))).await;
    assert_eq!(s, StatusCode::CREATED);
    let up: serde_json::Value = serde_json::from_slice(&v).unwrap();
    assert_eq!((up["name"].as_str(), up["size"].as_i64()), (Some("report"), Some(zip_bytes().len() as i64)));
    // Same job downloads it.
    let (s, body) = w.send_raw(Method::GET, &uri, &jt, None).await;
    assert_eq!((s, body), (StatusCode::OK, zip_bytes()));
    // A second upload with the same name replaces it.
    let (s, _) = w.send_raw(Method::PUT, &uri, &jt, Some((Bytes::from_static(b"newer"), "application/zip"))).await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(w.env.count("artifacts").await, 1);
    // Alice sees it through the REST API and downloads the zip, bob does not.
    let (s, list) = w.get(&format!("/api/v3/repos/alice/demo/actions/runs/{run}/artifacts"), &Who::User(w.alice)).await;
    assert_eq!((s, list["total_count"].as_i64(), list["artifacts"][0]["size_in_bytes"].as_i64()), (StatusCode::OK, Some(1), Some(5)));
    let id = list["artifacts"][0]["id"].as_i64().unwrap();
    let (s, zip) =
        w.send_raw(Method::GET, &format!("/api/v3/repos/alice/demo/actions/artifacts/{id}/zip"), &Who::User(w.alice), None).await;
    assert_eq!((s, &zip[..]), (StatusCode::OK, &b"newer"[..]));
    let (s, _) = w.send_raw(Method::GET, &format!("/api/v3/repos/alice/demo/actions/artifacts/{id}/zip"), &Who::User(w.bob), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // Deleting needs write access and releases the blob.
    let (s, _) = w.send(Method::DELETE, &format!("/api/v3/repos/alice/demo/actions/artifacts/{id}"), &Who::User(w.alice), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(w.env.count("artifacts").await, 0);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn artifact_uploads_are_checked(pool: PgPool) {
    let w = World::new(pool).await;
    let (_, _, _) = one_job(&w, ONE).await;
    let r = w.runner("r", &["x"]).await;
    let j = r.request(&w, 0).await.unwrap();
    let jt = Who::Bearer(j.token.clone());
    for bad in ["..%2Fx", ".hidden", "a%2Fb", "x%00y"] {
        let (s, _) = w
            .send_raw(
                Method::PUT,
                &format!("/api/runner/v1/jobs/{}/artifacts/{bad}", j.job_id),
                &jt,
                Some((zip_bytes(), "application/zip")),
            )
            .await;
        assert!(s == StatusCode::UNPROCESSABLE_ENTITY || s == StatusCode::NOT_FOUND || s == StatusCode::BAD_REQUEST, "{bad}: {s}");
    }
    // The runner's own credential cannot upload: only the job can.
    let (s, _) = w
        .send_raw(Method::PUT, &format!("/api/runner/v1/jobs/{}/artifacts/x", j.job_id), &r.who(), Some((zip_bytes(), "application/zip")))
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // A token for another job is no use here.
    let other = w.runner("r2", &["x"]).await;
    assert!(other.request(&w, 0).await.is_none());
    // Nothing without a credential.
    let (s, _) = w
        .send_raw(Method::PUT, &format!("/api/runner/v1/jobs/{}/artifacts/x", j.job_id), &Who::Anon, Some((zip_bytes(), "application/zip")))
        .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(w.env.count("artifacts").await, 0);
    // Once the job is over its token stops working.
    r.complete(&w, j.job_id, "success", json!({})).await;
    let (s, _) = w
        .send_raw(Method::PUT, &format!("/api/runner/v1/jobs/{}/artifacts/late", j.job_id), &jt, Some((zip_bytes(), "application/zip")))
        .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn expired_artifacts_are_removed_and_their_blobs_released(pool: PgPool) {
    let w = World::new(pool).await;
    one_job(&w, ONE).await;
    let r = w.runner("r", &["x"]).await;
    let j = r.request(&w, 0).await.unwrap();
    let jt = Who::Bearer(j.token.clone());
    w.send_raw(
        Method::PUT,
        &format!("/api/runner/v1/jobs/{}/artifacts/a?retention_days=1", j.job_id),
        &jt,
        Some((zip_bytes(), "application/zip")),
    )
    .await;
    let days: f64 = sqlx::query_scalar("SELECT (extract(epoch FROM expires_at - created_at) / 86400)::float8 FROM artifacts")
        .fetch_one(w.pool())
        .await
        .unwrap();
    assert!((0.9..1.1).contains(&days), "retention-days is honoured: {days}");
    assert_eq!(ch_ci::store::expire_artifacts(&w.ctx).await.unwrap(), 0);
    sqlx::query("UPDATE artifacts SET expires_at = now() - interval '1 minute'").execute(w.pool()).await.unwrap();
    assert_eq!(ch_ci::store::expire_artifacts(&w.ctx).await.unwrap(), 1);
    let refs: i64 = sqlx::query_scalar("SELECT refcount::bigint FROM blobs").fetch_one(w.pool()).await.unwrap();
    assert_eq!(refs, 0);
}

// ------------------------------------------------------------------ cache

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_cache_saves_looks_up_and_restores(pool: PgPool) {
    let w = World::new(pool).await;
    one_job(&w, ONE).await;
    let r = w.runner("r", &["x"]).await;
    let j = r.request(&w, 0).await.unwrap();
    let jt = Who::Bearer(j.token.clone());
    let put = |key: &str, body: &'static [u8]| {
        let (w, jt, uri) = (&w, jt.clone(), format!("/api/runner/v1/cache/{key}?version=v1"));
        async move { w.send_raw(Method::PUT, &uri, &jt, Some((Bytes::from_static(body), "application/octet-stream"))).await }
    };
    let (s, v) = put("cargo-linux-abc", b"deps one").await;
    assert_eq!(s, StatusCode::CREATED, "{v:?}");
    let (s, _) = put("cargo-linux-def", b"deps two").await;
    assert_eq!(s, StatusCode::CREATED);
    // Entries are immutable: saving a key again is a conflict.
    assert_eq!(put("cargo-linux-abc", b"changed").await.0, StatusCode::CONFLICT);

    let look = |keys: serde_json::Value, version: &str| {
        let (w, jt) = (&w, jt.clone());
        let version = version.to_string();
        async move { w.post("/api/runner/v1/cache/lookup", &jt, json!({"keys": keys, "version": version})).await }
    };
    let (_, hit) = look(json!(["cargo-linux-abc"]), "v1").await;
    assert_eq!((hit["hit"]["key"].as_str(), hit["hit"]["exact"].as_bool()), (Some("cargo-linux-abc"), Some(true)));
    // A restore key matches by prefix, newest first.
    let (_, hit) = look(json!(["cargo-linux-zzz", "cargo-linux-"]), "v1").await;
    assert_eq!((hit["hit"]["key"].as_str(), hit["hit"]["exact"].as_bool()), (Some("cargo-linux-def"), Some(false)));
    let (_, miss) = look(json!(["cargo-mac-"]), "v1").await;
    assert!(miss["hit"].is_null());
    let (_, miss) = look(json!(["cargo-linux-abc"]), "v2").await;
    assert!(miss["hit"].is_null(), "another version is another cache");
    assert_eq!(look(json!([]), "v1").await.0, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(look(json!(["a\nb"]), "v1").await.0, StatusCode::UNPROCESSABLE_ENTITY);

    let id = hit_id(&w, "cargo-linux-def").await;
    let (s, body) = w.send_raw(Method::GET, &format!("/api/runner/v1/cache/{id}"), &jt, None).await;
    assert_eq!((s, &body[..]), (StatusCode::OK, &b"deps two"[..]));
    assert_eq!(w.send_raw(Method::GET, "/api/runner/v1/cache/999999", &jt, None).await.0, StatusCode::NOT_FOUND);
}

async fn hit_id(w: &World, key: &str) -> i64 {
    sqlx::query_scalar("SELECT id FROM caches WHERE key = $1").bind(key).fetch_one(w.pool()).await.unwrap()
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn one_repos_cache_is_invisible_to_another(pool: PgPool) {
    let w = World::new(pool).await;
    let (a, ga) = w.repo_with(w.alice, "a", &[(CI_YML, ONE)]).await;
    let (b, gb) = w.repo_with(w.alice, "b", &[(CI_YML, ONE)]).await;
    w.push(a, &ga, "main", None).await;
    w.push(b, &gb, "main", None).await;
    let r = w.runner("r", &["x"]).await;
    let ja = r.request(&w, 0).await.unwrap();
    let jb = r.request(&w, 0).await.unwrap();
    assert_ne!(ja.repo.id, jb.repo.id);
    let ta = Who::Bearer(ja.token.clone());
    let tb = Who::Bearer(jb.token.clone());
    w.send_raw(Method::PUT, "/api/runner/v1/cache/k?version=", &ta, Some((Bytes::from_static(b"secret of a"), "application/octet-stream")))
        .await;
    let id = hit_id(&w, "k").await;
    let (_, hit) = w.post("/api/runner/v1/cache/lookup", &tb, json!({"keys": ["k"]})).await;
    assert!(hit["hit"].is_null());
    assert_eq!(w.send_raw(Method::GET, &format!("/api/runner/v1/cache/{id}"), &tb, None).await.0, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn old_caches_are_evicted(pool: PgPool) {
    let w = World::new(pool).await;
    one_job(&w, ONE).await;
    let r = w.runner("r", &["x"]).await;
    let j = r.request(&w, 0).await.unwrap();
    let jt = Who::Bearer(j.token.clone());
    w.send_raw(Method::PUT, "/api/runner/v1/cache/old", &jt, Some((Bytes::from_static(b"1"), "application/octet-stream"))).await;
    w.send_raw(Method::PUT, "/api/runner/v1/cache/new", &jt, Some((Bytes::from_static(b"2"), "application/octet-stream"))).await;
    sqlx::query("UPDATE caches SET last_used_at = now() - interval '8 days' WHERE key = 'old'").execute(w.pool()).await.unwrap();
    assert_eq!(ch_ci::store::evict_caches(&w.ctx).await.unwrap(), 1);
    let keys: Vec<String> = sqlx::query_scalar("SELECT key FROM caches").fetch_all(w.pool()).await.unwrap();
    assert_eq!(keys, vec!["new"]);
}
