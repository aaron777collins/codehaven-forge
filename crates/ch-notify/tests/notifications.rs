//! Notification fan-out and the inbox (ARCHITECTURE.md section 8.4).
#![allow(clippy::unwrap_used)]

mod common;
use common::*;

use ch_core::events::DomainEvent;
use ch_core::{CommentId, Error, IssueId, NotificationId, PullId, RepoId, UserId};
use ch_notify::fanout::Fanout;
use sqlx::PgPool;

struct World {
    c: Ctx,
    alice: UserId,
    bob: UserId,
    carol: UserId,
    repo: RepoId,
    fan: Fanout,
}

/// alice owns a private repo `demo`. bob and carol are collaborators.
async fn world(pool: PgPool) -> World {
    let c = ctx(pool).await;
    let alice = c.env.user("alice").await;
    let bob = c.env.user("bob").await;
    let carol = c.env.user("carol").await;
    let repo = c.env.repo(alice, "demo").await;
    c.env.collaborator(repo, bob, "write").await;
    c.env.collaborator(repo, carol, "read").await;
    let fan = Fanout::new(c.pool.clone(), c.state.authz.clone());
    World { c, alice, bob, carol, repo, fan }
}

impl World {
    async fn inbox(&self, u: UserId) -> Vec<ch_core::model::Notification> {
        self.c.state.notifications.inbox(&user(u), false, 1, 50).await.unwrap()
    }
    async fn watch(&self, u: UserId, level: &str) {
        sqlx::query(
            "INSERT INTO watches (user_id, repo_id, level) VALUES ($1, $2, $3) ON CONFLICT (user_id, repo_id) DO UPDATE SET level = $3",
        )
        .bind(u.0)
        .bind(self.repo.0)
        .bind(level)
        .execute(&self.c.pool)
        .await
        .unwrap();
    }
    async fn opened(&self, number: i64, author: UserId, title: &str, body: &str) -> (i64, DomainEvent) {
        let id = issue(&self.c.pool, self.repo, number, author, title, body, false).await;
        (id, DomainEvent::IssueOpened { repo_id: self.repo, issue_id: IssueId(id), number, actor: author })
    }
    async fn comment(&self, issue_id: i64, number: i64, author: UserId, text: &str) -> DomainEvent {
        let cid: i64 = sqlx::query_scalar("INSERT INTO issue_comments (issue_id, author_id, body) VALUES ($1, $2, $3) RETURNING id")
            .bind(issue_id)
            .bind(author.0)
            .bind(text)
            .fetch_one(&self.c.pool)
            .await
            .unwrap();
        DomainEvent::CommentCreated { repo_id: self.repo, issue_id: IssueId(issue_id), number, comment_id: CommentId(cid), actor: author }
    }
}

