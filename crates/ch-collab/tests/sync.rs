#![allow(clippy::unwrap_used)]

mod common;

use ch_collab::sync;
use ch_core::collab::*;
use ch_core::events::DomainEvent;
use ch_core::model::*;
use ch_core::{Error, UserId};
use common::*;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

const ZERO: &str = "0000000000000000000000000000000000000000";

async fn push(w: &World, branch: &str, before: &str, after: &str, pusher: Option<UserId>) {
    push_built(w, branch, before, after, pusher, false).await
}

async fn push_built(w: &World, branch: &str, before: &str, after: &str, pusher: Option<UserId>, server_built: bool) {
    let mut tx = w.pool.begin().await.unwrap();
    w.core
        .d
        .events
        .emit(
            &mut tx,
            DomainEvent::Push {
                repo_id: w.repo,
                git_ref: format!("refs/heads/{branch}"),
                before: before.into(),
                after: after.into(),
                pusher,
                forced: false,
                app: None,
                server_built,
            },
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

fn comment(line: u32) -> NewReviewComment {
    NewReviewComment {
        path: "src/lib.rs".into(),
        side: "RIGHT".into(),
        line,
        start_line: None,
        body: format!("about line {line}"),
        in_reply_to: None,
    }
}

fn diff_between_heads() -> Compare {
    // Old line 1 stays put one line lower, line 2 moves with it, line 3 is rewritten.
    let lines = vec![
        line(LineKind::Added, None, Some(1), "// new header"),
        line(LineKind::Context, Some(1), Some(2), "fn a() {}"),
        line(LineKind::Context, Some(2), Some(3), "fn b2() {}"),
        line(LineKind::Removed, Some(3), None, "fn c() {}"),
        line(LineKind::Added, None, Some(4), "fn c2() {}"),
    ];
    Compare {
        base_sha: sha_of(2),
        head_sha: sha_of(20),
        merge_base: sha_of(2),
        ahead_by: 1,
        behind_by: 0,
        commits: vec![commit_info(&sha_of(20), "more")],
        diff: Diff { files: vec![file_diff("src/lib.rs", lines)] },
    }
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_push_to_the_head_branch_updates_the_pull_request(pool: PgPool) {
    let w = World::new(pool).await;
    let n = w.pull(w.carol, "x", "").await.pull.issue.number;
    let keep = w.core.review_add_comment(&w.bob_a(), w.repo, n, comment(2), None, &w.c()).await.unwrap();
    let gone = w.core.review_add_comment(&w.bob_a(), w.repo, n, comment(3), None, &w.c()).await.unwrap();

    w.git.branch(w.repo, "feature", &sha_of(20));
    w.git.set_compare(w.repo, &sha_of(2), &sha_of(20), diff_between_heads());
    push(&w, "feature", &sha_of(2), &sha_of(20), Some(w.carol)).await;
    assert!(sync::run_pr_sync_once(&w.core).await.unwrap() >= 1);

    let v = w.core.pull_view(&w.carol_a(), w.repo, n).await.unwrap();
    assert_eq!(v.pull.head_sha, sha_of(20));
    assert!(w.events().await.contains(&"pr_synchronized".to_string()));
    let kinds: Vec<String> = w.core.timeline(&w.carol_a(), w.repo, n).await.unwrap().into_iter().map(|e| e.kind).collect();
    assert!(kinds.contains(&"head_ref_updated".to_string()));
    assert!(
        w.git.created_refs().iter().any(|(_, name, sha)| name == "refs/pull/1/head" && *sha == sha_of(20)),
        "the pull ref follows the branch"
    );

    let comments: Vec<ReviewCommentView> =
        w.core.review_threads(&w.carol_a(), w.repo, n).await.unwrap().into_iter().flat_map(|t| t.comments).collect();
    let keep = comments.iter().find(|c| c.comment.id == keep.comment.id).unwrap();
    let gone = comments.iter().find(|c| c.comment.id == gone.comment.id).unwrap();
    assert_eq!(
        (keep.comment.line, keep.comment.original_line, keep.comment.outdated),
        (Some(3), Some(2), false),
        "the line moved down with the code"
    );
    assert_eq!(keep.comment.commit_sha, sha_of(20));
    assert!(gone.comment.outdated, "the line it was about was rewritten");
    assert_eq!(gone.comment.original_line, Some(3));
    assert_eq!(gone.comment.commit_sha, sha_of(2), "an outdated comment keeps the commit it was made on");
    assert!(!gone.comment.diff_hunk.is_empty());

    // Handling the same event again changes nothing.
    let before = w.events().await.len();
    push(&w, "feature", &sha_of(2), &sha_of(20), Some(w.carol)).await;
    sync::run_pr_sync_once(&w.core).await.unwrap();
    assert_eq!(w.events().await.len(), before + 1, "only the push itself, no second synchronize");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn pushes_elsewhere_leave_pull_requests_alone(pool: PgPool) {
    let w = World::new(pool).await;
    let n = w.pull(w.carol, "x", "").await.pull.issue.number;
    w.git.branch(w.repo, "unrelated", &sha_of(40));
    push(&w, "unrelated", ZERO, &sha_of(40), Some(w.bob)).await;
    // Tags and deletions are not branch pushes either.
    let mut tx = w.pool.begin().await.unwrap();
    w.core
        .d
        .events
        .emit(
            &mut tx,
            DomainEvent::Push {
                repo_id: w.repo,
                git_ref: "refs/tags/v1".into(),
                before: ZERO.into(),
                after: sha_of(41),
                pusher: None,
                forced: false,
                app: None,
                server_built: false,
            },
        )
        .await
        .unwrap();
    w.core
        .d
        .events
        .emit(
            &mut tx,
            DomainEvent::Push {
                repo_id: w.repo,
                git_ref: "refs/heads/feature".into(),
                before: sha_of(2),
                after: ZERO.into(),
                pusher: None,
                forced: false,
                app: None,
                server_built: false,
            },
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    sync::run_pr_sync_once(&w.core).await.unwrap();
    let v = w.core.pull_view(&w.carol_a(), w.repo, n).await.unwrap();
    assert_eq!(v.pull.head_sha, sha_of(2));
    assert!(v.pull.issue.open);
    assert_eq!(w.count_events("pr_synchronized").await, 0);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn landing_the_branch_outside_the_ui_marks_it_merged(pool: PgPool) {
    let w = World::new(pool).await;
    w.issue(w.alice, "A bug", "").await;
    let merged_one = w.pull_from(w.carol, "feature", "Landed by hand", "Fixes #1").await.pull.issue.number;
    let other = w.pull_from(w.carol, "other", "Still open", "").await.pull.issue.number;

    // Someone pushes main so that it contains the whole feature branch.
    w.git.branch(w.repo, "main", &sha_of(30));
    w.git.set_compare(
        w.repo,
        &sha_of(30),
        &sha_of(2),
        Compare {
            base_sha: sha_of(30),
            head_sha: sha_of(2),
            merge_base: sha_of(2),
            ahead_by: 0,
            behind_by: 5,
            commits: vec![],
            diff: Diff { files: vec![] },
        },
    );
    push(&w, "main", &sha_of(1), &sha_of(30), Some(w.bob)).await;
    sync::run_pr_sync_once(&w.core).await.unwrap();

    let v = w.core.pull_view(&w.carol_a(), w.repo, merged_one).await.unwrap();
    assert!(v.pull.merged_at.is_some() && !v.pull.issue.open);
    assert_eq!(v.merged_by.unwrap().username, "bob");
    assert_eq!(v.pull.merge_commit_sha.as_deref(), Some(sha_of(30).as_str()));
    let kinds: Vec<_> =
        w.core.timeline(&w.carol_a(), w.repo, merged_one).await.unwrap().into_iter().filter(|e| e.kind == "merged").collect();
    assert_eq!(kinds[0].data["via"], "push");
    assert!(w.events().await.contains(&"pr_merged".to_string()));
    assert!(!w.core.issue_view(&w.carol_a(), w.repo, 1).await.unwrap().issue.open, "its closing keyword still counts");

    let o = w.core.pull_view(&w.carol_a(), w.repo, other).await.unwrap();
    assert!(o.pull.issue.open && o.pull.merged_at.is_none());
    assert_eq!(o.pull.base_sha, sha_of(30), "the other pull request follows the new base");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn keywords_in_pushed_commits_close_issues_on_the_default_branch(pool: PgPool) {
    let w = World::new(pool).await;
    for t in ["one", "two", "three"] {
        w.issue(w.carol, t, "").await;
    }
    let cmp = |msg: &str| Compare {
        base_sha: sha_of(1),
        head_sha: sha_of(31),
        merge_base: sha_of(1),
        ahead_by: 1,
        behind_by: 0,
        commits: vec![commit_info(&sha_of(31), msg)],
        diff: Diff { files: vec![] },
    };
    // On another branch nothing happens.
    w.git.branch(w.repo, "topic", &sha_of(31));
    w.git.set_compare(w.repo, &sha_of(1), &sha_of(31), cmp("Fixes #1, closes #2 and resolves alice/demo#3"));
    push(&w, "topic", &sha_of(1), &sha_of(31), Some(w.bob)).await;
    sync::run_pr_sync_once(&w.core).await.unwrap();
    assert!(w.core.issue_views(&w.alice_a(), w.repo, IssueFilter { open: Some(true), ..Default::default() }).await.unwrap().len() == 3);
    // On the default branch the keywords apply, and the pusher is the one who closed them.
    push(&w, "main", &sha_of(1), &sha_of(31), Some(w.bob)).await;
    sync::run_pr_sync_once(&w.core).await.unwrap();
    let closed = w.core.issue_views(&w.alice_a(), w.repo, IssueFilter { open: Some(false), ..Default::default() }).await.unwrap();
    assert_eq!(closed.len(), 3);
    assert!(closed.iter().all(|v| v.closed_by.as_ref().unwrap().username == "bob"));
    let t = w.core.timeline(&w.alice_a(), w.repo, 1).await.unwrap();
    assert_eq!(t.iter().find(|e| e.kind == "closed").unwrap().data["via"]["commit"], sha_of(31));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_keyword_cannot_close_an_issue_the_pusher_may_not_touch(pool: PgPool) {
    let w = World::new(pool).await;
    // A private repository where bob has no access holds issue 1.
    let secret = w.env.repo(w.eve, "secret").await;
    w.core
        .issue_create(
            &w.eve_a(),
            NewIssue {
                repo_id: secret,
                title: "Hidden".into(),
                body: String::new(),
                labels: vec![],
                assignees: vec![],
                milestone_id: None,
            },
            &w.c(),
        )
        .await
        .unwrap();
    w.git.set_compare(
        w.repo,
        &sha_of(1),
        &sha_of(31),
        Compare {
            base_sha: sha_of(1),
            head_sha: sha_of(31),
            merge_base: sha_of(1),
            ahead_by: 1,
            behind_by: 0,
            commits: vec![commit_info(&sha_of(31), "Closes eve/secret#1")],
            diff: Diff { files: vec![] },
        },
    );
    push(&w, "main", &sha_of(1), &sha_of(31), Some(w.bob)).await;
    sync::run_pr_sync_once(&w.core).await.unwrap();
    assert!(w.core.issue_view(&w.eve_a(), secret, 1).await.unwrap().issue.open);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_cursor_moves_and_a_second_pass_finds_nothing(pool: PgPool) {
    let w = World::new(pool).await;
    w.pull(w.carol, "x", "").await;
    assert_eq!(sync::run_pr_sync_once(&w.core).await.unwrap(), 1);
    assert_eq!(sync::run_pr_sync_once(&w.core).await.unwrap(), 0);
    w.issue(w.carol, "later", "").await;
    assert_eq!(sync::run_pr_sync_once(&w.core).await.unwrap(), 1);
    let last: i64 = sqlx::query_scalar("SELECT last_id FROM event_cursors WHERE consumer = 'pr-sync'").fetch_one(&w.pool).await.unwrap();
    let newest: i64 = sqlx::query_scalar("SELECT max(id) FROM outbox_events").fetch_one(&w.pool).await.unwrap();
    assert_eq!(last, newest);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn an_event_that_keeps_failing_does_not_block_the_ones_behind_it(pool: PgPool) {
    let w = World::new(pool).await;
    w.issue(w.carol, "a", "").await;
    w.issue(w.carol, "b", "").await;
    fn always_fails<'a>(
        _: &'a ch_collab::Core,
        e: &'a ch_core::events::StoredEvent,
    ) -> futures::future::BoxFuture<'a, ch_core::Result<()>> {
        let id = e.id;
        Box::pin(async move { if id % 2 == 0 { Err(Error::internal(anyhow::anyhow!("boom"))) } else { Ok(()) } })
    }
    let handled = sync::drain(&w.core, "flaky", always_fails, &CancellationToken::new()).await.unwrap();
    assert_eq!(handled, 2, "both events were passed over, the failing one after its retries");
    let last: i64 = sqlx::query_scalar("SELECT last_id FROM event_cursors WHERE consumer = 'flaky'").fetch_one(&w.pool).await.unwrap();
    assert_eq!(last, sqlx::query_scalar::<_, i64>("SELECT max(id) FROM outbox_events").fetch_one(&w.pool).await.unwrap());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_cancelled_consumer_stops_promptly(pool: PgPool) {
    let w = World::new(pool).await;
    let state = w.app_state().await;
    let workers = ch_collab::workers(&state);
    assert_eq!(workers.iter().map(|w| w.name).collect::<Vec<_>>(), vec!["collab-pr-sync", "collab-defaults"]);
    let token = CancellationToken::new();
    let handles: Vec<_> = workers.into_iter().map(|wk| wk.spawn(token.clone())).collect();
    w.pull(w.carol, "x", "").await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    token.cancel();
    for h in handles {
        tokio::time::timeout(std::time::Duration::from_secs(5), h).await.expect("worker stops within the time limit").unwrap();
    }
}

async fn last_payload(w: &World, kind: &str) -> serde_json::Value {
    sqlx::query_scalar("SELECT payload FROM outbox_events WHERE kind = $1 ORDER BY id DESC LIMIT 1")
        .bind(kind)
        .fetch_one(&w.pool)
        .await
        .unwrap()
}

/// ch-x12: the pusher on a push describes that push's own commit. When the branch moved on
/// before the event was handled, the head holds a later commit nobody can be named for.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_push_names_its_pusher_only_for_its_own_commit(pool: PgPool) {
    let w = World::new(pool).await;
    let n = w.pull(w.carol, "x", "").await.pull.issue.number;
    let start = w.git.sha_of_branch(w.repo, "feature");

    // Bob pushes B, and the branch still holds B when the event is handled.
    w.git.branch(w.repo, "feature", &sha_of(20));
    push(&w, "feature", &start, &sha_of(20), Some(w.bob)).await;
    sync::run_pr_sync_once(&w.core).await.unwrap();
    let p = last_payload(&w, "pr_synchronized").await;
    assert_eq!(
        (p["head_sha"].as_str(), p["pusher"].as_i64(), p["origin_known"].as_bool()),
        (Some(&*sha_of(20)), Some(w.bob.0), Some(true))
    );

    // Bob pushes C, then someone else pushes D before the first event is handled.
    w.git.branch(w.repo, "feature", &sha_of(22));
    push(&w, "feature", &sha_of(20), &sha_of(21), Some(w.bob)).await;
    push(&w, "feature", &sha_of(21), &sha_of(22), Some(w.carol)).await;
    sync::run_pr_sync_once(&w.core).await.unwrap();
    let synced: Vec<serde_json::Value> = sqlx::query_scalar("SELECT payload FROM outbox_events WHERE kind = 'pr_synchronized' ORDER BY id")
        .fetch_all(&w.pool)
        .await
        .unwrap();
    assert_eq!(synced.len(), 2, "one synchronize for the head that moved: {synced:?}");
    let p = &synced[1];
    assert_eq!(p["head_sha"].as_str(), Some(&*sha_of(22)));
    assert!(p["pusher"].is_null(), "Bob did not push {}: {p}", sha_of(22));
    assert_eq!(p["origin_known"].as_bool(), Some(false));
    assert_eq!(w.core.pull_view(&w.carol_a(), w.repo, n).await.unwrap().pull.head_sha, sha_of(22));
}

/// ch-x12: a commit the server wrote on top of the branch for someone (a web edit, the contents
/// API) holds what the branch held before. The synchronize says which commit that was, so CI
/// counts the person only if that commit was itself let run. A plain push names nothing.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_commit_the_server_built_names_the_commit_under_it(pool: PgPool) {
    let w = World::new(pool).await;
    w.pull(w.carol, "x", "").await;
    let start = w.git.sha_of_branch(w.repo, "feature");
    w.git.branch(w.repo, "feature", &sha_of(30));
    push(&w, "feature", &start, &sha_of(30), Some(w.bob)).await;
    sync::run_pr_sync_once(&w.core).await.unwrap();
    let p = last_payload(&w, "pr_synchronized").await;
    assert!(p["built_on"].is_null(), "a push names nothing under it: {p}");

    w.git.branch(w.repo, "feature", &sha_of(31));
    push_built(&w, "feature", &sha_of(30), &sha_of(31), Some(w.bob), true).await;
    sync::run_pr_sync_once(&w.core).await.unwrap();
    let p = last_payload(&w, "pr_synchronized").await;
    assert_eq!(
        (p["head_sha"].as_str(), p["pusher"].as_i64(), p["built_on"].as_str()),
        (Some(&*sha_of(31)), Some(w.bob.0), Some(&*sha_of(30))),
        "{p}"
    );
}

/// ch-hyd: another refresh (here a push to the base) sees the new head first. The branch
/// tracker still announces it once, so the push gets its CI run.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_head_seen_first_by_another_refresh_is_still_announced_once(pool: PgPool) {
    let w = World::new(pool).await;
    let n = w.pull(w.carol, "x", "").await.pull.issue.number;
    sync::run_pr_sync_once(&w.core).await.unwrap();
    let before = w.core.pull_view(&w.carol_a(), w.repo, n).await.unwrap().pull.head_sha;
    w.git.branch(w.repo, "feature", &sha_of(20));
    w.git.set_compare(w.repo, &sha_of(2), &sha_of(20), diff_between_heads());
    // A push to the base is handled first, and its refresh already moves the head.
    push(&w, "main", &sha_of(2), &sha_of(2), Some(w.bob)).await;
    push(&w, "feature", &sha_of(2), &sha_of(20), Some(w.carol)).await;
    sync::run_pr_sync_once(&w.core).await.unwrap();
    assert_eq!(w.count_events("pr_synchronized").await, 1, "announced although another refresh moved the head first");
    assert_ne!(before, sha_of(20));
    // The same push handled again announces nothing new.
    push(&w, "feature", &sha_of(2), &sha_of(20), Some(w.carol)).await;
    sync::run_pr_sync_once(&w.core).await.unwrap();
    assert_eq!(w.count_events("pr_synchronized").await, 1);
}
