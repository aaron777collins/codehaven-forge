#![allow(clippy::unwrap_used)]

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use bytes::Bytes;
use ch_core::Actor;
use common::*;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

struct Api {
    app: Router,
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl Reply {
    fn json(&self) -> Value {
        if self.body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&self.body).unwrap_or_else(|e| panic!("not json ({e}): {}", String::from_utf8_lossy(&self.body)))
        }
    }
    fn message(&self) -> String {
        self.json()["message"].as_str().unwrap_or_default().to_string()
    }
}

impl Api {
    async fn new(w: &World) -> Self {
        let state = w.app_state().await;
        Api { app: Router::new().nest("/api/v3", ch_collab::api_routes()).with_state(state) }
    }

    async fn raw(&self, actor: &Actor, method: &str, uri: &str, content_type: Option<&str>, accept: Option<&str>, body: Body) -> Reply {
        let mut b = Request::builder().method(method).uri(format!("/api/v3{uri}"));
        if let Some(ct) = content_type {
            b = b.header(header::CONTENT_TYPE, ct);
        }
        if let Some(a) = accept {
            b = b.header(header::ACCEPT, a);
        }
        let mut req = b.body(body).unwrap();
        req.extensions_mut().insert(actor.clone());
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let (parts, body) = resp.into_parts();
        Reply { status: parts.status, headers: parts.headers, body: body.collect().await.unwrap().to_bytes() }
    }

    async fn call(&self, actor: &Actor, method: &str, uri: &str, body: Option<Value>) -> Reply {
        match body {
            Some(v) => self.raw(actor, method, uri, Some("application/json"), None, Body::from(v.to_string())).await,
            None => self.raw(actor, method, uri, None, None, Body::empty()).await,
        }
    }

    async fn get(&self, actor: &Actor, uri: &str) -> Reply {
        self.call(actor, "GET", uri, None).await
    }
}

