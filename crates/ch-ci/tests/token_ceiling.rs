//! The repository setting that caps every job token (ch-ctk.8).

#![allow(clippy::unwrap_used)]

mod support;
use axum::http::{Method, StatusCode};
use serde_json::json;
use sqlx::PgPool;
use support::*;

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_token_ceiling_is_read_and_set_by_repository_admins_only(pool: PgPool) {
    let w = World::new(pool).await;
    let (_repo, _git) = w.repo_with(w.alice, "demo", &[]).await;
    let uri = "/api/v3/repos/alice/demo/actions/permissions/workflow";
    let (s, v) = w.get(uri, &Who::User(w.alice)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["default_workflow_permissions"], "write");
    assert_eq!(
        w.send(Method::PUT, uri, &Who::User(w.alice), Some(json!({"default_workflow_permissions": "read"}))).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(w.get(uri, &Who::User(w.alice)).await.1["default_workflow_permissions"], "read");
    assert_eq!(
        w.send(Method::PUT, uri, &Who::User(w.alice), Some(json!({"default_workflow_permissions": "admin"}))).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    // Bob has no access: the repository is missing to him.
    assert_eq!(w.get(uri, &Who::User(w.bob)).await.0, StatusCode::NOT_FOUND);
    assert_eq!(
        w.send(Method::PUT, uri, &Who::Anon, Some(json!({"default_workflow_permissions": "write"}))).await.0,
        StatusCode::UNAUTHORIZED
    );
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action = 'repo.actions_token_ceiling_changed'")
        .fetch_one(w.pool())
        .await
        .unwrap();
    assert_eq!(n, 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_actions_policy_is_for_site_admins_and_checks_its_patterns(pool: PgPool) {
    use ch_core::web_contract::ActionsPolicy;
    let w = World::new(pool).await;
    let admin = ch_core::Actor::System;
    let user = ch_core::Actor::User { id: w.alice, is_admin: false, has_2fa: true, via: ch_core::AuthVia::Session };
    let c = ch_core::model::ClientInfo::local();
    let seeded = w.state.pipelines.actions_policy(&admin).await.unwrap();
    assert_eq!(seeded.mode, "selected");
    assert!(seeded.allow.iter().any(|p| p == "actions/*"));
    assert!(w.state.pipelines.actions_policy(&user).await.is_err());
    let p = ActionsPolicy {
        mode: "all".into(),
        allow: vec!["a/*".into(), "A/*".into(), " ".into()],
        block: vec!["evil/x".into()],
        require_sha: true,
        allow_codehaven_repos: false,
    };
    assert!(w.state.pipelines.set_actions_policy(&user, p.clone(), &c).await.is_err());
    w.state.pipelines.set_actions_policy(&admin, p, &c).await.unwrap();
    let got = w.state.pipelines.actions_policy(&admin).await.unwrap();
    assert_eq!(
        (got.mode.as_str(), got.allow.clone(), got.block.clone(), got.require_sha, got.allow_codehaven_repos),
        ("all", vec!["a/*".to_string()], vec!["evil/x".to_string()], true, false)
    );
    let bad = ActionsPolicy {
        mode: "all".into(),
        allow: vec!["not a pattern".into()],
        block: vec![],
        require_sha: false,
        allow_codehaven_repos: true,
    };
    assert!(w.state.pipelines.set_actions_policy(&admin, bad, &c).await.is_err());
    let bad_mode = ActionsPolicy { mode: "some".into(), allow: vec![], block: vec![], require_sha: false, allow_codehaven_repos: true };
    assert!(w.state.pipelines.set_actions_policy(&admin, bad_mode, &c).await.is_err());
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action = 'instance.actions_policy_changed'")
        .fetch_one(w.pool())
        .await
        .unwrap();
    assert_eq!(n, 1);
}
