//! What a pull request may copy between repositories, and what it may merge (ch-g3r, ch-cjp,
//! ch-m2y). Real services, real git repositories, a real database.
//!
//! - Reading someone's private fork is not enough to publish its commits into a base through a
//!   pull request, and a later head is copied only while the author may still share it.
//! - `refs/pull/<n>/head` follows the head.
//! - Auto-merge is armed for one head: a push by someone without write access on the base turns
//!   it off, and so does a base change by the author.

#![allow(clippy::unwrap_used)]

use ch_core::collab::PullPatch;
use ch_core::model::*;
use ch_core::state::{AppState, AppStateBuilder};
use ch_core::{Actor, AuthVia, Config, Install, Permission, RepoId, UserId};
use ch_db::{FsBlobStore, PgAudit, PgEventBus};
use sqlx::PgPool;
use std::sync::Arc;

struct H {
    s: AppState,
    _dir: tempfile::TempDir,
}

async fn harness(pool: PgPool) -> H {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::for_tests();
    cfg.storage.data_dir = dir.path().to_path_buf();
    cfg.server.external_url = "https://code.example.com".into();
    let cfg = Arc::new(cfg);
    let blobs = FsBlobStore::new(cfg.blobs_dir());
    blobs.init().await.unwrap();
    let mut b =
        AppStateBuilder::new(cfg, pool.clone(), Arc::new(PgAudit::new(pool.clone())), Arc::new(PgEventBus::new(pool)), Arc::new(blobs));
    ch_auth::build_with(&b, ch_auth::AuthOptions { hash_params: ch_auth::password::HashParams::FAST_FOR_TESTS, ..Default::default() })
        .unwrap()
        .install(&mut b);
    ch_notify::build(&b).unwrap().install(&mut b);
    ch_git::build(&b).unwrap().install(&mut b);
    ch_collab::build(&b).unwrap().install(&mut b);
    ch_ci::build(&b).unwrap().install(&mut b);
    H { s: b.build(), _dir: dir }
}

fn c() -> ClientInfo {
    ClientInfo::local()
}

async fn user(h: &H, name: &str) -> (UserId, Actor) {
    let u = h
        .s
        .accounts
        .create_user(
            &Actor::System,
            NewUser { username: name.into(), email: format!("{name}@example.org"), display_name: None, is_admin: false, can_run_ci: true },
            &c(),
        )
        .await
        .unwrap();
    (u.id, Actor::User { id: u.id, is_admin: false, has_2fa: true, via: AuthVia::Session })
}

async fn commit(h: &H, who: &Actor, r: RepoId, branch: &str, path: &str, text: &str) -> String {
    let expected = h.s.git.resolve(r, &format!("refs/heads/{branch}")).await.unwrap();
    h.s.git
        .commit_file_change(
            who,
            CommitFileChange {
                repo_id: r,
                branch: branch.into(),
                new_branch: None,
                expected_sha: Some(expected),
                changes: vec![FileChange { path: path.into(), content: Some(text.as_bytes().to_vec()), renamed_from: None }],
                message: format!("Change {path}"),
                author: Signature { name: "A".into(), email: "a@example.org".into(), at: chrono::Utc::now() },
            },
            &c(),
        )
        .await
        .unwrap()
}

/// Runs the branch tracker until it has nothing left.
async fn settle(h: &H) {
    let core = ch_collab::Core::new(ch_collab::Deps::from_state(&h.s));
    for _ in 0..10 {
        if ch_collab::sync::run_pr_sync_once(&core).await.unwrap() == 0 {
            return;
        }
    }
    panic!("the branch tracker did not settle");
}

async fn repo(h: &H, who: &(UserId, Actor), name: &str, visibility: Visibility) -> RepoId {
    let new = NewRepo { owner_id: who.0, name: name.into(), description: String::new(), visibility, default_branch: None, auto_init: true };
    h.s.repos.create(&who.1, new, &c()).await.unwrap().id
}

