//! What a workflow job's own token may do (ch-ctk.8): what `permissions:` asks for, capped by the
//! repository's ceiling and, for a fork, down to reading. Real services, real git, a real
//! database. The job is started by a manual run and put in progress by hand, so its token is the
//! one `Authn` reads, not a fake.

#![allow(clippy::unwrap_used)]

use ch_core::model::*;
use ch_core::state::{AppState, AppStateBuilder};
use ch_core::web_contract::NewRelease;
use ch_core::{Actor, AuthVia, Config, Install, Permission, RepoId, UserId};
use ch_db::{FsBlobStore, PgAudit, PgEventBus};
use sqlx::PgPool;
use std::sync::Arc;

struct H {
    s: AppState,
    pool: PgPool,
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
    let mut b = AppStateBuilder::new(
        cfg,
        pool.clone(),
        Arc::new(PgAudit::new(pool.clone())),
        Arc::new(PgEventBus::new(pool.clone())),
        Arc::new(blobs),
    );
    ch_auth::build_with(&b, ch_auth::AuthOptions { hash_params: ch_auth::password::HashParams::FAST_FOR_TESTS, ..Default::default() })
        .unwrap()
        .install(&mut b);
    ch_notify::build(&b).unwrap().install(&mut b);
    ch_git::build(&b).unwrap().install(&mut b);
    ch_collab::build(&b).unwrap().install(&mut b);
    ch_ci::build(&b).unwrap().install(&mut b);
    H { s: b.build(), pool, _dir: dir }
}

fn c() -> ClientInfo {
    ClientInfo::local()
}

async fn alice(h: &H) -> (UserId, Actor) {
    let u = h
        .s
        .accounts
        .create_user(
            &Actor::System,
            NewUser { username: "alice".into(), email: "alice@example.org".into(), display_name: None, is_admin: false, can_run_ci: true },
            &c(),
        )
        .await
        .unwrap();
    (u.id, Actor::User { id: u.id, is_admin: false, has_2fa: true, via: AuthVia::Session })
}

async fn repo(h: &H, who: &(UserId, Actor)) -> RepoId {
    h.s.repos
        .create(
            &who.1,
            NewRepo {
                owner_id: who.0,
                name: "app".into(),
                description: String::new(),
                visibility: Visibility::Private,
                default_branch: None,
                auto_init: true,
            },
            &c(),
        )
        .await
        .unwrap()
        .id
}

async fn tip(h: &H, r: RepoId) -> String {
    h.s.git.resolve(r, "refs/heads/main").await.unwrap()
}

async fn commit(h: &H, who: &Actor, r: RepoId, path: &str, content: &str) -> ch_core::Result<String> {
    let expected = tip(h, r).await;
    h.s.git
        .commit_file_change(
            who,
            CommitFileChange {
                repo_id: r,
                branch: "main".into(),
                new_branch: None,
                expected_sha: Some(expected),
                changes: vec![FileChange { path: path.into(), content: Some(content.as_bytes().to_vec()), renamed_from: None }],
                message: "change".into(),
                author: Signature { name: "A".into(), email: "a@example.org".into(), at: chrono::Utc::now() },
            },
            &c(),
        )
        .await
}

/// Commits a manual workflow with this `permissions:` block (or none), runs it, and returns the
/// job's actor as `Authn` sees its token.
async fn job_with(h: &H, owner: &(UserId, Actor), r: RepoId, permissions: &str, n: u32) -> Actor {
    let file = format!(".github/workflows/w{n}.yml");
    let wf = format!(
        "name: w\non: workflow_dispatch\n{permissions}jobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n"
    );
    commit(h, &owner.1, r, &file, &wf).await.unwrap();
    let run = h.s.ci.dispatch(&owner.1, r, &format!("w{n}.yml"), "main", serde_json::json!({}), &c()).await.unwrap();
    let token = format!("chj_test{n}abcdefghijklmnopqrstuvwxyz0123456789");
    sqlx::query("UPDATE workflow_jobs SET status = 'in_progress', token_hash = $2 WHERE run_id = $1")
        .bind(run.0)
        .bind(ch_db::tokens::hash_token(&token))
        .execute(&h.pool)
        .await
        .unwrap();
    h.s.authn.from_bearer(&token, &c()).await.unwrap()
}

fn release(r: RepoId, tag: &str) -> NewRelease {
    NewRelease {
        repo_id: r,
        tag_name: tag.into(),
        target: "main".into(),
        name: tag.into(),
        body: String::new(),
        draft: false,
        prerelease: false,
    }
}

