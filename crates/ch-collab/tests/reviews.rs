#![allow(clippy::unwrap_used)]

mod common;

use ch_core::collab::*;
use ch_core::model::*;
use ch_core::{CommentId, Error};
use common::*;
use sqlx::PgPool;

fn comment(path: &str, side: &str, line: u32, body: &str) -> NewReviewComment {
    NewReviewComment { path: path.into(), side: side.into(), line, start_line: None, body: body.into(), in_reply_to: None }
}

fn is_validation<T>(r: &Result<T, Error>) -> bool {
    matches!(r, Err(Error::Validation(_)))
}

async fn open(w: &World) -> i64 {
    w.pull(w.carol, "Change", "").await.pull.issue.number
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn reviews_record_decisions_and_announce_them(pool: PgPool) {
    let w = World::new(pool).await;
    let n = open(&w).await;
    let commented = w.core.review_submit(&w.bob_a(), w.repo, n, ReviewState::Commented, "Nice start", vec![], &w.c()).await.unwrap();
    assert_eq!(commented.state, ReviewState::Commented);
    assert_eq!(commented.commit_sha, sha_of(2));
    assert!(commented.submitted_at.is_some());
    let approved = w.core.review_submit(&w.carol_a(), w.repo, n, ReviewState::Approved, "", vec![], &w.c()).await;
    assert!(is_validation(&approved), "the author cannot approve");
    // Anyone who can read may approve. It just does not count toward a required approval.
    assert!(w.core.review_submit(&w.dave_a(), w.repo, n, ReviewState::Approved, "", vec![], &w.c()).await.is_ok());
    assert!(
        is_validation(&w.core.review_submit(&w.bob_a(), w.repo, n, ReviewState::Commented, "  ", vec![], &w.c()).await),
        "an empty comment review says nothing"
    );
    assert!(
        is_validation(&w.core.review_submit(&w.bob_a(), w.repo, n, ReviewState::ChangesRequested, "", vec![], &w.c()).await),
        "a change request needs a reason"
    );
    assert!(is_validation(&w.core.review_submit(&w.bob_a(), w.repo, n, ReviewState::Dismissed, "x", vec![], &w.c()).await));
    assert!(matches!(
        w.core.review_submit(&w.eve_a(), w.repo, n, ReviewState::Commented, "hi", vec![], &w.c()).await,
        Err(Error::NotFound)
    ));

    let all = w.core.review_list(&w.carol_a(), w.repo, n).await.unwrap();
    assert_eq!(all.iter().map(|r| r.state).collect::<Vec<_>>(), vec![ReviewState::Commented, ReviewState::Approved]);
    assert_eq!(w.count_events("review_submitted").await, 2);
    // Reviews on a closed pull request can still comment, but not approve.
    w.core.pull_close(&w.carol_a(), w.repo, n, &w.c()).await.unwrap();
    assert!(w.core.review_submit(&w.bob_a(), w.repo, n, ReviewState::Commented, "late thought", vec![], &w.c()).await.is_ok());
    assert!(is_validation(&w.core.review_submit(&w.bob_a(), w.repo, n, ReviewState::Approved, "", vec![], &w.c()).await));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn inline_comments_must_sit_on_the_diff(pool: PgPool) {
    let w = World::new(pool).await;
    let n = open(&w).await;
    let add = |c: NewReviewComment| {
        let w = &w;
        async move { w.core.review_add_comment(&w.bob_a(), w.repo, n, c, None, &w.c()).await }
    };
    let ok = add(comment("src/lib.rs", "RIGHT", 2, "why b2?")).await.unwrap();
    assert_eq!(ok.comment.side, "right");
    assert_eq!((ok.comment.line, ok.comment.original_line), (Some(2), Some(2)));
    assert_eq!(ok.comment.commit_sha, sha_of(2));
    assert_eq!(ok.comment.diff_hunk, "@@ -1,3 +1,4 @@\n fn a() {}\n-fn b() {}\n+fn b2() {}");
    assert!(ok.comment.thread_id.is_some() && !ok.comment.pending && !ok.comment.outdated);

    let left = add(comment("src/lib.rs", "LEFT", 2, "this was fine")).await.unwrap();
    assert_eq!(left.comment.diff_hunk, "@@ -1,3 +1,4 @@\n fn a() {}\n-fn b() {}");
    assert!(is_validation(&add(comment("src/lib.rs", "RIGHT", 99, "x")).await), "line outside the diff");
    assert!(is_validation(&add(comment("src/other.rs", "RIGHT", 1, "x")).await), "file not in the pull request");
    assert!(is_validation(&add(comment("src/lib.rs", "RIGHT", 0, "x")).await));
    assert!(is_validation(&add(comment("src/lib.rs", "SIDEWAYS", 2, "x")).await));
    assert!(is_validation(&add(comment("src/lib.rs", "RIGHT", 2, "   ")).await));
    let mut range = comment("src/lib.rs", "RIGHT", 3, "these two");
    range.start_line = Some(2);
    assert_eq!(add(range.clone()).await.unwrap().comment.start_line, Some(2));
    range.start_line = Some(50);
    assert!(is_validation(&add(range.clone()).await));
    range.start_line = Some(4);
    assert!(is_validation(&add(range).await), "a range runs forward");
    assert!(matches!(
        w.core.review_add_comment(&w.eve_a(), w.repo, n, comment("src/lib.rs", "RIGHT", 2, "x"), None, &w.c()).await,
        Err(Error::NotFound)
    ));
    assert_eq!(w.count_events("review_comment_created").await, 3);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn replies_join_the_thread_and_deletes_clean_up(pool: PgPool) {
    let w = World::new(pool).await;
    let frank = w.env.user("frank").await;
    w.env.collaborator(w.repo, frank, "read").await;
    let n = open(&w).await;
    let root = w.core.review_add_comment(&w.bob_a(), w.repo, n, comment("src/lib.rs", "RIGHT", 2, "why?"), None, &w.c()).await.unwrap();
    let reply = NewReviewComment { in_reply_to: Some(root.comment.id), ..comment("ignored", "RIGHT", 1, "because") };
    let r1 = w.core.review_add_comment(&w.carol_a(), w.repo, n, reply, None, &w.c()).await.unwrap();
    assert_eq!(r1.comment.thread_id, root.comment.thread_id);
    assert_eq!((r1.comment.path.as_str(), r1.comment.line), ("src/lib.rs", Some(2)));
    // Replying to a reply lands in the same thread.
    let r2 = w
        .core
        .review_add_comment(
            &w.bob_a(),
            w.repo,
            n,
            NewReviewComment { in_reply_to: Some(r1.comment.id), ..comment("", "RIGHT", 1, "ok thanks") },
            None,
            &w.c(),
        )
        .await
        .unwrap();
    assert_eq!(r2.comment.in_reply_to, Some(root.comment.id));
    let threads = w.core.review_threads(&w.carol_a(), w.repo, n).await.unwrap();
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0].comments.len(), 3);
    assert!(!threads[0].resolved);
    let bad = w
        .core
        .review_add_comment(
            &w.bob_a(),
            w.repo,
            n,
            NewReviewComment { in_reply_to: Some(CommentId(999_999)), ..comment("", "RIGHT", 1, "x") },
            None,
            &w.c(),
        )
        .await;
    assert!(is_validation(&bad));

    // Edits belong to the author.
    assert_eq!(
        w.core.review_edit_comment(&w.carol_a(), w.repo, r1.comment.id, "because reasons", &w.c()).await.unwrap().comment.body,
        "because reasons"
    );
    assert!(matches!(w.core.review_edit_comment(&w.bob_a(), w.repo, r1.comment.id, "x", &w.c()).await, Err(Error::Forbidden(_))));
    assert!(matches!(w.core.review_delete_comment(&w.a(frank), w.repo, r1.comment.id, &w.c()).await, Err(Error::Forbidden(_))));
    // Deleting the first comment leaves the conversation standing.
    w.core.review_delete_comment(&w.bob_a(), w.repo, root.comment.id, &w.c()).await.unwrap();
    assert_eq!(w.core.review_threads(&w.carol_a(), w.repo, n).await.unwrap()[0].comments.len(), 2);
    w.core.review_delete_comment(&w.carol_a(), w.repo, r1.comment.id, &w.c()).await.unwrap();
    w.core.review_delete_comment(&w.alice_a(), w.repo, r2.comment.id, &w.c()).await.unwrap();
    assert!(w.core.review_threads(&w.carol_a(), w.repo, n).await.unwrap().is_empty());
    assert_eq!(w.env.count("review_threads").await, 0);
    assert!(w.audit_actions().await.contains(&"comment.deleted".to_string()), "alice removed someone else's words");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_pending_review_is_private_until_it_is_submitted(pool: PgPool) {
    let w = World::new(pool).await;
    let n = open(&w).await;
    let pending = w
        .core
        .review_submit(
            &w.bob_a(),
            w.repo,
            n,
            ReviewState::Pending,
            "",
            vec![comment("src/lib.rs", "RIGHT", 2, "first"), comment("src/lib.rs", "RIGHT", 3, "second")],
            &w.c(),
        )
        .await
        .unwrap();
    assert_eq!(pending.state, ReviewState::Pending);
    assert!(pending.submitted_at.is_none());
    let late = w
        .core
        .review_add_comment(&w.bob_a(), w.repo, n, comment("src/lib.rs", "LEFT", 2, "third"), Some(pending.id), &w.c())
        .await
        .unwrap();
    assert!(late.comment.pending);

    // Bob sees his draft. Nobody else knows it exists.
    let mine = w.core.review_threads(&w.bob_a(), w.repo, n).await.unwrap();
    assert_eq!(mine.iter().map(|t| t.comments.len()).sum::<usize>(), 3);
    assert!(w.core.review_threads(&w.carol_a(), w.repo, n).await.unwrap().is_empty());
    assert!(w.core.review_list(&w.carol_a(), w.repo, n).await.unwrap().is_empty());
    assert_eq!(w.core.review_list(&w.bob_a(), w.repo, n).await.unwrap().len(), 1);
    assert!(matches!(w.core.review_comment(&w.carol_a(), w.repo, late.comment.id).await, Err(Error::NotFound)));
    assert_eq!(w.core.pull_view(&w.carol_a(), w.repo, n).await.unwrap().review_comments, 0);
    assert_eq!(w.count_events("review_submitted").await + w.count_events("review_comment_created").await, 0);
    // Only the author can add to it.
    let dave_try =
        w.core.review_add_comment(&w.dave_a(), w.repo, n, comment("src/lib.rs", "RIGHT", 2, "x"), Some(pending.id), &w.c()).await;
    assert!(matches!(dave_try, Err(Error::NotFound)));
    assert!(matches!(
        w.core.review_submit_pending(&w.dave_a(), w.repo, n, pending.id, ReviewState::Commented, "x", &w.c()).await,
        Err(Error::NotFound)
    ));

    let done = w
        .core
        .review_submit_pending(&w.bob_a(), w.repo, n, pending.id, ReviewState::ChangesRequested, "a few things", &w.c())
        .await
        .unwrap();
    assert_eq!(done.state, ReviewState::ChangesRequested);
    assert_eq!(w.core.review_threads(&w.carol_a(), w.repo, n).await.unwrap().iter().map(|t| t.comments.len()).sum::<usize>(), 3);
    assert_eq!(w.count_events("review_submitted").await, 1);
    assert_eq!(w.count_events("review_comment_created").await, 3);
    assert_eq!(w.core.pull_view(&w.carol_a(), w.repo, n).await.unwrap().review_comments, 3);
    // A submitted review cannot be submitted twice.
    assert!(matches!(
        w.core.review_submit_pending(&w.bob_a(), w.repo, n, pending.id, ReviewState::Approved, "", &w.c()).await,
        Err(Error::NotFound)
    ));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn submitting_finishes_a_pending_review_instead_of_starting_another(pool: PgPool) {
    let w = World::new(pool).await;
    let n = open(&w).await;
    w.core
        .review_submit(&w.bob_a(), w.repo, n, ReviewState::Pending, "", vec![comment("src/lib.rs", "RIGHT", 2, "draft thought")], &w.c())
        .await
        .unwrap();
    let r = w.core.review_submit(&w.bob_a(), w.repo, n, ReviewState::Commented, "", vec![], &w.c()).await.unwrap();
    assert_eq!(r.state, ReviewState::Commented, "the draft's comment is what makes this review non-empty");
    assert_eq!(w.env.count("reviews").await, 1);
    assert_eq!(w.core.review_threads(&w.carol_a(), w.repo, n).await.unwrap().len(), 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_discarded_pending_review_leaves_nothing(pool: PgPool) {
    let w = World::new(pool).await;
    let n = open(&w).await;
    let p = w
        .core
        .review_submit(&w.dave_a(), w.repo, n, ReviewState::Pending, "", vec![comment("src/lib.rs", "RIGHT", 2, "maybe")], &w.c())
        .await
        .unwrap();
    assert!(matches!(w.core.review_delete_pending(&w.bob_a(), w.repo, n, p.id, &w.c()).await, Err(Error::NotFound)));
    w.core.review_delete_pending(&w.dave_a(), w.repo, n, p.id, &w.c()).await.unwrap();
    assert_eq!((w.env.count("reviews").await, w.env.count("review_comments").await, w.env.count("review_threads").await), (0, 0, 0));
}

const FILE: &str = "fn a() {}\nfn b2() {}\nfn c() {}\nfn d() {}\n";

fn suggestion(lines: &[&str]) -> String {
    format!("Maybe rename:\n```suggestion\n{}\n```", lines.join("\n"))
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn suggestions_become_commits_on_the_head_branch(pool: PgPool) {
    let w = World::new(pool).await;
    w.git.put_file(w.repo, "src/lib.rs", FILE);
    let n = open(&w).await;
    let c = w
        .core
        .review_add_comment(&w.bob_a(), w.repo, n, comment("src/lib.rs", "RIGHT", 2, &suggestion(&["fn better() {}"])), None, &w.c())
        .await
        .unwrap();
    assert!(c.has_suggestion);
    // The author has only read access, so the commit is not hers to make.
    assert!(matches!(w.core.review_apply_suggestion(&w.carol_a(), w.repo, c.comment.id, &w.c()).await, Err(Error::Forbidden(_))));
    let new_sha = w.core.review_apply_suggestion(&w.alice_a(), w.repo, c.comment.id, &w.c()).await.unwrap();

    let change = &w.git.file_changes()[0];
    assert_eq!(change.branch, "refs/heads/feature");
    assert_eq!(change.expected_sha.as_deref(), Some(sha_of(2).as_str()));
    assert_eq!(String::from_utf8(change.changes[0].content.clone().unwrap()).unwrap(), "fn a() {}\nfn better() {}\nfn c() {}\nfn d() {}\n");
    assert!(change.message.starts_with("Apply suggestion from @bob"));
    assert!(change.message.contains(&format!("Co-authored-by: bob <{}+bob@users.noreply.test.example>", w.bob.0)), "{}", change.message);
    let v = w.core.pull_view(&w.carol_a(), w.repo, n).await.unwrap();
    assert_eq!(v.pull.head_sha, new_sha);
    let threads = w.core.review_threads(&w.carol_a(), w.repo, n).await.unwrap();
    assert!(threads[0].resolved && threads[0].outdated);
    assert!(w.events().await.contains(&"pr_synchronized".to_string()));
    // ch-x12: the new commit holds the old head's code, so CI is told what it was built on.
    let p: serde_json::Value =
        sqlx::query_scalar("SELECT payload FROM outbox_events WHERE kind = 'pr_synchronized' ORDER BY id DESC LIMIT 1")
            .fetch_one(&w.pool)
            .await
            .unwrap();
    assert_eq!((p["head_sha"].as_str(), p["built_on"].as_str()), (Some(new_sha.as_str()), Some(&*sha_of(2))), "{p}");
    // The same suggestion cannot be applied twice.
    assert!(matches!(w.core.review_apply_suggestion(&w.alice_a(), w.repo, c.comment.id, &w.c()).await, Err(Error::Conflict(_))));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn suggestions_can_replace_ranges_and_delete_lines(pool: PgPool) {
    let w = World::new(pool).await;
    w.git.put_file(w.repo, "src/lib.rs", FILE);
    let n = open(&w).await;
    let mut ranged = comment("src/lib.rs", "RIGHT", 3, &suggestion(&["fn one() {}", "fn two() {}"]));
    ranged.start_line = Some(2);
    let c = w.core.review_add_comment(&w.bob_a(), w.repo, n, ranged, None, &w.c()).await.unwrap();
    w.core.review_apply_suggestion(&w.alice_a(), w.repo, c.comment.id, &w.c()).await.unwrap();
    assert_eq!(
        String::from_utf8(w.git.file_changes()[0].changes[0].content.clone().unwrap()).unwrap(),
        "fn a() {}\nfn one() {}\nfn two() {}\nfn d() {}\n"
    );

    let n2 = w.pull_from(w.carol, "other", "Another", "").await.pull.issue.number;
    let del = w
        .core
        .review_add_comment(&w.bob_a(), w.repo, n2, comment("src/lib.rs", "RIGHT", 3, "```suggestion\n```"), None, &w.c())
        .await
        .unwrap();
    w.core.review_apply_suggestion(&w.alice_a(), w.repo, del.comment.id, &w.c()).await.unwrap();
    assert_eq!(
        String::from_utf8(w.git.file_changes()[1].changes[0].content.clone().unwrap()).unwrap(),
        "fn a() {}\nfn b2() {}\nfn d() {}\n"
    );
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn only_real_suggestions_on_live_comments_apply(pool: PgPool) {
    let w = World::new(pool).await;
    w.git.put_file(w.repo, "src/lib.rs", FILE);
    let n = open(&w).await;
    let plain =
        w.core.review_add_comment(&w.bob_a(), w.repo, n, comment("src/lib.rs", "RIGHT", 2, "just words"), None, &w.c()).await.unwrap();
    assert!(!plain.has_suggestion);
    assert!(is_validation(&w.core.review_apply_suggestion(&w.alice_a(), w.repo, plain.comment.id, &w.c()).await));
    let on_left = w
        .core
        .review_add_comment(&w.bob_a(), w.repo, n, comment("src/lib.rs", "LEFT", 2, &suggestion(&["x"])), None, &w.c())
        .await
        .unwrap();
    assert!(!on_left.has_suggestion, "a suggestion on removed lines has nothing to replace");
    // A pending comment is not for anyone else to apply, not even for its author.
    let p = w
        .core
        .review_submit(
            &w.bob_a(),
            w.repo,
            n,
            ReviewState::Pending,
            "",
            vec![comment("src/lib.rs", "RIGHT", 2, &suggestion(&["y"]))],
            &w.c(),
        )
        .await
        .unwrap();
    let pending_comment = w
        .core
        .review_threads(&w.bob_a(), w.repo, n)
        .await
        .unwrap()
        .into_iter()
        .flat_map(|t| t.comments)
        .find(|c| c.comment.review_id == Some(p.id))
        .unwrap();
    assert!(is_validation(&w.core.review_apply_suggestion(&w.alice_a(), w.repo, pending_comment.comment.id, &w.c()).await));
    // Once the head moves under it, a suggestion is stale.
    let live = w
        .core
        .review_add_comment(&w.bob_a(), w.repo, n, comment("src/lib.rs", "RIGHT", 3, &suggestion(&["z"])), None, &w.c())
        .await
        .unwrap();
    sqlx::query("UPDATE pull_requests SET head_sha = $1").bind(sha_of(77)).execute(&w.pool).await.unwrap();
    assert!(matches!(w.core.review_apply_suggestion(&w.alice_a(), w.repo, live.comment.id, &w.c()).await, Err(Error::Conflict(_))));
    assert!(w.git.file_changes().is_empty());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn review_requests(pool: PgPool) {
    let w = World::new(pool).await;
    let frank = w.env.user("frank").await;
    w.env.collaborator(w.repo, frank, "read").await;
    let n = open(&w).await;
    w.core.review_request(&w.carol_a(), w.repo, n, vec![w.bob, w.dave], vec![], &w.c()).await.unwrap();
    let asked = w.core.review_requested(&w.carol_a(), w.repo, n).await.unwrap();
    assert_eq!(asked.users.iter().map(|u| u.username.as_str()).collect::<Vec<_>>(), vec!["bob", "dave"]);
    assert_eq!(w.count_events("review_requested").await, 2);
    // Asking twice does not announce twice.
    w.core.review_request(&w.carol_a(), w.repo, n, vec![w.bob], vec![], &w.c()).await.unwrap();
    assert_eq!(w.count_events("review_requested").await, 2);
    assert!(is_validation(&w.core.review_request(&w.carol_a(), w.repo, n, vec![w.carol], vec![], &w.c()).await), "not the author");
    assert!(
        is_validation(&w.core.review_request(&w.carol_a(), w.repo, n, vec![w.eve], vec![], &w.c()).await),
        "someone who cannot see the repository"
    );
    assert!(matches!(w.core.review_request(&w.a(frank), w.repo, n, vec![w.bob], vec![], &w.c()).await, Err(Error::Forbidden(_))));
    assert!(matches!(w.core.review_remove_request(&w.a(frank), w.repo, n, vec![w.bob], vec![], &w.c()).await, Err(Error::Forbidden(_))));

    let team = w.team("acme", "docs").await;
    w.core.review_request(&w.dave_a(), w.repo, n, vec![], vec![team], &w.c()).await.unwrap();
    let asked = w.core.review_requested(&w.carol_a(), w.repo, n).await.unwrap();
    assert_eq!((asked.teams[0].org.as_str(), asked.teams[0].name.as_str()), ("acme", "docs"));
    assert!(is_validation(&w.core.review_request(&w.dave_a(), w.repo, n, vec![], vec![ch_core::TeamId(999_999)], &w.c()).await));

    w.core.review_remove_request(&w.carol_a(), w.repo, n, vec![w.bob], vec![team], &w.c()).await.unwrap();
    let asked = w.core.review_requested(&w.carol_a(), w.repo, n).await.unwrap();
    assert_eq!((asked.users.len(), asked.teams.len()), (1, 0));
    let kinds: Vec<String> = w.core.timeline(&w.carol_a(), w.repo, n).await.unwrap().into_iter().map(|e| e.kind).collect();
    assert_eq!(kinds.iter().filter(|k| *k == "review_request_removed").count(), 2);
    // A closed pull request takes no new requests.
    w.core.pull_close(&w.carol_a(), w.repo, n, &w.c()).await.unwrap();
    assert!(is_validation(&w.core.review_request(&w.carol_a(), w.repo, n, vec![w.bob], vec![], &w.c()).await));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn review_comments_take_reactions(pool: PgPool) {
    let w = World::new(pool).await;
    let n = open(&w).await;
    let c = w.core.review_add_comment(&w.bob_a(), w.repo, n, comment("src/lib.rs", "RIGHT", 2, "hm"), None, &w.c()).await.unwrap();
    w.core.react(&w.carol_a(), w.repo, ReactionTarget::ReviewComment(c.comment.id), "eyes", &w.c()).await.unwrap();
    let t = w.core.review_threads(&w.carol_a(), w.repo, n).await.unwrap();
    assert_eq!(t[0].comments[0].reactions.eyes, 1);
    // A comment id from another pull request's repository does not resolve.
    let other = w.env.repo(w.alice, "elsewhere").await;
    assert!(matches!(
        w.core.react(&w.alice_a(), other, ReactionTarget::ReviewComment(c.comment.id), "eyes", &w.c()).await,
        Err(Error::NotFound)
    ));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_trait_objects_agree_with_the_engine(pool: PgPool) {
    use ch_core::services::Reviews;
    let w = World::new(pool).await;
    let n = open(&w).await;
    let state = w.app_state().await;
    let r: &dyn Reviews = &*state.reviews;
    let review = r
        .submit(&w.bob_a(), w.repo, n, ReviewState::Commented, "hello", vec![comment("src/lib.rs", "RIGHT", 2, "inline")], &w.c())
        .await
        .unwrap();
    assert_eq!(r.list(&w.carol_a(), w.repo, n).await.unwrap()[0].id, review.id);
    let threads = r.threads(&w.carol_a(), w.repo, n).await.unwrap();
    r.resolve_thread(&w.carol_a(), w.repo, threads[0].id, true, &w.c()).await.unwrap();
    r.request_review(&w.carol_a(), w.repo, n, vec![w.dave], vec![], &w.c()).await.unwrap();
    assert_eq!(r.requested(&w.carol_a(), w.repo, n).await.unwrap().users.len(), 1);
    assert_eq!(r.views(&w.carol_a(), w.repo, n).await.unwrap()[0].comment_count, 1);
}

/// ch-yci: `author_association` says how the author relates to the repository, as GitHub does. A
/// member of the owning organization with read access is `MEMBER`, not `NONE`.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn author_association_follows_github(pool: PgPool) {
    let w = World::new(pool).await;
    assert_eq!(w.core.association(w.repo, w.alice, true).await.unwrap(), "OWNER");
    assert_eq!(w.core.association(w.repo, w.bob, true).await.unwrap(), "COLLABORATOR");
    assert_eq!(w.core.association(w.repo, w.carol, false).await.unwrap(), "COLLABORATOR", "an explicit collaborator, even read");
    assert_eq!(w.core.association(w.repo, w.eve, false).await.unwrap(), "NONE");

    let org = w.env.org("acme").await;
    let repo = w.env.repo(org, "tools").await;
    for (u, role) in [(w.carol, "member"), (w.dave, "owner")] {
        sqlx::query("INSERT INTO org_members (org_id, user_id, role) VALUES ($1, $2, $3)")
            .bind(org.0)
            .bind(u.0)
            .bind(role)
            .execute(&w.pool)
            .await
            .unwrap();
    }
    assert_eq!(w.core.association(repo, w.carol, false).await.unwrap(), "MEMBER");
    assert_eq!(w.core.association(repo, w.dave, true).await.unwrap(), "OWNER");
    assert_eq!(w.core.association(repo, w.eve, false).await.unwrap(), "NONE");

    // The review object carries it.
    let n = open(&w).await;
    w.core.review_submit(&w.bob_a(), w.repo, n, ReviewState::Approved, "", vec![], &w.c()).await.unwrap();
    let views = w.core.review_views(&w.alice_a(), w.repo, n).await.unwrap();
    assert_eq!(views[0].association, "COLLABORATOR");
}
