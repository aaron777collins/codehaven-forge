//! A private fork carries its parent's code (ch-4pn.16): someone the fork's owner let in, who
//! cannot see a private repository it was forked from, gets nothing on the fork. The owner keeps
//! their fork.

#![allow(clippy::unwrap_used)]

use ch_core::model::*;
use ch_core::state::{AppState, AppStateBuilder};
use ch_core::{Actor, AuthVia, Config, Install, Permission, RepoId, RunId, UserId};
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
            NewUser { username: name.into(), email: format!("{name}@example.org"), display_name: None, is_admin: false, can_run_ci: false },
            &c(),
        )
        .await
        .unwrap();
    (u.id, Actor::User { id: u.id, is_admin: false, has_2fa: true, via: AuthVia::Session })
}

async fn perm(h: &H, who: &Actor, r: RepoId) -> Permission {
    h.s.authz.repo_permission(who, r).await.unwrap()
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_private_fork_is_closed_to_those_the_parent_is_closed_to(pool: PgPool) {
    let h = harness(pool).await;
    let alice = user(&h, "alice").await;
    let bob = user(&h, "bob").await;
    let carol = user(&h, "carol").await;
    let parent =
        h.s.repos
            .create(
                &alice.1,
                NewRepo {
                    owner_id: alice.0,
                    name: "secret".into(),
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
    h.s.repos.set_collaborator(&alice.1, parent, bob.0, Permission::Read, &c()).await.unwrap();
    let fork = h.s.repos.fork(&bob.1, parent, bob.0, &c()).await.unwrap().id;
    // Bob lets Carol into his fork, but Alice never let her into the original.
    h.s.repos.set_collaborator(&bob.1, fork, carol.0, Permission::Write, &c()).await.unwrap();
    assert_eq!(perm(&h, &carol.1, fork).await, Permission::None, "a grant on the fork alone does not open the parent's code");
    assert!(matches!(h.s.repos.get(&carol.1, fork).await, Err(ch_core::Error::NotFound)), "it looks missing");
    // Once Alice lets her in, the fork opens with the grant Bob gave.
    h.s.repos.set_collaborator(&alice.1, parent, carol.0, Permission::Read, &c()).await.unwrap();
    assert_eq!(perm(&h, &carol.1, fork).await, Permission::Write);
    // Alice closes the original to both: Carol loses the fork, Bob keeps his own.
    h.s.repos.remove_collaborator(&alice.1, parent, carol.0, &c()).await.unwrap();
    h.s.repos.remove_collaborator(&alice.1, parent, bob.0, &c()).await.unwrap();
    assert_eq!(perm(&h, &carol.1, fork).await, Permission::None);
    assert_eq!(perm(&h, &bob.1, fork).await, Permission::Admin, "the owner keeps the fork");
    // But it is locked: it starts no workflow any more.
    let ctx = ch_ci::ctx::Ctx::from_state(&h.s);
    assert!(ch_ci::triggers::fork_locked(&ctx, fork).await.unwrap());
    assert!(!ch_ci::triggers::fork_locked(&ctx, parent).await.unwrap(), "the original is not a fork");
    h.s.repos.set_collaborator(&alice.1, parent, bob.0, Permission::Read, &c()).await.unwrap();
    assert!(!ch_ci::triggers::fork_locked(&ctx, fork).await.unwrap(), "let back in, it runs again");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_locked_fork_sends_no_webhook_and_its_deploy_key_opens_nothing(pool: PgPool) {
    let h = harness(pool.clone()).await;
    let alice = user(&h, "alice").await;
    let bob = user(&h, "bob").await;
    let new = |name: &str| NewRepo {
        owner_id: alice.0,
        name: name.into(),
        description: String::new(),
        visibility: Visibility::Private,
        default_branch: None,
        auto_init: true,
    };
    let parent = h.s.repos.create(&alice.1, new("secret"), &c()).await.unwrap().id;
    h.s.repos.set_collaborator(&alice.1, parent, bob.0, Permission::Read, &c()).await.unwrap();
    let fork = h.s.repos.fork(&bob.1, parent, bob.0, &c()).await.unwrap().id;
    // A hook and a deploy key on Bob's fork.
    sqlx::query("INSERT INTO webhooks (scope, repo_id, url, events) VALUES ('repo', $1, 'https://hooks.example.org/x', '{push}')")
        .bind(fork.0)
        .execute(&pool)
        .await
        .unwrap();
    let key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHIzrFktyI9MkcRsBc8oM1OR/rHUuvLeg/hMMYkyIir5 second@codehaven";
    let parsed = ch_auth::sshkeys::parse_public_key(key).unwrap();
    sqlx::query("INSERT INTO deploy_keys (repo_id, title, public_key, fingerprint, read_only) VALUES ($1, 'ci', $2, $3, true)")
        .bind(fork.0)
        .bind(&parsed.normalized)
        .bind(&parsed.fingerprint)
        .execute(&pool)
        .await
        .unwrap();
    let push = ch_core::events::DomainEvent::Push {
        repo_id: fork,
        git_ref: "refs/heads/main".into(),
        before: "0".repeat(40),
        after: "b".repeat(40),
        pusher: Some(bob.0),
        forced: false,
        app: None,
        server_built: false,
    };
    let sent = |pool: PgPool, h: &H, push: &ch_core::events::DomainEvent| {
        let (cfg, authz, push) = (h.s.config.clone(), h.s.authz.clone(), push.clone());
        async move { ch_notify::webhooks::enqueue_event_for(&pool, &cfg, Some(authz.as_ref()), &push).await.unwrap() }
    };
    let opens = |h: &H| {
        let authn = h.s.authn.clone();
        let fp = parsed.fingerprint.clone();
        async move { authn.from_ssh_key(&fp).await.unwrap().is_some() }
    };
    // Open: both work.
    assert_eq!(sent(pool.clone(), &h, &push).await, 1);
    assert!(opens(&h).await);
    // Alice shuts Bob out of the original: the fork is his, but it is locked.
    h.s.repos.remove_collaborator(&alice.1, parent, bob.0, &c()).await.unwrap();
    assert_eq!(sent(pool.clone(), &h, &push).await, 0, "no webhook leaves a locked fork");
    assert!(!opens(&h).await, "its deploy key opens nothing");
    // Let back in, both work again.
    h.s.repos.set_collaborator(&alice.1, parent, bob.0, Permission::Read, &c()).await.unwrap();
    assert_eq!(sent(pool.clone(), &h, &push).await, 1);
    assert!(opens(&h).await);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn an_org_fork_is_locked_when_one_of_its_owners_loses_the_parent(pool: PgPool) {
    let h = harness(pool.clone()).await;
    let alice = user(&h, "alice").await;
    let bob = user(&h, "bob").await;
    let carol = user(&h, "carol").await;
    let parent =
        h.s.repos
            .create(
                &alice.1,
                NewRepo {
                    owner_id: alice.0,
                    name: "secret".into(),
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
    h.s.repos.set_collaborator(&alice.1, parent, bob.0, Permission::Read, &c()).await.unwrap();
    h.s.repos.set_collaborator(&alice.1, parent, carol.0, Permission::Read, &c()).await.unwrap();
    // Bob makes an organization, makes Carol an owner too, and forks the original into it.
    let org = h.s.orgs.create(&bob.1, "crew", "Crew", &c()).await.unwrap();
    h.s.orgs.set_member(&bob.1, ch_core::OrgId(org.id.0), carol.0, ch_core::services::OrgRole::Owner, &c()).await.unwrap();
    let fork = h.s.repos.fork(&bob.1, parent, UserId(org.id.0), &c()).await.unwrap().id;
    let ctx = ch_ci::ctx::Ctx::from_state(&h.s);
    assert!(!ch_ci::triggers::fork_locked(&ctx, fork).await.unwrap(), "every owner can still see the original");
    // Alice shuts Carol out of the original: one of the fork's owners is out, so the fork is locked.
    h.s.repos.remove_collaborator(&alice.1, parent, carol.0, &c()).await.unwrap();
    assert!(ch_ci::triggers::fork_locked(&ctx, fork).await.unwrap(), "an owner who lost the original locks the org's fork");
    assert!(ch_db::fork_lock::is_locked(&pool, h.s.authz.as_ref(), fork).await.unwrap());
    // Carol leaving the organization lets it run again; so does Alice letting her back in.
    h.s.orgs.remove_member(&bob.1, ch_core::OrgId(org.id.0), carol.0, &c()).await.unwrap();
    assert!(!ch_ci::triggers::fork_locked(&ctx, fork).await.unwrap());
    // Alice shuts Bob out as well: the last owner is out, locked again.
    h.s.repos.remove_collaborator(&alice.1, parent, bob.0, &c()).await.unwrap();
    assert!(ch_ci::triggers::fork_locked(&ctx, fork).await.unwrap());
    // A member who is not an owner does not matter: Dave, a plain member, never saw the original.
    let dave = user(&h, "dave").await;
    h.s.orgs.set_member(&bob.1, ch_core::OrgId(org.id.0), dave.0, ch_core::services::OrgRole::Member, &c()).await.unwrap();
    h.s.repos.set_collaborator(&alice.1, parent, bob.0, Permission::Read, &c()).await.unwrap();
    assert!(!ch_ci::triggers::fork_locked(&ctx, fork).await.unwrap(), "only owners decide the lock");
}

const MANUAL: &str = "name: w\non:\n  push:\n  workflow_dispatch:\n  schedule:\n    - cron: '* * * * *'\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n";

/// Commits a workflow to the fork's main as its owner.
async fn add_workflow(h: &H, who: &Actor, repo: RepoId) {
    let tip = h.s.git.resolve(repo, "refs/heads/main").await.unwrap();
    h.s.git
        .commit_file_change(
            who,
            CommitFileChange {
                repo_id: repo,
                branch: "main".into(),
                new_branch: None,
                expected_sha: Some(tip),
                changes: vec![FileChange {
                    path: ".github/workflows/w.yml".into(),
                    content: Some(MANUAL.as_bytes().to_vec()),
                    renamed_from: None,
                }],
                message: "workflow".into(),
                author: Signature { name: "A".into(), email: "a@example.org".into(), at: chrono::Utc::now() },
            },
            &c(),
        )
        .await
        .unwrap();
}

async fn dispatch(h: &H, who: &Actor, fork: RepoId) -> ch_core::Result<RunId> {
    h.s.ci.dispatch(who, fork, "w.yml", "main", serde_json::json!({}), &c()).await
}

async fn runs_of(pool: &PgPool, repo: RepoId) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM workflow_runs WHERE repo_id = $1").bind(repo.0).fetch_one(pool).await.unwrap()
}

/// A locked fork starts nothing by any road: not a manual run, not a rerun, not a schedule, not
/// the approval of a held run. Letting the owner back in turns all of them on again.
async fn a_locked_fork_runs_nothing(
    h: &H,
    pool: &PgPool,
    fork: RepoId,
    owner: &Actor,
    unlock: impl std::future::Future<Output = ()>,
    lock: impl std::future::Future<Output = ()>,
) {
    add_workflow(h, owner, fork).await;
    // Open: a manual run starts, and so can a rerun of a finished run, and a schedule.
    let first = dispatch(h, owner, fork).await.unwrap();
    sqlx::query("UPDATE workflow_runs SET status = 'completed', conclusion = 'success' WHERE id = $1")
        .bind(first.0)
        .execute(pool)
        .await
        .unwrap();
    let held: i64 = sqlx::query_scalar(
        "INSERT INTO workflow_runs (repo_id, workflow_id, run_number, attempt, event, head_sha, git_ref, title, status, event_payload)
         SELECT repo_id, workflow_id, run_number + 100, 1, event, head_sha, git_ref, title, 'waiting', event_payload FROM workflow_runs WHERE id = $1
         RETURNING id",
    )
    .bind(first.0)
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO schedules (workflow_id, repo_id, cron, next_run_at) SELECT workflow_id, repo_id, '* * * * *', now() - interval '1 minute' FROM workflow_runs WHERE id = $1
         ON CONFLICT DO NOTHING",
    )
    .bind(first.0)
    .execute(pool)
    .await
    .unwrap();
    let ctx = ch_ci::ctx::Ctx::from_state(&h.s);

    lock.await;
    let before = runs_of(pool, fork).await;
    let refused = |r: ch_core::Result<RunId>, what: &str| match r {
        Err(ch_core::Error::Forbidden(_)) => {}
        other => panic!("{what} on a locked fork: expected a refusal, got {other:?}"),
    };
    refused(dispatch(h, owner, fork).await, "dispatch");
    refused(h.s.ci.rerun(owner, fork, first, false, &c()).await, "rerun");
    refused(h.s.ci.rerun(owner, fork, first, true, &c()).await, "rerun of failed jobs");
    match h.s.ci.approve_run(owner, fork, RunId(held), &c()).await {
        Err(ch_core::Error::Forbidden(_)) => {}
        other => panic!("approve on a locked fork: expected a refusal, got {other:?}"),
    }
    assert_eq!(ch_ci::workers::fire_due_schedules(&ctx).await.unwrap(), 0, "the scheduler starts nothing here");
    assert_eq!(runs_of(pool, fork).await, before, "no run was made");
    let status: String = sqlx::query_scalar("SELECT status FROM workflow_runs WHERE id = $1").bind(held).fetch_one(pool).await.unwrap();
    assert_eq!(status, "waiting", "the held run stayed held");

    unlock.await;
    sqlx::query("UPDATE schedules SET next_run_at = now() - interval '1 minute'").execute(pool).await.unwrap();
    dispatch(h, owner, fork).await.unwrap();
    h.s.ci.rerun(owner, fork, first, false, &c()).await.unwrap();
    assert!(ch_ci::workers::fire_due_schedules(&ctx).await.unwrap() >= 1, "let back in, the scheduler runs it again");
    h.s.ci.approve_run(owner, fork, RunId(held), &c()).await.unwrap();
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_locked_personal_fork_starts_no_manual_run_rerun_schedule_or_approval(pool: PgPool) {
    let h = harness(pool.clone()).await;
    let alice = user(&h, "alice").await;
    let bob = user(&h, "bob").await;
    sqlx::query("UPDATE users SET can_run_ci = true").execute(&pool).await.unwrap();
    let parent = private_repo(&h, &alice, "secret").await;
    h.s.repos.set_collaborator(&alice.1, parent, bob.0, Permission::Read, &c()).await.unwrap();
    let fork = h.s.repos.fork(&bob.1, parent, bob.0, &c()).await.unwrap().id;
    let (s, a, b, p) = (&h.s, &alice.1, bob.0, parent);
    let lock = async move { s.repos.remove_collaborator(a, p, b, &c()).await.unwrap() };
    let unlock = async move { s.repos.set_collaborator(a, p, b, Permission::Read, &c()).await.unwrap() };
    a_locked_fork_runs_nothing(&h, &pool, fork, &bob.1, unlock, lock).await;
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_locked_org_fork_starts_no_manual_run_rerun_schedule_or_approval(pool: PgPool) {
    let h = harness(pool.clone()).await;
    let alice = user(&h, "alice").await;
    let bob = user(&h, "bob").await;
    let carol = user(&h, "carol").await;
    let parent = private_repo(&h, &alice, "secret").await;
    h.s.repos.set_collaborator(&alice.1, parent, bob.0, Permission::Read, &c()).await.unwrap();
    h.s.repos.set_collaborator(&alice.1, parent, carol.0, Permission::Read, &c()).await.unwrap();
    let org = h.s.orgs.create(&bob.1, "crew", "Crew", &c()).await.unwrap();
    h.s.orgs.set_member(&bob.1, ch_core::OrgId(org.id.0), carol.0, ch_core::services::OrgRole::Owner, &c()).await.unwrap();
    let fork = h.s.repos.fork(&bob.1, parent, UserId(org.id.0), &c()).await.unwrap().id;
    sqlx::query("UPDATE users SET can_run_ci = true").execute(&pool).await.unwrap();
    let (s, a, cr, p) = (&h.s, &alice.1, carol.0, parent);
    // Carol, one of the organization's owners, loses the original; Bob, the other, still sees it.
    let lock = async move { s.repos.remove_collaborator(a, p, cr, &c()).await.unwrap() };
    let unlock = async move { s.repos.set_collaborator(a, p, cr, Permission::Read, &c()).await.unwrap() };
    a_locked_fork_runs_nothing(&h, &pool, fork, &bob.1, unlock, lock).await;
}

async fn private_repo(h: &H, owner: &(UserId, Actor), name: &str) -> RepoId {
    h.s.repos
        .create(
            &owner.1,
            NewRepo {
                owner_id: owner.0,
                name: name.into(),
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
