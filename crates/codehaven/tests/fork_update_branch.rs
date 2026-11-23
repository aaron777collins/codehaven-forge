//! ch-x12: a writer's click never runs a non-writer's waiting commit. Update branch on a pull
//! request from a fork writes a merge commit that holds whatever the fork's branch holds, so the
//! person who clicked is not counted for a commit underneath that still waits for approval.
//! Real services, real git repositories, a real database, and the real event consumers.

#![allow(clippy::unwrap_used)]

use ch_core::model::*;
use ch_core::state::{AppState, AppStateBuilder};
use ch_core::{Actor, AuthVia, Config, Install, Permission, RepoId, UserId};
use ch_db::{FsBlobStore, PgAudit, PgEventBus};
use sqlx::PgPool;
use std::collections::BTreeMap;
use std::sync::Arc;

const WORKFLOW: &str = ".codehaven/workflows/ci.yml";

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

/// Runs the branch tracker and the CI trigger consumer until neither has anything left.
async fn settle(h: &H) {
    let core = ch_collab::Core::new(ch_collab::Deps::from_state(&h.s));
    let ctx = ch_ci::ctx::Ctx::from_state(&h.s);
    let mut failures = BTreeMap::new();
    for _ in 0..10 {
        let a = ch_collab::sync::run_pr_sync_once(&core).await.unwrap();
        let b = ch_ci::triggers::process_batch(&ctx, &mut failures).await.unwrap();
        if a == 0 && b == 0 {
            return;
        }
    }
    panic!("the event consumers did not settle");
}

async fn runs(h: &H, r: RepoId) -> Vec<(String, String)> {
    sqlx::query_as("SELECT head_sha, status FROM workflow_runs WHERE repo_id = $1 ORDER BY id").bind(r.0).fetch_all(&h.s.db).await.unwrap()
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn update_branch_does_not_run_a_waiting_commit_from_someone_without_write_access(pool: PgPool) {
    let h = harness(pool).await;
    let alice = user(&h, "alice").await;
    let carol = user(&h, "carol").await;
    let dave = user(&h, "dave").await;
    let demo =
        h.s.repos
            .create(
                &alice.1,
                NewRepo {
                    owner_id: alice.0,
                    name: "demo".into(),
                    description: String::new(),
                    visibility: Visibility::Private,
                    default_branch: None,
                    auto_init: true,
                },
                &c(),
            )
            .await
            .unwrap()
            .id;
    let wf = "on: [pull_request]\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n";
    commit(&h, &alice.1, demo, "main", WORKFLOW, wf).await;
    h.s.repos.set_collaborator(&alice.1, demo, carol.0, Permission::Write, &c()).await.unwrap();
    h.s.repos.set_collaborator(&alice.1, demo, dave.0, Permission::Read, &c()).await.unwrap();

    // Carol forks, and lets Dave push to her fork. Dave can only read alice/demo.
    let fork = h.s.repos.fork(&carol.1, demo, carol.0, &c()).await.unwrap().id;
    h.s.repos.set_collaborator(&carol.1, fork, dave.0, Permission::Write, &c()).await.unwrap();
    let main = h.s.git.resolve(fork, "refs/heads/main").await.unwrap();
    h.s.git.create_ref(&carol.1, fork, "ub", &main, &c()).await.unwrap();
    commit(&h, &carol.1, fork, "ub", "carol.txt", "carol's").await;
    settle(&h).await;

    // Carol opens the pull request. She can write here, so it runs at once.
    let pr =
        h.s.pulls
            .create(
                &carol.1,
                NewPull {
                    repo_id: demo,
                    title: "Carol's change".into(),
                    body: String::new(),
                    head_repo_id: fork,
                    head_ref: "ub".into(),
                    base_ref: "main".into(),
                    draft: false,
                },
                &c(),
            )
            .await
            .unwrap();
    let number = pr.issue.number;
    settle(&h).await;
    assert_eq!(runs(&h, demo).await.len(), 1, "a writer's own pull request runs");

    // Dave changes the workflow on Carol's branch. It waits for approval.
    let bad = "on: [pull_request]\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo DAVE-WF-RAN\n";
    let daves = commit(&h, &dave.1, fork, "ub", WORKFLOW, bad).await;
    settle(&h).await;
    assert_eq!(runs(&h, demo).await.len(), 1, "dave's commit waits");
    let ctx = ch_ci::ctx::Ctx::from_state(&h.s);
    assert_eq!(ch_ci::triggers::pending_fork_approval(&ctx, demo, number).await.unwrap(), Some(daves.clone()));

    // Main moves on, and Carol clicks Update branch.
    commit(&h, &alice.1, demo, "main", "main.txt", "main moves").await;
    settle(&h).await;
    let view = h.s.pulls.update_branch(&carol.1, demo, number, &c()).await.unwrap();
    let merged = view.pull.head_sha.clone();
    let parents = h.s.git.commit(fork, &merged).await.unwrap().parents;
    assert!(parents.contains(&daves), "the merge commit holds dave's commit: {parents:?}");
    settle(&h).await;
    let now = runs(&h, demo).await;
    assert_eq!(now.len(), 1, "carol's click ran dave's waiting commit: {now:?}");
    assert_eq!(
        ch_ci::triggers::pending_fork_approval(&ctx, demo, number).await.unwrap(),
        Some(merged.clone()),
        "the merge commit waits for approval like dave's did"
    );

    // Someone with write access approves the merge commit, and only then it runs.
    let made = ch_ci::triggers::approve_fork_commit(&ctx, demo, number, &merged, alice.0).await.unwrap();
    assert_eq!(made.len(), 1);
    assert_eq!(runs(&h, demo).await.last().unwrap().0, merged);
}
