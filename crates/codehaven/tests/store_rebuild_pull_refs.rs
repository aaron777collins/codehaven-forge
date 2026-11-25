//! Widening a repository rebuilds its store (ch-4pn). The pull refs it keeps are the ones the
//! pull requests still stand on: a ref that no pull request holds, or one that is not at its
//! pull request's stored head (a force-pushed or hand-moved one), is not carried into the new
//! store, and the ref is written again from the stored head when that commit is still there
//! (ch-4pn.16).

#![allow(clippy::unwrap_used)]

use ch_core::model::*;
use ch_core::state::{AppState, AppStateBuilder};
use ch_core::{Actor, AuthVia, Config, Install, RepoId, UserId};
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

async fn has_object(h: &H, r: RepoId, sha: &str) -> bool {
    h.s.git.commit(r, sha).await.is_ok()
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_rebuild_keeps_only_the_pull_refs_the_pull_requests_stand_on(pool: PgPool) {
    let h = harness(pool).await;
    let alice = user(&h, "alice").await;
    let base = repo(&h, &alice, "base", Visibility::Private).await;
    let main = h.s.git.resolve(base, "refs/heads/main").await.unwrap();
    // Pull request 1, from a branch of the same repository, and its ref.
    h.s.git.create_ref(&alice.1, base, "refs/heads/feat", &main, &c()).await.unwrap();
    commit(&h, &alice.1, base, "feat", "feature.txt", "the feature").await;
    let n = open(&h, &alice.1, base, base, "feat").await.unwrap();
    assert_eq!(n, 1);
    settle(&h).await;
    let head: String = sqlx::query_scalar("SELECT p.head_sha FROM pull_requests p JOIN issues i ON i.id = p.issue_id WHERE i.repo_id = $1")
        .bind(base.0)
        .fetch_one(&h.s.db)
        .await
        .unwrap();
    assert_eq!(h.s.git.resolve(base, "refs/pull/1/head").await.unwrap(), head);

    // A commit that only a stale ref holds: no pull request 9 exists.
    h.s.git.create_ref(&alice.1, base, "refs/heads/tmp", &main, &c()).await.unwrap();
    let stale = commit(&h, &alice.1, base, "tmp", "stale.txt", "a pull request that is gone").await;
    h.s.git.create_ref(&Actor::System, base, "refs/pull/9/head", &stale, &c()).await.unwrap();
    // And a commit that pull request 1's ref was moved to, away from its stored head.
    h.s.git.create_ref(&alice.1, base, "refs/heads/tmp2", &main, &c()).await.unwrap();
    let moved = commit(&h, &alice.1, base, "tmp2", "moved.txt", "a force-pushed head").await;
    h.s.git.update_ref(&Actor::System, base, "refs/pull/1/head", &moved, true, &c()).await.unwrap();
    for b in ["tmp", "tmp2"] {
        h.s.git.delete_ref(&alice.1, base, &format!("refs/heads/{b}"), &c()).await.unwrap();
    }
    assert!(has_object(&h, base, &stale).await && has_object(&h, base, &moved).await, "both are in the store before");

    h.s.repos.set_tier(&alice.1, base, Visibility::Internal, &c()).await.unwrap();

    assert!(h.s.git.resolve(base, "refs/pull/9/head").await.is_err(), "a ref no pull request holds is not carried");
    assert!(!has_object(&h, base, &stale).await, "and its commit is gone");
    assert_eq!(h.s.git.resolve(base, "refs/pull/1/head").await.unwrap(), head, "the ref is written again from the stored head");
    assert!(!has_object(&h, base, &moved).await, "the commit it was moved to is gone");
    assert!(has_object(&h, base, &head).await);
    assert_eq!(h.s.git.resolve(base, "refs/heads/feat").await.unwrap(), head);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_rebuild_carries_the_pull_ref_of_a_head_that_only_the_ref_holds(pool: PgPool) {
    let h = harness(pool).await;
    let alice = user(&h, "alice").await;
    let bob = user(&h, "bob").await;
    let base = repo(&h, &alice, "base", Visibility::Private).await;
    h.s.repos.set_collaborator(&alice.1, base, bob.0, ch_core::Permission::Read, &c()).await.unwrap();
    let fork = h.s.repos.fork(&bob.1, base, bob.0, &c()).await.unwrap().id;
    commit(&h, &bob.1, fork, "main", "from-fork.txt", "work in the fork").await;
    let n = open(&h, &bob.1, base, fork, "main").await.unwrap();
    settle(&h).await;
    let head = h.s.git.resolve(base, &format!("refs/pull/{n}/head")).await.unwrap();
    assert!(has_object(&h, base, &head).await, "copied into the base by the pull request");
    h.s.repos.set_tier(&alice.1, base, Visibility::Internal, &c()).await.unwrap();
    assert_eq!(h.s.git.resolve(base, &format!("refs/pull/{n}/head")).await.unwrap(), head, "the ref and its commit survive the rebuild");
    assert!(has_object(&h, base, &head).await);
}
