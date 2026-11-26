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

/// ch-mpv: a job queued before its fork became locked is not handed to a runner. The claim is
/// rolled back, the run is cancelled, and a runner that asks gets nothing.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_job_queued_before_its_fork_was_locked_is_never_claimed(pool: PgPool) {
    let h = harness(pool.clone()).await;
    let alice = user(&h, "alice").await;
    let bob = user(&h, "bob").await;
    sqlx::query("UPDATE users SET can_run_ci = true").execute(&pool).await.unwrap();
    let parent = private_repo(&h, &alice, "secret").await;
    h.s.repos.set_collaborator(&alice.1, parent, bob.0, Permission::Read, &c()).await.unwrap();
    let fork = h.s.repos.fork(&bob.1, parent, bob.0, &c()).await.unwrap().id;
    add_workflow(&h, &bob.1, fork).await;
    let run = dispatch(&h, &bob.1, fork).await.unwrap();
    let queued: i64 = sqlx::query_scalar("SELECT count(*) FROM workflow_jobs WHERE run_id = $1 AND status = 'queued'")
        .bind(run.0)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(queued >= 1, "the run has a queued job");
    let runner: i64 = sqlx::query_scalar(
        "INSERT INTO runners (name, labels, credential_hash) VALUES ('r', ARRAY['ubuntu-latest'], decode(repeat('ab', 32), 'hex')) RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let ctx = ch_ci::ctx::Ctx::from_state(&h.s);
    // The lock lands after the job was queued.
    h.s.repos.remove_collaborator(&alice.1, parent, bob.0, &c()).await.unwrap();
    assert!(ch_ci::triggers::fork_locked(&ctx, fork).await.unwrap());
    let labels = vec!["ubuntu-latest".to_string()];
    assert!(ch_ci::runner_api::try_claim(&ctx, runner, &labels, None).await.unwrap().is_none(), "a locked fork's job is not handed out");
    let running: i64 = sqlx::query_scalar("SELECT count(*) FROM workflow_jobs WHERE run_id = $1 AND status = 'in_progress'")
        .bind(run.0)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(running, 0);
    let still_queued: i64 = sqlx::query_scalar("SELECT count(*) FROM workflow_jobs WHERE run_id = $1 AND status = 'queued'")
        .bind(run.0)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(still_queued, 0, "the run was cancelled, so nothing waits to be claimed later");
    let (status, conclusion): (String, Option<String>) =
        sqlx::query_as("SELECT status, conclusion FROM workflow_runs WHERE id = $1").bind(run.0).fetch_one(&pool).await.unwrap();
    assert_eq!((status.as_str(), conclusion.as_deref()), ("completed", Some("cancelled")));
}

async fn topic_branch(h: &H, who: &Actor, repo: RepoId, name: &str) {
    let tip = h.s.git.resolve(repo, "refs/heads/main").await.unwrap();
    h.s.git
        .commit_file_change(
            who,
            CommitFileChange {
                repo_id: repo,
                branch: "main".into(),
                new_branch: Some(name.into()),
                expected_sha: Some(tip),
                changes: vec![FileChange { path: "t.txt".into(), content: Some(b"t".to_vec()), renamed_from: None }],
                message: "topic".into(),
                author: Signature { name: "A".into(), email: "a@example.org".into(), at: chrono::Utc::now() },
            },
            &c(),
        )
        .await
        .unwrap();
}

