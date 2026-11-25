//! Deleting an issue, moving it to another repository, and the order of pinned issues (ch-u6j).

#![allow(clippy::unwrap_used)]

mod common;

use ch_core::{Error, Permission, RepoId};
use common::*;
use sqlx::PgPool;

async fn tier(w: &World, repo: RepoId, tier: &str) {
    let vis = if tier == "private" { "private" } else { "public" };
    sqlx::query("UPDATE visibility_settings SET allow_public_repos = true").execute(&w.pool).await.unwrap();
    sqlx::query(
        "UPDATE repositories SET access_tier = $2, visibility = $3, public_since = CASE WHEN $2 = 'public' THEN now() END WHERE id = $1",
    )
    .bind(repo.0)
    .bind(tier)
    .bind(vis)
    .execute(&w.pool)
    .await
    .unwrap();
}

async fn grant(w: &World, repo: RepoId, user: ch_core::UserId, permission: &str) {
    sqlx::query(
        "INSERT INTO repo_collaborators (repo_id, user_id, permission) VALUES ($1, $2, $3)
         ON CONFLICT (repo_id, user_id) DO UPDATE SET permission = EXCLUDED.permission",
    )
    .bind(repo.0)
    .bind(user.0)
    .bind(permission)
    .execute(&w.pool)
    .await
    .unwrap();
}

