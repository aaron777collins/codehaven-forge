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

fn event_of(payload: &serde_json::Value) -> serde_json::Value {
    ch_ci::spec::split_payload(payload).0
}

async fn only_event(w: &World) -> serde_json::Value {
    let payload: serde_json::Value =
        sqlx::query_scalar("SELECT event_payload FROM workflow_runs ORDER BY id DESC LIMIT 1").fetch_one(w.pool()).await.unwrap();
    event_of(&payload)
}

/// ch-ctk.7: `github.event` carries the repository and the sender in GitHub's shape, and never an
/// e-mail address.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_push_event_has_the_full_repository_and_sender(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, &basic_workflow())]).await;
    w.push(repo, &git, "main", None).await;
    let e = only_event(&w).await;
    assert_eq!(e["repository"]["full_name"], "alice/demo");
    assert_eq!(e["repository"]["id"], repo.0);
    assert_eq!(e["repository"]["owner"]["login"], "alice");
    assert_eq!(e["repository"]["private"], true);
    assert_eq!(e["repository"]["default_branch"], "main");
    assert_eq!(e["repository"]["html_url"], format!("{}/alice/demo", w.ctx.base_url()));
    assert_eq!(e["sender"]["login"], "alice");
    assert_eq!(e["sender"]["id"], w.alice.0);
    assert_eq!(e["sender"]["type"], "User");
    // The account objects carry no e-mail address (the commit's own author line does, as in git).
    assert!(!e["repository"].to_string().contains('@') && !e["sender"].to_string().contains('@'), "{e}");
}

/// ch-ctk.7: a pull request event has the pull request's object: ids, state, labels, both sides
/// with their repository, the links.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_pull_request_event_has_the_full_pull_request(pool: PgPool) {
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, PR)]).await;
    let base = git.head();
    git.branch("feature");
    let head = git.commit_file("f.txt", "f\n", "Add the feature");
    let issue: i64 = sqlx::query_scalar(
        "INSERT INTO issues (repo_id, number, is_pull, author_id, title, body) VALUES ($1, 7, true, $2, 'Feature', 'Please merge') RETURNING id",
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
    let label: i64 = sqlx::query_scalar("INSERT INTO labels (repo_id, name) VALUES ($1, 'bug') RETURNING id")
        .bind(repo.0)
        .fetch_one(w.pool())
        .await
        .unwrap();
    sqlx::query("INSERT INTO issue_labels (issue_id, label_id) VALUES ($1, $2)").bind(issue).bind(label).execute(w.pool()).await.unwrap();
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
    let e = only_event(&w).await;
    let p = &e["pull_request"];
    assert_eq!(
        (p["number"].as_i64(), p["state"].as_str(), p["title"].as_str(), p["body"].as_str()),
        (Some(7), Some("open"), Some("Feature"), Some("Please merge"))
    );
    assert_eq!(p["user"]["login"], "alice");
    assert_eq!(p["labels"][0]["name"], "bug");
    assert_eq!((p["head"]["sha"].as_str(), p["head"]["ref"].as_str()), (Some(head.as_str()), Some("feature")));
    assert_eq!((p["base"]["sha"].as_str(), p["base"]["ref"].as_str()), (Some(base.as_str()), Some("main")));
    assert_eq!(p["base"]["repo"]["full_name"], "alice/demo");
    assert_eq!(p["head"]["repo"]["full_name"], "alice/demo");
    assert_eq!(p["merged"], false);
    assert!(p["html_url"].as_str().unwrap().ends_with("/alice/demo/pulls/7"));
    assert_eq!(e["action"], "opened");
    assert_eq!(e["number"], 7);
    assert_eq!(e["sender"]["login"], "alice");
}

/// ch-ctk.10: the `release` event carries the whole release with its files, digests included.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_release_event_carries_the_release_and_its_files(pool: PgPool) {
    const REL: &str = "name: rel
on:
  release:
    types: [published]
jobs:
  x:
    runs-on: ubuntu-latest
    steps:
      - run: echo hi
";
    let w = World::new(pool).await;
    let (repo, git) = w.repo_with(w.alice, "demo", &[(CI_YML, REL)]).await;
    git.git(&["tag", "v1.0.0"]);
    let rid: i64 = sqlx::query_scalar(
        "INSERT INTO releases (repo_id, tag_name, target_commitish, name, body, author_id, published_at) VALUES ($1, 'v1.0.0', 'main', 'One', 'The notes', $2, now()) RETURNING id",
    )
    .bind(repo.0)
    .bind(w.alice.0)
    .fetch_one(w.pool())
    .await
    .unwrap();
    sqlx::query("INSERT INTO release_assets (release_id, name, label, content_type, size, blob_sha256, uploader_id) VALUES ($1, 'app.tar.gz', '', 'application/gzip', 12, $2, $3)")
        .bind(rid)
        .bind("b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9")
        .bind(w.alice.0)
        .execute(w.pool())
        .await
        .unwrap();
    w.emit_and_process(DomainEvent::ReleasePublished {
        repo_id: repo,
        release_id: ch_core::ReleaseId(rid),
        tag: "v1.0.0".into(),
        actor: w.alice,
    })
    .await;
    let e = only_event(&w).await;
    assert_eq!(e["action"], "published");
    let r = &e["release"];
    assert_eq!(
        (r["id"].as_i64(), r["tag_name"].as_str(), r["name"].as_str(), r["body"].as_str()),
        (Some(rid), Some("v1.0.0"), Some("One"), Some("The notes"))
    );
    assert_eq!((r["draft"].as_bool(), r["prerelease"].as_bool()), (Some(false), Some(false)));
    assert_eq!(r["author"]["login"], "alice");
    let a = &r["assets"][0];
    assert_eq!((a["name"].as_str(), a["size"].as_i64(), a["state"].as_str()), (Some("app.tar.gz"), Some(12), Some("uploaded")));
    assert_eq!(a["digest"], "sha256:b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9");
    assert!(a["browser_download_url"].as_str().unwrap().ends_with("/alice/demo/releases/download/v1.0.0/app.tar.gz"));
    assert!(r["upload_url"].as_str().unwrap().contains("/api/v3/repos/alice/demo/releases/"));
    assert_eq!(e["repository"]["full_name"], "alice/demo");
    assert_eq!(e["sender"]["login"], "alice");
}