fn refused<T: std::fmt::Debug>(r: ch_core::Result<T>) {
    match r {
        Err(ch_core::Error::Forbidden(_)) | Err(ch_core::Error::NotFound) => {}
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_job_token_writes_only_what_its_workflow_asks_for(pool: PgPool) {
    let h = harness(pool).await;
    let a = alice(&h).await;
    let r = repo(&h, &a).await;

    // No `permissions:`: the restricted default reads and writes nothing.
    let plain = job_with(&h, &a, r, "", 1).await;
    assert_eq!(h.s.authz.repo_permission(&plain, r).await.unwrap(), Permission::Read);
    refused(h.s.tracker.create_release(&plain, release(r, "v0"), &c()).await);
    let sha = tip(&h, r).await;
    refused(h.s.checks.create_status(&plain, r, &sha, "success", "ci", None, None).await);

    // `contents: write` makes releases as the bot, but writes no statuses and no workflow files.
    let writer = job_with(&h, &a, r, "permissions:\n  contents: write\n", 2).await;
    assert_eq!(h.s.authz.repo_permission(&writer, r).await.unwrap(), Permission::Write);
    let rel = h.s.tracker.create_release(&writer, release(r, "v1"), &c()).await.unwrap();
    let author: (String, String) =
        sqlx::query_as("SELECT username::text, kind FROM users WHERE id = $1").bind(rel.author_id.0).fetch_one(&h.pool).await.unwrap();
    assert_eq!(author, ("codehaven-actions".to_string(), "bot".to_string()));
    let sha = tip(&h, r).await;
    refused(h.s.checks.create_status(&writer, r, &sha, "success", "ci", None, None).await);
    assert!(commit(&h, &writer, r, "src/main.rs", "fn main() {}\n").await.is_ok(), "contents: write pushes code");
    assert!(commit(&h, &writer, r, ".github/workflows/evil.yml", "on: push\n").await.is_err(), "but never a workflow file");

    // A job block replaces the workflow's: only statuses here.
    let statuses = job_with(&h, &a, r, "permissions:\n  statuses: write\n", 3).await;
    let sha = tip(&h, r).await;
    h.s.checks.create_status(&statuses, r, &sha, "success", "ci", None, None).await.unwrap();
    refused(h.s.tracker.create_release(&statuses, release(r, "v3"), &c()).await);

    // The repository ceiling lowers every token at once, running jobs included.
    sqlx::query("UPDATE repositories SET actions_token_ceiling = 'read' WHERE id = $1").bind(r.0).execute(&h.pool).await.unwrap();
    let capped = job_with(&h, &a, r, "permissions: write-all\n", 4).await;
    assert_eq!(h.s.authz.repo_permission(&capped, r).await.unwrap(), Permission::Read);
    refused(h.s.tracker.create_release(&capped, release(r, "v4"), &c()).await);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn what_a_job_token_does_starts_no_workflow(pool: PgPool) {
    let h = harness(pool).await;
    let a = alice(&h).await;
    let r = repo(&h, &a).await;
    // A workflow that would run on every release.
    commit(
        &h,
        &a.1,
        r,
        ".github/workflows/rel.yml",
        "on: release\njobs:\n  x:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo\n",
    )
    .await
    .unwrap();
    let writer = job_with(&h, &a, r, "permissions:\n  contents: write\n", 1).await;
    let rel = h.s.tracker.create_release(&writer, release(r, "v1"), &c()).await.unwrap();
    let ctx = ch_ci::ctx::Ctx::from_state(&h.s);
    let ev = ch_core::events::StoredEvent {
        id: 1,
        at: chrono::Utc::now(),
        event: ch_core::DomainEvent::ReleasePublished { repo_id: r, release_id: rel.id, tag: "v1".into(), actor: rel.author_id },
    };
    assert!(ch_ci::triggers::handle_event(&ctx, &ev).await.unwrap().is_empty(), "a job's release starts nothing");
    // The same release by a person starts the workflow.
    let ev = ch_core::events::StoredEvent {
        id: 2,
        at: chrono::Utc::now(),
        event: ch_core::DomainEvent::ReleasePublished { repo_id: r, release_id: rel.id, tag: "v1".into(), actor: a.0 },
    };
    assert_eq!(ch_ci::triggers::handle_event(&ctx, &ev).await.unwrap().len(), 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_job_token_never_starts_or_stops_a_run(pool: PgPool) {
    let h = harness(pool).await;
    let a = alice(&h).await;
    let r = repo(&h, &a).await;
    let writer = job_with(&h, &a, r, "permissions:\n  contents: write\n", 1).await;
    assert_eq!(h.s.authz.repo_permission(&writer, r).await.unwrap(), Permission::Write);
    // A run of someone else's making, to rerun and cancel.
    let run = h.s.ci.dispatch(&a.1, r, "w1.yml", "main", serde_json::json!({}), &c()).await.unwrap();
    refused(h.s.ci.dispatch(&writer, r, "w1.yml", "main", serde_json::json!({}), &c()).await);
    refused(h.s.ci.rerun(&writer, r, run, false, &c()).await);
    refused(h.s.ci.cancel(&writer, r, run, &c()).await);
    // The same calls by the person work.
    h.s.ci.cancel(&a.1, r, run, &c()).await.unwrap();
}