async fn count(w: &World, table: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT count(*) FROM {table}")).fetch_one(&w.pool).await.unwrap()
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn only_a_repository_admin_deletes_an_issue_and_it_is_audited(pool: PgPool) {
    let w = World::new(pool).await;
    let one = w.issue(w.bob, "first", "text").await;
    w.core.issue_add_comment(&w.carol_a(), w.repo, one.issue.number, "a comment", &w.c()).await.unwrap();
    let two = w.issue(w.bob, "second", "").await;
    let pr = w.pull(w.bob, "a pull request", "").await;
    // Write and triage are not enough, a reader and a stranger are refused, and the issue stays.
    for a in [w.bob_a(), w.dave_a()] {
        assert!(matches!(w.core.issue_delete(&a, w.repo, one.issue.number, &w.c()).await, Err(Error::Forbidden(_))));
    }
    assert!(matches!(w.core.issue_delete(&w.carol_a(), w.repo, one.issue.number, &w.c()).await, Err(Error::Forbidden(_))));
    assert!(matches!(w.core.issue_delete(&w.eve_a(), w.repo, one.issue.number, &w.c()).await, Err(Error::NotFound)));
    assert_eq!(count(&w, "issue_comments").await, 1);
    // A pull request is not an issue.
    assert!(matches!(w.core.issue_delete(&w.alice_a(), w.repo, pr.pull.issue.number, &w.c()).await, Err(Error::Validation(_))));
    // The admin deletes it: it and its comment are gone, the other issue is not, and the number is not reused.
    w.core.issue_delete(&w.alice_a(), w.repo, one.issue.number, &w.c()).await.unwrap();
    assert!(matches!(w.core.issue_row(w.repo, one.issue.number).await, Err(Error::NotFound)));
    assert_eq!(count(&w, "issue_comments").await, 0);
    assert!(w.core.issue_row(w.repo, two.issue.number).await.is_ok());
    assert_eq!(w.issue(w.bob, "third", "").await.issue.number, pr.pull.issue.number + 1);
    let audited: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action = 'issue.deleted'").fetch_one(&w.pool).await.unwrap();
    assert_eq!(audited, 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn an_issue_moves_with_its_comments_to_a_repository_that_is_not_more_visible(pool: PgPool) {
    let w = World::new(pool).await;
    w.label("bug").await;
    let issue = w.issue(w.bob, "move me", "body").await;
    let n = issue.issue.number;
    w.core.issue_add_comment(&w.carol_a(), w.repo, n, "a comment that moves too", &w.c()).await.unwrap();
    w.core.issue_set_pinned(&w.dave_a(), w.repo, n, true, &w.c()).await.unwrap();
    // A second repository of Alice's. Bob can write to demo but sees nothing of "elsewhere" yet.
    let elsewhere = w.env.repo(w.alice, "elsewhere").await;
    assert!(matches!(w.core.issue_transfer(&w.bob_a(), w.repo, n, elsewhere, &w.c()).await, Err(Error::NotFound)), "it looks missing");
    // Read is not enough on the target.
    grant(&w, elsewhere, w.bob, "read").await;
    assert!(matches!(w.core.issue_transfer(&w.bob_a(), w.repo, n, elsewhere, &w.c()).await, Err(Error::Forbidden(_))));
    // And the source needs write too: Carol (read on demo, write on elsewhere) cannot.
    grant(&w, elsewhere, w.carol, "write").await;
    assert!(matches!(w.core.issue_transfer(&w.carol_a(), w.repo, n, elsewhere, &w.c()).await, Err(Error::Forbidden(_))));
    // A more visible target is refused, even with write on both.
    grant(&w, elsewhere, w.bob, "write").await;
    tier(&w, elsewhere, "internal").await;
    let r = w.core.issue_transfer(&w.bob_a(), w.repo, n, elsewhere, &w.c()).await;
    assert!(matches!(&r, Err(Error::Validation(v)) if v[0].field == "target"), "{r:?}");
    assert!(w.core.issue_row(w.repo, n).await.is_ok(), "nothing moved");
    // The same tier is fine: both private.
    tier(&w, elsewhere, "private").await;
    let moved = w.core.issue_transfer(&w.bob_a(), w.repo, n, elsewhere, &w.c()).await.unwrap();
    assert_eq!((moved.issue.repo_id, moved.issue.number), (elsewhere, 1));
    assert_eq!(moved.issue.title, "move me");
    assert!(!moved.pinned);
    assert!(moved.labels.is_empty() && moved.assignees.is_empty() && moved.milestone.is_none());
    assert!(matches!(w.core.issue_row(w.repo, n).await, Err(Error::NotFound)), "the old address is empty");
    let comments = w.core.issue_comments(&w.bob_a(), elsewhere, 1).await.unwrap();
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0].body, "a comment that moves too");
    assert!(w.core.issue_pinned(&w.alice_a(), w.repo).await.unwrap().is_empty(), "it left the old pinned list");
    let audited: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action = 'issue.transferred'").fetch_one(&w.pool).await.unwrap();
    assert_eq!(audited, 1);
    // From a repository that is more visible to a less visible one: allowed. Public demo to private elsewhere.
    tier(&w, w.repo, "public").await;
    let back = w.issue(w.bob, "narrower", "").await;
    let moved = w.core.issue_transfer(&w.bob_a(), w.repo, back.issue.number, elsewhere, &w.c()).await.unwrap();
    assert_eq!(moved.issue.number, 2);
    // A pull request does not move; neither does an issue onto its own repository.
    let pr = w.pull(w.bob, "pr", "").await;
    assert!(matches!(w.core.issue_transfer(&w.bob_a(), w.repo, pr.pull.issue.number, elsewhere, &w.c()).await, Err(Error::Validation(_))));
    assert!(matches!(w.core.issue_transfer(&w.bob_a(), w.repo, 1, w.repo, &w.c()).await, Err(Error::Validation(_))));
    let _ = Permission::Read;
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn pinned_issues_keep_the_order_they_are_given(pool: PgPool) {
    let w = World::new(pool).await;
    for i in 0..4 {
        w.issue(w.alice, &format!("i{i}"), "").await;
    }
    for n in [1, 2, 3] {
        w.core.issue_set_pinned(&w.dave_a(), w.repo, n, true, &w.c()).await.unwrap();
    }
    let order = |v: Vec<ch_core::collab::IssueView>| v.iter().map(|i| i.issue.number).collect::<Vec<_>>();
    assert_eq!(order(w.core.issue_pinned(&w.carol_a(), w.repo).await.unwrap()), vec![1, 2, 3], "pinned first comes first");
    let got = w.core.issue_reorder_pinned(&w.dave_a(), w.repo, vec![3, 1, 2], &w.c()).await.unwrap();
    assert_eq!(order(got), vec![3, 1, 2]);
    // It must name each pinned issue once, no other.
    for bad in [vec![3, 1], vec![3, 1, 2, 4], vec![3, 3, 1], vec![]] {
        assert!(matches!(w.core.issue_reorder_pinned(&w.dave_a(), w.repo, bad, &w.c()).await, Err(Error::Validation(_))));
    }
    // Triage is needed, and a reader cannot.
    assert!(matches!(w.core.issue_reorder_pinned(&w.carol_a(), w.repo, vec![1, 2, 3], &w.c()).await, Err(Error::Forbidden(_))));
    assert!(matches!(w.core.issue_reorder_pinned(&w.eve_a(), w.repo, vec![1, 2, 3], &w.c()).await, Err(Error::NotFound)));
    // Unpinning closes the gap; a new pin goes to the end.
    w.core.issue_set_pinned(&w.dave_a(), w.repo, 1, false, &w.c()).await.unwrap();
    assert_eq!(order(w.core.issue_pinned(&w.carol_a(), w.repo).await.unwrap()), vec![3, 2]);
    w.core.issue_set_pinned(&w.dave_a(), w.repo, 4, true, &w.c()).await.unwrap();
    assert_eq!(order(w.core.issue_pinned(&w.carol_a(), w.repo).await.unwrap()), vec![3, 2, 4]);
    // Deleting a pinned issue leaves the rest in order.
    w.core.issue_delete(&w.alice_a(), w.repo, 2, &w.c()).await.unwrap();
    assert_eq!(order(w.core.issue_pinned(&w.carol_a(), w.repo).await.unwrap()), vec![3, 4]);
}
