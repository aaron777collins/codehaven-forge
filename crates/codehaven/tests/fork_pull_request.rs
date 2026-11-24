//! ch-8tj: a pull request from your own fork, after the base moved on. Forks keep their own
//! objects (C1b), so the base's new commits are not in the fork and the fork's are not in the
//! base. Opening the pull request, its files and commits, and its refresh after the base moves
//! again must all still work. Real services, real git repositories, a real database.

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

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_pull_request_from_your_own_fork_opens_after_the_base_moved(pool: PgPool) {
    let h = harness(pool).await;
    let alice = user(&h, "alice").await;
    let bob = user(&h, "bob").await;
    let intern =
        h.s.repos
            .create(
                &alice.1,
                NewRepo {
                    owner_id: alice.0,
                    name: "intern".into(),
                    description: String::new(),
                    visibility: Visibility::Internal,
                    default_branch: None,
                    auto_init: true,
                },
                &c(),
            )
            .await
            .unwrap()
            .id;
    // Bob has no grant. The tier lets him read and fork.
    let fork = h.s.repos.fork(&bob.1, intern, bob.0, &c()).await.unwrap().id;
    // The base moves on after the fork was made.
    commit(&h, &alice.1, intern, "main", "moved.txt", "alice moved main").await;
    let main = h.s.git.resolve(fork, "refs/heads/main").await.unwrap();
    h.s.git.create_ref(&bob.1, fork, "bob2", &main, &c()).await.unwrap();
    commit(&h, &bob.1, fork, "bob2", "bob.txt", "bob's change").await;

    let pr =
        h.s.pulls
            .create(
                &bob.1,
                NewPull {
                    repo_id: intern,
                    title: "Bob's change".into(),
                    body: String::new(),
                    head_repo_id: fork,
                    head_ref: "bob2".into(),
                    base_ref: "main".into(),
                    draft: false,
                },
                &c(),
            )
            .await
            .unwrap();
    let n = pr.issue.number;
    let paths = |d: &Diff| d.files.iter().filter_map(|f| f.new_path.clone()).collect::<Vec<_>>();
    assert_eq!(paths(&h.s.pulls.files(&bob.1, intern, n).await.unwrap()), vec!["bob.txt"]);
    assert_eq!(h.s.pulls.commits(&bob.1, intern, n).await.unwrap().len(), 1);

    // The base moves again: the pull request still shows only bob's change.
    commit(&h, &alice.1, intern, "main", "later.txt", "and again").await;
    settle(&h).await;
    let view = h.s.pulls.view(&alice.1, intern, n).await.unwrap();
    assert_eq!((view.stats.commits, view.stats.changed_files), (1, 1), "{:?}", view.stats);
    assert_eq!(paths(&h.s.pulls.files(&alice.1, intern, n).await.unwrap()), vec!["bob.txt"]);
}