/// ch-w3x: deleting the root of a fork network re-parents the other forks. A grant on a fork that
/// did nothing because its holder could not see the root must not start to work because the root
/// is gone. Open pull requests into the root are recorded, not dropped without a trace.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn deleting_a_private_root_does_not_open_its_forks_to_people_who_were_held_back(pool: PgPool) {
    let h = harness(pool.clone()).await;
    let alice = user(&h, "alice").await;
    let bob = user(&h, "bob").await;
    let carol = user(&h, "carol").await;
    let dave = user(&h, "dave").await;
    let erin = user(&h, "erin").await;
    let root = private_repo(&h, &alice, "secret").await;
    h.s.repos.set_collaborator(&alice.1, root, bob.0, Permission::Read, &c()).await.unwrap();
    h.s.repos.set_collaborator(&alice.1, root, carol.0, Permission::Read, &c()).await.unwrap();
    // Bob forks first, so his fork becomes the root of what is left. Carol forks second.
    let bobs = h.s.repos.fork(&bob.1, root, bob.0, &c()).await.unwrap().id;
    let carols = h.s.repos.fork(&carol.1, root, carol.0, &c()).await.unwrap().id;
    // Dave never saw the root. Bob lets him into his fork, Carol into hers; both grants are held
    // back by the lineage rule while the root stands. Erin can see the root and Carol's fork.
    h.s.repos.set_collaborator(&bob.1, bobs, dave.0, Permission::Read, &c()).await.unwrap();
    h.s.repos.set_collaborator(&carol.1, carols, dave.0, Permission::Write, &c()).await.unwrap();
    h.s.repos.set_collaborator(&alice.1, root, erin.0, Permission::Read, &c()).await.unwrap();
    h.s.repos.set_collaborator(&carol.1, carols, erin.0, Permission::Read, &c()).await.unwrap();
    assert_eq!(perm(&h, &dave.1, carols).await, Permission::None, "held back while the root stands");
    assert_eq!(perm(&h, &dave.1, bobs).await, Permission::None);
    assert_eq!(perm(&h, &erin.1, carols).await, Permission::Read);
    // An open pull request from Carol's fork into the root.
    topic_branch(&h, &carol.1, carols, "topic").await;
    let pr =
        h.s.pulls
            .create(
                &carol.1,
                NewPull {
                    repo_id: root,
                    title: "Carol's change".into(),
                    body: String::new(),
                    head_repo_id: carols,
                    head_ref: "topic".into(),
                    base_ref: "main".into(),
                    draft: false,
                },
                &c(),
            )
            .await
            .unwrap();

    h.s.repos.delete(&alice.1, root, &c()).await.unwrap();

    // Bob's fork is the root now. Nobody who was held back is let in by the delete.
    assert_eq!(perm(&h, &dave.1, bobs).await, Permission::None, "the new root's own grant stays held back too");
    assert_eq!(perm(&h, &dave.1, carols).await, Permission::None, "Dave's grant on the other fork did not start to work");
    // Those who could see it all along keep their access to the fork (Erin saw the root and the fork).
    // Bob's fork is the new parent and Erin cannot see it, so she is narrowed, never widened.
    assert_eq!(perm(&h, &erin.1, carols).await, Permission::None);
    assert_eq!(perm(&h, &carol.1, carols).await, Permission::Admin, "the owner keeps the fork");
    // The grants that did nothing were removed, in the audit log, and the open pull request too.
    let held: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repo_collaborators WHERE user_id = $1").bind(dave.0.0).fetch_one(&pool).await.unwrap();
    assert_eq!(held, 0, "Dave's dead grants were withdrawn, so nothing can wake up later");
    let actions: Vec<String> =
        sqlx::query_scalar("SELECT action FROM audit_events WHERE action IN ('repo.fork_grants_withdrawn', 'pull.closed_repo_deleted')")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(actions.contains(&"repo.fork_grants_withdrawn".to_string()), "{actions:?}");
    assert!(actions.contains(&"pull.closed_repo_deleted".to_string()), "{actions:?}");
    let details: serde_json::Value =
        sqlx::query_scalar("SELECT details FROM audit_events WHERE action = 'pull.closed_repo_deleted'").fetch_one(&pool).await.unwrap();
    assert_eq!(details["number"], pr.issue.number);
    assert_eq!(details["author"], "carol");
}

