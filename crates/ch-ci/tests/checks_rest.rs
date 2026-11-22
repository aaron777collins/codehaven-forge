//! Statuses and check runs, the REST views of workflows, runs and jobs, and who may see what.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;
use axum::http::{Method, StatusCode};
use serde_json::json;
use sqlx::PgPool;
use support::*;

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn statuses_are_created_listed_and_combined(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[]).await;
    let sha = git.head();
    let alice = Who::User(w.alice);
    let uri = format!("/api/v3/repos/alice/demo/statuses/{sha}");
    // Nothing yet: pending, not success.
    let (_, c) = w.get(&format!("/api/v3/repos/alice/demo/commits/{sha}/status"), &alice).await;
    assert_eq!((c["state"].as_str(), c["total_count"].as_i64()), (Some("pending"), Some(0)));

    let (s, v) = w
        .post(
            &uri,
            &alice,
            json!({"state": "pending", "context": "ci/external", "description": "building", "target_url": "https://ci.example/1"}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(
        (v["state"].as_str(), v["context"].as_str(), v["creator"]["login"].as_str()),
        (Some("pending"), Some("ci/external"), Some("alice"))
    );
    assert_eq!(w.state.checks.combined(repo, &sha).await.unwrap(), ch_core::model::CombinedState::Pending);
    w.post(&uri, &alice, json!({"state": "success", "context": "ci/external"})).await;
    w.post(&uri, &alice, json!({"state": "success", "context": "lint"})).await;
    let (_, c) = w.get("/api/v3/repos/alice/demo/commits/main/status", &alice).await;
    assert_eq!(
        (c["state"].as_str(), c["total_count"].as_i64()),
        (Some("success"), Some(2)),
        "the latest status of each context, found through a branch name: {c}"
    );
    let (_, history) = w.get(&format!("/api/v3/repos/alice/demo/commits/{sha}/statuses"), &alice).await;
    assert_eq!(history.as_array().unwrap().len(), 3, "the list keeps the history, newest first");
    assert_eq!(history[0]["context"], "lint");
    // A failure anywhere makes the commit failing.
    w.post(&uri, &alice, json!({"state": "failure", "context": "lint"})).await;
    assert_eq!(w.state.checks.combined(repo, &sha).await.unwrap(), ch_core::model::CombinedState::Failure);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn status_input_is_checked_and_access_enforced(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[]).await;
    let sha = git.head();
    let uri = format!("/api/v3/repos/alice/demo/statuses/{sha}");
    let alice = Who::User(w.alice);
    for bad in [
        json!({"state": "ok"}),
        json!({"state": "success", "context": "x".repeat(300)}),
        json!({"state": "success", "target_url": "javascript:alert(1)"}),
        json!({"state": "success", "target_url": "https://a b"}),
        json!({"state": "success", "description": "d".repeat(2000)}),
    ] {
        assert_eq!(w.post(&uri, &alice, bad.clone()).await.0, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
    }
    assert_eq!(
        w.post(&format!("/api/v3/repos/alice/demo/statuses/{}", "f".repeat(40)), &alice, json!({"state": "success"})).await.0,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a commit that does not exist"
    );
    assert_eq!(w.post(&uri, &Who::User(w.bob), json!({"state": "success"})).await.0, StatusCode::NOT_FOUND, "bob cannot see the repo");
    w.env.collaborator(repo, w.bob, "read").await;
    assert_eq!(w.post(&uri, &Who::User(w.bob), json!({"state": "success"})).await.0, StatusCode::FORBIDDEN, "reading is not enough");
    assert_eq!(w.post(&uri, &Who::Anon, json!({"state": "success"})).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(w.env.count("commit_statuses").await, 0);
    // A running job may report on its own repo's commits.
    let yml = "name: ci\non: push\njobs:\n  b:\n    runs-on: x\n    steps:\n      - run: echo\n";
    let (repo2, git2) = w.repo_with(w.alice, "other", &[(CI_YML, yml)]).await;
    w.push(repo2, &git2, "main", None).await;
    let r = w.runner("r", &["x"]).await;
    let job = r.request(&w, 0).await.unwrap();
    let jt = Who::Bearer(job.token.clone());
    let sha2 = git2.head();
    assert_eq!(
        w.post(&format!("/api/v3/repos/alice/other/statuses/{sha2}"), &jt, json!({"state": "success", "context": "from-job"})).await.0,
        StatusCode::CREATED
    );
    // But not on another repository: it cannot even tell that it exists.
    assert_eq!(w.post(&uri, &jt, json!({"state": "success"})).await.0, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn required_checks_need_every_context_passing(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, &basic_workflow())]).await;
    let run = w.push(repo, &git, "main", None).await[0];
    let sha = git.head();
    let need = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let checks = &w.state.checks;
    assert!(checks.required_satisfied(repo, &sha, &[]).await.unwrap(), "nothing required, nothing missing");
    assert!(!checks.required_satisfied(repo, &sha, &need(&["ci / lint"])).await.unwrap(), "queued is not passing");
    let r = w.runner("r", &["ubuntu-latest"]).await;
    r.run_one(&w, "success").await.unwrap();
    assert!(checks.required_satisfied(repo, &sha, &need(&["ci / lint"])).await.unwrap());
    assert!(!checks.required_satisfied(repo, &sha, &need(&["ci / lint", "ci / test (a)"])).await.unwrap(), "test (a) has not run");
    assert!(
        !checks.required_satisfied(repo, &sha, &need(&["ci / nonexistent"])).await.unwrap(),
        "a context nobody reports can never be satisfied"
    );
    r.run_one(&w, "failure").await.unwrap();
    r.run_one(&w, "success").await.unwrap();
    assert!(!checks.required_satisfied(repo, &sha, &need(&["ci / lint", "ci / test (a)", "ci / test (b)"])).await.unwrap(), "one failed");
    // A status counts under its context too, and a short sha finds the same commit.
    let alice = ch_testkit::fixtures::user_actor(w.alice);
    checks.create_status(&alice, repo, &sha, "success", "security", None, None).await.unwrap();
    assert!(checks.required_satisfied(repo, &sha[..10], &need(&["ci / lint", "security"])).await.unwrap());
    checks.create_status(&alice, repo, &sha, "pending", "security", None, None).await.unwrap();
    assert!(
        !checks.required_satisfied(repo, &sha, &need(&["security"])).await.unwrap(),
        "the newest status of a context is the one that counts"
    );
    let _ = run;
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn check_runs_are_listed_per_commit_with_filters(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, &basic_workflow())]).await;
    w.push(repo, &git, "main", None).await;
    let r = w.runner("r", &["ubuntu-latest"]).await;
    r.run_one(&w, "success").await.unwrap();
    let alice = Who::User(w.alice);
    let (s, v) = w.get("/api/v3/repos/alice/demo/commits/main/check-runs", &alice).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["total_count"], 3);
    let names: Vec<&str> = v["check_runs"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["ci / lint", "ci / test (a)", "ci / test (b)"]);
    let lint = &v["check_runs"][0];
    assert_eq!((lint["status"].as_str(), lint["conclusion"].as_str()), (Some("completed"), Some("success")));
    assert!(lint["html_url"].as_str().unwrap().starts_with("https://test.example/alice/demo/actions/runs/"));
    let (_, f) = w.get("/api/v3/repos/alice/demo/commits/main/check-runs?status=completed", &alice).await;
    assert_eq!(f["total_count"], 1);
    let (_, f) = w.get("/api/v3/repos/alice/demo/commits/main/check-runs?check_name=ci%20%2F%20test%20(a)", &alice).await;
    assert_eq!(f["check_runs"][0]["name"], "ci / test (a)");
    let (_, f) = w.get("/api/v3/repos/alice/demo/commits/main/check-runs?per_page=1&page=2", &alice).await;
    assert_eq!((f["check_runs"].as_array().unwrap().len(), f["total_count"].as_i64()), (1, Some(3)));
    let id = lint["id"].as_i64().unwrap();
    assert_eq!(w.get(&format!("/api/v3/repos/alice/demo/check-runs/{id}"), &alice).await.1["name"], "ci / lint");
    // Bob sees nothing of a private repo.
    assert_eq!(w.get("/api/v3/repos/alice/demo/commits/main/check-runs", &Who::User(w.bob)).await.0, StatusCode::NOT_FOUND);
    assert_eq!(w.get(&format!("/api/v3/repos/alice/demo/check-runs/{id}"), &Who::User(w.bob)).await.0, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn workflows_runs_and_jobs_over_rest(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w
        .repo_with(
            w.alice,
            "demo",
            &[(CI_YML, &basic_workflow()), (".codehaven/workflows/bad.yml", "on: push\njobs:\n  a:\n    steps: []\n")],
        )
        .await;
    let run = w.push(repo, &git, "main", None).await[0];
    let alice = Who::User(w.alice);
    let (s, wfs) = w.get("/api/v3/repos/alice/demo/actions/workflows", &alice).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(wfs["total_count"], 2);
    let ci = wfs["workflows"].as_array().unwrap().iter().find(|x| x["path"] == CI_YML).unwrap();
    assert_eq!((ci["name"].as_str(), ci["state"].as_str()), (Some("ci"), Some("active")));
    let wid = ci["id"].as_i64().unwrap();
    assert_eq!(w.get("/api/v3/repos/alice/demo/actions/workflows/ci.yml", &alice).await.1["id"], wid);
    assert_eq!(w.get(&format!("/api/v3/repos/alice/demo/actions/workflows/{wid}"), &alice).await.1["path"], CI_YML);

    let (_, runs) = w.get("/api/v3/repos/alice/demo/actions/runs", &alice).await;
    assert_eq!(runs["total_count"], 1);
    let r = &runs["workflow_runs"][0];
    assert_eq!(
        (r["id"].as_i64(), r["event"].as_str(), r["status"].as_str(), r["name"].as_str()),
        (Some(run), Some("push"), Some("queued"), Some("ci"))
    );
    assert_eq!(
        (r["head_sha"].as_str(), r["head_branch"].as_str(), r["actor"]["login"].as_str()),
        (Some(git.head().as_str()), Some("main"), Some("alice"))
    );
    assert_eq!(r["path"], CI_YML);
    assert_eq!(r["workflow_id"], wid);
    assert!(r["html_url"].as_str().unwrap().ends_with(&format!("/alice/demo/actions/runs/{run}")));
    let (_, f) = w.get("/api/v3/repos/alice/demo/actions/runs?status=queued&event=push&branch=main", &alice).await;
    assert_eq!(f["total_count"], 1);
    for q in ["status=completed", "event=pull_request", "branch=dev", "head_sha=abc"] {
        assert_eq!(w.get(&format!("/api/v3/repos/alice/demo/actions/runs?{q}"), &alice).await.1["total_count"], 0, "{q}");
    }
    assert_eq!(w.get(&format!("/api/v3/repos/alice/demo/actions/workflows/{wid}/runs"), &alice).await.1["total_count"], 1);
    assert_eq!(w.get(&format!("/api/v3/repos/alice/demo/actions/runs/{run}"), &alice).await.1["run_attempt"], 1);

    let (_, jobs) = w.get(&format!("/api/v3/repos/alice/demo/actions/runs/{run}/jobs"), &alice).await;
    assert_eq!(jobs["total_count"], 3);
    let lint = &jobs["jobs"][0];
    assert_eq!((lint["name"].as_str(), lint["status"].as_str()), (Some("lint"), Some("queued")));
    assert_eq!(lint["steps"][0]["name"], "Run echo lint");
    assert_eq!(lint["labels"][0], "ubuntu-latest");
    let jid = lint["id"].as_i64().unwrap();
    assert_eq!(w.get(&format!("/api/v3/repos/alice/demo/actions/jobs/{jid}"), &alice).await.1["id"], jid);
    // The service view the web pages use agrees.
    let user = ch_testkit::fixtures::user_actor(w.alice);
    let info = w.state.ci.jobs(&user, repo, ch_core::RunId(run)).await.unwrap();
    assert_eq!(info.len(), 3);
    assert_eq!(info[0].steps[0].name, "Run echo lint");
    assert_eq!(w.state.ci.runs(&user, repo, 1, 30).await.unwrap().len(), 1);
    assert_eq!(w.state.ci.get_run(&user, repo, ch_core::RunId(run)).await.unwrap().status, "queued");

    // A job or run of another repository is not found through this one.
    let (repo2, _) = w.repo_with(w.alice, "other", &[]).await;
    let _ = repo2;
    assert_eq!(w.get(&format!("/api/v3/repos/alice/other/actions/runs/{run}"), &alice).await.0, StatusCode::NOT_FOUND);
    assert_eq!(w.get(&format!("/api/v3/repos/alice/other/actions/jobs/{jid}"), &alice).await.0, StatusCode::NOT_FOUND);
    assert_eq!(w.get(&format!("/api/v3/repos/alice/other/actions/jobs/{jid}/logs"), &alice).await.0, StatusCode::NOT_FOUND);
    // Strangers and the signed-out see nothing.
    assert_eq!(w.get(&format!("/api/v3/repos/alice/demo/actions/runs/{run}"), &Who::User(w.bob)).await.0, StatusCode::NOT_FOUND);
    assert_eq!(w.get(&format!("/api/v3/repos/alice/demo/actions/runs/{run}"), &Who::Anon).await.0, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn run_logs_are_one_text_file_with_headings(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, &basic_workflow())]).await;
    let run = w.push(repo, &git, "main", None).await[0];
    let r = w.runner("r", &["ubuntu-latest"]).await;
    let j = r.request(&w, 0).await.unwrap();
    r.logs(&w, j.job_id, 0, 0, &["lint output"]).await;
    let (s, text) = w.get(&format!("/api/v3/repos/alice/demo/actions/runs/{run}/logs"), &Who::User(w.alice)).await;
    assert_eq!(s, StatusCode::OK);
    let t = text.as_str().unwrap();
    assert!(t.contains("===== lint =====\nlint output\n"), "{t}");
    assert!(t.contains("===== test (a) =====") && t.contains("===== test (b) ====="));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn runners_are_listed_for_admins_and_shown_busy_or_online(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) =
        w.repo_with(w.alice, "demo", &[(CI_YML, "name: ci\non: push\njobs:\n  b:\n    runs-on: x\n    steps:\n      - run: echo\n")]).await;
    let r = w.runner("runner-1", &["x", "linux"]).await;
    let admin = Who::Admin(w.root);
    let (s, v) = w.get("/api/v3/admin/actions/runners", &admin).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        (v["runners"][0]["name"].as_str(), v["runners"][0]["status"].as_str(), v["runners"][0]["busy"].as_bool()),
        (Some("runner-1"), Some("online"), Some(false))
    );
    assert_eq!(v["runners"][0]["labels"][1]["name"], "linux");
    w.push(repo, &git, "main", None).await;
    r.request(&w, 0).await.unwrap();
    let (_, v) = w.get("/api/v3/admin/actions/runners", &admin).await;
    assert_eq!(v["runners"][0]["busy"], true);
    // Not for everyone: a plain user is refused, and a repo admin sees runners for their repo.
    assert_eq!(w.get("/api/v3/admin/actions/runners", &Who::User(w.alice)).await.0, StatusCode::FORBIDDEN);
    let (s, v) = w.get("/api/v3/repos/alice/demo/actions/runners", &Who::User(w.alice)).await;
    assert_eq!((s, v["total_count"].as_i64()), (StatusCode::OK, Some(1)));
    sqlx::query("UPDATE runners SET last_seen_at = now() - interval '10 minutes'").execute(w.pool()).await.unwrap();
    sqlx::query("UPDATE workflow_jobs SET status = 'completed', conclusion = 'success'").execute(w.pool()).await.unwrap();
    let (_, v) = w.get("/api/v3/admin/actions/runners", &admin).await;
    assert_eq!(v["runners"][0]["status"], "offline");
    let _ = Method::GET;
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_pushed_workflow_in_a_private_repo_is_invisible_to_a_second_user(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, &basic_workflow())]).await;
    w.push(repo, &git, "main", None).await;
    let bob = ch_testkit::fixtures::user_actor(w.bob);
    assert!(w.state.ci.runs(&bob, repo, 1, 30).await.is_err());
    assert!(w.state.ci.workflows(&bob, repo, None).await.is_err());
    assert!(w.state.ci.jobs(&bob, repo, ch_core::RunId(1)).await.is_err());
    assert!(w.state.ci.runners(&bob).await.is_err());
    assert!(w.state.ci.create_runner_token(&bob, None, &ch_testkit::fixtures::client()).await.is_err());
    // Anonymous gets nothing from any service call.
    let anon = ch_core::Actor::Anonymous;
    assert!(w.state.ci.runs(&anon, repo, 1, 30).await.is_err());
}