fn reasons(v: &[ch_core::model::Notification]) -> Vec<String> {
    v.iter().map(|n| n.reason.clone()).collect()
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_new_issue_reaches_watchers_and_the_mentioned_but_not_its_author(pool: PgPool) {
    let w = world(pool).await;
    w.watch(w.bob, "all").await;
    let (_, ev) = w.opened(1, w.alice, "Crash on start", "cc @carol").await;
    assert_eq!(w.fan.process(&ev).await.unwrap(), 2);
    assert_eq!(reasons(&w.inbox(w.bob).await), vec!["subscribed"]);
    let carol = w.inbox(w.carol).await;
    assert_eq!(reasons(&carol), vec!["mention"]);
    assert_eq!((carol[0].title.as_str(), carol[0].subject_type.as_str(), carol[0].unread), ("Crash on start", "issue", true));
    assert_eq!(carol[0].repo_id, Some(w.repo));
    assert!(w.inbox(w.alice).await.is_empty(), "you do not notify yourself");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_mention_wins_over_a_weaker_reason(pool: PgPool) {
    let w = world(pool).await;
    w.watch(w.bob, "all").await;
    let (_, ev) = w.opened(1, w.alice, "Hello", "ping @bob").await;
    w.fan.process(&ev).await.unwrap();
    assert_eq!(reasons(&w.inbox(w.bob).await), vec!["mention"]);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn nobody_is_told_about_a_repository_they_cannot_read(pool: PgPool) {
    let w = world(pool).await;
    let dave = w.c.env.user("dave").await;
    w.watch(dave, "all").await;
    let (_, ev) = w.opened(1, w.alice, "Secret roadmap", "@dave look at this").await;
    assert_eq!(w.fan.process(&ev).await.unwrap(), 0, "dave watches and is mentioned, but cannot read the repository");
    assert!(w.inbox(dave).await.is_empty());
    // Once he can, he is told.
    w.c.env.collaborator(w.repo, dave, "read").await;
    assert_eq!(w.fan.process(&ev).await.unwrap(), 1);
    assert_eq!(w.inbox(dave).await.len(), 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn public_repos_notify_any_signed_in_watcher_but_org_names_are_not_people(pool: PgPool) {
    let c = ctx(pool).await;
    let alice = c.env.user("alice").await;
    let eve = c.env.user("eve").await;
    let org = c.env.org("haven").await;
    let repo = c.env.public_repo(alice, "open").await;
    sqlx::query("INSERT INTO watches (user_id, repo_id, level) VALUES ($1, $2, 'all')")
        .bind(eve.0)
        .bind(repo.0)
        .execute(&c.pool)
        .await
        .unwrap();
    let fan = Fanout::new(c.pool.clone(), c.state.authz.clone());
    let id = issue(&c.pool, repo, 1, alice, "Hi", "@haven @eve", false).await;
    let n = fan.process(&DomainEvent::IssueOpened { repo_id: repo, issue_id: IssueId(id), number: 1, actor: alice }).await.unwrap();
    assert_eq!(n, 1, "eve only: an org is not a person");
    let _ = org;
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn comments_reach_the_thread_with_the_right_reasons(pool: PgPool) {
    let w = world(pool).await;
    let (id, _) = w.opened(1, w.alice, "Bug", "details").await;
    sqlx::query("INSERT INTO issue_assignees (issue_id, user_id) VALUES ($1, $2)")
        .bind(id)
        .bind(w.carol.0)
        .execute(&w.c.pool)
        .await
        .unwrap();
    let dan = w.c.env.user("dan").await;
    w.c.env.collaborator(w.repo, dan, "read").await;
    sqlx::query("INSERT INTO issue_subscriptions (issue_id, user_id, subscribed) VALUES ($1, $2, true)")
        .bind(id)
        .bind(dan.0)
        .execute(&w.c.pool)
        .await
        .unwrap();

    let ev = w.comment(id, 1, w.bob, "I can reproduce this").await;
    w.fan.process(&ev).await.unwrap();
    assert_eq!(reasons(&w.inbox(w.alice).await), vec!["author"]);
    assert_eq!(reasons(&w.inbox(w.carol).await), vec!["assigned"]);
    assert_eq!(reasons(&w.inbox(dan).await), vec!["subscribed"]);
    assert!(w.inbox(w.bob).await.is_empty(), "the commenter is the actor");

    // Bob is now a participant, so Carol's reply reaches him as a comment.
    let ev = w.comment(id, 1, w.carol, "me too").await;
    w.fan.process(&ev).await.unwrap();
    assert_eq!(reasons(&w.inbox(w.bob).await), vec!["comment"]);
    // Still one row per thread per person, and unread again.
    assert_eq!(w.inbox(w.alice).await.len(), 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn one_row_per_thread_and_new_activity_reopens_it(pool: PgPool) {
    let w = world(pool).await;
    let (id, _) = w.opened(1, w.alice, "Bug", "details").await;
    let ev = w.comment(id, 1, w.bob, "first").await;
    w.fan.process(&ev).await.unwrap();
    let alice = user(w.alice);
    let n = w.inbox(w.alice).await.remove(0);
    w.c.state.notifications.mark_read(&alice, Some(n.id)).await.unwrap();
    assert_eq!(w.c.state.notifications.unread_count(&alice).await.unwrap(), 0);
    w.c.state.notifications.mark_done(&alice, n.id).await.unwrap();
    assert!(w.inbox(w.alice).await.is_empty(), "done leaves the inbox");

    let ev = w.comment(id, 1, w.bob, "second").await;
    w.fan.process(&ev).await.unwrap();
    let again = w.inbox(w.alice).await;
    assert_eq!(again.len(), 1);
    assert_eq!(again[0].id, n.id, "the same row");
    assert!(again[0].unread);
    assert_eq!(w.c.env.count("notifications").await, 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn ignore_and_mute_silence_things_but_a_mention_gets_through_a_mute(pool: PgPool) {
    let w = world(pool).await;
    let (id, _) = w.opened(1, w.alice, "Bug", "details").await;
    w.watch(w.bob, "ignore").await;
    // Carol mutes the thread.
    w.c.state.notifications.set_thread_subscription(&user(w.carol), "issue", id, false).await.unwrap();
    sqlx::query("INSERT INTO issue_comments (issue_id, author_id, body) VALUES ($1, $2, 'earlier')")
        .bind(id)
        .bind(w.carol.0)
        .execute(&w.c.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO issue_comments (issue_id, author_id, body) VALUES ($1, $2, 'earlier')")
        .bind(id)
        .bind(w.bob.0)
        .execute(&w.c.pool)
        .await
        .unwrap();

    let ev = w.comment(id, 1, w.alice, "any news?").await;
    w.fan.process(&ev).await.unwrap();
    assert!(w.inbox(w.bob).await.is_empty(), "ignored repo");
    assert!(w.inbox(w.carol).await.is_empty(), "muted thread");

    let ev = w.comment(id, 1, w.alice, "@carol @bob please look").await;
    w.fan.process(&ev).await.unwrap();
    assert_eq!(reasons(&w.inbox(w.carol).await), vec!["mention"], "a mention beats a mute");
    assert!(w.inbox(w.bob).await.is_empty(), "but nothing beats ignoring the repository");
    // Unmuting puts her back on the thread.
    w.c.state.notifications.set_thread_subscription(&user(w.carol), "issue", id, true).await.unwrap();
    assert!(w.c.state.notifications.set_thread_subscription(&user(w.carol), "commit", id, true).await.is_err());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn state_changes_and_pull_requests(pool: PgPool) {
    let w = world(pool).await;
    w.watch(w.carol, "all").await;
    let pull = issue(&w.c.pool, w.repo, 3, w.alice, "Add thing", "adds a thing", true).await;
    sqlx::query("INSERT INTO pull_requests (issue_id, repo_id, head_repo_id, head_ref, head_sha, base_ref, base_sha) VALUES ($1, $2, $2, 'f', 'a', 'main', 'b')").bind(pull).bind(w.repo.0).execute(&w.c.pool).await.unwrap();
    sqlx::query("INSERT INTO review_requests (pull_id, user_id, requested_by) VALUES ($1, $2, $3)")
        .bind(pull)
        .bind(w.bob.0)
        .bind(w.alice.0)
        .execute(&w.c.pool)
        .await
        .unwrap();

    w.fan
        .process(&DomainEvent::PrOpened {
            repo_id: w.repo,
            pull_id: PullId(pull),
            number: 3,
            actor: w.alice,
            app: None,
            head_sha: None,
            pusher: Some(w.alice),
        })
        .await
        .unwrap();
    let bob = w.inbox(w.bob).await;
    assert_eq!((reasons(&bob), bob[0].subject_type.as_str()), (vec!["review_requested".to_string()], "pull"));
    assert_eq!(reasons(&w.inbox(w.carol).await), vec!["subscribed"]);

    // Bob approves: Alice hears. Then Alice merges: the others hear a state change.
    // Collab clears the request when the review comes in.
    sqlx::query("DELETE FROM review_requests WHERE pull_id = $1").bind(pull).execute(&w.c.pool).await.unwrap();
    let review: i64 = sqlx::query_scalar(
        "INSERT INTO reviews (pull_id, author_id, state, commit_sha, submitted_at) VALUES ($1, $2, 'approved', 'a', now()) RETURNING id",
    )
    .bind(pull)
    .bind(w.bob.0)
    .fetch_one(&w.c.pool)
    .await
    .unwrap();
    w.fan
        .process(&DomainEvent::ReviewSubmitted {
            repo_id: w.repo,
            pull_id: PullId(pull),
            number: 3,
            review_id: ch_core::ReviewId(review),
            state: "approved".into(),
            actor: w.bob,
        })
        .await
        .unwrap();
    assert_eq!(reasons(&w.inbox(w.alice).await), vec!["author"]);
    w.fan
        .process(&DomainEvent::PrMerged { repo_id: w.repo, pull_id: PullId(pull), number: 3, merge_sha: "m".into(), actor: w.alice })
        .await
        .unwrap();
    assert_eq!(reasons(&w.inbox(w.carol).await), vec!["state_change"]);
    assert_eq!(reasons(&w.inbox(w.bob).await), vec!["state_change"], "the reviewer was a participant, so the merge reaches them");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn assigned_team_mentions_and_ci(pool: PgPool) {
    let w = world(pool).await;
    let (id, _) = w.opened(1, w.alice, "Bug", "hi").await;
    w.fan
        .process(&DomainEvent::IssueAssigned { repo_id: w.repo, issue_id: IssueId(id), number: 1, assignee: w.bob, actor: w.alice })
        .await
        .unwrap();
    assert_eq!(reasons(&w.inbox(w.bob).await), vec!["assigned"]);
    // Assigning yourself tells nobody.
    assert_eq!(
        w.fan
            .process(&DomainEvent::IssueAssigned { repo_id: w.repo, issue_id: IssueId(id), number: 1, assignee: w.alice, actor: w.alice })
            .await
            .unwrap(),
        0
    );

    // @org/team reaches the team's members who can read the repo.
    let org = w.c.env.org("haven").await;
    let team: i64 = sqlx::query_scalar("INSERT INTO teams (org_id, name) VALUES ($1, 'core') RETURNING id")
        .bind(org.0)
        .fetch_one(&w.c.pool)
        .await
        .unwrap();
    for u in [w.carol, w.c.env.user("zed").await] {
        sqlx::query("INSERT INTO team_members (team_id, user_id) VALUES ($1, $2)").bind(team).bind(u.0).execute(&w.c.pool).await.unwrap();
    }
    let ev = w.comment(id, 1, w.alice, "@haven/core can you triage?").await;
    w.fan.process(&ev).await.unwrap();
    assert_eq!(reasons(&w.inbox(w.carol).await), vec!["team_mention"]);
    let zed: i64 = sqlx::query_scalar("SELECT id FROM users WHERE username = 'zed'").fetch_one(&w.c.pool).await.unwrap();
    assert!(w.inbox(UserId(zed)).await.is_empty(), "zed is on the team but cannot read the repo");

    // CI: only the person who triggered the run, only when it finishes.
    let run: i64 = sqlx::query_scalar("INSERT INTO workflow_runs (repo_id, run_number, event, head_sha, actor_id, title) VALUES ($1, 1, 'push', 'a', $2, 'build') RETURNING id").bind(w.repo.0).bind(w.bob.0).fetch_one(&w.c.pool).await.unwrap();
    let ev = |status: &str, conclusion: Option<&str>| DomainEvent::WorkflowRunUpdated {
        repo_id: w.repo,
        run_id: ch_core::RunId(run),
        status: status.into(),
        conclusion: conclusion.map(String::from),
    };
    assert_eq!(w.fan.process(&ev("in_progress", None)).await.unwrap(), 0);
    // Only a run that failed is worth a notification (ch-ed1). Passing, cancelled and skipped
    // runs are on the Actions page.
    for quiet in ["success", "cancelled", "skipped", "neutral"] {
        assert_eq!(w.fan.process(&ev("completed", Some(quiet))).await.unwrap(), 0, "{quiet}");
    }
    assert!(w.inbox(w.bob).await.iter().all(|n| n.reason != "ci_activity"));
    assert_eq!(w.fan.process(&ev("completed", Some("failure"))).await.unwrap(), 1);
    let inbox = w.inbox(w.bob).await;
    let ci = inbox.iter().find(|n| n.reason == "ci_activity").unwrap();
    assert_eq!((ci.subject_type.as_str(), ci.title.as_str()), ("workflow_run", "build: failure"));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn suspended_accounts_and_events_nobody_cares_about(pool: PgPool) {
    let w = world(pool).await;
    w.watch(w.bob, "all").await;
    sqlx::query("UPDATE users SET suspended_at = now() WHERE id = $1").bind(w.bob.0).execute(&w.c.pool).await.unwrap();
    let (_, ev) = w.opened(1, w.alice, "Hi", "x").await;
    assert_eq!(w.fan.process(&ev).await.unwrap(), 0);
    assert_eq!(w.fan.process(&DomainEvent::RepoCreated { repo_id: w.repo, actor: None }).await.unwrap(), 0);
    assert_eq!(
        w.fan
            .process(&DomainEvent::PrSynchronized {
                repo_id: w.repo,
                pull_id: PullId(1),
                number: 1,
                head_sha: "a".into(),
                app: None,
                pusher: None,
                origin_known: true,
                built_on: None,
            })
            .await
            .unwrap(),
        0
    );
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn inbox_pages_filters_and_belongs_to_its_owner(pool: PgPool) {
    let w = world(pool).await;
    w.watch(w.bob, "all").await;
    for n in 1..=5 {
        let (_, ev) = w.opened(n, w.alice, &format!("Issue {n}"), "x").await;
        w.fan.process(&ev).await.unwrap();
    }
    let svc = &w.c.state.notifications;
    let bob = user(w.bob);
    let page1 = svc.inbox(&bob, false, 1, 2).await.unwrap();
    let page3 = svc.inbox(&bob, false, 3, 2).await.unwrap();
    assert_eq!((page1.len(), page3.len()), (2, 1));
    assert_eq!(page1[0].title, "Issue 5", "newest first");
    assert_eq!(svc.unread_count(&bob).await.unwrap(), 5);
    svc.mark_read(&bob, Some(page1[0].id)).await.unwrap();
    assert_eq!(svc.inbox(&bob, true, 1, 50).await.unwrap().len(), 4);
    assert_eq!(svc.inbox(&bob, false, 1, 50).await.unwrap().len(), 5);
    // Somebody else's notification is not found, whether reading or finishing it.
    let carol = user(w.carol);
    assert!(matches!(svc.mark_read(&carol, Some(page1[1].id)).await, Err(Error::NotFound)));
    assert!(matches!(svc.mark_done(&carol, page1[1].id).await, Err(Error::NotFound)));
    assert!(matches!(svc.mark_read(&bob, Some(NotificationId(99_999))).await, Err(Error::NotFound)));
    svc.mark_read(&bob, None).await.unwrap();
    assert_eq!(svc.unread_count(&bob).await.unwrap(), 0);
    assert!(svc.inbox(&ch_core::Actor::Anonymous, false, 1, 10).await.is_err());
    // A token needs a scope.
    let bad = ch_testkit::fixtures::token_actor(w.bob, &["user"]);
    assert!(svc.inbox(&bad, false, 1, 10).await.is_err());
    let good = ch_testkit::fixtures::token_actor(w.bob, &["notifications"]);
    assert_eq!(svc.inbox(&good, false, 1, 10).await.unwrap().len(), 5);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn losing_access_hides_the_inbox_rows(pool: PgPool) {
    let w = world(pool).await;
    w.watch(w.bob, "all").await;
    let (_, ev) = w.opened(1, w.alice, "Confidential", "x").await;
    w.fan.process(&ev).await.unwrap();
    assert_eq!(w.inbox(w.bob).await.len(), 1);
    sqlx::query("DELETE FROM repo_collaborators WHERE user_id = $1").bind(w.bob.0).execute(&w.c.pool).await.unwrap();
    assert!(w.inbox(w.bob).await.is_empty(), "a title must not outlive the access to read it");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn watch_levels(pool: PgPool) {
    let w = world(pool).await;
    let svc = &w.c.state.notifications;
    let bob = user(w.bob);
    assert_eq!(svc.watch_level(&bob, w.repo).await.unwrap(), "participating");
    for level in ["all", "ignore", "participating"] {
        svc.subscribe(&bob, w.repo, level).await.unwrap();
        assert_eq!(svc.watch_level(&bob, w.repo).await.unwrap(), level);
    }
    assert!(svc.subscribe(&bob, w.repo, "everything").await.is_err());
    let stranger = w.c.env.user("stranger").await;
    assert!(matches!(svc.subscribe(&user(stranger), w.repo, "all").await, Err(Error::NotFound)), "cannot watch what you cannot read");
    assert!(matches!(svc.watch_level(&user(stranger), w.repo).await, Err(Error::NotFound)));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_background_consumers_do_the_whole_job(pool: PgPool) {
    let w = world(pool).await;
    w.watch(w.bob, "all").await;
    let sink = sink().await;
    let hook = {
        let mut cfg = (*w.c.state.config).clone();
        cfg.webhooks.allow_private = true;
        let _ = cfg;
        sqlx::query_scalar::<_, i64>("INSERT INTO webhooks (scope, repo_id, url, events) VALUES ('repo', $1, $2, '{issues}') RETURNING id")
            .bind(w.repo.0)
            .bind(&sink.url)
            .fetch_one(&w.c.pool)
            .await
            .unwrap()
    };
    // The workers read the real outbox, so emit the way a service does: in a transaction.
    let id = issue(&w.c.pool, w.repo, 1, w.alice, "From the outbox", "x", false).await;
    let mut tx = w.c.pool.begin().await.unwrap();
    w.c.state
        .events
        .emit(&mut tx, DomainEvent::IssueOpened { repo_id: w.repo, issue_id: IssueId(id), number: 1, actor: w.alice })
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // `workers` uses the instance config, so build a state that allows the loopback sink.
    let mut env_cfg = (*w.c.state.config).clone();
    env_cfg.webhooks.allow_private = true;
    let mut state = w.c.state.clone();
    state.config = std::sync::Arc::new(env_cfg);
    let stop = tokio_util_token();
    let handles: Vec<_> = ch_notify::workers(&state).into_iter().map(|wk| wk.spawn(stop.clone())).collect();
    let mut seen = false;
    for _ in 0..100 {
        let rows: i64 =
            sqlx::query_scalar("SELECT count(*) FROM notifications WHERE user_id = $1").bind(w.bob.0).fetch_one(&w.c.pool).await.unwrap();
        if rows == 1 && sink.count() >= 1 {
            seen = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    stop.cancel();
    for h in handles {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), h).await;
    }
    assert!(seen, "the consumers should have written the inbox row and delivered the hook");
    // The cursors moved, so a restart does not repeat the work.
    let cursors: i64 =
        sqlx::query_scalar("SELECT count(*) FROM event_cursors WHERE consumer IN ('notifications', 'webhooks') AND last_id > 0")
            .fetch_one(&w.c.pool)
            .await
            .unwrap();
    assert_eq!(cursors, 2);
    let _ = hook;
    assert_eq!(sink.last().headers.get("x-github-event").unwrap(), "issues");
}

fn tokio_util_token() -> tokio_util::sync::CancellationToken {
    tokio_util::sync::CancellationToken::new()
}