/// ch-ze4: a read collaborator can make an organization, add an account that cannot see the
/// repository and fork the repository into it. Deleting the repository must not be blocked by
/// that (the owner does not control the organization), and it must not open the fork to the
/// member either: they keep only what they could see before, which is nothing.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn deleting_a_private_repository_goes_ahead_and_holds_blind_org_members_back_from_the_fork(pool: PgPool) {
    let h = harness(pool.clone()).await;
    let alice = user(&h, "alice").await;
    let bob = user(&h, "bob").await;
    let dave = user(&h, "dave").await;
    let erin = user(&h, "erin").await;
    let root = private_repo(&h, &alice, "secret").await;
    h.s.repos.set_collaborator(&alice.1, root, bob.0, Permission::Read, &c()).await.unwrap();
    h.s.repos.set_collaborator(&alice.1, root, erin.0, Permission::Read, &c()).await.unwrap();
    // Bob's organization (base permission read), with Dave (never saw the root) and Erin (did).
    let org = h.s.orgs.create(&bob.1, "crew", "Crew", &c()).await.unwrap();
    let org_id = ch_core::OrgId(org.id.0);
    h.s.orgs.set_member(&bob.1, org_id, dave.0, ch_core::services::OrgRole::Member, &c()).await.unwrap();
    h.s.orgs.set_member(&bob.1, org_id, erin.0, ch_core::services::OrgRole::Member, &c()).await.unwrap();
    let fork = h.s.repos.fork(&bob.1, root, UserId(org.id.0), &c()).await.unwrap().id;
    assert_eq!(perm(&h, &dave.1, fork).await, Permission::None, "held back by the lineage rule while the root stands");
    assert_eq!(perm(&h, &erin.1, fork).await, Permission::Read, "Erin sees the root, so she sees the fork");

    // The delete is not refused.
    h.s.repos.delete(&alice.1, root, &c()).await.unwrap();

    assert_eq!(perm(&h, &dave.1, fork).await, Permission::None, "the delete did not open the fork to Dave");
    assert_eq!(perm(&h, &erin.1, fork).await, Permission::Read, "Erin keeps what she could see");
    assert_eq!(perm(&h, &bob.1, fork).await, Permission::Admin, "the organization's owner keeps the fork");
    let (holds,): (i64,) = sqlx::query_as("SELECT count(*) FROM repo_access_holds").fetch_one(&pool).await.unwrap();
    assert_eq!(holds, 1, "one hold, for Dave");
    let details: serde_json::Value =
        sqlx::query_scalar("SELECT details FROM audit_events WHERE action = 'repo.fork_grants_withdrawn'").fetch_one(&pool).await.unwrap();
    assert_eq!(details["org_members_held_back"], 1);

    // A grant made to Dave by name afterwards is a decision of the fork's admin, and it works.
    h.s.repos.set_collaborator(&bob.1, fork, dave.0, Permission::Read, &c()).await.unwrap();
    assert_eq!(perm(&h, &dave.1, fork).await, Permission::Read);
    h.s.repos.remove_collaborator(&bob.1, fork, dave.0, &c()).await.unwrap();
    assert_eq!(perm(&h, &dave.1, fork).await, Permission::None, "and removing it puts him back to held");

    // Leaving the organization ends the hold, so a person who joins again is an ordinary member.
    h.s.orgs.remove_member(&bob.1, org_id, dave.0, &c()).await.unwrap();
    let (holds,): (i64,) = sqlx::query_as("SELECT count(*) FROM repo_access_holds").fetch_one(&pool).await.unwrap();
    assert_eq!(holds, 0);
    h.s.orgs.set_member(&bob.1, org_id, dave.0, ch_core::services::OrgRole::Member, &c()).await.unwrap();
    assert_eq!(perm(&h, &dave.1, fork).await, Permission::Read, "a new membership is judged anew");
}

/// ch-697: deleting a repository takes its open pull requests with it. Their authors are told in
/// the app, and the note outlives the repository. The person deleting is not told about their own.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn deleting_a_repository_tells_the_authors_of_the_open_pull_requests_it_took(pool: PgPool) {
    let h = harness(pool.clone()).await;
    let alice = user(&h, "alice").await;
    let carol = user(&h, "carol").await;
    let root =
        h.s.repos
            .create(
                &alice.1,
                NewRepo {
                    owner_id: alice.0,
                    name: "open".into(),
                    description: String::new(),
                    visibility: Visibility::Public,
                    default_branch: None,
                    auto_init: true,
                },
                &c(),
            )
            .await
            .unwrap()
            .id;
    let carols = h.s.repos.fork(&carol.1, root, carol.0, &c()).await.unwrap().id;
    topic_branch(&h, &carol.1, carols, "topic").await;
    topic_branch(&h, &alice.1, root, "mine").await;
    let new_pull = |title: &str, head_repo_id: RepoId, head_ref: &str| NewPull {
        repo_id: root,
        title: title.into(),
        body: String::new(),
        head_repo_id,
        head_ref: head_ref.into(),
        base_ref: "main".into(),
        draft: false,
    };
    let theirs = h.s.pulls.create(&carol.1, new_pull("Carol's change", carols, "topic"), &c()).await.unwrap();
    h.s.pulls.create(&alice.1, new_pull("Alice's own", root, "mine"), &c()).await.unwrap();
    assert_eq!(h.s.notifications.unread_count(&carol.1).await.unwrap(), 0);

    h.s.repos.delete(&alice.1, root, &c()).await.unwrap();

    let inbox = h.s.notifications.inbox(&carol.1, false, 1, 30).await.unwrap();
    assert_eq!(inbox.len(), 1, "{inbox:?}");
    let n = &inbox[0];
    assert!(n.unread && n.repo_id.is_none(), "it belongs to no repository, so the delete left it");
    assert_eq!(n.subject_id, theirs.issue.id.0);
    for part in ["#1", "Carol's change", "alice/open", "deleted"] {
        assert!(n.title.contains(part), "{part} in {:?}", n.title);
    }
    assert_eq!(h.s.notifications.unread_count(&carol.1).await.unwrap(), 1);
    assert_eq!(h.s.notifications.inbox(&alice.1, false, 1, 30).await.unwrap().len(), 0, "nobody tells Alice about her own pull request");
}