const R: &str = "/repos/alice/demo";

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn an_issue_goes_through_its_life(pool: PgPool) {
    let w = World::new(pool).await;
    let api = Api::new(&w).await;
    let (carol, dave, bob, eve) = (w.carol_a(), w.dave_a(), w.bob_a(), w.eve_a());

    let r = api.call(&carol, "POST", &format!("{R}/issues"), Some(json!({ "title": "Login fails", "body": "details" }))).await;
    assert_eq!(r.status, StatusCode::CREATED);
    let j = r.json();
    assert_eq!(
        (j["number"].as_i64(), j["state"].as_str(), j["title"].as_str(), j["user"]["login"].as_str()),
        (Some(1), Some("open"), Some("Login fails"), Some("carol"))
    );
    assert_eq!(j["html_url"], "https://test.example/alice/demo/issues/1");
    assert_eq!(j["url"], "https://test.example/api/v3/repos/alice/demo/issues/1");
    assert_eq!(j["repository_url"], "https://test.example/api/v3/repos/alice/demo");
    assert_eq!(j["labels"], json!([]));
    assert_eq!(j["reactions"]["total_count"], 0);
    assert!(j.get("pull_request").is_none());

    // What a stranger and a signed-out caller see.
    let r = api.get(&eve, &format!("{R}/issues/1")).await;
    assert_eq!((r.status, r.message().as_str()), (StatusCode::NOT_FOUND, "Not Found"));
    assert_eq!(api.get(&Actor::Anonymous, &format!("{R}/issues/1")).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(api.get(&eve, "/repos/alice/does-not-exist/issues").await.status, StatusCode::NOT_FOUND);

    // Bad bodies are 422s in GitHub's shape.
    let r = api.raw(&carol, "POST", &format!("{R}/issues"), Some("application/json"), None, Body::from("{\"title\":")).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.json()["errors"][0]["message"].as_str().unwrap().contains("Problems parsing JSON"), "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(
        api.call(&carol, "POST", &format!("{R}/issues"), Some(json!({ "body": "no title" }))).await.status,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let r = api.call(&carol, "POST", &format!("{R}/issues"), Some(json!({ "title": "  " }))).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.json()["errors"][0]["field"], "title");

    // Triage: labels, assignees and milestones.
    assert_eq!(
        api.call(&dave, "POST", &format!("{R}/labels"), Some(json!({ "name": "bug", "color": "ff0000" }))).await.status,
        StatusCode::CREATED
    );
    assert_eq!(api.call(&dave, "POST", &format!("{R}/milestones"), Some(json!({ "title": "v1" }))).await.status, StatusCode::CREATED);
    let r = api
        .call(
            &dave,
            "PATCH",
            &format!("{R}/issues/1"),
            Some(json!({ "state": "closed", "state_reason": "not_planned", "labels": ["bug"], "assignees": ["bob"], "milestone": 1 })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", String::from_utf8_lossy(&r.body));
    let j = r.json();
    assert_eq!(
        (
            j["state"].as_str(),
            j["state_reason"].as_str(),
            j["labels"][0]["name"].as_str(),
            j["assignee"]["login"].as_str(),
            j["milestone"]["number"].as_i64()
        ),
        (Some("closed"), Some("not_planned"), Some("bug"), Some("bob"), Some(1))
    );
    assert_eq!(j["closed_by"]["login"], "dave");
    assert_eq!(j["milestone"]["closed_issues"], 1);
    let j = api.call(&dave, "PATCH", &format!("{R}/issues/1"), Some(json!({ "milestone": null, "state": "open" }))).await.json();
    assert!(j["milestone"].is_null() && j["state"] == "open");
    assert_eq!(
        api.call(&dave, "PATCH", &format!("{R}/issues/1"), Some(json!({ "state": "weird" }))).await.status,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        api.call(&dave, "PATCH", &format!("{R}/issues/1"), Some(json!({ "assignees": ["nobody"] }))).await.status,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let r = api.call(&carol, "PATCH", &format!("{R}/issues/1"), Some(json!({ "labels": [] }))).await;
    assert_eq!((r.status, r.message().contains("triage")), (StatusCode::FORBIDDEN, true));
    // An absent field is left alone: the labels survived the patches above.
    assert_eq!(api.get(&carol, &format!("{R}/issues/1")).await.json()["labels"][0]["name"], "bug");

    // Comments and reactions.
    let r = api.call(&carol, "POST", &format!("{R}/issues/1/comments"), Some(json!({ "body": "me too" }))).await;
    assert_eq!(r.status, StatusCode::CREATED);
    let cid = r.json()["id"].as_i64().unwrap();
    assert_eq!(r.json()["html_url"], format!("https://test.example/alice/demo/issues/1#issuecomment-{cid}"));
    assert_eq!(api.get(&carol, &format!("{R}/issues/1/comments")).await.json().as_array().unwrap().len(), 1);
    assert_eq!(
        api.call(&carol, "PATCH", &format!("{R}/issues/comments/{cid}"), Some(json!({ "body": "edited" }))).await.json()["body"],
        "edited"
    );
    assert_eq!(
        api.get(&carol, &format!("{R}/issues/comments/{cid}")).await.json()["issue_url"],
        "https://test.example/api/v3/repos/alice/demo/issues/1"
    );
    let r = api.call(&bob, "POST", &format!("{R}/issues/1/reactions"), Some(json!({ "content": "+1" }))).await;
    assert_eq!((r.status, r.json()["content"].as_str()), (StatusCode::CREATED, Some("+1")));
    let rid = r.json()["id"].as_i64().unwrap();
    assert_eq!(api.get(&bob, &format!("{R}/issues/1")).await.json()["reactions"]["+1"], 1);
    assert_eq!(
        api.call(&bob, "POST", &format!("{R}/issues/1/reactions"), Some(json!({ "content": "nope" }))).await.status,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        api.call(&carol, "DELETE", &format!("{R}/issues/1/reactions/{rid}"), None).await.status,
        StatusCode::NOT_FOUND,
        "someone else's reaction"
    );
    assert_eq!(api.call(&bob, "DELETE", &format!("{R}/issues/1/reactions/{rid}"), None).await.status, StatusCode::NO_CONTENT);
    let r = api.call(&bob, "POST", &format!("{R}/issues/comments/{cid}/reactions"), Some(json!({ "content": "heart" }))).await;
    assert_eq!(r.status, StatusCode::CREATED);
    assert_eq!(api.call(&carol, "DELETE", &format!("{R}/issues/comments/{cid}"), None).await.status, StatusCode::NO_CONTENT);

    // Lock and unlock take no body.
    assert_eq!(api.call(&dave, "PUT", &format!("{R}/issues/1/lock"), None).await.status, StatusCode::NO_CONTENT);
    assert_eq!(
        api.call(&dave, "PUT", &format!("{R}/issues/1/lock"), Some(json!({ "lock_reason": "resolved" }))).await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(api.get(&carol, &format!("{R}/issues/1")).await.json()["locked"], true);
    assert_eq!(
        api.call(&carol, "POST", &format!("{R}/issues/1/comments"), Some(json!({ "body": "hello?" }))).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(api.call(&dave, "DELETE", &format!("{R}/issues/1/lock"), None).await.status, StatusCode::NO_CONTENT);

    // The timeline names its events the way GitHub does.
    let t = api.get(&carol, &format!("{R}/issues/1/timeline")).await.json();
    let names: Vec<&str> = t.as_array().unwrap().iter().map(|e| e["event"].as_str().unwrap()).collect();
    for expected in ["labeled", "assigned", "milestoned", "closed", "reopened", "locked", "unlocked"] {
        assert!(names.contains(&expected), "{expected} in {names:?}");
    }
    let events = api.get(&carol, &format!("{R}/issues/1/events")).await.json();
    assert!(events.as_array().unwrap().iter().all(|e| e["event"] != "commented"));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn lists_page_with_link_headers_and_filters(pool: PgPool) {
    let w = World::new(pool).await;
    let api = Api::new(&w).await;
    let carol = w.carol_a();
    w.label("bug").await;
    for t in ["one", "two", "three"] {
        w.issue(w.carol, t, "").await;
    }
    w.core.issue_add_labels(&w.dave_a(), w.repo, 2, vec!["bug".into()], &w.c()).await.unwrap();
    let r = api.get(&carol, &format!("{R}/issues?per_page=2&state=all")).await;
    let titles: Vec<String> = r.json().as_array().unwrap().iter().map(|i| i["title"].as_str().unwrap().to_string()).collect();
    assert_eq!(titles, vec!["three", "two"]);
    let link = r.headers[header::LINK].to_str().unwrap().to_string();
    assert!(
        link.contains("page=2")
            && link.contains("rel=\"next\"")
            && link.contains("state=all")
            && link.starts_with("<https://test.example/api/v3/repos/alice/demo/issues?"),
        "{link}"
    );
    let r = api.get(&carol, &format!("{R}/issues?per_page=2&page=2&state=all")).await;
    assert_eq!(r.json().as_array().unwrap().len(), 1);
    let link = r.headers[header::LINK].to_str().unwrap();
    assert!(link.contains("rel=\"prev\"") && !link.contains("rel=\"next\""), "{link}");
    assert_eq!(api.get(&carol, &format!("{R}/issues?labels=bug")).await.json().as_array().unwrap().len(), 1);
    assert_eq!(api.get(&carol, &format!("{R}/issues?sort=created&direction=asc")).await.json()[0]["title"], "one");
    assert_eq!(api.get(&carol, &format!("{R}/issues?q=is%3Aissue%20three")).await.json().as_array().unwrap().len(), 1);
    // Across repositories: the issues assigned to the caller.
    w.core.issue_add_assignees(&w.dave_a(), w.repo, 1, vec![w.carol], &w.c()).await.unwrap();
    let mine = api.get(&carol, "/issues?filter=assigned").await.json();
    assert_eq!(mine[0]["repository_url"], "https://test.example/api/v3/repos/alice/demo");
    assert_eq!(api.get(&carol, "/user/issues?filter=bogus").await.status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn labels_and_milestones_over_http(pool: PgPool) {
    let w = World::new(pool).await;
    let api = Api::new(&w).await;
    let (dave, carol) = (w.dave_a(), w.carol_a());
    let r = api
        .call(
            &dave,
            "POST",
            &format!("{R}/labels"),
            Some(json!({ "name": "good first issue", "color": "00ff00", "description": "Start here" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED);
    assert_eq!(r.json()["url"], "https://test.example/api/v3/repos/alice/demo/labels/good%20first%20issue");
    assert_eq!(
        api.call(&dave, "POST", &format!("{R}/labels"), Some(json!({ "name": "GOOD FIRST ISSUE", "color": "00ff00" }))).await.status,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(api.get(&carol, &format!("{R}/labels/good%20first%20issue")).await.json()["description"], "Start here");
    let r = api
        .call(&dave, "PATCH", &format!("{R}/labels/good%20first%20issue"), Some(json!({ "new_name": "beginner", "color": "0000ff" })))
        .await;
    assert_eq!((r.json()["name"].as_str(), r.json()["color"].as_str()), (Some("beginner"), Some("0000ff")));
    assert_eq!(api.call(&carol, "DELETE", &format!("{R}/labels/beginner"), None).await.status, StatusCode::FORBIDDEN);
    assert_eq!(api.call(&dave, "DELETE", &format!("{R}/labels/beginner"), None).await.status, StatusCode::NO_CONTENT);
    assert_eq!(api.get(&carol, &format!("{R}/labels/beginner")).await.status, StatusCode::NOT_FOUND);

    let r = api
        .call(
            &dave,
            "POST",
            &format!("{R}/milestones"),
            Some(json!({ "title": "v1", "due_on": "2030-01-02T03:04:05Z", "description": "first" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED);
    assert_eq!(
        (r.json()["number"].as_i64(), r.json()["state"].as_str(), r.json()["due_on"].as_str()),
        (Some(1), Some("open"), Some("2030-01-02T03:04:05Z"))
    );
    api.call(&dave, "POST", &format!("{R}/milestones"), Some(json!({ "title": "v2", "state": "closed" }))).await;
    assert_eq!(api.get(&carol, &format!("{R}/milestones")).await.json().as_array().unwrap().len(), 1, "open ones by default");
    assert_eq!(api.get(&carol, &format!("{R}/milestones?state=all")).await.json().as_array().unwrap().len(), 2);
    let j = api.call(&dave, "PATCH", &format!("{R}/milestones/1"), Some(json!({ "due_on": null, "state": "closed" }))).await.json();
    assert!(j["due_on"].is_null() && j["state"] == "closed" && j["closed_at"].is_string());
    assert_eq!(
        api.call(&dave, "POST", &format!("{R}/milestones"), Some(json!({ "title": "x", "state": "weird" }))).await.status,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(api.call(&dave, "DELETE", &format!("{R}/milestones/2"), None).await.status, StatusCode::NO_CONTENT);
    assert_eq!(api.get(&dave, &format!("{R}/assignees")).await.json().as_array().unwrap().len(), 4);
    assert_eq!(api.get(&dave, &format!("{R}/assignees/bob")).await.status, StatusCode::NO_CONTENT);
    assert_eq!(api.get(&dave, &format!("{R}/assignees/eve")).await.status, StatusCode::NOT_FOUND);
    // A reader does not get the list of who has access (ch-uph).
    assert_eq!(api.get(&carol, &format!("{R}/assignees")).await.status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn issue_labels_accept_both_body_shapes(pool: PgPool) {
    let w = World::new(pool).await;
    let api = Api::new(&w).await;
    let dave = w.dave_a();
    for l in ["a", "b", "c"] {
        w.label(l).await;
    }
    w.issue(w.carol, "x", "").await;
    let names =
        |r: &Reply| -> Vec<String> { r.json().as_array().unwrap().iter().map(|l| l["name"].as_str().unwrap().to_string()).collect() };
    assert_eq!(names(&api.call(&dave, "POST", &format!("{R}/issues/1/labels"), Some(json!(["a"]))).await), vec!["a"]);
    assert_eq!(names(&api.call(&dave, "POST", &format!("{R}/issues/1/labels"), Some(json!({ "labels": ["b"] }))).await), vec!["a", "b"]);
    assert_eq!(
        names(&api.call(&dave, "PUT", &format!("{R}/issues/1/labels"), Some(json!({ "labels": [{ "name": "c" }] }))).await),
        vec!["c"]
    );
    assert_eq!(
        api.call(&dave, "POST", &format!("{R}/issues/1/labels"), Some(json!(["nope"]))).await.status,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(names(&api.call(&dave, "DELETE", &format!("{R}/issues/1/labels/c"), None).await), Vec::<String>::new());
    api.call(&dave, "POST", &format!("{R}/issues/1/labels"), Some(json!(["a", "b"]))).await;
    assert_eq!(api.call(&dave, "DELETE", &format!("{R}/issues/1/labels"), None).await.status, StatusCode::NO_CONTENT);
    assert_eq!(api.get(&dave, &format!("{R}/issues/1/labels")).await.json(), json!([]));
    let r = api.call(&dave, "POST", &format!("{R}/issues/1/assignees"), Some(json!({ "assignees": ["bob"] }))).await;
    assert_eq!((r.status, r.json()["assignees"][0]["login"].as_str()), (StatusCode::CREATED, Some("bob")));
    let r = api.call(&dave, "DELETE", &format!("{R}/issues/1/assignees"), Some(json!({ "assignees": ["bob"] }))).await;
    assert_eq!((r.status, r.json()["assignees"].as_array().unwrap().len()), (StatusCode::OK, 0));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_pull_request_goes_through_its_life(pool: PgPool) {
    let w = World::new(pool).await;
    let api = Api::new(&w).await;
    let (carol, dave, bob) = (w.carol_a(), w.dave_a(), w.bob_a());
    let r = api
        .call(
            &carol,
            "POST",
            &format!("{R}/pulls"),
            Some(json!({ "title": "Add search", "head": "feature", "base": "main", "body": "Fixes nothing" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.message());
    let j = r.json();
    assert_eq!(
        (j["number"].as_i64(), j["state"].as_str(), j["draft"].as_bool(), j["merged"].as_bool()),
        (Some(1), Some("open"), Some(false), Some(false))
    );
    assert_eq!(
        (j["head"]["ref"].as_str(), j["base"]["ref"].as_str(), j["head"]["sha"].as_str()),
        (Some("feature"), Some("main"), Some(sha_of(2).as_str()))
    );
    assert_eq!(j["head"]["label"], "alice:feature");
    assert_eq!(j["head"]["repo"]["full_name"], "alice/demo");
    assert_eq!(j["base"]["repo"]["clone_url"], "https://test.example/alice/demo.git");
    assert_eq!((j["mergeable"].as_bool(), j["mergeable_state"].as_str()), (Some(true), Some("clean")));
    assert_eq!(
        (j["commits"].as_i64(), j["additions"].as_i64(), j["deletions"].as_i64(), j["changed_files"].as_i64()),
        (Some(1), Some(2), Some(1), Some(1))
    );
    assert_eq!(j["html_url"], "https://test.example/alice/demo/pull/1");
    assert_eq!(
        api.get(&carol, &format!("{R}/issues/1")).await.json()["pull_request"]["url"],
        "https://test.example/api/v3/repos/alice/demo/pulls/1"
    );

    assert_eq!(
        api.call(&carol, "POST", &format!("{R}/pulls"), Some(json!({ "title": "Again", "head": "feature", "base": "main" }))).await.status,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(api.get(&carol, &format!("{R}/pulls?head=alice:feature")).await.json().as_array().unwrap().len(), 1);
    assert_eq!(api.get(&carol, &format!("{R}/pulls?head=alice:other")).await.json().as_array().unwrap().len(), 0);
    let files = api.get(&carol, &format!("{R}/pulls/1/files")).await.json();
    assert_eq!(
        (files[0]["filename"].as_str(), files[0]["status"].as_str(), files[0]["additions"].as_i64()),
        (Some("src/lib.rs"), Some("modified"), Some(2))
    );
    assert!(files[0]["patch"].as_str().unwrap().contains("+fn b2() {}"));
    let commits = api.get(&carol, &format!("{R}/pulls/1/commits")).await.json();
    assert_eq!(commits[0]["sha"], sha_of(2));

    // Merging.
    assert_eq!(api.get(&carol, &format!("{R}/pulls/1/merge")).await.status, StatusCode::NOT_FOUND);
    assert_eq!(api.call(&carol, "PUT", &format!("{R}/pulls/1/merge"), None).await.status, StatusCode::FORBIDDEN);
    assert_eq!(api.call(&dave, "PUT", &format!("{R}/pulls/1/merge"), None).await.status, StatusCode::FORBIDDEN);
    assert_eq!(
        api.call(&bob, "PUT", &format!("{R}/pulls/1/merge"), Some(json!({ "merge_method": "fast-forward" }))).await.status,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        api.call(&bob, "PUT", &format!("{R}/pulls/1/merge"), Some(json!({ "sha": "0000000000000000000000000000000000000001" })))
            .await
            .status,
        StatusCode::CONFLICT
    );
    let r =
        api.call(&bob, "PUT", &format!("{R}/pulls/1/merge"), Some(json!({ "merge_method": "squash", "commit_title": "Squashed" }))).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.message());
    assert_eq!((r.json()["merged"].as_bool(), r.json()["message"].as_str()), (Some(true), Some("Pull Request successfully merged")));
    assert_eq!(r.json()["sha"], w.git.sha_of_branch(w.repo, "main"));
    assert_eq!(api.get(&carol, &format!("{R}/pulls/1/merge")).await.status, StatusCode::NO_CONTENT);
    let j = api.get(&carol, &format!("{R}/pulls/1")).await.json();
    assert_eq!((j["merged"].as_bool(), j["state"].as_str(), j["merged_by"]["login"].as_str()), (Some(true), Some("closed"), Some("bob")));
    let r = api.call(&bob, "PUT", &format!("{R}/pulls/1/merge"), None).await;
    assert_eq!((r.status, r.message().contains("already merged")), (StatusCode::METHOD_NOT_ALLOWED, true));
    assert_eq!(w.git.merges()[0].title, "Squashed");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_blocked_merge_says_why(pool: PgPool) {
    let w = World::new(pool).await;
    w.protect("main", 1, &["ci"]).await;
    let api = Api::new(&w).await;
    let bob = w.bob_a();
    api.call(&w.carol_a(), "POST", &format!("{R}/pulls"), Some(json!({ "title": "x", "head": "feature", "base": "main" }))).await;
    let r = api.call(&bob, "PUT", &format!("{R}/pulls/1/merge"), None).await;
    assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED);
    assert!(
        r.message().contains("Needs 1 more approving review") && r.message().contains("Required check \"ci\" has not passed"),
        "{}",
        r.message()
    );
    let g = api.get(&bob, &format!("{R}/pulls/1/merge-gate")).await.json();
    assert_eq!(g["mergeable"], false);
    assert_eq!(g["reasons"].as_array().unwrap().len(), 2);
    let r = api.call(&bob, "PUT", &format!("{R}/pulls/1/auto-merge"), Some(json!({ "merge_method": "squash" }))).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.message());
    assert_eq!(r.json()["auto_merge"]["merge_method"], "squash");
    assert!(api.call(&bob, "DELETE", &format!("{R}/pulls/1/auto-merge"), None).await.json()["auto_merge"].is_null());
    assert!(w.git.merges().is_empty());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn reviews_over_http(pool: PgPool) {
    let w = World::new(pool).await;
    let api = Api::new(&w).await;
    let (carol, bob) = (w.carol_a(), w.bob_a());
    api.call(&carol, "POST", &format!("{R}/pulls"), Some(json!({ "title": "x", "head": "feature", "base": "main" }))).await;

    let r = api.call(&bob, "POST", &format!("{R}/pulls/1/reviews"), Some(json!({ "event": "APPROVE", "body": "LGTM" }))).await;
    assert_eq!((r.status, r.json()["state"].as_str()), (StatusCode::OK, Some("APPROVED")));
    assert_eq!(
        api.call(&bob, "POST", &format!("{R}/pulls/1/reviews"), Some(json!({ "event": "LGTM" }))).await.status,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        api.call(&carol, "POST", &format!("{R}/pulls/1/reviews"), Some(json!({ "event": "APPROVE" }))).await.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "own pull request"
    );

    // A pending review with an inline comment, then submitted.
    let r = api
        .call(
            &w.dave_a(),
            "POST",
            &format!("{R}/pulls/1/reviews"),
            Some(json!({ "comments": [{ "path": "src/lib.rs", "line": 2, "side": "RIGHT", "body": "why?" }] })),
        )
        .await;
    assert_eq!((r.status, r.json()["state"].as_str()), (StatusCode::OK, Some("PENDING")));
    let rid = r.json()["id"].as_i64().unwrap();
    assert_eq!(api.get(&carol, &format!("{R}/pulls/1/comments")).await.json().as_array().unwrap().len(), 0, "pending comments are private");
    let r = api
        .call(&w.dave_a(), "POST", &format!("{R}/pulls/1/reviews/{rid}/events"), Some(json!({ "event": "COMMENT", "body": "a question" })))
        .await;
    assert_eq!((r.status, r.json()["state"].as_str()), (StatusCode::OK, Some("COMMENTED")));
    let comments = api.get(&carol, &format!("{R}/pulls/1/comments")).await.json();
    let c = &comments[0];
    assert_eq!(
        (c["path"].as_str(), c["line"].as_i64(), c["side"].as_str(), c["pull_request_review_id"].as_i64()),
        (Some("src/lib.rs"), Some(2), Some("RIGHT"), Some(rid))
    );
    assert!(c["diff_hunk"].as_str().unwrap().contains("+fn b2() {}"));
    let cid = c["id"].as_i64().unwrap();
    let r = api
        .call(
            &carol,
            "POST",
            &format!("{R}/pulls/1/comments"),
            Some(json!({ "body": "because", "commit_id": sha_of(2), "path": "src/lib.rs", "in_reply_to": cid })),
        )
        .await;
    assert_eq!((r.status, r.json()["in_reply_to_id"].as_i64()), (StatusCode::CREATED, Some(cid)));
    assert_eq!(api.get(&carol, &format!("{R}/pulls/1/reviews/{rid}/comments")).await.json().as_array().unwrap().len(), 1);
    let threads = api.get(&carol, &format!("{R}/pulls/1/threads")).await.json();
    assert_eq!(threads[0]["comments"].as_array().unwrap().len(), 2);
    let tid = threads[0]["id"].as_i64().unwrap();
    assert_eq!(api.call(&carol, "PUT", &format!("{R}/pulls/1/threads/{tid}/resolve"), None).await.status, StatusCode::NO_CONTENT);
    assert_eq!(api.get(&carol, &format!("{R}/pulls/1/threads")).await.json()[0]["resolved"], true);
    assert_eq!(
        api.call(&w.dave_a(), "PATCH", &format!("{R}/pulls/comments/{cid}"), Some(json!({ "body": "a better question" }))).await.json()["body"],
        "a better question"
    );
    assert_eq!(api.call(&w.dave_a(), "DELETE", &format!("{R}/pulls/comments/{cid}"), None).await.status, StatusCode::NO_CONTENT);
    assert_eq!(api.get(&carol, &format!("{R}/pulls/1/reviews")).await.json().as_array().unwrap().len(), 2);

    // Requested reviewers.
    let r = api.call(&carol, "POST", &format!("{R}/pulls/1/requested_reviewers"), Some(json!({ "reviewers": ["dave"] }))).await;
    assert_eq!((r.status, r.json()["requested_reviewers"][0]["login"].as_str()), (StatusCode::CREATED, Some("dave")));
    assert_eq!(api.get(&carol, &format!("{R}/pulls/1/requested_reviewers")).await.json()["users"][0]["login"], "dave");
    assert_eq!(
        api.call(&carol, "POST", &format!("{R}/pulls/1/requested_reviewers"), Some(json!({ "reviewers": ["ghost"] }))).await.status,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        api.call(&carol, "DELETE", &format!("{R}/pulls/1/requested_reviewers"), Some(json!({ "reviewers": ["dave"] }))).await.json()["requested_reviewers"],
        json!([])
    );
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn editing_closing_and_drafting_over_http(pool: PgPool) {
    let w = World::new(pool).await;
    let api = Api::new(&w).await;
    let carol = w.carol_a();
    api.call(&carol, "POST", &format!("{R}/pulls"), Some(json!({ "title": "x", "head": "feature", "base": "main" }))).await;
    let j = api.call(&carol, "PATCH", &format!("{R}/pulls/1"), Some(json!({ "title": "Better", "body": "more" }))).await.json();
    assert_eq!((j["title"].as_str(), j["body"].as_str()), (Some("Better"), Some("more")));
    assert_eq!(api.call(&carol, "POST", &format!("{R}/pulls/1/convert_to_draft"), None).await.json()["draft"], true);
    assert_eq!(api.call(&carol, "POST", &format!("{R}/pulls/1/ready_for_review"), None).await.json()["draft"], false);
    assert_eq!(api.call(&carol, "PATCH", &format!("{R}/pulls/1"), Some(json!({ "state": "closed" }))).await.json()["state"], "closed");
    assert_eq!(api.call(&carol, "PATCH", &format!("{R}/pulls/1"), Some(json!({ "state": "open" }))).await.json()["state"], "open");
    assert_eq!(
        api.call(&carol, "PATCH", &format!("{R}/pulls/1"), Some(json!({ "state": "sideways" }))).await.status,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(api.call(&carol, "PUT", &format!("{R}/pulls/1/update-branch"), None).await.status, StatusCode::FORBIDDEN);
    let r = api.call(&w.bob_a(), "PUT", &format!("{R}/pulls/1/update-branch"), None).await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.message());
    assert_eq!(api.get(&carol, &format!("{R}/pulls/999")).await.status, StatusCode::NOT_FOUND);
    assert_eq!(api.get(&w.eve_a(), &format!("{R}/pulls/1")).await.status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn releases_over_http(pool: PgPool) {
    let w = World::new(pool).await;
    let api = Api::new(&w).await;
    let (bob, carol, eve) = (w.bob_a(), w.carol_a(), w.eve_a());
    let r =
        api.call(&bob, "POST", &format!("{R}/releases"), Some(json!({ "tag_name": "release/1.0", "name": "One", "body": "First" }))).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.message());
    let j = r.json();
    let id = j["id"].as_i64().unwrap();
    assert_eq!(
        (j["tag_name"].as_str(), j["name"].as_str(), j["draft"].as_bool(), j["author"]["login"].as_str()),
        (Some("release/1.0"), Some("One"), Some(false), Some("bob"))
    );
    assert_eq!(j["html_url"], "https://test.example/alice/demo/releases/tag/release%2F1.0");
    assert!(j["upload_url"].as_str().unwrap().ends_with("/assets{?name,label}"));
    assert_eq!(api.call(&carol, "POST", &format!("{R}/releases"), Some(json!({ "tag_name": "v9" }))).await.status, StatusCode::FORBIDDEN);
    assert_eq!(api.call(&eve, "POST", &format!("{R}/releases"), Some(json!({ "tag_name": "v9" }))).await.status, StatusCode::NOT_FOUND);
    assert_eq!(
        api.call(&bob, "POST", &format!("{R}/releases"), Some(json!({ "tag_name": "release/1.0" }))).await.status,
        StatusCode::UNPROCESSABLE_ENTITY
    );

    // A tag with a slash in it is still one path.
    assert_eq!(api.get(&carol, &format!("{R}/releases/tags/release/1.0")).await.json()["id"], id);
    assert_eq!(api.get(&carol, &format!("{R}/releases/latest")).await.json()["id"], id);
    assert_eq!(api.get(&carol, &format!("{R}/releases")).await.json().as_array().unwrap().len(), 1);
    assert_eq!(api.get(&carol, &format!("{R}/releases/{id}")).await.json()["tag_name"], "release/1.0");

    // Upload, describe, download, delete.
    let r = api
        .raw(
            &bob,
            "POST",
            &format!("{R}/releases/{id}/assets?name=notes.txt&label=Notes"),
            Some("text/plain"),
            None,
            Body::from("hello release"),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.message());
    let a = r.json();
    let aid = a["id"].as_i64().unwrap();
    assert_eq!(
        (a["name"].as_str(), a["size"].as_i64(), a["content_type"].as_str(), a["label"].as_str()),
        (Some("notes.txt"), Some(13), Some("text/plain"), Some("Notes"))
    );
    assert_eq!(a["browser_download_url"], "https://test.example/alice/demo/releases/download/release%2F1.0/notes.txt");
    assert_eq!(
        api.raw(&carol, "POST", &format!("{R}/releases/{id}/assets?name=x"), Some("text/plain"), None, Body::from("x")).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        api.raw(&bob, "POST", &format!("{R}/releases/{id}/assets?name=notes.txt"), Some("text/plain"), None, Body::from("again"))
            .await
            .status,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(api.get(&carol, &format!("{R}/releases/assets/{aid}")).await.json()["download_count"], 0);
    let r = api.raw(&carol, "GET", &format!("{R}/releases/assets/{aid}"), None, Some("application/octet-stream"), Body::empty()).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(&r.body[..], b"hello release");
    assert_eq!(r.headers[header::CONTENT_DISPOSITION], "attachment; filename=\"notes.txt\"");
    assert_eq!(r.headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert_eq!(r.headers[header::CONTENT_LENGTH], "13");
    assert_eq!(api.get(&carol, &format!("{R}/releases/assets/{aid}")).await.json()["download_count"], 1);
    assert_eq!(
        api.call(&bob, "PATCH", &format!("{R}/releases/assets/{aid}"), Some(json!({ "name": "x" }))).await.status,
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(api.call(&bob, "DELETE", &format!("{R}/releases/assets/{aid}"), None).await.status, StatusCode::NO_CONTENT);
    assert_eq!(api.get(&carol, &format!("{R}/releases/assets/{aid}")).await.status, StatusCode::NOT_FOUND);

    let j = api.call(&bob, "PATCH", &format!("{R}/releases/{id}"), Some(json!({ "prerelease": true, "body": "changed" }))).await.json();
    assert_eq!((j["prerelease"].as_bool(), j["body"].as_str()), (Some(true), Some("changed")));
    let g = api.call(&bob, "POST", &format!("{R}/releases/generate-notes"), Some(json!({ "tag_name": "v2" }))).await;
    assert_eq!(g.status, StatusCode::OK, "{}", g.message());
    assert_eq!(g.json()["name"], "v2");
    assert_eq!(api.call(&bob, "DELETE", &format!("{R}/releases/{id}"), None).await.status, StatusCode::NO_CONTENT);
    assert_eq!(api.get(&carol, &format!("{R}/releases/{id}")).await.status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn search_over_http(pool: PgPool) {
    let w = World::new(pool).await;
    let api = Api::new(&w).await;
    let carol = w.carol_a();
    w.issue(w.alice, "Login crash", "").await;
    let r = api.get(&carol, "/search/repositories?q=demo").await.json();
    assert_eq!(
        (r["total_count"].as_i64(), r["incomplete_results"].as_bool(), r["items"][0]["full_name"].as_str()),
        (Some(1), Some(false), Some("alice/demo"))
    );
    assert_eq!(r["items"][0]["private"], true);
    assert_eq!(r["items"][0]["open_issues_count"], 1);
    let r = api.get(&carol, "/search/issues?q=crash").await.json();
    assert_eq!((r["total_count"].as_i64(), r["items"][0]["title"].as_str()), (Some(1), Some("Login crash")));
    assert_eq!(r["items"][0]["repository_url"], "https://test.example/api/v3/repos/alice/demo");
    let r = api.get(&carol, "/search/users?q=ali").await.json();
    assert_eq!(r["items"][0]["login"], "alice");
    w.git.st.lock().unwrap().grep_hits =
        vec![ch_core::model::SearchHit { kind: "code".into(), id: w.repo.0, title: "src/lib.rs".into(), snippet: "fn needle() {}".into() }];
    let r = api.get(&carol, "/search/code?q=needle%20repo%3Aalice%2Fdemo").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.message());
    let item = &r.json()["items"][0];
    assert_eq!(
        (item["name"].as_str(), item["path"].as_str(), item["text_matches"][0]["fragment"].as_str()),
        (Some("lib.rs"), Some("src/lib.rs"), Some("fn needle() {}"))
    );
    assert_eq!(item["html_url"], "https://test.example/alice/demo/blob/main/src/lib.rs");
    assert_eq!(api.get(&carol, "/search/code?q=needle").await.status, StatusCode::UNPROCESSABLE_ENTITY, "code search needs a repository");
    assert_eq!(api.get(&w.eve_a(), "/search/code?q=needle%20repo%3Aalice%2Fdemo").await.status, StatusCode::NOT_FOUND);
    assert_eq!(api.get(&w.eve_a(), "/search/repositories?q=demo").await.json()["total_count"], 0);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn nothing_is_served_to_a_caller_without_credentials(pool: PgPool) {
    let w = World::new(pool).await;
    w.issue(w.alice, "x", "").await;
    let api = Api::new(&w).await;
    let anon = Actor::Anonymous;
    for (m, u) in [
        ("GET", format!("{R}/issues")),
        ("GET", format!("{R}/issues/1")),
        ("POST", format!("{R}/issues")),
        ("GET", format!("{R}/labels")),
        ("GET", format!("{R}/milestones")),
        ("GET", format!("{R}/pulls")),
        ("GET", format!("{R}/releases")),
        ("GET", format!("{R}/releases/latest")),
        ("GET", format!("{R}/assignees")),
    ] {
        let r = api.call(&anon, m, &u, Some(json!({ "title": "x" }))).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{m} {u}");
        assert!(!String::from_utf8_lossy(&r.body).contains("alice"), "{m} {u}");
    }
    for u in ["/search/repositories?q=demo", "/search/issues?q=x", "/search/users?q=ali"] {
        let r = api.get(&anon, u).await;
        assert_eq!(r.json()["total_count"], 0, "{u}");
    }
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_read_only_token_cannot_change_anything(pool: PgPool) {
    let w = World::new(pool).await;
    let api = Api::new(&w).await;
    w.issue(w.alice, "x", "").await;
    let read_only = ch_testkit::fixtures::token_actor(w.bob, &["repo:read"]);
    let full = ch_testkit::fixtures::token_actor(w.bob, &["repo"]);
    // Reading works.
    assert_eq!(api.get(&read_only, &format!("{R}/issues/1")).await.status, StatusCode::OK);
    assert_eq!(api.get(&read_only, "/issues?filter=assigned").await.status, StatusCode::OK);
    // Anything that writes does not, even things a reader may do with a password.
    let denied = [
        ("POST", format!("{R}/issues"), json!({ "title": "t" })),
        ("POST", format!("{R}/issues/1/comments"), json!({ "body": "hi" })),
        ("POST", format!("{R}/issues/1/reactions"), json!({ "content": "+1" })),
        ("PATCH", format!("{R}/issues/1"), json!({ "title": "t" })),
        ("POST", format!("{R}/pulls"), json!({ "title": "t", "head": "feature", "base": "main" })),
        ("POST", format!("{R}/releases"), json!({ "tag_name": "v1" })),
    ];
    for (m, u, b) in denied {
        let r = api.call(&read_only, m, &u, Some(b.clone())).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{m} {u}: {}", String::from_utf8_lossy(&r.body));
        assert!(r.message().contains("read-only"), "{m} {u}");
    }
    assert_eq!(api.call(&full, "POST", &format!("{R}/issues"), Some(json!({ "title": "t" }))).await.status, StatusCode::CREATED);
    assert_eq!(w.env.count("issues").await, 2, "only the full token's issue was created");
}
