//! A private fork carries its parent's code (ch-4pn.16): someone the fork's owner let in, who
//! cannot see a private repository it was forked from, gets nothing on the fork. The owner keeps
//! their fork.

#![allow(clippy::unwrap_used)]

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
