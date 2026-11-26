//! The merge queue (ch-dxk): pull requests merge one at a time, in order, once the gate is clear.

#![allow(clippy::unwrap_used)]

mod common;

use ch_core::Error;
use ch_core::model::*;
use common::*;
use sqlx::PgPool;

async fn queue_on(w: &World) {
    sqlx::query("UPDATE repositories SET merge_queue = true WHERE id = $1").bind(w.repo).execute(&w.pool).await.unwrap();
}

/// A pull request from a new branch that is a copy of the usual one.
async fn pr(w: &World, head: &str, title: &str) -> i64 {
    let sha = w.git.sha_of_branch(w.repo, "feature");
    w.git.branch(w.repo, head, &sha);
    w.pull_from(w.carol, head, title, "").await.pull.issue.number
}

async fn approve(w: &World, n: i64) {
    w.core.review_submit(&w.a(w.alice), w.repo, n, ReviewState::Approved, "", vec![], &w.c()).await.unwrap();
}

async fn queued(w: &World, n: i64) -> bool {
    w.core.queue_state(&w.alice_a(), w.repo, n).await.unwrap().is_some()
}

async fn events(w: &World, n: i64) -> Vec<(String, String)> {
    w.core.timeline(&w.alice_a(), w.repo, n).await.unwrap().into_iter().map(|e| (e.kind, e.data.to_string())).collect()
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_repository_without_a_queue_refuses_to_queue_and_one_with_it_refuses_a_direct_merge(pool: PgPool) {
    let w = World::new(pool).await;
    w.protect("main", 1, &[]).await;
    let n = w.pull(w.carol, "x", "").await.pull.issue.number;
    let r = w.core.queue_add(&w.bob_a(), w.repo, n, MergeMethod::Merge, &w.c()).await;
    assert!(matches!(r, Err(Error::Validation(_))), "no queue here: {r:?}");
    queue_on(&w).await;
    let r = w.core.pull_merge(&w.bob_a(), w.repo, n, MergeMethod::Merge, &w.c()).await;
    assert!(matches!(r, Err(Error::MergeBlocked(_))), "the queue is the only way in: {r:?}");
    let r = w.core.pull_set_auto_merge(&w.bob_a(), w.repo, n, Some(MergeMethod::Merge), &w.c()).await;
    assert!(matches!(r, Err(Error::Validation(_))), "and auto-merge is not another way: {r:?}");
    // Only someone who can write queues, and each pull request once.
    assert!(matches!(w.core.queue_add(&w.dave_a(), w.repo, n, MergeMethod::Merge, &w.c()).await, Err(Error::Forbidden(_))));
    w.core.queue_add(&w.bob_a(), w.repo, n, MergeMethod::Merge, &w.c()).await.unwrap();
    assert!(matches!(w.core.queue_add(&w.bob_a(), w.repo, n, MergeMethod::Merge, &w.c()).await, Err(Error::Conflict(_))));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_queue_merges_in_order_and_the_one_in_front_holds_the_rest_back(pool: PgPool) {
    let w = World::new(pool).await;
    w.protect("main", 1, &[]).await;
    queue_on(&w).await;
    let first = pr(&w, "one", "One").await;
    let second = pr(&w, "two", "Two").await;
    w.core.queue_add(&w.bob_a(), w.repo, first, MergeMethod::Squash, &w.c()).await.unwrap();
    w.core.queue_add(&w.bob_a(), w.repo, second, MergeMethod::Merge, &w.c()).await.unwrap();
    // The second is ready, the first is not: nothing may jump the line.
    approve(&w, second).await;
    assert_eq!(w.core.queue_advance(w.repo, "main").await.unwrap(), 0);
    assert!(w.git.merges().is_empty());
    let v = w.core.queue_state(&w.alice_a(), w.repo, second).await.unwrap().unwrap();
    assert_eq!((v.position, v.total), (2, 2));
    let v = w.core.queue_state(&w.alice_a(), w.repo, first).await.unwrap().unwrap();
    assert_eq!((v.position, v.total, v.method), (1, 2, MergeMethod::Squash));
    assert!(v.note.starts_with("Waiting"), "{}", v.note);
    // Once the first is approved both merge, first then second, each by its own method.
    approve(&w, first).await;
    assert_eq!(w.core.queue_advance(w.repo, "main").await.unwrap(), 2);
    let merges = w.git.merges();
    assert_eq!(merges.iter().map(|m| m.method).collect::<Vec<_>>(), vec![MergeMethod::Squash, MergeMethod::Merge]);
    assert!(merges.iter().all(|m| m.committer.name == "bob"), "merged as the person who queued them");
    assert!(!queued(&w, first).await && !queued(&w, second).await);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_failed_required_check_conflicts_new_commits_or_leaving_take_a_pull_request_out_with_a_reason(pool: PgPool) {
    let w = World::new(pool).await;
    w.protect("main", 0, &["ci"]).await;
    queue_on(&w).await;
    let n = pr(&w, "a", "A").await;
    w.core.queue_add(&w.bob_a(), w.repo, n, MergeMethod::Merge, &w.c()).await.unwrap();
    // Waiting for the check is not a failure.
    w.core.queue_advance(w.repo, "main").await.unwrap();
    assert!(queued(&w, n).await);
    w.checks.fail("ci");
    w.core.queue_advance(w.repo, "main").await.unwrap();
    assert!(!queued(&w, n).await);
    assert!(events(&w, n).await.iter().any(|(k, d)| k == "merge_queue_removed" && d.contains("required check failed")));
    assert!(w.git.merges().is_empty());

    // New commits that the queue did not make.
    w.checks.failing.lock().unwrap().clear();
    let m = pr(&w, "b", "B").await;
    w.core.queue_add(&w.bob_a(), w.repo, m, MergeMethod::Merge, &w.c()).await.unwrap();
    sqlx::query(
        "UPDATE pull_requests p SET head_sha = repeat('c', 40) FROM issues i WHERE i.id = p.issue_id AND i.number = $1 AND i.repo_id = $2",
    )
    .bind(m)
    .bind(w.repo)
    .execute(&w.pool)
    .await
    .unwrap();
    w.core.queue_advance(w.repo, "main").await.unwrap();
    assert!(!queued(&w, m).await);
    assert!(events(&w, m).await.iter().any(|(k, d)| k == "merge_queue_removed" && d.contains("new commits")));

    // A conflict, and a person taking it out.
    let c = pr(&w, "c", "C").await;
    w.core.queue_add(&w.bob_a(), w.repo, c, MergeMethod::Merge, &w.c()).await.unwrap();
    w.git.set_mergeable(MergeableState::Conflict);
    w.core.queue_advance(w.repo, "main").await.unwrap();
    assert!(events(&w, c).await.iter().any(|(k, d)| k == "merge_queue_removed" && d.contains("conflicts")));
    w.git.set_mergeable(MergeableState::Clean);
    let d = pr(&w, "d", "D").await;
    w.core.queue_add(&w.bob_a(), w.repo, d, MergeMethod::Merge, &w.c()).await.unwrap();
    assert!(matches!(w.core.queue_remove(&w.dave_a(), w.repo, d, &w.c()).await, Err(Error::Forbidden(_))));
    w.core.queue_remove(&w.bob_a(), w.repo, d, &w.c()).await.unwrap();
    assert!(!queued(&w, d).await);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_pull_request_behind_its_base_is_updated_first_and_merges_only_after_that(pool: PgPool) {
    let w = World::new(pool).await;
    w.protect("main", 1, &[]).await;
    queue_on(&w).await;
    let n = pr(&w, "a", "A").await;
    w.core.queue_add(&w.bob_a(), w.repo, n, MergeMethod::Merge, &w.c()).await.unwrap();
    w.git.set_mergeable(MergeableState::Behind);
    w.core.queue_advance(w.repo, "main").await.unwrap();
    let merges = w.git.merges();
    assert_eq!(merges.len(), 1, "the base was merged into the branch");
    assert!(merges[0].update_branch, "that is the update-branch write, not a merge of the pull request");
    assert!(queued(&w, n).await, "and the pull request waits for checks on the result");
    let head: String = sqlx::query_scalar("SELECT head_sha FROM merge_queue_entries").fetch_one(&w.pool).await.unwrap();
    let pull_head: String = sqlx::query_scalar(
        "SELECT head_sha FROM pull_requests p JOIN issues i ON i.id = p.issue_id WHERE i.number = $1 AND i.repo_id = $2",
    )
    .bind(n)
    .bind(w.repo)
    .fetch_one(&w.pool)
    .await
    .unwrap();
    assert_eq!(head, pull_head, "the queue follows its own commit");
}

/// What the real git layer reports: the test merge never says "behind", so the queue has to see
/// for itself that the head does not contain the base's tip.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_head_that_lacks_the_base_tip_is_updated_before_it_merges_even_when_the_test_merge_says_clean(pool: PgPool) {
    let w = World::new(pool).await;
    w.protect("main", 1, &[]).await;
    queue_on(&w).await;
    let n = pr(&w, "a", "A").await;
    approve(&w, n).await;
    // The base moved on after the head was made: the merge base is an older commit.
    let base = w.git.sha_of_branch(w.repo, "main");
    let head = w.git.sha_of_branch(w.repo, "a");
    w.git.set_compare(
        w.repo,
        &base,
        &head,
        Compare {
            base_sha: base.clone(),
            head_sha: head.clone(),
            merge_base: "e".repeat(40),
            ahead_by: 1,
            behind_by: 1,
            commits: vec![],
            diff: Diff { files: vec![] },
        },
    );
    w.git.set_mergeable(MergeableState::Clean);
    // Queueing already works on the line, so this is where it would merge on a result nobody tested.
    w.core.queue_add(&w.bob_a(), w.repo, n, MergeMethod::Merge, &w.c()).await.unwrap();
    let merges = w.git.merges();
    assert_eq!(merges.len(), 1);
    assert!(merges[0].update_branch, "the only write is the base merged into the branch");
    assert!(queued(&w, n).await);
    // Once the head holds the base's tip (the fake's default compare), it merges.
    assert_eq!(w.core.queue_advance(w.repo, "main").await.unwrap(), 1);
    let merges = w.git.merges();
    assert_eq!(merges.len(), 2);
    assert!(!merges[1].update_branch);
}

/// Several triggers (the add request, a push worker, a check event, the sweep) can advance the
/// same line at the same moment. Two of them must not both update the branch, because the second
/// would fail on a head that moved and drop a good pull request from the queue.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn two_advances_at_the_same_moment_do_not_drop_a_good_pull_request(pool: PgPool) {
    let w = World::new(pool).await;
    w.protect("main", 1, &[]).await;
    queue_on(&w).await;
    let n = pr(&w, "a", "A").await;
    w.core.queue_add(&w.bob_a(), w.repo, n, MergeMethod::Merge, &w.c()).await.unwrap();
    // The base moved on after the head was made: the merge base is an older commit.
    let base = w.git.sha_of_branch(w.repo, "main");
    let head = w.git.sha_of_branch(w.repo, "a");
    w.git.set_compare(
        w.repo,
        &base,
        &head,
        Compare {
            base_sha: base.clone(),
            head_sha: head.clone(),
            merge_base: "e".repeat(40),
            ahead_by: 1,
            behind_by: 1,
            commits: vec![],
            diff: Diff { files: vec![] },
        },
    );
    let (a, b, c) =
        tokio::join!(w.core.queue_advance(w.repo, "main"), w.core.queue_advance(w.repo, "main"), w.core.queue_advance(w.repo, "main"));
    a.unwrap();
    b.unwrap();
    c.unwrap();
    assert!(queued(&w, n).await, "a good pull request stays queued: {:?}", events(&w, n).await);
    let merges = w.git.merges();
    assert_eq!(merges.len(), 1, "the branch is brought up to date once");
    assert!(merges[0].update_branch);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn only_an_admin_turns_the_queue_on_and_turning_it_off_empties_it(pool: PgPool) {
    let w = World::new(pool).await;
    w.protect("main", 1, &[]).await;
    assert!(!w.core.merge_queue_on(w.repo).await.unwrap());
    assert!(matches!(w.core.set_merge_queue(&w.bob_a(), w.repo, true, &w.c()).await, Err(Error::Forbidden(_))), "write is not admin");
    // Armed for auto-merge before the queue existed: it must not slip past it.
    let armed = pr(&w, "armed", "Armed").await;
    w.core.pull_set_auto_merge(&w.bob_a(), w.repo, armed, Some(MergeMethod::Merge), &w.c()).await.unwrap();
    w.core.set_merge_queue(&w.alice_a(), w.repo, true, &w.c()).await.unwrap();
    assert!(w.core.pull_view(&w.alice_a(), w.repo, armed).await.unwrap().auto_merge.is_none(), "turning the queue on disarms auto-merge");
    let n = pr(&w, "a", "A").await;
    w.core.queue_add(&w.bob_a(), w.repo, n, MergeMethod::Merge, &w.c()).await.unwrap();
    assert!(queued(&w, n).await);
    w.core.set_merge_queue(&w.alice_a(), w.repo, false, &w.c()).await.unwrap();
    assert!(!queued(&w, n).await);
    assert!(events(&w, n).await.iter().any(|(k, d)| k == "merge_queue_removed" && d.contains("turned off")));
    assert!(w.audit_actions().await.contains(&"repo.merge_queue_changed".to_string()));
}
