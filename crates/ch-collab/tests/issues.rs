#![allow(clippy::unwrap_used)]

mod common;

use ch_core::collab::*;
use ch_core::model::*;
use ch_core::{CommentId, Error, UserId};
use common::*;
use sqlx::PgPool;

fn is_validation<T>(r: &Result<T, Error>) -> bool {
    matches!(r, Err(Error::Validation(_)))
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn numbers_are_shared_between_issues_and_pulls(pool: PgPool) {
    let w = World::new(pool).await;
    assert_eq!(w.issue(w.carol, "first", "").await.issue.number, 1);
    assert_eq!(w.pull(w.bob, "A pull request", "").await.pull.issue.number, 2);
    assert_eq!(w.issue(w.bob, "third", "").await.issue.number, 3);
    assert_eq!(w.events().await, vec!["issue_opened", "pr_opened", "issue_opened"]);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn readers_open_issues_but_only_triagers_label_and_assign(pool: PgPool) {
    let w = World::new(pool).await;
    w.label("bug").await;
    let by_reader = w
        .core
        .issue_create(
            &w.carol_a(),
            NewIssue {
                repo_id: w.repo,
                title: "x".into(),
                body: String::new(),
                labels: vec!["bug".into()],
                assignees: vec![w.bob],
                milestone_id: None,
            },
            &w.c(),
        )
        .await
        .unwrap();
    assert!(by_reader.labels.is_empty());
    assert!(by_reader.assignees.is_empty());
    let by_triager = w
        .core
        .issue_create(
            &w.dave_a(),
            NewIssue {
                repo_id: w.repo,
                title: "y".into(),
                body: String::new(),
                labels: vec!["BUG".into()],
                assignees: vec![w.bob],
                milestone_id: None,
            },
            &w.c(),
        )
        .await
        .unwrap();
    assert_eq!(by_triager.labels.len(), 1);
    assert_eq!(by_triager.assignees[0].username, "bob");
    assert_eq!(w.count_events("issue_labeled").await, 1);
    assert_eq!(w.count_events("issue_assigned").await, 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn an_unknown_label_is_a_validation_error(pool: PgPool) {
    let w = World::new(pool).await;
    let r = w
        .core
        .issue_create(
            &w.dave_a(),
            NewIssue {
                repo_id: w.repo,
                title: "y".into(),
                body: String::new(),
                labels: vec!["nope".into()],
                assignees: vec![],
                milestone_id: None,
            },
            &w.c(),
        )
        .await;
    assert!(is_validation(&r));
    assert_eq!(w.env.count("issues").await, 0, "nothing was half created");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_stranger_cannot_even_tell_the_repository_exists(pool: PgPool) {
    let w = World::new(pool).await;
    w.issue(w.alice, "secret plans", "").await;
    assert!(matches!(w.core.issue_view(&w.eve_a(), w.repo, 1).await, Err(Error::NotFound)));
    assert!(matches!(w.core.issue_views(&w.eve_a(), w.repo, IssueFilter::default()).await, Err(Error::NotFound)));
    let r = w
        .core
        .issue_create(
            &w.eve_a(),
            NewIssue { repo_id: w.repo, title: "hi".into(), body: String::new(), labels: vec![], assignees: vec![], milestone_id: None },
            &w.c(),
        )
        .await;
    assert!(matches!(r, Err(Error::NotFound)));
    assert!(matches!(w.core.issue_view(&ch_core::Actor::Anonymous, w.repo, 1).await, Err(Error::NotFound)));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn who_may_close_and_reopen(pool: PgPool) {
    let w = World::new(pool).await;
    let frank = w.env.user("frank").await;
    w.env.collaborator(w.repo, frank, "read").await;
    let mine = w.issue(w.carol, "mine", "").await;
    let theirs = w.issue(w.bob, "theirs", "").await;

    let closed = w.core.issue_set_state(&w.carol_a(), w.repo, mine.issue.number, false, Some("not_planned"), &w.c()).await.unwrap();
    assert!(!closed.issue.open);
    assert_eq!(closed.issue.state_reason.as_deref(), Some("not_planned"));
    assert_eq!(closed.closed_by.unwrap().username, "carol");
    let reopened = w.core.issue_set_state(&w.carol_a(), w.repo, mine.issue.number, true, None, &w.c()).await.unwrap();
    assert!(reopened.issue.open);
    assert_eq!(reopened.issue.state_reason.as_deref(), Some("reopened"));
    assert!(reopened.issue.closed_at.is_none());

    let r = w.core.issue_set_state(&w.carol_a(), w.repo, theirs.issue.number, false, None, &w.c()).await;
    assert!(matches!(r, Err(Error::Forbidden(_))));
    assert!(w.core.issue_set_state(&w.dave_a(), w.repo, theirs.issue.number, false, None, &w.c()).await.is_ok());
    let bad = w.core.issue_set_state(&w.dave_a(), w.repo, mine.issue.number, false, Some("because"), &w.c()).await;
    assert!(is_validation(&bad));
    assert_eq!(w.count_events("issue_closed").await, 2);
    assert_eq!(w.count_events("issue_reopened").await, 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn editing_is_for_the_author_and_triagers_and_is_recorded(pool: PgPool) {
    let w = World::new(pool).await;
    let frank = w.env.user("frank").await;
    w.env.collaborator(w.repo, frank, "read").await;
    let i = w.issue(w.carol, "Old title", "body").await;
    let n = i.issue.number;
    let v = w.core.issue_edit(&w.carol_a(), w.repo, n, Some("New title".into()), None, &w.c()).await.unwrap();
    assert_eq!(v.issue.title, "New title");
    assert!(matches!(w.core.issue_edit(&w.a(frank), w.repo, n, Some("hijack".into()), None, &w.c()).await, Err(Error::Forbidden(_))));
    assert!(w.core.issue_edit(&w.dave_a(), w.repo, n, None, Some("clarified".into()), &w.c()).await.is_ok());
    assert!(is_validation(&w.core.issue_edit(&w.carol_a(), w.repo, n, Some("  ".into()), None, &w.c()).await));
    let t = w.core.timeline(&w.carol_a(), w.repo, n).await.unwrap();
    let renamed = t.iter().find(|e| e.kind == "renamed").unwrap();
    assert_eq!(renamed.data["from"], "Old title");
    assert_eq!(renamed.data["to"], "New title");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_locked_conversation_only_hears_triagers(pool: PgPool) {
    let w = World::new(pool).await;
    let i = w.issue(w.carol, "heated", "").await;
    let n = i.issue.number;
    assert!(is_validation(&w.core.issue_set_locked(&w.dave_a(), w.repo, n, true, Some("because".into()), &w.c()).await));
    let v = w.core.issue_set_locked(&w.dave_a(), w.repo, n, true, Some("too heated".into()), &w.c()).await.unwrap();
    assert!(v.issue.locked);
    assert_eq!(v.lock_reason.as_deref(), Some("too heated"));
    assert!(matches!(w.core.issue_add_comment(&w.carol_a(), w.repo, n, "but I", &w.c()).await, Err(Error::Forbidden(_))));
    assert!(matches!(w.core.issue_edit(&w.carol_a(), w.repo, n, Some("t".into()), None, &w.c()).await, Err(Error::Forbidden(_))));
    assert!(w.core.issue_add_comment(&w.dave_a(), w.repo, n, "calm down", &w.c()).await.is_ok());
    assert!(matches!(w.core.issue_set_locked(&w.carol_a(), w.repo, n, false, None, &w.c()).await, Err(Error::Forbidden(_))));
    let v = w.core.issue_set_locked(&w.dave_a(), w.repo, n, false, None, &w.c()).await.unwrap();
    assert!(!v.issue.locked && v.lock_reason.is_none());
    assert!(w.audit_actions().await.iter().filter(|a| *a == "issue.lock_changed").count() == 2);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn labels_come_and_go_with_a_trail(pool: PgPool) {
    let w = World::new(pool).await;
    for l in ["bug", "enhancement", "docs"] {
        w.label(l).await;
    }
    let n = w.issue(w.carol, "x", "").await.issue.number;
    let added = w.core.issue_add_labels(&w.dave_a(), w.repo, n, vec!["bug".into(), "docs".into(), "bug".into()], &w.c()).await.unwrap();
    assert_eq!(added.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(), vec!["bug", "docs"]);
    let after = w.core.issue_remove_label(&w.dave_a(), w.repo, n, "docs", &w.c()).await.unwrap();
    assert_eq!(after.len(), 1);
    assert!(matches!(w.core.issue_remove_label(&w.dave_a(), w.repo, n, "missing", &w.c()).await, Err(Error::NotFound)));
    let patched = w
        .core
        .issue_update(&w.dave_a(), w.repo, n, IssuePatch { labels: Some(vec!["enhancement".into()]), ..Default::default() }, &w.c())
        .await
        .unwrap();
    assert_eq!(patched.labels.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(), vec!["enhancement"]);
    assert!(matches!(w.core.issue_add_labels(&w.carol_a(), w.repo, n, vec!["bug".into()], &w.c()).await, Err(Error::Forbidden(_))));
    let kinds: Vec<String> = w.core.timeline(&w.dave_a(), w.repo, n).await.unwrap().into_iter().map(|e| e.kind).collect();
    assert_eq!(kinds.iter().filter(|k| *k == "labeled").count(), 3);
    assert_eq!(kinds.iter().filter(|k| *k == "unlabeled").count(), 2);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn assignees_need_to_see_the_repository_and_are_capped(pool: PgPool) {
    let w = World::new(pool).await;
    let n = w.issue(w.carol, "x", "").await.issue.number;
    let v = w.core.issue_add_assignees(&w.dave_a(), w.repo, n, vec![w.bob, w.carol], &w.c()).await.unwrap();
    assert_eq!(v.assignees.len(), 2);
    assert!(is_validation(&w.core.issue_add_assignees(&w.dave_a(), w.repo, n, vec![w.eve], &w.c()).await));
    assert!(is_validation(&w.core.issue_add_assignees(&w.dave_a(), w.repo, n, vec![w.bob; 11], &w.c()).await));
    let v = w.core.issue_remove_assignees(&w.dave_a(), w.repo, n, vec![w.bob], &w.c()).await.unwrap();
    assert_eq!(v.assignees.len(), 1);
    // Adding someone already there changes nothing and does not announce it twice.
    w.core.issue_add_assignees(&w.dave_a(), w.repo, n, vec![w.carol], &w.c()).await.unwrap();
    assert_eq!(w.count_events("issue_assigned").await, 2);
    let who = w.core.assignable(&w.dave_a(), w.repo).await.unwrap();
    assert_eq!(who.iter().map(|u| u.username.as_str()).collect::<Vec<_>>(), vec!["alice", "bob", "carol", "dave"]);
    // Who has access is for Triage and up, not every reader (ch-uph).
    assert!(matches!(w.core.assignable(&w.carol_a(), w.repo).await, Err(Error::Forbidden(_))));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn milestones_track_progress(pool: PgPool) {
    let w = World::new(pool).await;
    let m = w
        .core
        .create_milestone(&w.dave_a(), w.repo, MilestoneDraft { title: "v1".into(), description: None, due_on: None, open: None }, &w.c())
        .await
        .unwrap();
    assert_eq!(m.number, 1);
    let a = w.issue(w.carol, "a", "").await.issue.number;
    let b = w.issue(w.carol, "b", "").await.issue.number;
    w.core.issue_set_milestone(&w.dave_a(), w.repo, a, Some(1), &w.c()).await.unwrap();
    w.core.issue_set_milestone(&w.dave_a(), w.repo, b, Some(1), &w.c()).await.unwrap();
    w.core.issue_set_state(&w.dave_a(), w.repo, a, false, None, &w.c()).await.unwrap();
    let m = w.core.milestone(&w.carol_a(), w.repo, 1).await.unwrap();
    assert_eq!((m.open_issues, m.closed_issues, m.percent_complete()), (1, 1, 50));
    assert!(is_validation(&w.core.issue_set_milestone(&w.dave_a(), w.repo, a, Some(9), &w.c()).await));
    let v = w.core.issue_set_milestone(&w.dave_a(), w.repo, a, None, &w.c()).await.unwrap();
    assert!(v.milestone.is_none());
    let kinds: Vec<String> = w.core.timeline(&w.dave_a(), w.repo, a).await.unwrap().into_iter().map(|e| e.kind).collect();
    assert!(kinds.contains(&"milestoned".to_string()) && kinds.contains(&"demilestoned".to_string()));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn comments_are_counted_edited_and_deleted_with_a_record(pool: PgPool) {
    let w = World::new(pool).await;
    let frank = w.env.user("frank").await;
    w.env.collaborator(w.repo, frank, "read").await;
    let n = w.issue(w.alice, "x", "").await.issue.number;
    let c = w.core.issue_add_comment(&w.carol_a(), w.repo, n, "me too", &w.c()).await.unwrap();
    assert_eq!(w.core.issue_view(&w.carol_a(), w.repo, n).await.unwrap().issue.comments, 1);
    assert!(is_validation(&w.core.issue_add_comment(&w.carol_a(), w.repo, n, "   ", &w.c()).await));
    let v = w.core.issue_edit_comment(&w.carol_a(), w.repo, c.id, "me three", &w.c()).await.unwrap();
    assert_eq!(v.comment.body, "me three");
    assert!(matches!(w.core.issue_edit_comment(&w.a(frank), w.repo, c.id, "nope", &w.c()).await, Err(Error::Forbidden(_))));
    assert!(matches!(w.core.issue_delete_comment(&w.a(frank), w.repo, c.id, &w.c()).await, Err(Error::Forbidden(_))));
    assert!(matches!(w.core.comment_view(&w.carol_a(), w.repo, CommentId(999_999)).await, Err(Error::NotFound)));
    w.core.issue_delete_comment(&w.dave_a(), w.repo, c.id, &w.c()).await.unwrap();
    assert_eq!(w.core.issue_view(&w.carol_a(), w.repo, n).await.unwrap().issue.comments, 0);
    assert!(w.audit_actions().await.contains(&"comment.deleted".to_string()));
    assert_eq!(w.count_events("comment_created").await, 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_comment_id_from_another_repository_is_not_found(pool: PgPool) {
    let w = World::new(pool).await;
    let other = w.env.repo(w.alice, "other").await;
    let n = w.issue(w.alice, "x", "").await.issue.number;
    let c = w.core.issue_add_comment(&w.alice_a(), w.repo, n, "hello", &w.c()).await.unwrap();
    assert!(matches!(w.core.comment_view(&w.alice_a(), other, c.id).await, Err(Error::NotFound)));
    assert!(matches!(w.core.issue_delete_comment(&w.alice_a(), other, c.id, &w.c()).await, Err(Error::NotFound)));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn reactions_are_idempotent_and_personal(pool: PgPool) {
    let w = World::new(pool).await;
    let n = w.issue(w.alice, "x", "").await.issue.number;
    let c = w.core.issue_add_comment(&w.alice_a(), w.repo, n, "hello", &w.c()).await.unwrap();
    let r1 = w.core.react(&w.carol_a(), w.repo, ReactionTarget::Issue(n), "+1", &w.c()).await.unwrap();
    let r2 = w.core.react(&w.carol_a(), w.repo, ReactionTarget::Issue(n), "+1", &w.c()).await.unwrap();
    assert_eq!(r1.id, r2.id);
    w.core.react(&w.bob_a(), w.repo, ReactionTarget::Issue(n), "heart", &w.c()).await.unwrap();
    w.core.react(&w.bob_a(), w.repo, ReactionTarget::IssueComment(c.id), "rocket", &w.c()).await.unwrap();
    assert!(is_validation(&w.core.react(&w.bob_a(), w.repo, ReactionTarget::Issue(n), "party", &w.c()).await));
    let v = w.core.issue_view(&w.carol_a(), w.repo, n).await.unwrap();
    assert_eq!((v.reactions.plus_one, v.reactions.heart, v.reactions.total()), (1, 1, 2));
    assert_eq!(w.core.comment_view(&w.carol_a(), w.repo, c.id).await.unwrap().reactions.rocket, 1);
    assert!(matches!(w.core.unreact(&w.bob_a(), w.repo, ReactionTarget::Issue(n), r1.id, &w.c()).await, Err(Error::NotFound)));
    w.core.unreact(&w.carol_a(), w.repo, ReactionTarget::Issue(n), r1.id, &w.c()).await.unwrap();
    assert_eq!(w.core.reactions(&w.carol_a(), w.repo, ReactionTarget::Issue(n)).await.unwrap().len(), 1);
    assert!(matches!(w.core.react(&w.eve_a(), w.repo, ReactionTarget::Issue(n), "+1", &w.c()).await, Err(Error::NotFound)));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn references_leave_one_trail_and_do_not_leak_private_titles(pool: PgPool) {
    let w = World::new(pool).await;
    w.issue(w.alice, "Target", "").await;
    // Bob owns a private repository nobody else can see and mentions demo#1 from it.
    let secret = w.env.repo(w.bob, "secret").await;
    let from_secret = w
        .core
        .issue_create(
            &w.bob_a(),
            NewIssue {
                repo_id: secret,
                title: "Launch codename".into(),
                body: "blocked by alice/demo#1".into(),
                labels: vec![],
                assignees: vec![],
                milestone_id: None,
            },
            &w.c(),
        )
        .await
        .unwrap();
    // A same repo reference, twice, with an edit in between.
    let two = w.issue(w.carol, "Second", "relates to #1").await;
    w.core.issue_edit(&w.carol_a(), w.repo, two.issue.number, None, Some("relates to #1 and again #1".into()), &w.c()).await.unwrap();

    let seen_by_bob = w.core.timeline(&w.bob_a(), w.repo, 1).await.unwrap();
    let refs: Vec<_> = seen_by_bob.iter().filter(|e| e.kind == "cross-referenced").collect();
    assert_eq!(refs.len(), 2, "one from the secret repo and one from #2, never twice");
    assert!(refs.iter().any(|e| e.data["source"]["title"] == "Launch codename"));
    let seen_by_carol = w.core.timeline(&w.carol_a(), w.repo, 1).await.unwrap();
    let refs: Vec<_> = seen_by_carol.iter().filter(|e| e.kind == "cross-referenced").collect();
    assert_eq!(refs.len(), 1, "carol cannot read the secret repository");
    assert_eq!(refs[0].data["source"]["title"], "Second");
    let _ = from_secret;

    // A caller who cannot read the target repository leaves nothing behind.
    let mine = w.env.repo(w.eve, "mine").await;
    w.core
        .issue_create(
            &w.eve_a(),
            NewIssue {
                repo_id: mine,
                title: "Probe".into(),
                body: "see alice/demo#1".into(),
                labels: vec![],
                assignees: vec![],
                milestone_id: None,
            },
            &w.c(),
        )
        .await
        .unwrap();
    let after = w.core.timeline(&w.bob_a(), w.repo, 1).await.unwrap();
    assert_eq!(after.iter().filter(|e| e.kind == "cross-referenced").count(), 2);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn mentions_subscribe_people_who_can_read(pool: PgPool) {
    let w = World::new(pool).await;
    let i = w.issue(w.alice, "Review please", "@bob and @eve, take a look").await;
    let subs: Vec<(UserId, String)> =
        sqlx::query_as("SELECT user_id, reason FROM issue_subscriptions WHERE issue_id = $1 ORDER BY user_id")
            .bind(i.issue.id)
            .fetch_all(&w.pool)
            .await
            .unwrap();
    assert!(subs.contains(&(w.bob, "mentioned".into())));
    assert!(subs.contains(&(w.alice, "author".into())));
    assert!(!subs.iter().any(|(u, _)| *u == w.eve), "eve cannot read the repository");
    assert_eq!(w.core.subscription(&w.bob_a(), w.repo, i.issue.number).await.unwrap(), Some(true));
    w.core.set_subscription(&w.bob_a(), w.repo, i.issue.number, false).await.unwrap();
    assert_eq!(w.core.subscription(&w.bob_a(), w.repo, i.issue.number).await.unwrap(), Some(false));
    assert_eq!(w.core.subscription(&w.carol_a(), w.repo, i.issue.number).await.unwrap(), None);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn pinning_is_for_issues_and_capped(pool: PgPool) {
    let w = World::new(pool).await;
    for i in 0..4 {
        w.issue(w.alice, &format!("i{i}"), "").await;
    }
    for n in 1..=3 {
        assert!(w.core.issue_set_pinned(&w.dave_a(), w.repo, n, true, &w.c()).await.unwrap().pinned);
    }
    assert!(matches!(w.core.issue_set_pinned(&w.dave_a(), w.repo, 4, true, &w.c()).await, Err(Error::Conflict(_))));
    w.core.issue_set_pinned(&w.dave_a(), w.repo, 1, false, &w.c()).await.unwrap();
    assert!(w.core.issue_set_pinned(&w.dave_a(), w.repo, 4, true, &w.c()).await.is_ok());
    let p = w.pull(w.bob, "pr", "").await;
    assert!(is_validation(&w.core.issue_set_pinned(&w.dave_a(), w.repo, p.pull.issue.number, true, &w.c()).await));
    assert!(matches!(w.core.issue_set_pinned(&w.carol_a(), w.repo, 2, false, &w.c()).await, Err(Error::Forbidden(_))));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn an_archived_repository_is_read_only(pool: PgPool) {
    let w = World::new(pool).await;
    let n = w.issue(w.alice, "x", "").await.issue.number;
    sqlx::query("UPDATE repositories SET archived = true WHERE id = $1").bind(w.repo).execute(&w.pool).await.unwrap();
    let r = w
        .core
        .issue_create(
            &w.alice_a(),
            NewIssue { repo_id: w.repo, title: "y".into(), body: String::new(), labels: vec![], assignees: vec![], milestone_id: None },
            &w.c(),
        )
        .await;
    assert!(matches!(&r, Err(Error::Forbidden(m)) if m.contains("archived")));
    assert!(matches!(w.core.issue_add_comment(&w.alice_a(), w.repo, n, "hi", &w.c()).await, Err(Error::Forbidden(_))));
    assert!(matches!(w.core.issue_set_state(&w.alice_a(), w.repo, n, false, None, &w.c()).await, Err(Error::Forbidden(_))));
    assert!(w.core.issue_view(&w.carol_a(), w.repo, n).await.is_ok());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn issues_can_be_turned_off(pool: PgPool) {
    let w = World::new(pool).await;
    sqlx::query("UPDATE repositories SET has_issues = false WHERE id = $1").bind(w.repo).execute(&w.pool).await.unwrap();
    let r = w
        .core
        .issue_create(
            &w.alice_a(),
            NewIssue { repo_id: w.repo, title: "y".into(), body: String::new(), labels: vec![], assignees: vec![], milestone_id: None },
            &w.c(),
        )
        .await;
    assert!(is_validation(&r));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn templates_come_from_the_default_branch(pool: PgPool) {
    let w = World::new(pool).await;
    w.git.put_file(
        w.repo,
        ".codehaven/ISSUE_TEMPLATE/bug.md",
        "---\nname: Bug report\nabout: Something broke\nlabels: bug\n---\nWhat happened?\n",
    );
    w.git.put_file(w.repo, ".codehaven/ISSUE_TEMPLATE/idea.md", "An idea\n");
    w.git.put_file(w.repo, ".codehaven/ISSUE_TEMPLATE/config.yml", "blank_issues_enabled: false\n");
    let t = w.core.templates(&w.carol_a(), w.repo).await.unwrap();
    assert_eq!(t.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec!["Bug report", "idea"]);
    assert_eq!(t[0].labels, vec!["bug"]);
    assert!(matches!(w.core.templates(&w.eve_a(), w.repo).await, Err(Error::NotFound)));
}

// ------------------------------------------------------------------ lists

async fn seeded(w: &World) {
    w.label("bug").await;
    w.label("enhancement").await;
    let mk = |title: &'static str, by, labels: Vec<&'static str>, assignees: Vec<UserId>| {
        let core = w.core.clone();
        let repo = w.repo;
        let c = w.c();
        let actor = w.a(by);
        async move {
            core.issue_create(
                &actor,
                NewIssue {
                    repo_id: repo,
                    title: title.into(),
                    body: String::new(),
                    labels: labels.into_iter().map(String::from).collect(),
                    assignees,
                    milestone_id: None,
                },
                &c,
            )
            .await
            .unwrap()
        }
    };
    mk("Login fails on mobile", w.alice, vec!["bug"], vec![w.bob]).await;
    mk("Add dark mode", w.bob, vec!["enhancement"], vec![]).await;
    mk("Crash on save", w.dave, vec!["bug"], vec![]).await;
    w.pull(w.bob, "Fix the thing", "").await;
}

fn q(query: &str) -> IssueFilter {
    IssueFilter { query: Some(query.into()), ..Default::default() }
}

async fn numbers(w: &World, actor: &ch_core::Actor, f: IssueFilter) -> Vec<i64> {
    w.core.issue_views(actor, w.repo, f).await.unwrap().into_iter().map(|v| v.issue.number).collect()
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn lists_filter_sort_and_page(pool: PgPool) {
    let w = World::new(pool).await;
    seeded(&w).await;
    let a = w.alice_a();
    assert_eq!(
        numbers(&w, &a, IssueFilter { open: Some(true), ..Default::default() }).await,
        vec![4, 3, 2, 1],
        "newest first, pull requests included"
    );
    assert_eq!(numbers(&w, &a, q("is:issue")).await, vec![3, 2, 1]);
    assert_eq!(numbers(&w, &a, q("is:pr")).await, vec![4]);
    assert_eq!(numbers(&w, &a, q("label:bug")).await, vec![3, 1]);
    assert_eq!(numbers(&w, &a, q("-label:bug is:issue")).await, vec![2]);
    assert_eq!(numbers(&w, &a, IssueFilter { labels: vec!["BUG".into()], ..Default::default() }).await, vec![3, 1]);
    assert_eq!(numbers(&w, &a, q("assignee:bob")).await, vec![1]);
    assert_eq!(numbers(&w, &a, IssueFilter { assignee: Some(w.bob), ..Default::default() }).await, vec![1]);
    assert_eq!(numbers(&w, &a, q("assignee:nobody-by-that-name")).await, Vec::<i64>::new());
    assert_eq!(numbers(&w, &a, q("no:assignee is:issue")).await, vec![3, 2]);
    assert_eq!(numbers(&w, &w.bob_a(), q("author:@me")).await, vec![4, 2]);
    assert_eq!(numbers(&w, &a, q("sort:created-asc is:issue")).await, vec![1, 2, 3]);
    assert_eq!(numbers(&w, &a, q("crash")).await, vec![3]);
    assert_eq!(numbers(&w, &a, q("log")).await, vec![1], "partial words match titles");
    assert_eq!(numbers(&w, &a, IssueFilter { per_page: 2, page: 2, ..Default::default() }).await, vec![2, 1]);
    assert_eq!(
        numbers(&w, &a, IssueFilter { query: Some("is:issue".into()), sort: Some("comments-desc".into()), ..Default::default() })
            .await
            .len(),
        3
    );
    w.core.issue_set_state(&w.dave_a(), w.repo, 3, false, None, &w.c()).await.unwrap();
    assert_eq!(numbers(&w, &a, IssueFilter { open: Some(false), ..Default::default() }).await, vec![3]);
    assert_eq!(numbers(&w, &a, q("is:closed")).await.len(), 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn queries_with_hostile_text_are_just_text(pool: PgPool) {
    let w = World::new(pool).await;
    seeded(&w).await;
    for nasty in ["'; DROP TABLE issues; --", "%", "_", "\\", "label:\"", "\u{0}", "assignee:\"a'b\""] {
        let r = w.core.issue_views(&w.alice_a(), w.repo, q(nasty)).await;
        assert!(r.is_ok(), "{nasty}: {r:?}");
    }
    assert_eq!(w.env.count("issues").await, 4);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn cross_repository_lists_respect_permissions(pool: PgPool) {
    let w = World::new(pool).await;
    seeded(&w).await;
    // A stale assignment in a repository bob cannot read must not show up for him.
    let secret = w.env.repo(w.eve, "secret").await;
    let hidden = w
        .core
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
    sqlx::query("INSERT INTO issue_assignees (issue_id, user_id) VALUES ($1, $2)")
        .bind(hidden.issue.id)
        .bind(w.bob)
        .execute(&w.pool)
        .await
        .unwrap();
    let mine = w.core.issue_for_user(&w.bob_a(), UserIssueScope::Assigned, IssueFilter::default()).await.unwrap();
    assert_eq!(mine.iter().map(|v| v.issue.title.as_str()).collect::<Vec<_>>(), vec!["Login fails on mobile"]);
    assert_eq!(mine[0].repo_full_name, "alice/demo");
    let created =
        w.core.issue_for_user(&w.bob_a(), UserIssueScope::Created, IssueFilter { open: Some(true), ..Default::default() }).await.unwrap();
    assert_eq!(created.len(), 2);
    assert!(matches!(
        w.core.issue_for_user(&ch_core::Actor::Anonymous, UserIssueScope::Assigned, IssueFilter::default()).await,
        Err(Error::Forbidden(_))
    ));
    // By id, in the caller's order, leaving out what they cannot read.
    let ids = [hidden.issue.id, ch_core::IssueId(mine[0].issue.id.0)];
    let got = w.core.issue_views_by_ids(&w.bob_a(), &ids).await.unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(w.core.issue_views_by_ids(&w.eve_a(), &ids).await.unwrap().len(), 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_trait_objects_agree_with_the_engine(pool: PgPool) {
    use ch_core::services::Issues;
    let w = World::new(pool).await;
    let state = w.app_state().await;
    let i: &dyn Issues = &*state.issues;
    let created = i
        .create(
            &w.carol_a(),
            NewIssue {
                repo_id: w.repo,
                title: "via trait".into(),
                body: "b".into(),
                labels: vec![],
                assignees: vec![],
                milestone_id: None,
            },
            &w.c(),
        )
        .await
        .unwrap();
    assert_eq!(created.number, 1);
    assert_eq!(i.get(&w.carol_a(), w.repo, 1).await.unwrap().title, "via trait");
    assert_eq!(i.list(&w.carol_a(), w.repo, IssueFilter::default()).await.unwrap().len(), 1);
    let c = i.add_comment(&w.carol_a(), w.repo, 1, "hi", &w.c()).await.unwrap();
    assert_eq!(i.comments(&w.carol_a(), w.repo, 1).await.unwrap()[0].id, c.id);
    assert!(!i.set_state(&w.carol_a(), w.repo, 1, false, None, &w.c()).await.unwrap().open);
}