/// ch-697: an author who already has an inbox row for the pull request (a comment or a review
/// made one) still gets the closing note. The old row belongs to the repository and would go with
/// it, so it is rewritten into the note: unread, not done, no repository.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_closing_note_reaches_an_author_who_already_had_an_inbox_row(pool: PgPool) {
    let h = harness(pool.clone()).await;
    let alice = user(&h, "alice").await;
    let carol = user(&h, "carol").await;
    let root =
        h.s.repos
            .create(
                &alice.1,
                NewRepo {
                    owner_id: alice.0,
                    name: "busy".into(),
                    description: String::new(),
                    visibility: Visibility::Public,
                    default_branch: None,
                    auto_init: true,
                },
                &c(),
            )
            .await
            .unwrap()
            .id;
    let carols = h.s.repos.fork(&carol.1, root, carol.0, &c()).await.unwrap().id;
    topic_branch(&h, &carol.1, carols, "topic").await;
    let theirs =
        h.s.pulls
            .create(
                &carol.1,
                NewPull {
                    repo_id: root,
                    title: "Carol's change".into(),
                    body: String::new(),
                    head_repo_id: carols,
                    head_ref: "topic".into(),
                    base_ref: "main".into(),
                    draft: false,
                },
                &c(),
            )
            .await
            .unwrap();
    // What a comment from Alice leaves behind: a row tied to the repository, already read and done.
    sqlx::query(
        "INSERT INTO notifications (user_id, repo_id, reason, subject_type, subject_id, title, unread, done)
         VALUES ($1, $2, 'author', 'pull', $3, 'old', false, true)
         ON CONFLICT (user_id, subject_type, subject_id) DO UPDATE SET repo_id = $2, reason = 'author', title = 'old', unread = false, done = true",
    )
    .bind(carol.0 .0)
    .bind(root.0)
    .bind(theirs.issue.id.0)
    .execute(&pool)
    .await
    .unwrap();

    h.s.repos.delete(&alice.1, root, &c()).await.unwrap();

    let inbox = h.s.notifications.inbox(&carol.1, false, 1, 30).await.unwrap();
    assert_eq!(inbox.len(), 1, "{inbox:?}");
    let n = &inbox[0];
    assert!(n.unread && n.repo_id.is_none(), "{n:?}");
    assert!(n.title.contains("busy") && n.title.contains("deleted"), "{:?}", n.title);
    assert_eq!(h.s.notifications.unread_count(&carol.1).await.unwrap(), 1);
}

/// ch-6ko: a queued job on a locked fork whose run cannot be cancelled must not be found again
/// and again. A run that is already completed refuses a cancel, so a queued job under one stands
/// in for a cancel that fails: the job is marked failed with a plain reason, after a few tries.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_locked_fork_job_whose_run_cannot_be_cancelled_is_failed_not_retried_without_end(pool: PgPool) {
    let h = harness(pool.clone()).await;
    let alice = user(&h, "alice").await;
    let bob = user(&h, "bob").await;
    sqlx::query("UPDATE users SET can_run_ci = true").execute(&pool).await.unwrap();
    let parent = private_repo(&h, &alice, "secret").await;
    h.s.repos.set_collaborator(&alice.1, parent, bob.0, Permission::Read, &c()).await.unwrap();
    let fork = h.s.repos.fork(&bob.1, parent, bob.0, &c()).await.unwrap().id;
    add_workflow(&h, &bob.1, fork).await;
    let run = dispatch(&h, &bob.1, fork).await.unwrap();
    let runner: i64 = sqlx::query_scalar(
        "INSERT INTO runners (name, labels, credential_hash) VALUES ('r', ARRAY['ubuntu-latest'], decode(repeat('ab', 32), 'hex')) RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let ctx = ch_ci::ctx::Ctx::from_state(&h.s);
    h.s.repos.remove_collaborator(&alice.1, parent, bob.0, &c()).await.unwrap();
    assert!(ch_ci::triggers::fork_locked(&ctx, fork).await.unwrap());
    // The run says it is finished while its job still waits: a cancel is refused.
    sqlx::query("UPDATE workflow_runs SET status = 'completed', conclusion = 'cancelled' WHERE id = $1")
        .bind(run.0)
        .execute(&pool)
        .await
        .unwrap();
    let labels = vec!["ubuntu-latest".to_string()];
    let got = tokio::time::timeout(std::time::Duration::from_secs(20), ch_ci::runner_api::try_claim(&ctx, runner, &labels, None))
        .await
        .expect("the request ended instead of spinning on the same job")
        .unwrap();
    assert!(got.is_none(), "nothing is handed out");
    let (status, conclusion): (String, Option<String>) =
        sqlx::query_as("SELECT status, conclusion FROM workflow_jobs WHERE run_id = $1 ORDER BY id LIMIT 1")
            .bind(run.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((status.as_str(), conclusion.as_deref()), ("completed", Some("failure")), "the job is marked failed, not left queued");
}
