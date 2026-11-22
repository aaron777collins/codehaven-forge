//! The REST endpoints for hooks and notifications.
#![allow(clippy::unwrap_used)]

mod common;
use common::*;

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode, header};
use axum::middleware::{Next, from_fn};
use ch_core::Actor;
use ch_core::events::DomainEvent;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

fn router(c: &Ctx, actor: Actor) -> Router {
    Router::new().nest("/api/v3", ch_notify::api_routes()).with_state(c.state.clone()).layer(from_fn(
        move |mut req: Request, next: Next| {
            let actor = actor.clone();
            async move {
                req.extensions_mut().insert(actor);
                req.extensions_mut().insert(client());
                next.run(req).await
            }
        },
    ))
}

async fn call(app: &Router, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(format!("/api/v3{path}"));
    let body = match body {
        Some(v) => {
            b = b.header(header::CONTENT_TYPE, "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(b.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn repo_hooks_over_rest(pool: PgPool) {
    let c = ctx_with(pool, |cfg| cfg.webhooks.allow_private = true).await;
    let alice = c.env.user("alice").await;
    let repo = c.env.repo(alice, "demo").await;
    let other = c.env.repo(alice, "other").await;
    let stranger = c.env.user("stranger").await;
    let sink = sink().await;
    let app = router(&c, user(alice));

    let (st, hook) = call(
        &app,
        Method::POST,
        "/repos/alice/demo/hooks",
        Some(json!({"config": {"url": sink.url, "secret": "shh-shh", "content_type": "json"}, "events": ["push", "issues"]})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{hook}");
    assert_eq!(hook["name"], "web");
    assert_eq!(hook["config"]["secret"], "********", "the secret is masked, never returned");
    assert!(!hook.to_string().contains("shh-shh"));
    assert_eq!(hook["events"], json!(["issues", "push"]));
    assert!(hook["url"].as_str().unwrap().ends_with(&format!("/api/v3/repos/alice/demo/hooks/{}", hook["id"])));
    assert!(hook["ping_url"].as_str().unwrap().ends_with("/pings"));
    let id = hook["id"].as_i64().unwrap();

    assert_eq!(call(&app, Method::GET, "/repos/alice/demo/hooks", None).await.1.as_array().unwrap().len(), 1);
    assert_eq!(call(&app, Method::GET, &format!("/repos/alice/demo/hooks/{id}"), None).await.0, StatusCode::OK);
    // A hook id is only reachable through its own repository.
    assert_eq!(call(&app, Method::GET, &format!("/repos/alice/other/hooks/{id}"), None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(call(&app, Method::DELETE, &format!("/repos/alice/other/hooks/{id}"), None).await.0, StatusCode::NOT_FOUND);
    let _ = other;

    let (st, patched) = call(
        &app,
        Method::PATCH,
        &format!("/repos/alice/demo/hooks/{id}"),
        Some(json!({"active": false, "config": {"url": sink.url, "secret": "********", "content_type": "form"}})),
    )
    .await;
    assert_eq!((st, patched["active"].as_bool(), patched["config"]["content_type"].as_str()), (StatusCode::OK, Some(false), Some("form")));
    assert_eq!(patched["config"]["secret"], "********", "the mask did not overwrite the real secret");
    let sealed: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT secret_sealed FROM webhooks WHERE id = $1").bind(id).fetch_one(&c.pool).await.unwrap();
    assert!(sealed.is_some());
    call(
        &app,
        Method::PATCH,
        &format!("/repos/alice/demo/hooks/{id}"),
        Some(json!({"active": true, "config": {"url": sink.url, "content_type": "json"}})),
    )
    .await;

    // Deliveries: the ping from creation, then a ping on request, then redeliver.
    c.deliverer(true).run_due(10).await.unwrap();
    assert_eq!(call(&app, Method::POST, &format!("/repos/alice/demo/hooks/{id}/pings"), None).await.0, StatusCode::NO_CONTENT);
    c.deliverer(true).run_due(10).await.unwrap();
    let (_, list) = call(&app, Method::GET, &format!("/repos/alice/demo/hooks/{id}/deliveries"), None).await;
    assert_eq!(list.as_array().unwrap().len(), 2);
    assert_eq!(
        (list[0]["event"].as_str(), list[0]["status"].as_str(), list[0]["status_code"].as_i64()),
        (Some("ping"), Some("OK"), Some(200))
    );
    let did = list[0]["id"].as_i64().unwrap();
    let (st, detail) = call(&app, Method::GET, &format!("/repos/alice/demo/hooks/{id}/deliveries/{did}"), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(detail["request"]["headers"]["X-GitHub-Event"], "ping");
    assert_eq!(detail["request"]["payload"]["hook_id"], id);
    assert_eq!(detail["response"]["payload"], "ok");
    assert_eq!(
        call(&app, Method::POST, &format!("/repos/alice/demo/hooks/{id}/deliveries/{did}/attempts"), None).await.0,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        call(&app, Method::POST, &format!("/repos/alice/demo/hooks/{id}/deliveries/99999/attempts"), None).await.0,
        StatusCode::NOT_FOUND
    );

    // Strangers see nothing, validation is a 422, and removal is a 204.
    let s = router(&c, user(stranger));
    assert_eq!(call(&s, Method::GET, "/repos/alice/demo/hooks", None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(
        call(&s, Method::POST, "/repos/alice/demo/hooks", Some(json!({"config": {"url": "https://example.org/x"}}))).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&app, Method::POST, "/repos/alice/demo/hooks", Some(json!({"config": {"url": "ftp://x"}}))).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        call(&app, Method::POST, "/repos/alice/demo/hooks", Some(json!({"events": ["push"]}))).await.0,
        StatusCode::UNPROCESSABLE_ENTITY,
        "config is required"
    );
    assert_eq!(
        call(&app, Method::POST, "/repos/alice/nothere/hooks", Some(json!({"config": {"url": "https://example.org/x"}}))).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(call(&app, Method::DELETE, &format!("/repos/alice/demo/hooks/{id}"), None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(call(&app, Method::GET, &format!("/repos/alice/demo/hooks/{id}"), None).await.0, StatusCode::NOT_FOUND);
    let _ = repo;
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn org_hooks_over_rest(pool: PgPool) {
    let c = ctx(pool).await;
    let alice = c.env.user("alice").await;
    let org = c.env.org("haven").await;
    sqlx::query("INSERT INTO org_members (org_id, user_id, role) VALUES ($1, $2, 'owner')")
        .bind(org.0)
        .bind(alice.0)
        .execute(&c.pool)
        .await
        .unwrap();
    let app = router(&c, user(alice));
    let (st, hook) =
        call(&app, Method::POST, "/orgs/haven/hooks", Some(json!({"config": {"url": "https://example.org/hook"}, "events": ["push"]})))
            .await;
    assert_eq!(st, StatusCode::CREATED, "{hook}");
    assert!(hook["url"].as_str().unwrap().contains("/orgs/haven/hooks/"));
    let id = hook["id"].as_i64().unwrap();
    assert_eq!(call(&app, Method::GET, "/orgs/haven/hooks", None).await.1.as_array().unwrap().len(), 1);
    assert_eq!(
        call(&app, Method::PATCH, &format!("/orgs/haven/hooks/{id}"), Some(json!({"events": ["issues"]}))).await.1["events"],
        json!(["issues"])
    );
    let outsider = router(&c, user(c.env.user("out").await));
    assert_eq!(call(&outsider, Method::GET, "/orgs/haven/hooks", None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(call(&app, Method::DELETE, &format!("/orgs/haven/hooks/{id}"), None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(call(&app, Method::GET, "/orgs/nope/hooks", None).await.0, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn notifications_over_rest(pool: PgPool) {
    let c = ctx(pool).await;
    let alice = c.env.user("alice").await;
    let bob = c.env.user("bob").await;
    let repo = c.env.repo(alice, "demo").await;
    c.env.collaborator(repo, bob, "write").await;
    sqlx::query("INSERT INTO watches (user_id, repo_id, level) VALUES ($1, $2, 'all')")
        .bind(bob.0)
        .bind(repo.0)
        .execute(&c.pool)
        .await
        .unwrap();
    let fan = ch_notify::fanout::Fanout::new(c.pool.clone(), c.state.authz.clone());
    for n in 1..=2 {
        let id = issue(&c.pool, repo, n, alice, &format!("Thing {n}"), "x", false).await;
        fan.process(&DomainEvent::IssueOpened { repo_id: repo, issue_id: ch_core::IssueId(id), number: n, actor: alice }).await.unwrap();
    }
    let app = router(&c, user(bob));
    let (st, inbox) = call(&app, Method::GET, "/notifications", None).await;
    assert_eq!(st, StatusCode::OK);
    let list = inbox.as_array().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0]["subject"]["title"], "Thing 2");
    assert_eq!(list[0]["subject"]["type"], "Issue");
    assert_eq!(list[0]["reason"], "subscribed");
    assert_eq!(list[0]["repository"]["full_name"], "alice/demo");
    assert_eq!(list[0]["unread"], true);
    assert!(list[0]["subject"]["url"].as_str().unwrap().ends_with("/repos/alice/demo/issues/2"));
    assert_eq!(call(&app, Method::GET, "/notifications/unread_count", None).await.1["unread"], 2);

    let id = list[0]["id"].as_str().unwrap();
    assert_eq!(call(&app, Method::GET, &format!("/notifications/threads/{id}"), None).await.0, StatusCode::OK);
    assert_eq!(call(&app, Method::PATCH, &format!("/notifications/threads/{id}"), None).await.0, StatusCode::RESET_CONTENT);
    assert_eq!(call(&app, Method::GET, "/notifications", None).await.1.as_array().unwrap().len(), 1, "unread only by default");
    assert_eq!(call(&app, Method::GET, "/notifications?all=true", None).await.1.as_array().unwrap().len(), 2);
    assert_eq!(call(&app, Method::DELETE, &format!("/notifications/threads/{id}"), None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(call(&app, Method::PUT, "/notifications", Some(json!({}))).await.0, StatusCode::RESET_CONTENT);
    assert_eq!(call(&app, Method::GET, "/notifications/unread_count", None).await.1["unread"], 0);
    // Alice cannot touch Bob's thread.
    let a = router(&c, user(alice));
    assert_eq!(call(&a, Method::PATCH, &format!("/notifications/threads/{id}"), None).await.0, StatusCode::NOT_FOUND);

    // Watching.
    assert_eq!(call(&app, Method::GET, "/repos/alice/demo/subscription", None).await.1["subscribed"], true);
    let (st, w) = call(&app, Method::PUT, "/repos/alice/demo/subscription", Some(json!({"ignored": true}))).await;
    assert_eq!((st, w["ignored"].as_bool(), w["subscribed"].as_bool()), (StatusCode::OK, Some(true), Some(false)));
    assert_eq!(call(&app, Method::DELETE, "/repos/alice/demo/subscription", None).await.0, StatusCode::NO_CONTENT);
    let (_, w) = call(&app, Method::GET, "/repos/alice/demo/subscription", None).await;
    assert_eq!((w["subscribed"].as_bool(), w["ignored"].as_bool()), (Some(false), Some(false)));
    let stranger = router(&c, user(c.env.user("stranger").await));
    assert_eq!(call(&stranger, Method::GET, "/repos/alice/demo/subscription", None).await.0, StatusCode::NOT_FOUND);
}
