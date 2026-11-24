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
