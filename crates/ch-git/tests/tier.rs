//! Changing a repository's tier (epic ch-4pn): who may, the store rebuild before any widening,
//! and the fork cap.
#![allow(clippy::unwrap_used)]

mod common;

use ch_core::actor::Scopes;
use ch_core::model::*;
use ch_core::{Actor, Error, TokenId};
use common::*;
use sqlx::PgPool;

async fn switch(f: &Fixture, on: bool) {
    sqlx::query("UPDATE visibility_settings SET allow_public_repos = $1").bind(on).execute(&f.env.pool).await.unwrap();
}

async fn has_object(f: &Fixture, repo: &Repo, sha: &str) -> bool {
    f.core.store.cmd(repo.id).args(["cat-file", "-e", sha]).run_any().await.unwrap().code == 0
}

fn forbidden_with(r: ch_core::Result<Repo>, msg: &str) {
    match r {
        Err(Error::Forbidden(m)) => assert_eq!(m, msg),
        other => panic!("expected 403 {msg:?}, got {other:?}"),
    }
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn widening_rebuilds_the_store_so_deleted_history_is_gone(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let alice = f.user("alice").await;
    let repo = f.repo(alice, "proj", true).await;
    let main = f.branch_sha(&repo, "main").await;
    // A commit on a branch that is then deleted: unreachable, but still in the store.
    f.state.git.create_ref(&f.actor(alice), repo.id, "refs/heads/tmp", &main, &f.client()).await.unwrap();
    let secret = f.commit(&repo, alice, "tmp", "secret.txt", "a password\n", "Oops").await;
    let blob = f.state.git.blob(repo.id, &secret, "secret.txt", 100).await.unwrap().sha;
    f.state.git.delete_ref(&f.actor(alice), repo.id, "refs/heads/tmp", &f.client()).await.unwrap();
    assert!(has_object(&f, &repo, &secret).await, "git keeps it until something cleans the store");

    let wider = f.state.repos.set_tier(&f.actor(alice), repo.id, Visibility::Internal, &f.client()).await.unwrap();
    assert_eq!(wider.visibility, Visibility::Internal);
    assert!(!has_object(&f, &repo, &secret).await, "the deleted commit is gone");
    assert!(!has_object(&f, &repo, &blob).await, "and so is its file");
    assert_eq!(f.branch_sha(&repo, "main").await, main, "every ref still resolves");
    let fsck = f.core.store.cmd(repo.id).args(["fsck", "--connectivity-only"]).run().await;
    assert!(fsck.is_ok(), "{fsck:?}");
    // The old store waits, unreachable, outside the repository.
    let parked: Vec<_> = std::fs::read_dir(f.core.store.root.join("_prepublish")).unwrap().flatten().collect();
    assert_eq!(parked.len(), 1);
    assert!(f.audit_actions().await.contains(&"repo.visibility_changed".to_string()));
    // Pushes still work after the swap.
    f.commit(&repo, alice, "main", "after.txt", "after\n", "After").await;
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn only_the_owner_in_a_browser_with_the_switch_on_makes_a_repository_public(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let (alice, bob) = (f.user("alice").await, f.user("bob").await);
    let repo = f.repo(alice, "proj", true).await;
    f.env.collaborator(repo.id, bob, "admin").await;

    forbidden_with(
        f.state.repos.set_tier(&f.actor(alice), repo.id, Visibility::Public, &f.client()).await,
        "Public repositories are off on this server.",
    );
    switch(&f, true).await;
    let token = Actor::Token { user_id: alice, token_id: TokenId(1), scopes: Scopes::new(["repo"]) };
    forbidden_with(
        f.state.repos.set_tier(&token, repo.id, Visibility::Public, &f.client()).await,
        "Make a repository public in the browser.",
    );
    forbidden_with(
        f.state.repos.set_tier(&f.actor(bob), repo.id, Visibility::Public, &f.client()).await,
        "Only the owner can make this public.",
    );
    let stranger = f.user("mallory").await;
    assert!(matches!(f.state.repos.set_tier(&f.actor(stranger), repo.id, Visibility::Public, &f.client()).await, Err(Error::NotFound)));

    let public = f.state.repos.set_tier(&f.actor(alice), repo.id, Visibility::Public, &f.client()).await.unwrap();
    assert_eq!(public.visibility, Visibility::Public);
    let by: Option<i64> =
        sqlx::query_scalar("SELECT public_by FROM repositories WHERE id = $1").bind(repo.id.0).fetch_one(&f.env.pool).await.unwrap();
    assert_eq!(by, Some(alice.0));

    // Narrowing takes repo admin with any credential, at once.
    let narrower = f.state.repos.set_tier(&token, repo.id, Visibility::Internal, &f.client()).await.unwrap();
    assert_eq!(narrower.visibility, Visibility::Internal);
    let private = f.state.repos.set_tier(&f.actor(bob), repo.id, Visibility::Private, &f.client()).await.unwrap();
    assert_eq!(private.visibility, Visibility::Private);
    // Turning the switch off lowers public repositories, and refusals were audited.
    f.state.repos.set_tier(&f.actor(alice), repo.id, Visibility::Public, &f.client()).await.unwrap();
    switch(&f, false).await;
    assert_eq!(f.state.repos.get(&Actor::System, repo.id).await.unwrap().visibility, Visibility::Internal);
    let refusals: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action = 'repo.visibility_changed' AND outcome = 'failure'")
            .fetch_one(&f.env.pool)
            .await
            .unwrap();
    assert!(refusals >= 3, "refusals are audited: {refusals}");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_fork_never_goes_above_its_parent(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let (alice, bob) = (f.user("alice").await, f.user("bob").await);
    let root = f.repo(alice, "proj", true).await;
    f.env.collaborator(root.id, bob, "read").await;
    let fork = f.state.repos.fork(&f.actor(bob), root.id, bob, &f.client()).await.unwrap();
    match f.state.repos.set_tier(&f.actor(bob), fork.id, Visibility::Internal, &f.client()).await {
        Err(Error::Validation(v)) => assert!(format!("{v:?}").contains("A fork cannot be more visible"), "{v:?}"),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        f.state.repos.fork_as(&f.actor(bob), root.id, bob, Visibility::Internal, &f.client()).await,
        Err(Error::Validation(_))
    ));
    // Once the parent is internal the fork may follow, and lowering the parent lowers it again.
    f.state.repos.set_tier(&f.actor(alice), root.id, Visibility::Internal, &f.client()).await.unwrap();
    f.state.repos.set_tier(&f.actor(bob), fork.id, Visibility::Internal, &f.client()).await.unwrap();
    f.state.repos.set_tier(&f.actor(alice), root.id, Visibility::Private, &f.client()).await.unwrap();
    assert_eq!(f.state.repos.get(&Actor::System, fork.id).await.unwrap().visibility, Visibility::Private);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn widening_a_parent_first_makes_old_borrowing_forks_independent(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let alice = f.user("alice").await;
    let root = f.repo(alice, "proj", true).await;
    let bob = f.user("bob").await;
    f.env.collaborator(root.id, bob, "read").await;
    let fork = f.state.repos.fork(&f.actor(bob), root.id, bob, &f.client()).await.unwrap();
    // Make the fork look like one made before this release: its objects live in the root.
    let path = f.core.store.repo_path(fork.id);
    let objects = path.join("objects");
    for e in std::fs::read_dir(objects.join("pack")).unwrap().flatten() {
        std::fs::remove_file(e.path()).unwrap();
    }
    for e in std::fs::read_dir(&objects).unwrap().flatten() {
        if e.file_name().len() == 2 {
            std::fs::remove_dir_all(e.path()).unwrap();
        }
    }
    std::fs::write(objects.join("info/alternates"), format!("{}\n", f.core.store.repo_path(root.id).join("objects").display())).unwrap();
    assert!(f.core.store.cmd(fork.id).args(["fsck", "--connectivity-only"]).run().await.is_ok(), "works through alternates");

    f.state.repos.set_tier(&f.actor(alice), root.id, Visibility::Internal, &f.client()).await.unwrap();
    assert!(!objects.join("info/alternates").exists(), "the fork borrows nothing now");
    let fsck = f.core.store.cmd(fork.id).args(["fsck", "--connectivity-only"]).run().await;
    assert!(fsck.is_ok(), "and is complete on its own: {fsck:?}");
}

/// ch-8tj and ch-ugt: forks are listed and counted only when the viewer can read them, anyone
/// signed in who can see an internal repository may fork and star it, and a star is per person.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn forks_and_stars_follow_what_each_person_can_see(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let (alice, bob, carol, dave) = (f.user("alice").await, f.user("bob").await, f.user("carol").await, f.user("dave").await);
    let root = f.repo(alice, "proj", true).await;
    f.state.repos.set_tier(&f.actor(alice), root.id, Visibility::Internal, &f.client()).await.unwrap();
    // Bob has no grant: the tier lets him fork into his own account, at most internal.
    let open = f.state.repos.fork_as(&f.actor(bob), root.id, bob, Visibility::Internal, &f.client()).await.unwrap();
    let hidden = f.state.repos.fork(&f.actor(carol), root.id, carol, &f.client()).await.unwrap();
    assert_eq!(hidden.visibility, Visibility::Private, "a fork starts private");

    let names = |v: Vec<Repo>| v.into_iter().map(|r| r.full_name()).collect::<Vec<_>>();
    assert_eq!(names(f.state.repos.forks(&f.actor(dave), root.id).await.unwrap()), vec!["bob/proj"]);
    let mut carols = names(f.state.repos.forks(&f.actor(carol), root.id).await.unwrap());
    carols.sort();
    assert_eq!(carols, vec!["bob/proj", "carol/proj"]);
    assert_eq!(f.state.repos.social(&f.actor(dave), root.id).await.unwrap().forks, 1, "carol's private fork is not counted for dave");
    assert_eq!(f.state.repos.social(&f.actor(carol), root.id).await.unwrap().forks, 2);

    f.state.repos.set_star(&f.actor(dave), root.id, true).await.unwrap();
    f.state.repos.set_star(&f.actor(dave), root.id, true).await.unwrap();
    let s = f.state.repos.social(&f.actor(dave), root.id).await.unwrap();
    assert_eq!((s.stars, s.starred), (1, true), "starring twice is one star");
    assert!(!f.state.repos.social(&f.actor(bob), root.id).await.unwrap().starred);
    f.state.repos.set_star(&f.actor(dave), root.id, false).await.unwrap();
    assert_eq!(f.state.repos.social(&f.actor(dave), root.id).await.unwrap().stars, 0);

    // Nothing about a private repository for someone who cannot read it, and no anonymous star.
    let private = f.repo(alice, "secret", true).await;
    assert!(matches!(f.state.repos.set_star(&f.actor(dave), private.id, true).await, Err(Error::NotFound)));
    assert!(matches!(f.state.repos.forks(&f.actor(dave), private.id).await, Err(Error::NotFound)));
    assert!(f.state.repos.set_star(&Actor::Anonymous, root.id, true).await.is_err());
    let _ = open;
}