async fn open(h: &H, who: &Actor, base: RepoId, head: RepoId, branch: &str) -> Result<i64, ch_core::Error> {
    let new = NewPull {
        repo_id: base,
        title: "A change".into(),
        body: String::new(),
        head_repo_id: head,
        head_ref: branch.into(),
        base_ref: "main".into(),
        draft: false,
    };
    h.s.pulls.create(who, new, &c()).await.map(|p| p.issue.number)
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_reader_of_someone_elses_private_fork_cannot_publish_it(pool: PgPool) {
    let h = harness(pool).await;
    let alice = user(&h, "alice").await;
    let bob = user(&h, "bob").await;
    let carol = user(&h, "carol").await;
    let base = repo(&h, &alice, "base", Visibility::Internal).await;
    // Carol has a grant on the base, so she may open pull requests from forks that are not hers.
    h.s.repos.set_collaborator(&alice.1, base, carol.0, Permission::Read, &c()).await.unwrap();
    // Bob's fork is private, and he lets Carol read it.
    let fork = h.s.repos.fork(&bob.1, base, bob.0, &c()).await.unwrap().id;
    h.s.repos.set_collaborator(&bob.1, fork, carol.0, Permission::Read, &c()).await.unwrap();
    commit(&h, &bob.1, fork, "main", "secret.txt", "bob's private work").await;
    let secret = h.s.git.resolve(fork, "refs/heads/main").await.unwrap();

    let r = open(&h, &carol.1, base, fork, "main").await;
    assert!(matches!(&r, Err(ch_core::Error::Validation(v)) if v[0].field == "head"), "{r:?}");
    assert!(h.s.git.commit(base, &secret).await.is_err(), "nothing was copied into the base");
    // Bob shares his own fork, which is his to share.
    open(&h, &bob.1, base, fork, "main").await.unwrap();
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_later_head_is_copied_only_while_its_author_may_share_it(pool: PgPool) {
    let h = harness(pool).await;
    let alice = user(&h, "alice").await;
    let bob = user(&h, "bob").await;
    let carol = user(&h, "carol").await;
    let base = repo(&h, &alice, "base", Visibility::Internal).await;
    h.s.repos.set_collaborator(&alice.1, base, carol.0, Permission::Read, &c()).await.unwrap();
    let fork = h.s.repos.fork(&bob.1, base, bob.0, &c()).await.unwrap().id;
    // Carol may write to Bob's fork, so she may share it.
    h.s.repos.set_collaborator(&bob.1, fork, carol.0, Permission::Write, &c()).await.unwrap();
    let first = commit(&h, &carol.1, fork, "main", "carol.txt", "carol's change").await;
    let n = open(&h, &carol.1, base, fork, "main").await.unwrap();
    settle(&h).await;
    assert_eq!(h.s.git.resolve(base, &format!("refs/pull/{n}/head")).await.unwrap(), first);

    // Bob takes Carol's access away and pushes private work to the same branch.
    h.s.repos.remove_collaborator(&bob.1, fork, carol.0, &c()).await.unwrap();
    let later = commit(&h, &bob.1, fork, "main", "later.txt", "bob only").await;
    settle(&h).await;
    let view = h.s.pulls.view(&alice.1, base, n).await.unwrap();
    assert_eq!(view.pull.head_sha, first, "the pull request keeps the head Carol could share");
    assert_eq!(h.s.git.resolve(base, &format!("refs/pull/{n}/head")).await.unwrap(), first);
    assert!(h.s.git.commit(base, &later).await.is_err(), "Bob's later commit was not copied into the base");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_pull_ref_follows_the_head(pool: PgPool) {
    let h = harness(pool).await;
    let alice = user(&h, "alice").await;
    let bob = user(&h, "bob").await;
    let base = repo(&h, &alice, "base", Visibility::Internal).await;
    let fork = h.s.repos.fork(&bob.1, base, bob.0, &c()).await.unwrap().id;
    commit(&h, &bob.1, fork, "main", "one.txt", "1").await;
    let n = open(&h, &bob.1, base, fork, "main").await.unwrap();
    settle(&h).await;
    let second = commit(&h, &bob.1, fork, "main", "two.txt", "2").await;
    settle(&h).await;
    assert_eq!(h.s.git.resolve(base, &format!("refs/pull/{n}/head")).await.unwrap(), second, "ch-cjp: no longer stuck at the first head");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn auto_merge_is_armed_for_one_head_and_one_base(pool: PgPool) {
    let h = harness(pool).await;
    let alice = user(&h, "alice").await;
    let bob = user(&h, "bob").await;
    let base = repo(&h, &alice, "base", Visibility::Internal).await;
    let main = h.s.git.resolve(base, "refs/heads/main").await.unwrap();
    h.s.git.create_ref(&alice.1, base, "release", &main, &c()).await.unwrap();
    // One approval is needed, so auto-merge has something to wait for.
    let rule = BranchProtection { pattern: "main".into(), required_approvals: 1, ..Default::default() };
    h.s.repos.set_protection(&alice.1, base, rule, &c()).await.unwrap();
    let fork = h.s.repos.fork(&bob.1, base, bob.0, &c()).await.unwrap().id;
    commit(&h, &bob.1, fork, "main", "one.txt", "1").await;
    let n = open(&h, &bob.1, base, fork, "main").await.unwrap();
    settle(&h).await;

    // Alice arms it. Bob, who cannot write to the base, pushes new code: it turns off.
    h.s.pulls.set_auto_merge(&alice.1, base, n, Some(MergeMethod::Merge), &c()).await.unwrap();
    assert!(h.s.pulls.view(&alice.1, base, n).await.unwrap().auto_merge.is_some());
    commit(&h, &bob.1, fork, "main", "two.txt", "unreviewed").await;
    settle(&h).await;
    assert!(h.s.pulls.view(&alice.1, base, n).await.unwrap().auto_merge.is_none(), "a fork owner's push turns auto-merge off");

    // Armed again, then Bob moves the base: it turns off again.
    h.s.pulls.set_auto_merge(&alice.1, base, n, Some(MergeMethod::Merge), &c()).await.unwrap();
    let patch = PullPatch { title: None, body: None, base: Some("release".into()) };
    h.s.pulls.update(&bob.1, base, n, patch, &c()).await.unwrap();
    assert!(h.s.pulls.view(&alice.1, base, n).await.unwrap().auto_merge.is_none(), "the author's retarget turns auto-merge off");
}

async fn make_public(h: &H, r: RepoId) {
    sqlx::query("UPDATE visibility_settings SET allow_public_repos = true").execute(&h.s.db).await.unwrap();
    sqlx::query("UPDATE repositories SET access_tier = 'public', visibility = 'public', public_since = now() WHERE id = $1")
        .bind(r.0)
        .execute(&h.s.db)
        .await
        .unwrap();
}

fn new_pull(base: RepoId, head: RepoId, branch: &str) -> NewPull {
    NewPull {
        repo_id: base,
        title: "A change".into(),
        body: String::new(),
        head_repo_id: head,
        head_ref: branch.into(),
        base_ref: "main".into(),
        draft: false,
    }
}

async fn publish_tier(h: &H, base: RepoId, n: i64) -> Option<String> {
    sqlx::query_scalar(
        "SELECT p.head_publish_tier FROM pull_requests p JOIN issues i ON i.id = p.issue_id WHERE i.repo_id = $1 AND i.number = $2",
    )
    .bind(base.0)
    .bind(n)
    .fetch_one(&h.s.db)
    .await
    .unwrap()
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_pull_request_from_a_less_visible_fork_into_a_public_base_needs_a_publish_confirm(pool: PgPool) {
    let h = harness(pool).await;
    let alice = user(&h, "alice").await;
    let bob = user(&h, "bob").await;
    let base = repo(&h, &alice, "base", Visibility::Internal).await;
    make_public(&h, base).await;
    let fork = h.s.repos.fork(&bob.1, base, bob.0, &c()).await.unwrap().id;
    commit(&h, &bob.1, fork, "main", "private-work.txt", "work nobody has seen").await;
    let work = h.s.git.resolve(fork, "refs/heads/main").await.unwrap();

    // Without the confirm: refused with the reason, and nothing was copied into the public base.
    let r = h.s.pulls.create(&bob.1, new_pull(base, fork, "main"), &c()).await;
    assert!(matches!(&r, Err(ch_core::Error::Validation(v)) if v[0].field == "publish_head"), "{r:?}");
    assert!(h.s.git.commit(base, &work).await.is_err(), "nothing was copied");
    let r = h.s.pulls.create_publishing(&bob.1, new_pull(base, fork, "main"), false, &c()).await;
    assert!(matches!(&r, Err(ch_core::Error::Validation(v)) if v[0].field == "publish_head"), "{r:?}");
    // With it: opened, copied, and the tier it was published to is recorded.
    let pr = h.s.pulls.create_publishing(&bob.1, new_pull(base, fork, "main"), true, &c()).await.unwrap();
    assert_eq!(publish_tier(&h, base, pr.issue.number).await.as_deref(), Some("public"));
    assert!(h.s.git.commit(base, &work).await.is_ok(), "the head is in the base now, by his say-so");

    // A base nobody outside the instance can read needs no confirm; the tier is still recorded.
    let inner = repo(&h, &alice, "inner", Visibility::Internal).await;
    let fork2 = h.s.repos.fork(&bob.1, inner, bob.0, &c()).await.unwrap().id;
    commit(&h, &bob.1, fork2, "main", "more.txt", "more work").await;
    let pr2 = h.s.pulls.create(&bob.1, new_pull(inner, fork2, "main"), &c()).await.unwrap();
    assert_eq!(publish_tier(&h, inner, pr2.issue.number).await.as_deref(), Some("internal"), "the tier it was published to");
    // A branch of the public base itself is already public: no confirm.
    h.s.git.create_ref(&alice.1, base, "refs/heads/topic", &h.s.git.resolve(base, "refs/heads/main").await.unwrap(), &c()).await.unwrap();
    commit(&h, &alice.1, base, "topic", "t.txt", "t").await;
    let pr3 = h.s.pulls.create(&alice.1, NewPull { base_ref: "main".into(), ..new_pull(base, base, "topic") }, &c()).await.unwrap();
    assert_eq!(publish_tier(&h, base, pr3.issue.number).await, None);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_later_head_is_not_copied_into_a_base_that_became_public_after_the_pull_request(pool: PgPool) {
    let h = harness(pool).await;
    let alice = user(&h, "alice").await;
    let bob = user(&h, "bob").await;
    let base = repo(&h, &alice, "base", Visibility::Internal).await;
    let fork = h.s.repos.fork(&bob.1, base, bob.0, &c()).await.unwrap().id;
    commit(&h, &bob.1, fork, "main", "one.txt", "one").await;
    let n = open(&h, &bob.1, base, fork, "main").await.unwrap();
    settle(&h).await;
    let first = h.s.git.resolve(base, &format!("refs/pull/{n}/head")).await.unwrap();
    // The base is made public later. Bob never agreed to publish anything there.
    make_public(&h, base).await;
    commit(&h, &bob.1, fork, "main", "two.txt", "two, not yet agreed to be public").await;
    let second = h.s.git.resolve(fork, "refs/heads/main").await.unwrap();
    settle(&h).await;
    assert_eq!(h.s.git.resolve(base, &format!("refs/pull/{n}/head")).await.unwrap(), first, "the pull request keeps the head it had");
    assert!(h.s.git.commit(base, &second).await.is_err(), "the new commit was not copied into the public base");
}

async fn allow_public(h: &H) {
    sqlx::query("UPDATE visibility_settings SET allow_public_repos = true").execute(&h.s.db).await.unwrap();
}

/// A private fork's commits were copied into an internal base. The base owner makes the base
/// public. The author never agreed to that, so the copy is withdrawn and not written again.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn making_a_base_public_withdraws_the_private_commits_copied_into_it(pool: PgPool) {
    let h = harness(pool).await;
    let alice = user(&h, "alice").await;
    let carol = user(&h, "carol").await;
    let base = repo(&h, &alice, "base", Visibility::Internal).await;
    allow_public(&h).await;
    let fork = h.s.repos.fork(&carol.1, base, carol.0, &c()).await.unwrap().id;
    let first = commit(&h, &carol.1, fork, "main", "carol-private-1.txt", "private work").await;
    let n = open(&h, &carol.1, base, fork, "main").await.unwrap();
    settle(&h).await;
    assert_eq!(h.s.git.resolve(base, &format!("refs/pull/{n}/head")).await.unwrap(), first, "copied while the base is internal");

    h.s.repos.set_tier(&alice.1, base, Visibility::Public, &c()).await.unwrap();

    assert!(h.s.git.resolve(base, &format!("refs/pull/{n}/head")).await.is_err(), "the pull ref is not carried into the public base");
    assert!(h.s.git.commit(base, &first).await.is_err(), "and neither is the commit");
    // The pull request still reads from the fork, and a later push is not copied either.
    let view = h.s.pulls.view(&alice.1, base, n).await.unwrap();
    assert_eq!(view.pull.head_sha, first);
    let later = commit(&h, &carol.1, fork, "main", "carol-private-2.txt", "more private work").await;
    settle(&h).await;
    assert!(h.s.git.resolve(base, &format!("refs/pull/{n}/head")).await.is_err(), "no ref is written");
    assert!(h.s.git.commit(base, &later).await.is_err(), "the later commit was not copied");
    // The author's own say-so is what publishes it: it cannot be merged in until then.
    let r = h.s.pulls.merge(&alice.1, base, n, MergeMethod::Merge, &c()).await;
    assert!(matches!(r, Err(ch_core::Error::MergeBlocked(_))), "{r:?}");
    assert!(h.s.git.commit(base, &first).await.is_err(), "nothing was merged");
}

/// The author agreed to publish a private head into a public base. That agreement survives the
/// base being made private and public again, and covers later heads.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_head_the_author_agreed_to_publish_survives_the_base_being_widened_again(pool: PgPool) {
    let h = harness(pool).await;
    let alice = user(&h, "alice").await;
    let bob = user(&h, "bob").await;
    let base = repo(&h, &alice, "base", Visibility::Internal).await;
    make_public(&h, base).await;
    let fork = h.s.repos.fork(&bob.1, base, bob.0, &c()).await.unwrap().id;
    let first = commit(&h, &bob.1, fork, "main", "one.txt", "one").await;
    let pr = h.s.pulls.create_publishing(&bob.1, new_pull(base, fork, "main"), true, &c()).await.unwrap();
    let n = pr.issue.number;
    settle(&h).await;

    h.s.repos.set_tier(&alice.1, base, Visibility::Private, &c()).await.unwrap();
    h.s.repos.set_tier(&alice.1, base, Visibility::Public, &c()).await.unwrap();
    assert_eq!(h.s.git.resolve(base, &format!("refs/pull/{n}/head")).await.unwrap(), first, "he said yes, so the copy stays");

    let second = commit(&h, &bob.1, fork, "main", "two.txt", "two").await;
    settle(&h).await;
    assert_eq!(h.s.git.resolve(base, &format!("refs/pull/{n}/head")).await.unwrap(), second, "and a later head follows");
}

/// A pull request from a public fork is not a consent to publish what the fork becomes later.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_fork_that_was_public_and_became_private_does_not_publish_new_commits(pool: PgPool) {
    let h = harness(pool).await;
    let alice = user(&h, "alice").await;
    let bob = user(&h, "bob").await;
    let base = repo(&h, &alice, "base", Visibility::Internal).await;
    make_public(&h, base).await;
    let fork = h.s.repos.fork(&bob.1, base, bob.0, &c()).await.unwrap().id;
    make_public(&h, fork).await;
    let first = commit(&h, &bob.1, fork, "main", "b-public.txt", "public work").await;
    let n = open(&h, &bob.1, base, fork, "main").await.unwrap();
    assert_eq!(publish_tier(&h, base, n).await, None, "no consent was needed, and none was given");
    settle(&h).await;
    assert_eq!(h.s.git.resolve(base, &format!("refs/pull/{n}/head")).await.unwrap(), first);

    // Bob makes the fork private and keeps working in it.
    h.s.repos.set_tier(&bob.1, fork, Visibility::Private, &c()).await.unwrap();
    let later = commit(&h, &bob.1, fork, "main", "b.txt", "private now").await;
    settle(&h).await;
    assert_eq!(h.s.git.resolve(base, &format!("refs/pull/{n}/head")).await.unwrap(), first, "the pull request keeps the head it had");
    assert!(h.s.git.commit(base, &later).await.is_err(), "the private commit was not copied into the public base");
}

/// The same pull request, after the base went public: a private fork that never agreed to
/// publish is not shown through the pull request's files and commits to anyone who cannot read
/// that fork, signed out included (ch-4pn.14, review round 2).
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_withheld_head_is_not_shown_to_people_who_cannot_read_its_fork(pool: PgPool) {
    let h = harness(pool).await;
    let alice = user(&h, "alice").await;
    let bob = user(&h, "bob").await;
    let carol = user(&h, "carol").await;
    let base = repo(&h, &alice, "base", Visibility::Internal).await;
    allow_public(&h).await;
    let fork = h.s.repos.fork(&bob.1, base, bob.0, &c()).await.unwrap().id;
    commit(&h, &bob.1, fork, "main", "topsecret.txt", "TOPSECRET-CONTENT").await;
    let n = open(&h, &bob.1, base, fork, "main").await.unwrap();
    settle(&h).await;
    // While the base is internal the people who read it see the pull request's changes.
    assert_eq!(h.s.pulls.files(&carol.1, base, n).await.unwrap().files.len(), 1);

    h.s.repos.set_tier(&alice.1, base, Visibility::Public, &c()).await.unwrap();
    settle(&h).await;

    // Bob can read his own fork, so he still sees what he sent.
    let mine = h.s.pulls.files(&bob.1, base, n).await.unwrap();
    assert!(mine.files.iter().any(|f| f.new_path.as_deref() == Some("topsecret.txt")));
    assert_eq!(h.s.pulls.commits(&bob.1, base, n).await.unwrap().len(), 1);
    // Nobody else does, and a signed-out reader above all.
    for (who, label) in [(Actor::Anonymous, "signed out"), (carol.1.clone(), "carol"), (alice.1.clone(), "alice")] {
        let files = h.s.pulls.files(&who, base, n).await.unwrap();
        assert!(files.files.is_empty(), "{label} must not see the private diff");
        let commits = h.s.pulls.commits(&who, base, n).await.unwrap();
        assert!(commits.is_empty(), "{label} must not see the private commit messages");
    }
}

/// Inline review comments keep a piece of the diff (the hunk) and the file's path. Once the base
/// is public and the head is withheld, those are code from a private fork too. Everyone who
/// cannot read the fork gets them blank, in the thread list, in a single comment and in the
/// repository-wide list. The rows are not touched: whoever can read the fork still sees them
/// (ch-4pn.14, review round 3).
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_withheld_head_does_not_leak_its_code_through_review_comments(pool: PgPool) {
    let h = harness(pool).await;
    let alice = user(&h, "alice").await;
    let bob = user(&h, "bob").await;
    let carol = user(&h, "carol").await;
    let base = repo(&h, &alice, "base", Visibility::Internal).await;
    allow_public(&h).await;
    let fork = h.s.repos.fork(&bob.1, base, bob.0, &c()).await.unwrap().id;
    commit(&h, &bob.1, fork, "main", "a5-file.txt", "line one\nA5-HUNK-SECRET-LINE\nline three\n").await;
    let n = open(&h, &bob.1, base, fork, "main").await.unwrap();
    settle(&h).await;
    // While the base is internal alice reads the diff, so she can comment on a line of it.
    let new = NewReviewComment {
        path: "a5-file.txt".into(),
        side: "right".into(),
        line: 2,
        start_line: None,
        body: "Why this line?".into(),
        in_reply_to: None,
    };
    let posted = h.s.reviews.add_comment(&alice.1, base, n, new, None, &c()).await.unwrap();
    assert!(posted.comment.diff_hunk.contains("A5-HUNK-SECRET-LINE"), "the hunk is stored: {}", posted.comment.diff_hunk);
    let id = posted.comment.id;

    h.s.repos.set_tier(&alice.1, base, Visibility::Public, &c()).await.unwrap();
    settle(&h).await;

    let core = ch_collab::Core::new(ch_collab::Deps::from_state(&h.s));
    for (who, label) in [(Actor::Anonymous, "signed out"), (carol.1.clone(), "carol"), (alice.1.clone(), "alice")] {
        let threads = h.s.reviews.threads(&who, base, n).await.unwrap();
        assert_eq!(threads.len(), 1, "{label}: the conversation itself is still there");
        assert!(threads[0].path.is_empty(), "{label}: the thread names no file");
        for cm in &threads[0].comments {
            assert_eq!(cm.comment.body, "Why this line?", "{label}: the words stay");
            assert!(cm.comment.diff_hunk.is_empty(), "{label}: no hunk, got {:?}", cm.comment.diff_hunk);
            assert!(cm.comment.path.is_empty(), "{label}: no path, got {:?}", cm.comment.path);
        }
        let one = h.s.reviews.comment(&who, base, id).await.unwrap();
        assert!(one.comment.diff_hunk.is_empty() && one.comment.path.is_empty(), "{label}: single comment");
        let all = core.repo_review_comment_views(&who, base, None, "created", "asc", 1, 30).await.unwrap();
        assert_eq!(all.len(), 1, "{label}");
        assert!(all[0].0.comment.diff_hunk.is_empty() && all[0].0.comment.path.is_empty(), "{label}: repository-wide list");
    }
    // Bob can read his own fork, so he still sees the code the comment points at.
    let mine = h.s.reviews.threads(&bob.1, base, n).await.unwrap();
    assert!(mine[0].comments[0].comment.diff_hunk.contains("A5-HUNK-SECRET-LINE"));
    assert_eq!(mine[0].path, "a5-file.txt");
    assert_eq!(h.s.reviews.comment(&bob.1, base, id).await.unwrap().comment.path, "a5-file.txt");
    // Nothing was cleared in the database.
    let stored: String =
        sqlx::query_scalar("SELECT diff_hunk FROM review_comments WHERE id = $1").bind(id.0).fetch_one(&h.s.db).await.unwrap();
    assert!(stored.contains("A5-HUNK-SECRET-LINE"));
}

/// A refresh or a merge that passed its check while the base was internal can reach the git
/// layer after the base was made public. The git layer decides again, under the write ticket,
/// and does not copy a private head into a public base the author did not agree to (ch-4pn.14).
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_late_test_merge_or_merge_does_not_copy_a_private_head_into_a_public_base(pool: PgPool) {
    let h = harness(pool).await;
    let alice = user(&h, "alice").await;
    let bob = user(&h, "bob").await;
    let base = repo(&h, &alice, "base", Visibility::Internal).await;
    allow_public(&h).await;
    let fork = h.s.repos.fork(&bob.1, base, bob.0, &c()).await.unwrap().id;
    let secret = commit(&h, &bob.1, fork, "main", "late.txt", "TOPSECRET-LATE").await;
    let n = open(&h, &bob.1, base, fork, "main").await.unwrap();
    settle(&h).await;
    h.s.repos.set_tier(&alice.1, base, Visibility::Public, &c()).await.unwrap();
    assert!(h.s.git.commit(base, &secret).await.is_err(), "the rebuild withdrew it");

    let r = h.s.git.test_merge(base, "refs/heads/main", fork, &secret).await;
    assert!(r.is_err(), "the test merge must not copy it: {r:?}");
    assert!(h.s.git.commit(base, &secret).await.is_err(), "not copied by the test merge");

    let expected = h.s.git.resolve(base, "refs/heads/main").await.unwrap();
    let req = MergeRequest {
        repo_id: base,
        base_ref: "main".into(),
        head_repo_id: fork,
        head_ref: secret.clone(),
        method: MergeMethod::Merge,
        title: "Late".into(),
        message: String::new(),
        author: Signature { name: "A".into(), email: "a@example.org".into(), at: chrono::Utc::now() },
        committer: Signature { name: "A".into(), email: "a@example.org".into(), at: chrono::Utc::now() },
        expected_base_sha: expected.clone(),
        update_branch: false,
        expected_head_sha: None,
    };
    let r = h.s.git.merge(&alice.1, req, &c()).await;
    assert!(r.is_err(), "the merge must not copy it: {r:?}");
    assert!(h.s.git.commit(base, &secret).await.is_err(), "not copied by the merge");
    assert_eq!(h.s.git.resolve(base, "refs/heads/main").await.unwrap(), expected, "the base did not move");
    let r = h.s.git.create_ref(&Actor::System, base, &format!("refs/pull/{n}/head"), &secret, &c()).await;
    assert!(r.is_err(), "and the pull ref is not written: {r:?}");
}
