//! Webhooks: management, delivery, signing, retries and the guard against private targets.
#![allow(clippy::unwrap_used)]

mod common;
use common::*;

use ch_core::error::Error;
use ch_core::events::DomainEvent;
use ch_core::model::*;
use ch_core::{Actor, Permission, RepoId, UserId, WebhookId};
use sqlx::PgPool;

fn new_hook(repo: RepoId, url: &str, events: &[&str], secret: Option<&str>) -> NewWebhook {
    NewWebhook {
        repo_id: Some(repo),
        org_id: None,
        url: url.into(),
        secret: secret.map(String::from),
        events: events.iter().map(|s| s.to_string()).collect(),
        active: true,
    }
}

async fn setup(pool: PgPool) -> (Ctx, UserId, RepoId) {
    let c = ctx_with(pool, |cfg| cfg.webhooks.allow_private = true).await;
    let owner = c.env.user("alice").await;
    let repo = c.env.repo(owner, "demo").await;
    (c, owner, repo)
}

async fn deliveries(c: &Ctx, hook: WebhookId) -> Vec<WebhookDelivery> {
    c.state.webhooks.deliveries(&user_of(&c.pool).await, hook, 50).await.unwrap()
}

async fn user_of(pool: &PgPool) -> Actor {
    let id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE username = 'alice'").fetch_one(pool).await.unwrap();
    user(UserId(id))
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn create_pings_signs_and_records_the_exchange(pool: PgPool) {
    let (c, owner, repo) = setup(pool).await;
    let sink = sink().await;
    let me = user(owner);
    let hook = c
        .state
        .webhooks
        .create(&me, new_hook(repo, &sink.url, &["push", "issues"], Some("It's a Secret to Everybody")), &client())
        .await
        .unwrap();
    assert_eq!(hook.events, vec!["issues", "push"]);

    // Creating a hook queues a ping.
    let d = deliveries(&c, hook.id).await;
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].event, "ping");
    assert_eq!(d[0].success, None);

    assert_eq!(c.deliverer(true).run_due(10).await.unwrap(), 1);
    assert_eq!(sink.count(), 1);
    let got = sink.last();
    let h = |n: &str| got.headers.get(n).map(|v| v.to_str().unwrap().to_string());
    assert_eq!(h("content-type").as_deref(), Some("application/json"));
    assert_eq!(h("x-codehaven-event").as_deref(), Some("ping"));
    assert_eq!(h("x-github-event").as_deref(), Some("ping"));
    assert_eq!(h("x-codehaven-hook-id"), Some(hook.id.0.to_string()));
    assert_eq!(h("x-github-delivery"), h("x-codehaven-delivery"));
    assert!(h("user-agent").unwrap().starts_with("Codehaven-Hookshot/"));
    // The signature is HMAC-SHA256 of the exact body bytes under the secret.
    assert_eq!(h("x-hub-signature-256").unwrap(), ch_notify::deliver::sign(b"It's a Secret to Everybody", got.body.as_bytes()));
    let body: serde_json::Value = serde_json::from_str(&got.body).unwrap();
    assert_eq!(body["hook_id"], hook.id.0);
    assert_eq!(body["repository"]["full_name"], "alice/demo");
    assert_eq!(body["sender"]["login"], "alice");
    assert!(body["zen"].as_str().unwrap().len() > 3);

    let d = &deliveries(&c, hook.id).await[0];
    assert_eq!(d.success, Some(true));
    assert_eq!(d.response_status, Some(200));
    assert_eq!(d.response_body.as_deref(), Some("ok"));
    assert!(d.delivered_at.is_some() && d.next_attempt_at.is_none());
    assert_eq!(d.attempt, 1);
    assert!(d.request_headers.as_ref().unwrap()["X-Hub-Signature-256"].as_str().unwrap().starts_with("sha256="));
    // Nothing more is due.
    assert_eq!(c.deliverer(true).run_due(10).await.unwrap(), 0);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn without_a_secret_there_is_no_signature(pool: PgPool) {
    let (c, owner, repo) = setup(pool).await;
    let sink = sink().await;
    c.state.webhooks.create(&user(owner), new_hook(repo, &sink.url, &["push"], None), &client()).await.unwrap();
    c.deliverer(true).run_due(10).await.unwrap();
    assert!(sink.last().headers.get("x-hub-signature-256").is_none());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_secret_is_sealed_at_rest_bound_to_its_row_and_kept_out_of_the_audit_log(pool: PgPool) {
    let (c, owner, repo) = setup(pool).await;
    let me = user(owner);
    let a =
        c.state.webhooks.create(&me, new_hook(repo, "http://127.0.0.1:9/a", &["push"], Some("hunter2-hunter2")), &client()).await.unwrap();
    let b = c.state.webhooks.create(&me, new_hook(repo, "http://127.0.0.1:9/b", &["push"], None), &client()).await.unwrap();
    let sealed: Vec<u8> =
        sqlx::query_scalar("SELECT secret_sealed FROM webhooks WHERE id = $1").bind(a.id.0).fetch_one(&c.pool).await.unwrap();
    assert!(!sealed.windows(15).any(|w| w == b"hunter2-hunter2"));
    assert!(c.key.open(&sealed, format!("webhook:{}", a.id.0).as_bytes()).is_ok());
    assert!(c.key.open(&sealed, format!("webhook:{}", b.id.0).as_bytes()).is_err(), "bound to the row");
    let audit: String = sqlx::query_scalar("SELECT coalesce(string_agg(row_to_json(a)::text, E'\\n'), '') FROM audit_events a")
        .fetch_one(&c.pool)
        .await
        .unwrap();
    assert!(!audit.contains("hunter2"));
    assert!(audit.contains("webhook.created") && audit.contains("\"has_secret\": true"));
    // A secret can be changed or removed.
    c.state.webhooks.update(&me, a.id, WebhookChange { secret: Some(String::new()), ..Default::default() }, &client()).await.unwrap();
    let sealed: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT secret_sealed FROM webhooks WHERE id = $1").bind(a.id.0).fetch_one(&c.pool).await.unwrap();
    assert!(sealed.is_none());
    assert!(
        c.state.webhooks.update(&me, a.id, WebhookChange { secret: Some("x".repeat(256)), ..Default::default() }, &client()).await.is_err()
    );
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn failures_retry_on_the_documented_schedule_then_stop(pool: PgPool) {
    let (c, owner, repo) = setup(pool).await;
    let sink = sink().await;
    sink.set_status(500);
    let hook = c.state.webhooks.create(&user(owner), new_hook(repo, &sink.url, &["push"], None), &client()).await.unwrap();
    let want = [60i64, 300, 1800, 7200, 21600];
    for (i, secs) in want.iter().enumerate() {
        assert_eq!(c.deliverer(true).run_due(10).await.unwrap(), 1, "attempt {}", i + 1);
        let d = deliveries(&c, hook.id).await.remove(0);
        assert_eq!((d.attempt, d.success, d.response_status), (i as i32 + 1, Some(false), Some(500)));
        assert!(d.delivered_at.is_none(), "still being retried");
        let wait = (d.next_attempt_at.unwrap() - chrono::Utc::now()).num_seconds();
        assert!((secs - 3..=*secs).contains(&wait), "attempt {} retries in {secs}s, not {wait}s", i + 1);
        // Not due yet, so another sweep does nothing.
        assert_eq!(c.deliverer(true).run_due(10).await.unwrap(), 0);
        make_due(&c.pool).await;
    }
    assert_eq!(c.deliverer(true).run_due(10).await.unwrap(), 1, "the sixth and last attempt");
    let d = deliveries(&c, hook.id).await.remove(0);
    assert_eq!((d.attempt, d.success), (6, Some(false)));
    assert!(d.delivered_at.is_some() && d.next_attempt_at.is_none(), "given up");
    assert_eq!(sink.count(), 6);
    make_due(&c.pool).await;
    assert_eq!(c.deliverer(true).run_due(10).await.unwrap(), 0);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_retry_that_succeeds_finishes_the_delivery(pool: PgPool) {
    let (c, owner, repo) = setup(pool).await;
    let sink = sink().await;
    sink.set_status(503);
    let hook = c.state.webhooks.create(&user(owner), new_hook(repo, &sink.url, &["push"], None), &client()).await.unwrap();
    c.deliverer(true).run_due(10).await.unwrap();
    sink.set_status(204);
    make_due(&c.pool).await;
    c.deliverer(true).run_due(10).await.unwrap();
    let d = deliveries(&c, hook.id).await.remove(0);
    assert_eq!((d.attempt, d.success, d.response_status), (2, Some(true), Some(204)));
    assert!(d.delivered_at.is_some() && d.next_attempt_at.is_none());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn redirects_are_not_followed(pool: PgPool) {
    let (c, owner, repo) = setup(pool).await;
    let sink = sink().await;
    sink.set_status(302);
    let hook = c.state.webhooks.create(&user(owner), new_hook(repo, &sink.url, &["push"], None), &client()).await.unwrap();
    c.deliverer(true).run_due(10).await.unwrap();
    assert_eq!(sink.count(), 1, "the redirect target was never requested");
    assert_eq!(sink.last().path, "/hook");
    let d = deliveries(&c, hook.id).await.remove(0);
    assert_eq!((d.success, d.response_status), (Some(false), Some(302)));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn huge_responses_are_cut_down(pool: PgPool) {
    let (c, owner, repo) = setup(pool).await;
    let sink = sink().await;
    *sink.reply.lock().unwrap() = "x".repeat(300_000);
    let hook = c.state.webhooks.create(&user(owner), new_hook(repo, &sink.url, &["push"], None), &client()).await.unwrap();
    c.deliverer(true).run_due(10).await.unwrap();
    let d = deliveries(&c, hook.id).await.remove(0);
    assert_eq!(d.success, Some(true));
    assert!(d.response_body.unwrap().len() <= 16 * 1024);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn form_content_type_sends_a_payload_field_and_signs_that_body(pool: PgPool) {
    let (c, owner, repo) = setup(pool).await;
    let sink = sink().await;
    let hook = c.state.webhooks.create(&user(owner), new_hook(repo, &sink.url, &["push"], Some("s3cret")), &client()).await.unwrap();
    sqlx::query("UPDATE webhooks SET content_type = 'form' WHERE id = $1").bind(hook.id.0).execute(&c.pool).await.unwrap();
    c.deliverer(true).run_due(10).await.unwrap();
    let got = sink.last();
    assert_eq!(got.headers.get("content-type").unwrap(), "application/x-www-form-urlencoded");
    assert!(got.body.starts_with("payload=%7B"));
    assert_eq!(got.headers.get("x-hub-signature-256").unwrap().to_str().unwrap(), ch_notify::deliver::sign(b"s3cret", got.body.as_bytes()));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn private_targets_are_refused_when_saved_and_when_delivered(pool: PgPool) {
    // Default config: allow_private is off.
    let c = ctx(pool).await;
    let owner = c.env.user("alice").await;
    let repo = c.env.repo(owner, "demo").await;
    let me = user(owner);
    for url in [
        "http://127.0.0.1:3200/hook",
        "http://localhost/hook",
        "http://169.254.169.254/latest/meta-data/",
        "http://10.0.0.5/x",
        "http://[::1]/",
        "http://172.20.0.2:3200/",
        "file:///etc/passwd",
        "https://user:pw@example.org/",
    ] {
        let r = c.state.webhooks.create(&me, new_hook(repo, url, &["push"], None), &client()).await;
        assert!(matches!(r, Err(Error::Validation(_))), "{url}: {r:?}");
        assert!(
            c.state
                .webhooks
                .update(&me, WebhookId(1), WebhookChange { url: Some(url.into()), ..Default::default() }, &client())
                .await
                .is_err()
        );
    }
    assert_eq!(c.env.count("webhooks").await, 0);

    // Even if one gets into the table some other way, delivery refuses it and never connects.
    let sink = sink().await;
    let hook: i64 = sqlx::query_scalar("INSERT INTO webhooks (scope, repo_id, url, events) VALUES ('repo', $1, $2, '{push}') RETURNING id")
        .bind(repo.0)
        .bind(&sink.url)
        .fetch_one(&c.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO webhook_deliveries (webhook_id, event, payload, next_attempt_at) VALUES ($1, 'push', '{}', now())")
        .bind(hook)
        .execute(&c.pool)
        .await
        .unwrap();
    assert_eq!(c.deliverer(false).run_due(10).await.unwrap(), 1);
    assert_eq!(sink.count(), 0, "nothing reached the loopback listener");
    let row: (Option<bool>, Option<String>) = sqlx::query_as("SELECT success, response_body FROM webhook_deliveries WHERE webhook_id = $1")
        .bind(hook)
        .fetch_one(&c.pool)
        .await
        .unwrap();
    assert_eq!(row.0, Some(false));
    assert!(row.1.unwrap().contains("private"), "the delivery says why");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn events_reach_only_the_hooks_that_want_them(pool: PgPool) {
    let (c, owner, repo) = setup(pool).await;
    let other_repo = c.env.repo(owner, "other").await;
    let me = user(owner);
    let sink = sink().await;
    let wants = c.state.webhooks.create(&me, new_hook(repo, &sink.url, &["issues"], None), &client()).await.unwrap();
    let pushes_only = c.state.webhooks.create(&me, new_hook(repo, &sink.url, &["push"], None), &client()).await.unwrap();
    let everything = c.state.webhooks.create(&me, new_hook(repo, &sink.url, &["*"], None), &client()).await.unwrap();
    let elsewhere = c.state.webhooks.create(&me, new_hook(other_repo, &sink.url, &["issues"], None), &client()).await.unwrap();
    let off = c
        .state
        .webhooks
        .create(&me, NewWebhook { active: false, ..new_hook(repo, &sink.url, &["issues"], None) }, &client())
        .await
        .unwrap();
    sqlx::query("DELETE FROM webhook_deliveries").execute(&c.pool).await.unwrap();

    let issue_id = issue(&c.pool, repo, 1, owner, "Broken thing", "It does not work", false).await;
    let ev = DomainEvent::IssueOpened { repo_id: repo, issue_id: ch_core::IssueId(issue_id), number: 1, actor: owner };
    let n = ch_notify::webhooks::enqueue_event(&c.pool, &c.state.config, &ev).await.unwrap();
    assert_eq!(n, 2);
    let count = |h: WebhookId| {
        let pool = c.pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM webhook_deliveries WHERE webhook_id = $1")
                .bind(h.0)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    assert_eq!(count(wants.id).await, 1);
    assert_eq!(count(everything.id).await, 1);
    assert_eq!(count(pushes_only.id).await, 0);
    assert_eq!(count(elsewhere.id).await, 0);
    assert_eq!(count(off.id).await, 0);

    let d = sqlx::query_as::<_, (String, Option<String>, serde_json::Value)>(
        "SELECT event, action, payload FROM webhook_deliveries WHERE webhook_id = $1",
    )
    .bind(wants.id.0)
    .fetch_one(&c.pool)
    .await
    .unwrap();
    assert_eq!((d.0.as_str(), d.1.as_deref()), ("issues", Some("opened")));
    assert_eq!(d.2["action"], "opened");
    assert_eq!(d.2["issue"]["number"], 1);
    assert_eq!(d.2["issue"]["title"], "Broken thing");
    assert_eq!(d.2["issue"]["state"], "open");
    assert_eq!(d.2["issue"]["user"]["login"], "alice");
    assert_eq!(d.2["repository"]["full_name"], "alice/demo");
    assert_eq!(d.2["sender"]["login"], "alice");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn push_create_delete_status_and_pull_request_payloads(pool: PgPool) {
    let (c, owner, repo) = setup(pool).await;
    let me = user(owner);
    let sink = sink().await;
    let hook = c.state.webhooks.create(&me, new_hook(repo, &sink.url, &["*"], None), &client()).await.unwrap();
    let post = |ev: DomainEvent| {
        let (pool, cfg) = (c.pool.clone(), c.state.config.clone());
        async move { ch_notify::webhooks::enqueue_event(&pool, &cfg, &ev).await.unwrap() }
    };
    let zero = "0".repeat(40);
    let sha = "a".repeat(40);
    assert_eq!(
        post(DomainEvent::Push {
            repo_id: repo,
            git_ref: "refs/heads/main".into(),
            before: zero.clone(),
            after: sha.clone(),
            pusher: Some(owner),
            forced: false,
            app: None,
            server_built: false,
        })
        .await,
        1
    );
    assert_eq!(
        post(DomainEvent::RefCreated { repo_id: repo, git_ref: "refs/tags/v1".into(), sha: sha.clone(), actor: Some(owner) }).await,
        1
    );
    assert_eq!(post(DomainEvent::RefDeleted { repo_id: repo, git_ref: "refs/heads/old".into(), sha: sha.clone(), actor: None }).await, 1);
    sqlx::query("INSERT INTO commit_statuses (repo_id, sha, context, state, creator_id) VALUES ($1, $2, 'ci/build', 'success', $3)")
        .bind(repo.0)
        .bind(&sha)
        .bind(owner.0)
        .execute(&c.pool)
        .await
        .unwrap();
    assert_eq!(
        post(DomainEvent::StatusCreated { repo_id: repo, sha: sha.clone(), context: "ci/build".into(), state: "success".into() }).await,
        1
    );
    let pull = issue(&c.pool, repo, 2, owner, "Add a thing", "Closes #1", true).await;
    sqlx::query("INSERT INTO pull_requests (issue_id, repo_id, head_repo_id, head_ref, head_sha, base_ref, base_sha) VALUES ($1, $2, $2, 'feature', $3, 'main', $4)").bind(pull).bind(repo.0).bind(&sha).bind(&zero).execute(&c.pool).await.unwrap();
    assert_eq!(
        post(DomainEvent::PrOpened {
            repo_id: repo,
            pull_id: ch_core::PullId(pull),
            number: 2,
            actor: owner,
            app: None,
            head_sha: None,
            pusher: Some(owner)
        })
        .await,
        1
    );
    assert_eq!(
        post(DomainEvent::PrMerged { repo_id: repo, pull_id: ch_core::PullId(pull), number: 2, merge_sha: sha.clone(), actor: owner })
            .await,
        1
    );
    // An event hooks cannot see queues nothing.
    assert_eq!(post(DomainEvent::RepoCreated { repo_id: repo, actor: Some(owner) }).await, 0);

    let rows: Vec<(String, Option<String>, serde_json::Value)> =
        sqlx::query_as("SELECT event, action, payload FROM webhook_deliveries WHERE webhook_id = $1 AND event <> 'ping' ORDER BY id")
            .bind(hook.id.0)
            .fetch_all(&c.pool)
            .await
            .unwrap();
    let kinds: Vec<(&str, Option<&str>)> = rows.iter().map(|r| (r.0.as_str(), r.1.as_deref())).collect();
    assert_eq!(
        kinds,
        vec![
            ("push", None),
            ("create", None),
            ("delete", None),
            ("status", None),
            ("pull_request", Some("opened")),
            ("pull_request", Some("closed"))
        ]
    );
    let push = &rows[0].2;
    assert_eq!(
        (push["ref"].as_str(), push["created"].as_bool(), push["deleted"].as_bool()),
        (Some("refs/heads/main"), Some(true), Some(false))
    );
    assert_eq!(push["pusher"]["name"], "alice");
    let create = &rows[1].2;
    assert_eq!((create["ref"].as_str(), create["ref_type"].as_str()), (Some("v1"), Some("tag")));
    assert_eq!(rows[2].2["ref_type"], "branch");
    assert_eq!(rows[2].2["sender"]["login"], "codehaven", "a delete with no actor comes from the server");
    assert_eq!(rows[3].2["context"], "ci/build");
    assert_eq!(rows[3].2["state"], "success");
    assert_eq!(rows[3].2["name"], "alice/demo");
    let pr = &rows[4].2;
    assert_eq!(
        (pr["number"].as_i64(), pr["pull_request"]["head"]["ref"].as_str(), pr["pull_request"]["base"]["ref"].as_str()),
        (Some(2), Some("feature"), Some("main"))
    );
    assert_eq!(pr["pull_request"]["merged"], false);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn org_hooks_see_events_from_the_orgs_repositories_only(pool: PgPool) {
    let (c, owner, _) = setup(pool).await;
    let org = c.env.org("haven").await;
    sqlx::query("INSERT INTO org_members (org_id, user_id, role) VALUES ($1, $2, 'owner')")
        .bind(org.0)
        .bind(owner.0)
        .execute(&c.pool)
        .await
        .unwrap();
    let org_repo = c.env.repo(org, "tools").await;
    let personal = c.env.repo(owner, "mine").await;
    let sink = sink().await;
    let me = user(owner);
    let hook = c
        .state
        .webhooks
        .create(
            &me,
            NewWebhook {
                repo_id: None,
                org_id: Some(ch_core::OrgId(org.0)),
                url: sink.url.clone(),
                secret: None,
                events: vec!["push".into()],
                active: true,
            },
            &client(),
        )
        .await
        .unwrap();
    sqlx::query("DELETE FROM webhook_deliveries").execute(&c.pool).await.unwrap();
    let push = |repo| DomainEvent::Push {
        repo_id: repo,
        git_ref: "refs/heads/main".into(),
        before: "0".repeat(40),
        after: "b".repeat(40),
        pusher: Some(owner),
        forced: false,
        app: None,
        server_built: false,
    };
    assert_eq!(ch_notify::webhooks::enqueue_event(&c.pool, &c.state.config, &push(org_repo)).await.unwrap(), 1);
    assert_eq!(ch_notify::webhooks::enqueue_event(&c.pool, &c.state.config, &push(personal)).await.unwrap(), 0);
    c.deliverer(true).run_due(10).await.unwrap();
    assert_eq!(sink.count(), 1);
    assert_eq!(hook.org_id, Some(ch_core::OrgId(org.0)));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn who_may_manage_hooks(pool: PgPool) {
    let (c, owner, repo) = setup(pool).await;
    let maintainer = c.env.user("maint").await;
    let writer = c.env.user("writer").await;
    let stranger = c.env.user("stranger").await;
    c.env.collaborator(repo, maintainer, "maintain").await;
    c.env.collaborator(repo, writer, "write").await;
    let url = "http://127.0.0.1:9/h";
    assert!(
        c.state.webhooks.create(&user(maintainer), new_hook(repo, url, &["push"], None), &client()).await.is_ok(),
        "maintain is enough"
    );
    assert!(matches!(
        c.state.webhooks.create(&user(writer), new_hook(repo, url, &["push"], None), &client()).await,
        Err(Error::Forbidden(_))
    ));
    assert!(
        matches!(c.state.webhooks.create(&user(stranger), new_hook(repo, url, &["push"], None), &client()).await, Err(Error::NotFound)),
        "a private repo does not exist to a stranger"
    );
    assert!(matches!(
        c.state.webhooks.create(&Actor::Anonymous, new_hook(repo, url, &["push"], None), &client()).await,
        Err(Error::Unauthorized(_))
    ));
    assert!(c.state.webhooks.list(&user(writer), Some(repo), None).await.is_err());
    assert!(matches!(c.state.webhooks.list(&user(stranger), Some(repo), None).await, Err(Error::NotFound)));
    let hook = c.state.webhooks.list(&user(owner), Some(repo), None).await.unwrap().remove(0);
    for who in [user(writer), user(stranger)] {
        assert!(c.state.webhooks.get(&who, hook.id).await.is_err());
        assert!(c.state.webhooks.delete(&who, hook.id, &client()).await.is_err());
        assert!(c.state.webhooks.ping(&who, hook.id, &client()).await.is_err());
        assert!(c.state.webhooks.update(&who, hook.id, WebhookChange::default(), &client()).await.is_err());
        assert!(c.state.webhooks.deliveries(&who, hook.id, 10).await.is_err());
    }
    // Tokens need the hook scope, and never get past the repository permission.
    let scoped = ch_testkit::fixtures::token_actor(owner, &["admin:repo_hook"]);
    assert!(
        matches!(c.state.webhooks.list(&scoped, Some(repo), None).await, Err(Error::NotFound) | Err(Error::Forbidden(_))),
        "the scope alone grants no repo access"
    );
    let repo_token = ch_testkit::fixtures::token_actor(owner, &["repo"]);
    assert!(c.state.webhooks.list(&repo_token, Some(repo), None).await.is_ok());
    let read_token = ch_testkit::fixtures::token_actor(owner, &["repo:read"]);
    assert!(c.state.webhooks.create(&read_token, new_hook(repo, url, &["push"], None), &client()).await.is_err());
    let user_token = ch_testkit::fixtures::token_actor(owner, &["user"]);
    assert!(matches!(
        c.state.webhooks.create(&user_token, new_hook(repo, url, &["push"], None), &client()).await,
        Err(Error::Forbidden(_))
    ));
    // A runner or job credential is not a person.
    assert!(c.state.webhooks.list(&Actor::Runner { runner_id: ch_core::RunnerId(1) }, Some(repo), None).await.is_err());
    let _ = Permission::Admin;
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn org_hooks_need_an_org_owner(pool: PgPool) {
    let (c, owner, _) = setup(pool).await;
    let org = c.env.org("haven").await;
    let member = c.env.user("member").await;
    sqlx::query("INSERT INTO org_members (org_id, user_id, role) VALUES ($1, $2, 'owner'), ($1, $3, 'member')")
        .bind(org.0)
        .bind(owner.0)
        .bind(member.0)
        .execute(&c.pool)
        .await
        .unwrap();
    let mk = |who: &Actor| {
        let who = who.clone();
        let st = c.state.clone();
        async move {
            st.webhooks
                .create(
                    &who,
                    NewWebhook {
                        repo_id: None,
                        org_id: Some(ch_core::OrgId(org.0)),
                        url: "http://127.0.0.1:9/h".into(),
                        secret: None,
                        events: vec!["push".into()],
                        active: true,
                    },
                    &client(),
                )
                .await
        }
    };
    assert!(matches!(mk(&user(member)).await, Err(Error::Forbidden(_))));
    assert!(mk(&user(owner)).await.is_ok());
    let outsider = c.env.user("out").await;
    assert!(matches!(mk(&user(outsider)).await, Err(Error::NotFound)));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn validation_limits_and_audit(pool: PgPool) {
    let (c, owner, repo) = setup(pool).await;
    let me = user(owner);
    let url = "http://127.0.0.1:9/h";
    assert!(c.state.webhooks.create(&me, new_hook(repo, url, &[], None), &client()).await.is_err(), "no events");
    assert!(c.state.webhooks.create(&me, new_hook(repo, url, &["nope"], None), &client()).await.is_err());
    assert!(
        c.state
            .webhooks
            .create(
                &me,
                NewWebhook { repo_id: Some(repo), org_id: Some(ch_core::OrgId(1)), ..new_hook(repo, url, &["push"], None) },
                &client()
            )
            .await
            .is_err(),
        "both scopes"
    );
    assert!(
        c.state.webhooks.create(&me, NewWebhook { repo_id: None, ..new_hook(repo, url, &["push"], None) }, &client()).await.is_err(),
        "no scope"
    );
    let star = c.state.webhooks.create(&me, new_hook(repo, url, &["*"], None), &client()).await.unwrap();
    assert!(star.events.len() >= 15 && star.events.contains(&"ping".to_string()));
    for _ in 0..19 {
        c.state.webhooks.create(&me, new_hook(repo, url, &["push"], None), &client()).await.unwrap();
    }
    assert!(
        matches!(c.state.webhooks.create(&me, new_hook(repo, url, &["push"], None), &client()).await, Err(Error::Conflict(_))),
        "20 at most"
    );
    let id = star.id;
    c.state
        .webhooks
        .update(&me, id, WebhookChange { active: Some(false), events: Some(vec!["push".into()]), ..Default::default() }, &client())
        .await
        .unwrap();
    assert!(!c.state.webhooks.get(&me, id).await.unwrap().active);
    c.state.webhooks.delete(&me, id, &client()).await.unwrap();
    assert!(matches!(c.state.webhooks.get(&me, id).await, Err(Error::NotFound)));
    let actions: Vec<String> =
        sqlx::query_scalar("SELECT DISTINCT action FROM audit_events ORDER BY action").fetch_all(&c.pool).await.unwrap();
    assert_eq!(actions, vec!["webhook.created", "webhook.deleted", "webhook.updated"]);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn ping_redeliver_and_stop_when_the_hook_is_off_or_gone(pool: PgPool) {
    let (c, owner, repo) = setup(pool).await;
    let me = user(owner);
    let sink = sink().await;
    let hook = c.state.webhooks.create(&me, new_hook(repo, &sink.url, &["push"], None), &client()).await.unwrap();
    c.deliverer(true).run_due(10).await.unwrap();
    c.state.webhooks.ping(&me, hook.id, &client()).await.unwrap();
    c.deliverer(true).run_due(10).await.unwrap();
    assert_eq!(sink.count(), 2);

    let first = deliveries(&c, hook.id).await.pop().unwrap();
    c.state.webhooks.redeliver(&me, first.id).await.unwrap();
    c.deliverer(true).run_due(10).await.unwrap();
    assert_eq!(sink.count(), 3);
    let all = deliveries(&c, hook.id).await;
    assert_eq!(all.len(), 3);
    assert_eq!(all.iter().map(|d| d.guid.clone()).collect::<std::collections::BTreeSet<_>>().len(), 3, "a redelivery has its own guid");
    assert!(matches!(c.state.webhooks.redeliver(&me, 99_999).await, Err(Error::NotFound)));
    let stranger = c.env.user("stranger").await;
    assert!(c.state.webhooks.redeliver(&user(stranger), first.id).await.is_err());

    // A hook switched off after something was queued does not send it.
    c.state.webhooks.ping(&me, hook.id, &client()).await.unwrap();
    c.state.webhooks.update(&me, hook.id, WebhookChange { active: Some(false), ..Default::default() }, &client()).await.unwrap();
    c.deliverer(true).run_due(10).await.unwrap();
    assert_eq!(sink.count(), 3);
    let last = deliveries(&c, hook.id).await.remove(0);
    assert!(last.delivered_at.is_some() && last.next_attempt_at.is_none());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn two_workers_do_not_send_the_same_delivery_twice(pool: PgPool) {
    let (c, owner, repo) = setup(pool).await;
    let sink = sink().await;
    let me = user(owner);
    for _ in 0..6 {
        c.state.webhooks.create(&me, new_hook(repo, &sink.url, &["push"], None), &client()).await.unwrap();
    }
    let (a, b) = (c.deliverer(true), c.deliverer(true));
    let (ra, rb) = tokio::join!(a.run_due(20), b.run_due(20));
    assert_eq!(ra.unwrap() + rb.unwrap(), 6);
    assert_eq!(sink.count(), 6);
}
