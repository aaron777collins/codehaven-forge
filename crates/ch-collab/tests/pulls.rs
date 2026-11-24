#![allow(clippy::unwrap_used)]

mod common;

use ch_core::collab::*;
use ch_core::model::*;
use ch_core::{Error, RepoId};
use common::*;
use sqlx::PgPool;

fn is_validation<T>(r: &Result<T, Error>) -> bool {
    matches!(r, Err(Error::Validation(_)))
}

fn new_pull(w: &World, head: &str, base: &str) -> NewPull {
    NewPull {
        repo_id: w.repo,
        title: "Change".into(),
        body: String::new(),
        head_repo_id: w.repo,
        head_ref: head.into(),
        base_ref: base.into(),
        draft: false,
    }
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn opening_a_pull_request_records_everything(pool: PgPool) {
    let w = World::new(pool).await;
    let v = w.pull(w.carol, "Add the thing", "Fixes a bug").await;
    assert_eq!(v.pull.issue.number, 1);
    assert!(v.pull.issue.is_pull && v.pull.issue.open && !v.pull.draft);
    assert_eq!(v.pull.head_ref, "feature");
    assert_eq!(v.pull.base_ref, "main");
    assert_eq!(v.pull.head_sha, sha_of(2));
    assert_eq!(v.pull.base_sha, sha_of(1));
    assert_eq!(v.pull.mergeable, MergeableState::Clean);
    assert_eq!(v.stats, PullStats { commits: 1, additions: 2, deletions: 1, changed_files: 1 });
    assert_eq!(v.base_repo.full_name(), "alice/demo");
    assert!(v.head_repo.is_none(), "same repository pull requests have no separate head repository");
    assert_eq!(v.issue.author.username, "carol");
    assert_eq!(w.git.created_refs(), vec![(w.repo.0, "refs/pull/1/head".to_string(), sha_of(2))]);
    assert_eq!(w.events().await, vec!["pr_opened"]);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn bad_requests_are_refused_with_reasons(pool: PgPool) {
    let w = World::new(pool).await;
    let actor = w.carol_a();
    assert!(is_validation(&w.core.pull_create(&actor, new_pull(&w, "main", "main"), &w.c()).await), "same branch");
    assert!(is_validation(&w.core.pull_create(&actor, new_pull(&w, "ghost", "main"), &w.c()).await), "unknown head");
    assert!(is_validation(&w.core.pull_create(&actor, new_pull(&w, "feature", "ghost"), &w.c()).await), "unknown base");
    let mut blank = new_pull(&w, "feature", "main");
    blank.title = "  ".into();
    assert!(is_validation(&w.core.pull_create(&actor, blank, &w.c()).await), "no title");
    assert!(matches!(w.core.pull_create(&w.eve_a(), new_pull(&w, "feature", "main"), &w.c()).await, Err(Error::NotFound)));

    // A branch that holds nothing new is refused.
    w.git.branch(w.repo, "same", &sha_of(1));
    assert!(is_validation(&w.core.pull_create(&actor, new_pull(&w, "same", "main"), &w.c()).await), "no commits between");

    // One open pull request per head and base.
    w.core.pull_create(&actor, new_pull(&w, "feature", "main"), &w.c()).await.unwrap();
    let dup = w.core.pull_create(&w.bob_a(), new_pull(&w, "refs/heads/feature", "main"), &w.c()).await;
    assert!(matches!(&dup, Err(Error::Validation(e)) if e[0].message.as_deref().is_some_and(|m| m.contains("already exists"))), "{dup:?}");
    assert_eq!(w.env.count("pull_requests").await, 1);
    assert_eq!(w.env.count("issues").await, 1, "the refused attempts took no numbers");

    sqlx::query("UPDATE repositories SET has_pulls = false WHERE id = $1").bind(w.repo).execute(&w.pool).await.unwrap();
    assert!(is_validation(&w.core.pull_create(&w.bob_a(), new_pull(&w, "other", "main"), &w.c()).await));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_fork_can_open_a_pull_request_but_a_stranger_repository_cannot(pool: PgPool) {
    let w = World::new(pool).await;
    // Bob's fork of alice/demo, in the same fork network.
    let fork: i64 = sqlx::query_scalar("INSERT INTO repositories (owner_id, name, visibility, fork_parent_id, fork_root_id) VALUES ($1, 'demo', 'public', $2, $2) RETURNING id")
        .bind(w.bob)
        .bind(w.repo)
        .fetch_one(&w.pool)
        .await
        .unwrap();
    let fork = RepoId(fork);
    w.git.branch(fork, "patch", &sha_of(9));
    let mut np = new_pull(&w, "patch", "main");
    np.head_repo_id = fork;
    let v = w.core.pull_create(&w.bob_a(), np.clone(), &w.c()).await.unwrap();
    assert_eq!(v.head_repo.as_ref().unwrap().full_name(), "bob/demo");
    assert_eq!(v.pull.head_repo_id, Some(fork));
    assert!(w.git.created_refs().iter().any(|(r, n, _)| *r == w.repo.0 && n == "refs/pull/1/head"));

    // A repository outside the fork network cannot be the head.
    let stranger = w.env.public_repo(w.eve, "unrelated").await;
    w.git.branch(stranger, "patch", &sha_of(9));
    np.head_repo_id = stranger;
    assert!(is_validation(&w.core.pull_create(&w.bob_a(), np, &w.c()).await));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn editing_and_retargeting(pool: PgPool) {
    let w = World::new(pool).await;
    let n = w.pull(w.carol, "Old", "").await.pull.issue.number;
    let patch =
        |title: Option<&str>, base: Option<&str>| PullPatch { title: title.map(String::from), body: None, base: base.map(String::from) };
    let v = w.core.pull_update(&w.carol_a(), w.repo, n, patch(Some("New"), None), &w.c()).await.unwrap();
    assert_eq!(v.pull.issue.title, "New");
    assert!(matches!(
        w.core.pull_update(&w.a(w.env.user("frank").await), w.repo, n, patch(Some("x"), None), &w.c()).await,
        Err(Error::NotFound)
    ));
    assert!(w.core.pull_update(&w.bob_a(), w.repo, n, patch(Some("by writer"), None), &w.c()).await.is_ok());

    w.git.branch(w.repo, "release", &sha_of(7));
    let v = w.core.pull_update(&w.carol_a(), w.repo, n, patch(None, Some("release")), &w.c()).await.unwrap();
    assert_eq!((v.pull.base_ref.as_str(), v.pull.base_sha.as_str()), ("release", sha_of(7).as_str()));
    let kinds: Vec<String> = w.core.timeline(&w.carol_a(), w.repo, n).await.unwrap().into_iter().map(|e| e.kind).collect();
    assert!(kinds.contains(&"base_ref_changed".to_string()) && kinds.contains(&"renamed".to_string()));
    assert!(is_validation(&w.core.pull_update(&w.carol_a(), w.repo, n, patch(None, Some("ghost")), &w.c()).await));
    assert!(
        is_validation(&w.core.pull_update(&w.carol_a(), w.repo, n, patch(None, Some("feature")), &w.c()).await),
        "the base cannot be the head"
    );
    w.core.pull_close(&w.carol_a(), w.repo, n, &w.c()).await.unwrap();
    assert!(is_validation(&w.core.pull_update(&w.carol_a(), w.repo, n, patch(None, Some("main")), &w.c()).await), "closed");
    // Retitling a closed pull request is fine.
    assert!(w.core.pull_update(&w.carol_a(), w.repo, n, patch(Some("Still fine"), None), &w.c()).await.is_ok());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn closing_and_reopening(pool: PgPool) {
    let w = World::new(pool).await;
    let n = w.pull(w.carol, "x", "").await.pull.issue.number;
    let closed = w.core.pull_close(&w.carol_a(), w.repo, n, &w.c()).await.unwrap();
    assert!(!closed.issue.open);
    assert!(matches!(w.core.pull_set_open(&w.a(w.env.user("frank").await), w.repo, n, true, &w.c()).await, Err(Error::NotFound)));
    let reopened = w.core.pull_set_open(&w.carol_a(), w.repo, n, true, &w.c()).await.unwrap();
    assert!(reopened.pull.issue.open);
    assert_eq!(w.events().await, vec!["pr_opened", "pr_closed", "pr_opened"]);

    // Without its branch a pull request stays closed.
    w.core.pull_close(&w.carol_a(), w.repo, n, &w.c()).await.unwrap();
    w.git.st.lock().unwrap().refs.remove(&(w.repo.0, "refs/heads/feature".to_string()));
    assert!(is_validation(&w.core.pull_set_open(&w.carol_a(), w.repo, n, true, &w.c()).await));
    // Closing through the issue endpoints reaches the same rules.
    assert!(is_validation(&w.core.issue_set_state(&w.carol_a(), w.repo, n, true, None, &w.c()).await));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn draft_state(pool: PgPool) {
    let w = World::new(pool).await;
    let n = w.pull(w.carol, "x", "").await.pull.issue.number;
    let v = w.core.pull_set_draft(&w.carol_a(), w.repo, n, true, &w.c()).await.unwrap();
    assert!(v.pull.draft);
    let other = w.env.user("frank").await;
    w.env.collaborator(w.repo, other, "read").await;
    assert!(matches!(w.core.pull_set_draft(&w.a(other), w.repo, n, false, &w.c()).await, Err(Error::Forbidden(_))));
    let v = w.core.pull_set_draft(&w.bob_a(), w.repo, n, false, &w.c()).await.unwrap();
    assert!(!v.pull.draft);
    let kinds: Vec<String> = w.core.timeline(&w.carol_a(), w.repo, n).await.unwrap().into_iter().map(|e| e.kind).collect();
    assert_eq!(kinds, vec!["convert_to_draft", "ready_for_review"]);
    let mut np = new_pull(&w, "other", "main");
    np.draft = true;
    assert!(w.core.pull_create(&w.bob_a(), np, &w.c()).await.unwrap().pull.draft);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn lists_find_pull_requests_by_branch(pool: PgPool) {
    let w = World::new(pool).await;
    w.pull_from(w.bob, "feature", "One", "").await;
    w.pull_from(w.bob, "other", "Two", "").await;
    let list = |f: PullFilter| {
        let w = &w;
        async move { w.core.pull_views(&w.carol_a(), w.repo, f).await.unwrap().into_iter().map(|v| v.pull.issue.number).collect::<Vec<_>>() }
    };
    assert_eq!(list(PullFilter { issue: IssueFilter { open: Some(true), ..Default::default() }, ..Default::default() }).await, vec![2, 1]);
    assert_eq!(list(PullFilter { head: Some("feature".into()), ..Default::default() }).await, vec![1]);
    assert_eq!(list(PullFilter { head: Some("alice:other".into()), ..Default::default() }).await, vec![2]);
    assert_eq!(list(PullFilter { head: Some("nobody:other".into()), ..Default::default() }).await, Vec::<i64>::new());
    assert_eq!(list(PullFilter { base: Some("main".into()), ..Default::default() }).await, vec![2, 1]);
    assert_eq!(list(PullFilter { base: Some("release".into()), ..Default::default() }).await, Vec::<i64>::new());

    let open = w.core.pull_open_for_branch(&w.carol_a(), w.repo, w.repo, "feature", "main").await.unwrap().unwrap();
    assert_eq!(open.pull.issue.number, 1);
    w.core.pull_close(&w.bob_a(), w.repo, 1, &w.c()).await.unwrap();
    assert!(w.core.pull_open_for_branch(&w.carol_a(), w.repo, w.repo, "feature", "main").await.unwrap().is_none());
    assert_eq!(list(PullFilter { issue: IssueFilter { open: Some(false), ..Default::default() }, ..Default::default() }).await, vec![1]);
    assert!(matches!(w.core.pull_views(&w.eve_a(), w.repo, PullFilter::default()).await, Err(Error::NotFound)));
    // A plain issue is not a pull request.
    let issue = w.issue(w.carol, "plain", "").await;
    assert!(matches!(w.core.pull_view(&w.carol_a(), w.repo, issue.issue.number).await, Err(Error::NotFound)));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn files_and_commits_come_from_git(pool: PgPool) {
    let w = World::new(pool).await;
    let n = w.pull(w.bob, "x", "").await.pull.issue.number;
    let files = w.core.pull_files(&w.carol_a(), w.repo, n).await.unwrap();
    assert_eq!(files.files.len(), 1);
    assert_eq!(files.files[0].new_path.as_deref(), Some("src/lib.rs"));
    let commits = w.core.pull_commits(&w.carol_a(), w.repo, n).await.unwrap();
    assert_eq!(commits.len(), 1);
    assert!(matches!(w.core.pull_files(&w.eve_a(), w.repo, n).await, Err(Error::NotFound)));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_trait_objects_agree_with_the_engine(pool: PgPool) {
    use ch_core::services::Pulls;
    let w = World::new(pool).await;
    let state = w.app_state().await;
    let p: &dyn Pulls = &*state.pulls;
    let created = p.create(&w.bob_a(), new_pull(&w, "feature", "main"), &w.c()).await.unwrap();
    assert_eq!(created.issue.number, 1);
    assert_eq!(p.get(&w.carol_a(), w.repo, 1).await.unwrap().head_ref, "feature");
    assert_eq!(p.list(&w.carol_a(), w.repo, IssueFilter { open: Some(true), ..Default::default() }).await.unwrap().len(), 1);
    assert!(p.merge_gate(&w.carol_a(), w.repo, 1).await.unwrap().mergeable);
    assert!(!p.close(&w.bob_a(), w.repo, 1, &w.c()).await.unwrap().issue.open);
}

/// ch-x12: opening a pull request names the opener as the pusher of the head it was opened at.
/// Reopening names nobody, because whoever reopens did not push the head.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn reopening_names_nobody_as_the_pusher(pool: PgPool) {
    let w = World::new(pool).await;
    let v = w.pull(w.carol, "x", "").await;
    let n = v.pull.issue.number;
    let opened: Vec<serde_json::Value> =
        sqlx::query_scalar("SELECT payload FROM outbox_events WHERE kind = 'pr_opened' ORDER BY id").fetch_all(&w.pool).await.unwrap();
    assert_eq!(opened[0]["pusher"].as_i64(), Some(w.carol.0));
    assert_eq!(opened[0]["head_sha"].as_str(), Some(&*v.pull.head_sha));
    w.core.pull_close(&w.carol_a(), w.repo, n, &w.c()).await.unwrap();
    w.core.pull_set_open(&w.bob_a(), w.repo, n, true, &w.c()).await.unwrap();
    let opened: Vec<serde_json::Value> =
        sqlx::query_scalar("SELECT payload FROM outbox_events WHERE kind = 'pr_opened' ORDER BY id").fetch_all(&w.pool).await.unwrap();
    assert_eq!(opened.len(), 2);
    assert!(opened[1]["pusher"].is_null(), "reopening is not pushing: {}", opened[1]);
    assert_eq!(opened[1]["actor"].as_i64(), Some(w.bob.0));
}

/// ch-8tj: someone with no grant on the base sends a pull request from their own fork only.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn without_a_grant_a_pull_request_comes_from_your_own_fork(pool: PgPool) {
    let w = World::new(pool).await;
    sqlx::query("UPDATE repositories SET access_tier = 'internal' WHERE id = $1").bind(w.repo).execute(&w.pool).await.unwrap();
    let fork_of = |owner: ch_core::UserId| {
        let pool = w.pool.clone();
        let base = w.repo;
        async move {
            let id: i64 = sqlx::query_scalar(
                "INSERT INTO repositories (owner_id, name, visibility, fork_parent_id, fork_root_id) VALUES ($1, 'demo', 'public', $2, $2) RETURNING id",
            )
            .bind(owner)
            .bind(base)
            .fetch_one(&pool)
            .await
            .unwrap();
            RepoId(id)
        }
    };
    let bobs = fork_of(w.bob).await;
    let eves = fork_of(w.eve).await;
    w.git.branch(bobs, "patch", &sha_of(9));
    w.git.branch(eves, "patch", &sha_of(9));
    let mut np = new_pull(&w, "patch", "main");
    np.head_repo_id = bobs;
    let r = w.core.pull_create(&w.a(w.eve), np.clone(), &w.c()).await;
    assert!(matches!(&r, Err(Error::Validation(v)) if format!("{v:?}").contains("a fork you own")), "someone else's fork: {r:?}");
    np.head_ref = "feature".into();
    np.head_repo_id = w.repo;
    let r = w.core.pull_create(&w.a(w.eve), np.clone(), &w.c()).await;
    assert!(matches!(&r, Err(Error::Validation(v)) if format!("{v:?}").contains("a fork you own")), "a branch of the base: {r:?}");
    np.head_ref = "patch".into();
    np.head_repo_id = eves;
    let v = w.core.pull_create(&w.a(w.eve), np, &w.c()).await.unwrap();
    assert_eq!(v.pull.head_repo_id, Some(eves));
}
