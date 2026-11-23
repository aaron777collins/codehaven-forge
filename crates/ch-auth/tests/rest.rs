//! The REST endpoints, driven through a router with the caller fixed, as the gate leaves it.
#![allow(clippy::unwrap_used)]

mod common;
use common::keys::*;
use common::*;

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode, header};
use axum::middleware::{Next, from_fn};
use ch_core::{Actor, AuthVia, Permission};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

fn router(c: &Ctx, actor: Actor) -> Router {
    Router::new().nest("/api/v3", ch_auth::api_routes()).with_state(c.state.clone()).layer(from_fn(move |mut req: Request, next: Next| {
        let actor = actor.clone();
        async move {
            req.extensions_mut().insert(actor);
            req.extensions_mut().insert(c1());
            next.run(req).await
        }
    }))
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
async fn profile_and_users(pool: PgPool) {
    let c = ctx(pool).await;
    make_user(&c, "alice", false).await;
    make_user(&c, "bob", false).await;
    let (_, me) = session(&c, "alice").await;
    let app = router(&c, me);

    let (st, me_json) = call(&app, Method::GET, "/user", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(me_json["login"], "alice");
    assert_eq!(me_json["email"], "alice@example.org");
    assert_eq!(me_json["two_factor_authentication"], false);
    assert_eq!(me_json["type"], "User");
    assert!(me_json["html_url"].as_str().unwrap().starts_with("https://test.example/"));

    let (st, p) = call(&app, Method::PATCH, "/user", Some(json!({"name": "Alice C", "bio": "hi"}))).await;
    assert_eq!((st, p["name"].as_str(), p["bio"].as_str()), (StatusCode::OK, Some("Alice C"), Some("hi")));

    // Another person's profile hides the email.
    let (st, bob) = call(&app, Method::GET, "/users/BOB", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(bob["login"], "bob");
    // GitHub sends `email: null` for someone else (ch-yci); the value is never there.
    assert!(bob["email"].is_null() && bob.get("two_factor_authentication").is_none());
    let (st, err) = call(&app, Method::GET, "/users/nobody", None).await;
    assert_eq!((st, err["message"].as_str()), (StatusCode::NOT_FOUND, Some("Not Found")));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn keys_tokens_and_emails(pool: PgPool) {
    let c = ctx(pool).await;
    make_user(&c, "alice", false).await;
    let (_, me) = session(&c, "alice").await;
    let app = router(&c, me);

    let (st, k) = call(&app, Method::POST, "/user/keys", Some(json!({"title": "laptop", "key": ED1}))).await;
    assert_eq!(st, StatusCode::CREATED, "{k}");
    assert_eq!(k["title"], "laptop");
    assert!(k["key"].as_str().unwrap().starts_with("ssh-ed25519 "));
    let (st, bad) = call(&app, Method::POST, "/user/keys", Some(json!({"title": "x", "key": RSA2048}))).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(bad["errors"][0]["message"].as_str().unwrap().contains("3072"));
    let (_, list) = call(&app, Method::GET, "/user/keys", None).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    let (_, other) = call(&app, Method::GET, "/users/alice/keys", None).await;
    assert_eq!(other.as_array().unwrap().len(), 1);
    let id = k["id"].as_i64().unwrap();
    assert_eq!(call(&app, Method::GET, &format!("/user/keys/{id}"), None).await.0, StatusCode::OK);
    assert_eq!(call(&app, Method::DELETE, &format!("/user/keys/{id}"), None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(call(&app, Method::DELETE, &format!("/user/keys/{id}"), None).await.0, StatusCode::NOT_FOUND);

    let (st, t) = call(&app, Method::POST, "/user/tokens", Some(json!({"name": "ci", "scopes": ["repo"], "expires_in_days": 30}))).await;
    assert_eq!(st, StatusCode::CREATED, "{t}");
    let secret = t["token"].as_str().unwrap().to_string();
    assert!(secret.starts_with("chp_"));
    let (_, listed) = call(&app, Method::GET, "/user/tokens", None).await;
    assert!(listed[0].get("token").is_none(), "the secret is shown once");
    assert_eq!(
        call(&app, Method::POST, "/user/tokens", Some(json!({"name": "x", "scopes": ["nope"]}))).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        call(&app, Method::POST, "/user/tokens", Some(json!({"name": "x", "scopes": ["repo"], "expires_in_days": 999}))).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    // The new token works, and cannot make another.
    let token_actor = c.state.authn.from_bearer(&secret, &c1()).await.unwrap();
    let as_token = router(&c, token_actor);
    assert_eq!(call(&as_token, Method::GET, "/user", None).await.0, StatusCode::OK);
    assert_eq!(
        call(&as_token, Method::POST, "/user/tokens", Some(json!({"name": "child", "scopes": ["repo"]}))).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(call(&app, Method::DELETE, &format!("/user/tokens/{}", t["id"]), None).await.0, StatusCode::NO_CONTENT);

    let (st, e) = call(&app, Method::POST, "/user/emails", Some(json!({"emails": ["second@example.org"]}))).await;
    assert_eq!((st, e[0]["verified"].as_bool()), (StatusCode::CREATED, Some(false)));
    let (_, emails) = call(&app, Method::GET, "/user/emails", None).await;
    assert_eq!(emails.as_array().unwrap().len(), 2);
    assert_eq!(emails[0]["primary"], true);
    assert_eq!(call(&app, Method::DELETE, "/user/emails", Some(json!({"emails": ["second@example.org"]}))).await.0, StatusCode::NO_CONTENT);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn orgs_and_teams(pool: PgPool) {
    let c = ctx(pool).await;
    make_user(&c, "alice", false).await;
    make_user(&c, "bob", false).await;
    let (_, me) = session(&c, "alice").await;
    let app = router(&c, me.clone());

    let (st, org) = call(&app, Method::POST, "/user/orgs", Some(json!({"name": "haven", "description": "harbour"}))).await;
    assert_eq!((st, org["login"].as_str()), (StatusCode::CREATED, Some("haven")));
    assert_eq!(call(&app, Method::POST, "/user/orgs", Some(json!({"name": "haven"}))).await.0, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(call(&app, Method::GET, "/orgs/haven", None).await.1["description"], "harbour");
    assert_eq!(call(&app, Method::GET, "/user/orgs", None).await.1.as_array().unwrap().len(), 1);
    assert_eq!(call(&app, Method::PATCH, "/orgs/haven", Some(json!({"default_repository_permission": "write"}))).await.0, StatusCode::OK);
    assert_eq!(
        call(&app, Method::PATCH, "/orgs/haven", Some(json!({"default_repository_permission": "root"}))).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );

    assert_eq!(call(&app, Method::PUT, "/orgs/haven/memberships/bob", Some(json!({"role": "member"}))).await.0, StatusCode::OK);
    let (_, members) = call(&app, Method::GET, "/orgs/haven/members", None).await;
    assert_eq!(members.as_array().unwrap().len(), 2);
    assert_eq!(members[0]["role"], "admin");

    let (st, team) = call(&app, Method::POST, "/orgs/haven/teams", Some(json!({"name": "Devs", "permission": "push"}))).await;
    assert_eq!((st, team["slug"].as_str(), team["permission"].as_str()), (StatusCode::CREATED, Some("devs"), Some("push")));
    assert_eq!(call(&app, Method::GET, "/orgs/haven/teams/devs", None).await.1["permission"], "push");
    // An absent permission means read. Neither "none" nor an unknown word makes a team.
    assert_eq!(call(&app, Method::POST, "/orgs/haven/teams", Some(json!({"name": "Readers"}))).await.1["permission"], "pull");
    for bad in ["bogus", "none"] {
        assert_eq!(
            call(&app, Method::POST, "/orgs/haven/teams", Some(json!({"name": "Broken", "permission": bad}))).await.0,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{bad} is not a team permission"
        );
    }
    assert_eq!(call(&app, Method::GET, "/orgs/haven/teams", None).await.1.as_array().unwrap().len(), 2, "nothing was created");
    assert_eq!(call(&app, Method::PUT, "/orgs/haven/teams/devs/memberships/bob", None).await.0, StatusCode::OK);
    assert_eq!(call(&app, Method::GET, "/orgs/haven/teams/devs/members", None).await.1.as_array().unwrap().len(), 1);
    let org_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE username = 'haven'").fetch_one(&c.pool).await.unwrap();
    c.env.repo(ch_core::UserId(org_id), "tools").await;
    // With no body the repository gets the team's own permission, not a hard coded read.
    assert_eq!(call(&app, Method::PUT, "/orgs/haven/teams/devs/repos/haven/tools", None).await.0, StatusCode::NO_CONTENT);
    let team_id = ch_core::TeamId(team["id"].as_i64().unwrap());
    assert_eq!(c.state.orgs.team_repos(&me, team_id).await.unwrap()[0].permission, Permission::Write);
    assert_eq!(
        call(&app, Method::PUT, "/orgs/haven/teams/devs/repos/haven/tools", Some(json!({"permission": "push"}))).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(call(&app, Method::PUT, "/orgs/haven/teams/devs/repos/haven/missing", None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(call(&app, Method::DELETE, "/orgs/haven/teams/devs/repos/haven/tools", None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(call(&app, Method::DELETE, "/orgs/haven/teams/devs/memberships/bob", None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(call(&app, Method::DELETE, "/orgs/haven/teams/devs", None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(call(&app, Method::DELETE, "/orgs/haven/members/bob", None).await.0, StatusCode::NO_CONTENT);
    // The org still has repos, so deleting it is refused.
    assert_eq!(call(&app, Method::DELETE, "/orgs/haven", None).await.0, StatusCode::CONFLICT);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn site_admin_routes_need_a_two_factor_session(pool: PgPool) {
    let c = ctx(pool).await;
    let (_, admin) = site_admin(&c, "boss").await;
    let bob = make_user(&c, "bob", false).await;
    let (_, plain) = session(&c, "bob").await;
    make_user(&c, "eve", true).await;
    let (_, admin_no_2fa) = session(&c, "eve").await;
    // A token of a real site admin is still not one.
    let boss_id = admin.user_id().unwrap();
    let boss_session = Actor::User { id: boss_id, is_admin: true, has_2fa: true, via: AuthVia::Session };
    let (_, secret) =
        c.state.accounts.create_token(&boss_session, boss_id, "t", vec!["repo".into(), "admin:org".into()], 5, &c1()).await.unwrap();
    let token = c.state.authn.from_bearer(&secret, &c1()).await.unwrap();

    let routes: Vec<(Method, &str, Option<Value>)> = vec![
        (Method::GET, "/admin/users", None),
        (Method::GET, "/admin/invites", None),
        (Method::POST, "/admin/invites", Some(json!({}))),
        (Method::DELETE, "/admin/invites/1", None),
        (Method::GET, "/admin/audit", None),
        (Method::PUT, "/users/bob/suspended", None),
        (Method::PUT, "/users/bob/site_admin", None),
        (Method::PUT, "/admin/users/bob/ci", None),
        (Method::POST, "/admin/users/bob/unlock", None),
    ];
    for who in [&plain, &admin_no_2fa, &token, &Actor::Anonymous] {
        let app = router(&c, who.clone());
        for (m, p, b) in &routes {
            let (st, body) = call(&app, m.clone(), p, b.clone()).await;
            assert!(
                matches!(st, StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED | StatusCode::NOT_FOUND),
                "{m} {p} as {who:?} gave {st} {body}"
            );
        }
    }
    let (st, body) = call(&router(&c, admin_no_2fa.clone()), Method::GET, "/admin/invites", None).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert!(body["message"].as_str().unwrap().contains("two-factor"), "{body}");

    let app = router(&c, admin.clone());
    let (st, inv) =
        call(&app, Method::POST, "/admin/invites", Some(json!({"email": "pal@example.org", "expires_in_hours": 24, "note": "pal"}))).await;
    assert_eq!(st, StatusCode::CREATED, "{inv}");
    assert!(inv["url"].as_str().unwrap().contains("/invite/chi_"));
    assert_eq!(inv["status"], "pending");
    let (_, list) = call(&app, Method::GET, "/admin/invites", None).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert!(list[0].get("url").is_none(), "the link is shown once, at creation");
    assert_eq!(call(&app, Method::DELETE, &format!("/admin/invites/{}", inv["id"]), None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(call(&app, Method::POST, "/admin/invites", Some(json!({"expires_in_hours": 0}))).await.0, StatusCode::UNPROCESSABLE_ENTITY);

    let (_, users) = call(&app, Method::GET, "/admin/users?q=bob", None).await;
    assert_eq!(users.as_array().unwrap().len(), 1);
    assert_eq!(call(&app, Method::PUT, "/users/bob/suspended", None).await.0, StatusCode::NO_CONTENT);
    assert!(c.state.accounts.get_user(bob).await.unwrap().suspended);
    assert_eq!(call(&app, Method::DELETE, "/users/bob/suspended", None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(call(&app, Method::PUT, "/admin/users/bob/ci", None).await.0, StatusCode::NO_CONTENT);
    assert!(c.state.accounts.get_user(bob).await.unwrap().can_run_ci);
    assert_eq!(call(&app, Method::PUT, "/users/bob/site_admin", None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(call(&app, Method::DELETE, "/users/bob/site_admin", None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(call(&app, Method::POST, "/admin/users/bob/unlock", None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(call(&app, Method::PUT, "/users/nobody/suspended", None).await.0, StatusCode::NOT_FOUND);

    let (st, audit) = call(&app, Method::GET, "/admin/audit?action=invite.created", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(audit.as_array().unwrap().len(), 1);
    assert_eq!(audit[0]["action"], "invite.created");
    let (_, by_ip) = call(&app, Method::GET, "/admin/audit?ip=203.0.113.10&limit=2", None).await;
    assert!(by_ip.as_array().unwrap().len() <= 2);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_site_admin_can_allow_ci_for_an_org_by_name(pool: PgPool) {
    let c = ctx(pool).await;
    let (_, admin) = site_admin(&c, "boss").await;
    make_user(&c, "bob", false).await;
    let (_, bob) = session(&c, "bob").await;
    c.state.orgs.create(&bob, "acme", "", &c1()).await.unwrap();
    let app = router(&c, admin);
    assert_eq!(call(&app, Method::PUT, "/admin/users/acme/ci", None).await.0, StatusCode::NO_CONTENT);
    let on: bool = sqlx::query_scalar("SELECT can_run_ci FROM users WHERE username = 'acme'").fetch_one(&c.pool).await.unwrap();
    assert!(on);
    assert_eq!(call(&app, Method::DELETE, "/admin/users/acme/ci", None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(call(&app, Method::PUT, "/admin/users/nobody-here/ci", None).await.0, StatusCode::NOT_FOUND);
}
