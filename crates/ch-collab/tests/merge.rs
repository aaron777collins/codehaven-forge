#![allow(clippy::unwrap_used)]

mod common;

use ch_collab::sync;
use ch_core::collab::*;
use ch_core::events::DomainEvent;
use ch_core::model::*;
use ch_core::{Actor, Error, UserId};
use common::*;
use sqlx::PgPool;

#[allow(clippy::too_many_arguments)]
async fn protect_full(
    w: &World,
    pattern: &str,
    approvals: i32,
    dismiss_stale: bool,
    owners: bool,
    checks: &[&str],
    strict: bool,
    threads: bool,
) {
    sqlx::query(
        "INSERT INTO branch_protections (repo_id, pattern, required_approvals, dismiss_stale_reviews, require_code_owner_review, required_checks, strict, require_resolved_threads)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(w.repo)
    .bind(pattern)
    .bind(approvals)
    .bind(dismiss_stale)
    .bind(owners)
    .bind(checks.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    .bind(strict)
    .bind(threads)
    .execute(&w.pool)
    .await
    .unwrap();
}

async fn review(w: &World, by: UserId, n: i64, state: ReviewState, body: &str) -> Review {
    w.core.review_submit(&w.a(by), w.repo, n, state, body, vec![], &w.c()).await.unwrap()
}

async fn gate(w: &World, n: i64) -> GateReport {
    w.core.pull_merge_gate(&w.alice_a(), w.repo, n).await.unwrap()
}

async fn merge(w: &World, by: &Actor, n: i64, method: MergeMethod) -> Result<PullRequest, Error> {
    w.core.pull_merge(by, w.repo, n, method, &w.c()).await
}

fn blocked(r: &Result<PullRequest, Error>) -> Vec<String> {
    match r {
        Err(Error::MergeBlocked(v)) => v.clone(),
        other => panic!("expected a blocked merge, got {other:?}"),
    }
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn merging_records_the_merge_everywhere(pool: PgPool) {
    let w = World::new(pool).await;
    w.issue(w.alice, "A bug", "").await;
    let n = w.pull(w.carol, "Add feature", "Closes #1").await.pull.issue.number;
    assert_eq!(n, 2);
    let merged = merge(&w, &w.bob_a(), n, MergeMethod::Merge).await.unwrap();
    assert!(merged.merged_at.is_some() && !merged.issue.open);
    assert_eq!(merged.merged_by, Some(w.bob));
    assert_eq!(merged.merge_commit_sha.as_deref(), Some(w.git.sha_of_branch(w.repo, "main").as_str()));

    let req = &w.git.merges()[0];
    assert_eq!(req.repo_id, w.repo);
    assert_eq!((req.base_ref.as_str(), req.head_ref.as_str()), ("refs/heads/main", "refs/heads/feature"));
    assert_eq!(req.method, MergeMethod::Merge);
    assert_eq!(req.title, "Merge pull request #2 from feature");
    assert_eq!(req.message, "Add feature");
    assert_eq!(req.expected_base_sha, sha_of(1));
    // Server-made commits carry the noreply address, never the account email.
    let bob_noreply = format!("{}+bob@users.noreply.test.example", w.bob.0);
    assert_eq!((req.committer.name.as_str(), req.committer.email.as_str()), ("bob", bob_noreply.as_str()));
    assert_eq!(req.author.email, req.committer.email);

    let events = w.events().await;
    assert!(events.contains(&"pr_closed".to_string()) && events.contains(&"pr_merged".to_string()));
    assert!(w.audit_actions().await.contains(&"pull.merged".to_string()));
    let t = w.core.timeline(&w.carol_a(), w.repo, n).await.unwrap();
    assert_eq!(t.iter().find(|e| e.kind == "merged").unwrap().data["method"], "merge");
    // The keyword in the description closed the issue, and the trail says why.
    let issue = w.core.issue_view(&w.carol_a(), w.repo, 1).await.unwrap();
    assert!(!issue.issue.open);
    let closed = w.core.timeline(&w.carol_a(), w.repo, 1).await.unwrap().into_iter().find(|e| e.kind == "closed").unwrap();
    assert_eq!(closed.data["via"]["pull"], 2);
    // Done is done.
    let again = merge(&w, &w.bob_a(), n, MergeMethod::Merge).await;
    assert_eq!(blocked(&again), vec!["This pull request is already merged"]);
    assert_eq!(w.git.merges().len(), 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn squash_and_rebase_write_their_own_messages(pool: PgPool) {
    let w = World::new(pool).await;
    let n1 = w.pull_from(w.carol, "feature", "Add feature", "").await.pull.issue.number;
    merge(&w, &w.bob_a(), n1, MergeMethod::Squash).await.unwrap();
    let after_first = w.git.sha_of_branch(w.repo, "main");
    let sq = &w.git.merges()[0];
    assert_eq!(sq.method, MergeMethod::Squash);
    assert_eq!(sq.title, "Add feature (#1)");
    assert!(sq.message.starts_with("* Change something"));
    assert_eq!(sq.author.email, format!("{}+carol@users.noreply.test.example", w.carol.0), "the squash keeps the pull request author");
    assert_eq!(sq.committer.email, format!("{}+bob@users.noreply.test.example", w.bob.0));

    let n2 = w.pull_from(w.carol, "other", "Second", "").await.pull.issue.number;
    merge(&w, &w.bob_a(), n2, MergeMethod::Rebase).await.unwrap();
    let rb = &w.git.merges()[1];
    assert_eq!((rb.method, rb.title.as_str(), rb.message.as_str()), (MergeMethod::Rebase, "Second", ""));
    assert_eq!(rb.expected_base_sha, after_first, "the second merge builds on the first");

    // A custom title and message win.
    w.git.branch(w.repo, "third", &sha_of(11));
    let n3 = w.pull_from(w.carol, "third", "Third", "").await.pull.issue.number;
    w.core
        .pull_merge_with(
            &w.bob_a(),
            w.repo,
            n3,
            MergeOptions {
                method: Some(MergeMethod::Merge),
                title: Some("Ship it".into()),
                message: Some("Because".into()),
                ..Default::default()
            },
            &w.c(),
        )
        .await
        .unwrap();
    let last = w.git.merges().pop().unwrap();
    assert_eq!((last.title.as_str(), last.message.as_str()), ("Ship it", "Because"));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_repository_decides_which_methods_exist(pool: PgPool) {
    let w = World::new(pool).await;
    let n = w.pull(w.carol, "x", "").await.pull.issue.number;
    sqlx::query("UPDATE repositories SET allow_merge_commit = false WHERE id = $1").bind(w.repo).execute(&w.pool).await.unwrap();
    let r = merge(&w, &w.bob_a(), n, MergeMethod::Merge).await;
    assert_eq!(blocked(&r), vec!["Merge commits are turned off for this repository"]);
    assert!(w.git.merges().is_empty());
    // Without an explicit choice the first allowed method is used.
    w.core.pull_merge_with(&w.bob_a(), w.repo, n, MergeOptions::default(), &w.c()).await.unwrap();
    assert_eq!(w.git.merges()[0].method, MergeMethod::Squash);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn who_may_merge(pool: PgPool) {
    let w = World::new(pool).await;
    let n = w.pull(w.carol, "x", "").await.pull.issue.number;
    assert!(
        matches!(merge(&w, &w.carol_a(), n, MergeMethod::Merge).await, Err(Error::Forbidden(_))),
        "readers cannot merge, even their own"
    );
    assert!(matches!(merge(&w, &w.dave_a(), n, MergeMethod::Merge).await, Err(Error::Forbidden(_))), "triage is not enough");
    assert!(matches!(merge(&w, &w.eve_a(), n, MergeMethod::Merge).await, Err(Error::NotFound)));
    assert!(matches!(merge(&w, &Actor::Anonymous, n, MergeMethod::Merge).await, Err(Error::Forbidden(_)) | Err(Error::NotFound)));
    assert!(w.git.merges().is_empty());
    assert!(merge(&w, &w.bob_a(), n, MergeMethod::Merge).await.is_ok());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn drafts_and_closed_pull_requests_are_blocked(pool: PgPool) {
    let w = World::new(pool).await;
    let n = w.pull(w.carol, "x", "").await.pull.issue.number;
    w.core.pull_set_draft(&w.carol_a(), w.repo, n, true, &w.c()).await.unwrap();
    let r = merge(&w, &w.bob_a(), n, MergeMethod::Merge).await;
    assert!(blocked(&r)[0].contains("draft"));
    w.core.pull_set_draft(&w.carol_a(), w.repo, n, false, &w.c()).await.unwrap();
    w.core.pull_close(&w.carol_a(), w.repo, n, &w.c()).await.unwrap();
    assert_eq!(blocked(&merge(&w, &w.bob_a(), n, MergeMethod::Merge).await), vec!["This pull request is closed"]);
    assert!(w.git.merges().is_empty());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn conflicts_and_races_do_not_merge(pool: PgPool) {
    let w = World::new(pool).await;
    let n = w.pull(w.carol, "x", "").await.pull.issue.number;
    w.git.set_mergeable(MergeableState::Conflict);
    let r = merge(&w, &w.bob_a(), n, MergeMethod::Merge).await;
    assert_eq!(blocked(&r), vec!["There are merge conflicts to resolve"]);
    assert!(w.git.merges().is_empty(), "git was never asked");

    // The branches change between the check and the update.
    w.git.set_mergeable(MergeableState::Clean);
    w.git.set_merge_outcome(MergeOutcome::Conflict { paths: vec!["a.rs".into(), "b.rs".into()] });
    let r = merge(&w, &w.bob_a(), n, MergeMethod::Merge).await;
    assert!(blocked(&r)[0].contains("a.rs, b.rs"));
    let view = w.core.pull_view(&w.carol_a(), w.repo, n).await.unwrap();
    assert!(view.pull.issue.open);
    w.git.set_merge_outcome(MergeOutcome::BaseMoved { current_sha: sha_of(9) });
    assert!(matches!(merge(&w, &w.bob_a(), n, MergeMethod::Merge).await, Err(Error::Conflict(_))));
    assert!(w.core.pull_view(&w.carol_a(), w.repo, n).await.unwrap().pull.merged_at.is_none());
    assert_eq!(w.count_events("pr_merged").await, 0);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn approvals_must_come_from_people_with_write_access(pool: PgPool) {
    let w = World::new(pool).await;
    protect_full(&w, "main", 1, false, false, &[], false, false).await;
    let n = w.pull(w.carol, "x", "").await.pull.issue.number;
    assert_eq!(gate(&w, n).await.reasons, vec!["Needs 1 more approving review from someone with write access"]);

    let own = w.core.review_submit(&w.carol_a(), w.repo, n, ReviewState::Approved, "", vec![], &w.c()).await;
    assert!(matches!(own, Err(Error::Validation(_))), "no approving your own work");
    review(&w, w.dave, n, ReviewState::Approved, "looks good").await;
    assert!(!gate(&w, n).await.mergeable, "triage access does not count toward required approvals");
    let views = w.core.review_views(&w.carol_a(), w.repo, n).await.unwrap();
    assert!(!views[0].counts);

    review(&w, w.bob, n, ReviewState::Approved, "").await;
    assert!(gate(&w, n).await.mergeable);
    assert!(merge(&w, &w.bob_a(), n, MergeMethod::Merge).await.is_ok());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn required_checks_must_pass_and_must_be_readable(pool: PgPool) {
    let w = World::new(pool).await;
    protect_full(&w, "main", 0, false, false, &["ci", "lint"], false, false).await;
    let n = w.pull(w.carol, "x", "").await.pull.issue.number;
    assert_eq!(gate(&w, n).await.reasons, vec!["Required check \"ci\" has not passed", "Required check \"lint\" has not passed"]);
    w.checks.pass("ci");
    assert_eq!(gate(&w, n).await.reasons, vec!["Required check \"lint\" has not passed"]);
    assert!(blocked(&merge(&w, &w.bob_a(), n, MergeMethod::Merge).await)[0].contains("lint"));
    w.checks.pass("lint");
    assert!(gate(&w, n).await.mergeable);
    // If CI cannot be asked, the answer is no.
    *w.checks.broken.lock().unwrap() = true;
    assert_eq!(gate(&w, n).await.reasons, vec!["Required checks could not be verified right now"]);
    assert!(w.git.merges().is_empty());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn stale_approvals_can_be_dropped(pool: PgPool) {
    let w = World::new(pool).await;
    protect_full(&w, "main", 1, true, false, &[], false, false).await;
    let n = w.pull(w.carol, "x", "").await.pull.issue.number;
    review(&w, w.bob, n, ReviewState::Approved, "").await;
    assert!(gate(&w, n).await.mergeable);
    // New commits arrive after the approval.
    sqlx::query("UPDATE pull_requests SET head_sha = $1 WHERE issue_id = (SELECT id FROM issues WHERE repo_id = $2 AND number = $3)")
        .bind(sha_of(55))
        .bind(w.repo)
        .bind(n)
        .execute(&w.pool)
        .await
        .unwrap();
    assert!(gate(&w, n).await.reasons[0].starts_with("Needs 1 more approving review"));
    review(&w, w.alice, n, ReviewState::Approved, "").await;
    assert!(gate(&w, n).await.mergeable);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_change_request_blocks_until_it_is_dismissed(pool: PgPool) {
    let w = World::new(pool).await;
    protect_full(&w, "main", 0, false, false, &[], false, false).await;
    let n = w.pull(w.carol, "x", "").await.pull.issue.number;
    let r = review(&w, w.bob, n, ReviewState::ChangesRequested, "please add tests").await;
    assert_eq!(gate(&w, n).await.reasons, vec!["Changes requested by bob"]);
    assert!(matches!(w.core.review_dismiss(&w.carol_a(), w.repo, n, r.id, "no", &w.c()).await, Err(Error::Forbidden(_))));
    assert!(matches!(w.core.review_dismiss(&w.bob_a(), w.repo, n, r.id, "  ", &w.c()).await, Err(Error::Validation(_))));
    let d = w.core.review_dismiss(&w.alice_a(), w.repo, n, r.id, "addressed offline", &w.c()).await.unwrap();
    assert_eq!(d.state, ReviewState::Dismissed);
    assert!(gate(&w, n).await.mergeable);
    assert!(w.audit_actions().await.contains(&"review.dismissed".to_string()));
    // Only an approval or a change request can be dismissed.
    assert!(matches!(w.core.review_dismiss(&w.alice_a(), w.repo, n, r.id, "again", &w.c()).await, Err(Error::Validation(_))));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn unresolved_conversations_block_when_required(pool: PgPool) {
    let w = World::new(pool).await;
    protect_full(&w, "main", 0, false, false, &[], false, true).await;
    let n = w.pull(w.carol, "x", "").await.pull.issue.number;
    assert!(gate(&w, n).await.mergeable);
    let c = w
        .core
        .review_add_comment(
            &w.bob_a(),
            w.repo,
            n,
            NewReviewComment {
                path: "src/lib.rs".into(),
                side: "RIGHT".into(),
                line: 2,
                start_line: None,
                body: "why?".into(),
                in_reply_to: None,
            },
            None,
            &w.c(),
        )
        .await
        .unwrap();
    assert_eq!(gate(&w, n).await.reasons, vec!["1 review conversation still needs to be resolved"]);
    let thread = c.comment.thread_id.unwrap();
    assert!(matches!(
        w.core.review_resolve_thread(&w.a(w.env.user("frank").await), w.repo, thread, true, &w.c()).await,
        Err(Error::NotFound)
    ));
    w.core.review_resolve_thread(&w.carol_a(), w.repo, thread, true, &w.c()).await.unwrap();
    assert!(gate(&w, n).await.mergeable);
    w.core.review_resolve_thread(&w.bob_a(), w.repo, thread, false, &w.c()).await.unwrap();
    assert!(!gate(&w, n).await.mergeable);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn code_owners_are_asked_and_then_required(pool: PgPool) {
    let w = World::new(pool).await;
    w.git.put_file(w.repo, ".codehaven/CODEOWNERS", "# who looks after what\nsrc/ @bob\n");
    protect_full(&w, "main", 0, false, true, &[], false, false).await;
    let n = w.pull(w.carol, "x", "").await.pull.issue.number;
    let asked = w.core.review_requested(&w.carol_a(), w.repo, n).await.unwrap();
    assert_eq!(asked.users.iter().map(|u| u.username.as_str()).collect::<Vec<_>>(), vec!["bob"]);
    assert_eq!(w.count_events("review_requested").await, 1);
    let g = gate(&w, n).await;
    assert_eq!(g.reasons.len(), 1);
    assert!(g.reasons[0].contains("src/ (@bob)"), "{:?}", g.reasons);

    // A different writer approving is not enough. The owner is.
    review(&w, w.alice, n, ReviewState::Approved, "").await;
    assert!(!gate(&w, n).await.mergeable);
    review(&w, w.bob, n, ReviewState::Approved, "").await;
    assert!(gate(&w, n).await.mergeable);
    // Reviewing answers the request.
    assert!(w.core.review_requested(&w.carol_a(), w.repo, n).await.unwrap().users.is_empty());
    // Asking again does not ask twice.
    let issue = w.core.issue_row(w.repo, n).await.unwrap();
    let pull = w.core.pull_row(issue.id).await.unwrap();
    w.core.request_code_owners(&issue, &pull).await;
    assert_eq!(w.count_events("review_requested").await, 1, "bob already reviewed");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn an_owner_who_wrote_the_change_is_not_required_to_review_it(pool: PgPool) {
    let w = World::new(pool).await;
    w.git.put_file(w.repo, "CODEOWNERS", "* @bob\n");
    protect_full(&w, "main", 0, false, true, &[], false, false).await;
    let n = w.pull(w.bob, "x", "").await.pull.issue.number;
    assert!(w.core.review_requested(&w.bob_a(), w.repo, n).await.unwrap().users.is_empty());
    assert!(gate(&w, n).await.mergeable, "bob cannot approve his own pull request, so he cannot be required to");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn strict_branches_must_be_current_and_can_be_updated(pool: PgPool) {
    let w = World::new(pool).await;
    protect_full(&w, "main", 0, false, false, &[], true, false).await;
    w.git.set_mergeable(MergeableState::Behind);
    let n = w.pull(w.carol, "x", "").await.pull.issue.number;
    assert_eq!(gate(&w, n).await.reasons, vec!["The branch is out of date with main. Update the branch first"]);
    assert!(matches!(w.core.pull_update_branch(&w.carol_a(), w.repo, n, &w.c()).await, Err(Error::Forbidden(_))));
    w.git.set_mergeable(MergeableState::Clean);
    let v = w.core.pull_update_branch(&w.bob_a(), w.repo, n, &w.c()).await.unwrap();
    assert_ne!(v.pull.head_sha, sha_of(2));
    let req = &w.git.merges()[0];
    assert_eq!(
        (req.base_ref.as_str(), req.head_ref.as_str()),
        ("refs/heads/feature", "refs/heads/main"),
        "main is merged into the feature branch"
    );
    assert_eq!(req.expected_base_sha, sha_of(2));
    assert!(w.events().await.contains(&"pr_synchronized".to_string()));
    assert!(gate(&w, n).await.mergeable);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_late_push_cannot_slip_in_behind_a_review(pool: PgPool) {
    let w = World::new(pool).await;
    let n = w.pull(w.carol, "x", "").await.pull.issue.number;
    let stale = MergeOptions { method: Some(MergeMethod::Merge), expected_head_sha: Some(sha_of(99)), ..Default::default() };
    assert!(matches!(w.core.pull_merge_with(&w.bob_a(), w.repo, n, stale, &w.c()).await, Err(Error::Conflict(_))));
    assert!(w.git.merges().is_empty());
    let fresh = MergeOptions { method: Some(MergeMethod::Merge), expected_head_sha: Some(sha_of(2)), ..Default::default() };
    assert!(w.core.pull_merge_with(&w.bob_a(), w.repo, n, fresh, &w.c()).await.is_ok());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_head_branch_is_deleted_only_when_that_is_safe(pool: PgPool) {
    let w = World::new(pool).await;
    let n1 = w.pull_from(w.carol, "feature", "One", "").await.pull.issue.number;
    w.core
        .pull_merge_with(
            &w.bob_a(),
            w.repo,
            n1,
            MergeOptions { method: Some(MergeMethod::Merge), delete_branch: Some(true), ..Default::default() },
            &w.c(),
        )
        .await
        .unwrap();
    assert_eq!(w.git.deleted_refs(), vec![(w.repo.0, "refs/heads/feature".to_string())]);
    let kinds: Vec<String> = w.core.timeline(&w.carol_a(), w.repo, n1).await.unwrap().into_iter().map(|e| e.kind).collect();
    assert!(kinds.contains(&"head_ref_deleted".to_string()));

    // A protected branch stays, even when asked.
    protect_full(&w, "other", 0, false, false, &[], false, false).await;
    let n2 = w.pull_from(w.carol, "other", "Two", "").await.pull.issue.number;
    w.core
        .pull_merge_with(
            &w.bob_a(),
            w.repo,
            n2,
            MergeOptions { method: Some(MergeMethod::Merge), delete_branch: Some(true), ..Default::default() },
            &w.c(),
        )
        .await
        .unwrap();
    assert_eq!(w.git.deleted_refs().len(), 1);

    // The repository setting applies when nothing is said.
    sqlx::query("UPDATE repositories SET delete_branch_on_merge = true WHERE id = $1").bind(w.repo).execute(&w.pool).await.unwrap();
    w.git.branch(w.repo, "third", &sha_of(12));
    let n3 = w.pull_from(w.carol, "third", "Three", "").await.pull.issue.number;
    w.core.pull_merge(&w.bob_a(), w.repo, n3, MergeMethod::Merge, &w.c()).await.unwrap();
    assert!(w.git.deleted_refs().contains(&(w.repo.0, "refs/heads/third".to_string())));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn closing_keywords_only_work_on_the_default_branch(pool: PgPool) {
    let w = World::new(pool).await;
    w.issue(w.alice, "A bug", "").await;
    w.git.branch(w.repo, "release", &sha_of(7));
    let np = NewPull {
        repo_id: w.repo,
        title: "Backport".into(),
        body: "Fixes #1".into(),
        head_repo_id: w.repo,
        head_ref: "feature".into(),
        base_ref: "release".into(),
        draft: false,
    };
    let n = w.core.pull_create(&w.carol_a(), np, &w.c()).await.unwrap().pull.issue.number;
    w.core.pull_merge(&w.bob_a(), w.repo, n, MergeMethod::Merge, &w.c()).await.unwrap();
    assert!(w.core.issue_view(&w.carol_a(), w.repo, 1).await.unwrap().issue.open, "release is not the default branch");
    // A pull request is never closed by a keyword.
    let p = w.pull_from(w.carol, "other", "Other", &format!("Fixes #{n}")).await;
    w.core.pull_merge(&w.bob_a(), w.repo, p.pull.issue.number, MergeMethod::Merge, &w.c()).await.unwrap();
    assert!(w.core.issue_view(&w.carol_a(), w.repo, 1).await.unwrap().issue.open);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn auto_merge_waits_for_the_gate_then_merges_as_the_person_who_asked(pool: PgPool) {
    let w = World::new(pool).await;
    protect_full(&w, "main", 1, false, false, &[], false, false).await;
    let n = w.pull(w.carol, "x", "").await.pull.issue.number;
    assert!(matches!(
        w.core.pull_set_auto_merge(&w.dave_a(), w.repo, n, Some(MergeMethod::Squash), &w.c()).await,
        Err(Error::Forbidden(_))
    ));
    let v = w.core.pull_set_auto_merge(&w.bob_a(), w.repo, n, Some(MergeMethod::Squash), &w.c()).await.unwrap();
    let am = v.auto_merge.unwrap();
    assert_eq!((am.method, am.enabled_by.username.as_str()), (MergeMethod::Squash, "bob"));
    assert!(w.core.pull_set_auto_merge(&w.bob_a(), w.repo, n, None, &w.c()).await.unwrap().auto_merge.is_none());
    w.core.pull_set_auto_merge(&w.bob_a(), w.repo, n, Some(MergeMethod::Squash), &w.c()).await.unwrap();

    // Nothing happens until the approval arrives.
    sync::run_pr_sync_once(&w.core).await.unwrap();
    assert!(w.git.merges().is_empty());
    review(&w, w.alice, n, ReviewState::Approved, "").await;
    sync::run_pr_sync_once(&w.core).await.unwrap();
    let merges = w.git.merges();
    assert_eq!(merges.len(), 1);
    assert_eq!((merges[0].method, merges[0].committer.name.as_str()), (MergeMethod::Squash, "bob"));
    let v = w.core.pull_view(&w.carol_a(), w.repo, n).await.unwrap();
    assert!(v.pull.merged_at.is_some() && v.auto_merge.is_none());
    assert_eq!(v.merged_by.unwrap().username, "bob");
    let kinds: Vec<String> = w.core.timeline(&w.carol_a(), w.repo, n).await.unwrap().into_iter().map(|e| e.kind).collect();
    assert!(kinds.contains(&"auto_merge_enabled".to_string()) && kinds.contains(&"auto_merge_disabled".to_string()));
    assert!(w.audit_actions().await.iter().filter(|a| *a == "pull.auto_merge_changed").count() >= 3);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn auto_merge_is_refused_when_there_is_nothing_to_wait_for(pool: PgPool) {
    let w = World::new(pool).await;
    let n = w.pull(w.carol, "x", "").await.pull.issue.number;
    let r = w.core.pull_set_auto_merge(&w.bob_a(), w.repo, n, Some(MergeMethod::Merge), &w.c()).await;
    assert!(matches!(r, Err(Error::Validation(_))), "it can just be merged");
    w.core.pull_close(&w.carol_a(), w.repo, n, &w.c()).await.unwrap();
    assert!(matches!(w.core.pull_set_auto_merge(&w.bob_a(), w.repo, n, Some(MergeMethod::Merge), &w.c()).await, Err(Error::Validation(_))));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn auto_merge_follows_check_results(pool: PgPool) {
    let w = World::new(pool).await;
    protect_full(&w, "main", 0, false, false, &["ci"], false, false).await;
    let v = w.pull(w.carol, "x", "").await;
    let n = v.pull.issue.number;
    w.core.pull_set_auto_merge(&w.bob_a(), w.repo, n, Some(MergeMethod::Merge), &w.c()).await.unwrap();
    // A check finishing on some other commit changes nothing.
    w.checks.pass("ci");
    let mut tx = w.pool.begin().await.unwrap();
    w.core
        .d
        .events
        .emit(
            &mut tx,
            DomainEvent::CheckRunUpdated {
                repo_id: w.repo,
                sha: sha_of(77),
                name: "ci".into(),
                status: "completed".into(),
                conclusion: Some("success".into()),
            },
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    sync::run_pr_sync_once(&w.core).await.unwrap();
    assert!(w.git.merges().is_empty());
    let mut tx = w.pool.begin().await.unwrap();
    w.core
        .d
        .events
        .emit(
            &mut tx,
            DomainEvent::CheckRunUpdated {
                repo_id: w.repo,
                sha: v.pull.head_sha.clone(),
                name: "ci".into(),
                status: "completed".into(),
                conclusion: Some("success".into()),
            },
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    sync::run_pr_sync_once(&w.core).await.unwrap();
    assert_eq!(w.git.merges().len(), 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn auto_merge_stops_when_its_owner_loses_access(pool: PgPool) {
    let w = World::new(pool).await;
    protect_full(&w, "main", 1, false, false, &[], false, false).await;
    let n = w.pull(w.carol, "x", "").await.pull.issue.number;
    w.core.pull_set_auto_merge(&w.bob_a(), w.repo, n, Some(MergeMethod::Merge), &w.c()).await.unwrap();
    sqlx::query("UPDATE repo_collaborators SET permission = 'read' WHERE repo_id = $1 AND user_id = $2")
        .bind(w.repo)
        .bind(w.bob)
        .execute(&w.pool)
        .await
        .unwrap();
    review(&w, w.alice, n, ReviewState::Approved, "").await;
    sync::run_pr_sync_once(&w.core).await.unwrap();
    assert!(w.git.merges().is_empty());
    assert!(w.core.pull_view(&w.alice_a(), w.repo, n).await.unwrap().auto_merge.is_none());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn update_branch_cannot_write_into_a_protected_head_branch(pool: PgPool) {
    // ch-jnm: a pull request whose head is the protected main and whose base is the writer's own
    // branch. "Update branch" would merge that branch into main with no review and no checks.
    let w = World::new(pool).await;
    protect_full(&w, "main", 1, false, false, &["ci"], true, false).await;
    let n = w
        .core
        .pull_create(
            &w.bob_a(),
            NewPull {
                repo_id: w.repo,
                title: "Backwards".into(),
                body: String::new(),
                head_repo_id: w.repo,
                head_ref: "main".into(),
                base_ref: "feature".into(),
                draft: false,
            },
            &w.c(),
        )
        .await
        .unwrap()
        .pull
        .issue
        .number;
    let e = w.core.pull_update_branch(&w.bob_a(), w.repo, n, &w.c()).await.unwrap_err();
    assert!(matches!(&e, Error::Forbidden(m) if m.contains("protected")), "{e:?}");
    assert!(w.git.merges().is_empty(), "nothing may be written into main");
    // An unprotected head branch still updates as before.
    let n2 = w.pull(w.carol, "x", "").await.pull.issue.number;
    w.git.set_mergeable(MergeableState::Clean);
    w.core.pull_update_branch(&w.bob_a(), w.repo, n2, &w.c()).await.unwrap();
    let req = &w.git.merges()[0];
    assert!(req.update_branch, "the git layer is told this is not a gated merge");
}
