//! Who may open issues and comment without a grant (epic ch-4pn): every signed-in account on an
//! internal or public repository, unless the owner limits it to people with access. Real
//! services and a real database.

#![allow(clippy::unwrap_used)]

use ch_core::model::*;
use ch_core::{Actor, AuthVia, Config, Error, Scopes, TokenId, UserId};
use codehaven::wiring;
use sqlx::PgPool;
use std::sync::Arc;

fn session(id: UserId) -> Actor {
    Actor::User { id, is_admin: false, has_2fa: false, via: AuthVia::Session }
}

fn c() -> ClientInfo {
    ClientInfo::local()
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_owner_can_limit_contribution_to_people_with_access(pool: PgPool) {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::for_tests();
    cfg.storage.data_dir = dir.path().to_path_buf();
    cfg.server.external_url = "https://code.example.com".into();
    let s = wiring::build_state(Arc::new(cfg), pool.clone()).await.unwrap();
    let mut ids = Vec::new();
    for name in ["alice", "bob", "carol"] {
        let u = s
            .accounts
            .create_user(
                &Actor::System,
                NewUser {
                    username: name.into(),
                    email: format!("{name}@example.org"),
                    display_name: None,
                    is_admin: false,
                    can_run_ci: false,
                },
                &c(),
            )
            .await
            .unwrap();
        ids.push(u.id);
    }
    let (alice, bob, carol) = (ids[0], ids[1], ids[2]);
    let new = NewRepo {
        owner_id: alice,
        name: "proj".into(),
        description: String::new(),
        visibility: Visibility::Internal,
        default_branch: None,
        auto_init: true,
    };
    let repo = s.repos.create(&session(alice), new, &c()).await.unwrap();
    s.repos.set_collaborator(&session(alice), repo.id, carol, ch_core::Permission::Read, &c()).await.unwrap();
    let issue = |title: &str| NewIssue {
        repo_id: repo.id,
        title: title.into(),
        body: String::new(),
        labels: vec![],
        assignees: vec![],
        milestone_id: None,
    };

    // Open by default: bob, signed in with no grant, opens an issue and comments.
    let n = s.issues.create(&session(bob), issue("from bob"), &c()).await.unwrap().number;
    s.issues.add_comment(&session(bob), repo.id, n, "hello", &c()).await.unwrap();
    // A read-only token never writes, even where anyone may contribute.
    let ro = Actor::Token { user_id: bob, token_id: TokenId(1), scopes: Scopes::new(["repo:read"]) };
    assert!(matches!(s.issues.add_comment(&ro, repo.id, n, "nope", &c()).await, Err(Error::Forbidden(_))));

    // Only the account owner changes it.
    assert!(s.repos.set_contribution_policy(&session(carol), repo.id, ContributionPolicy::Grants, &c()).await.is_err());
    s.repos.set_contribution_policy(&session(alice), repo.id, ContributionPolicy::Grants, &c()).await.unwrap();

    // Limited: bob can no longer open or comment, but can still close his own issue.
    match s.issues.add_comment(&session(bob), repo.id, n, "again", &c()).await {
        Err(Error::Forbidden(m)) => assert_eq!(m, "The owner limits issues, comments and pull requests to people with access."),
        other => panic!("{other:?}"),
    }
    assert!(matches!(s.issues.create(&session(bob), issue("blocked"), &c()).await, Err(Error::Forbidden(_))));
    s.issues.set_state(&session(bob), repo.id, n, false, None, &c()).await.unwrap();
    assert!(
        matches!(s.issues.set_state(&session(bob), repo.id, n, true, None, &c()).await, Err(Error::Forbidden(_))),
        "reopening is contributing"
    );
    // People with access are not affected.
    s.issues.add_comment(&session(carol), repo.id, n, "still here", &c()).await.unwrap();
    s.issues.create(&session(alice), issue("from the owner"), &c()).await.unwrap();
    // Bob still reads it.
    assert!(s.issues.get(&session(bob), repo.id, n).await.is_ok());

    let audited: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action = 'repo.contribution_policy_changed'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(audited, 1);
}
