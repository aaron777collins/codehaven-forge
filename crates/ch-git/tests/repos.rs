//! Repository records: create, lookup, rename, transfer, fork, delete, collaborators,
//! protections and settings, against a real database and real bare repositories.
#![allow(clippy::unwrap_used)]

mod common;

use ch_core::model::*;
use ch_core::{Actor, AuthVia, Error, Permission};
use common::*;
use sqlx::PgPool;

fn new_repo(owner: ch_core::UserId, name: &str) -> NewRepo {
    NewRepo {
        owner_id: owner,
        name: name.into(),
        description: String::new(),
        visibility: Visibility::Private,
        default_branch: None,
        auto_init: false,
    }
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn creating_with_auto_init_makes_a_bare_repo_with_a_readme(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let alice = f.user("alice").await;
    let repo = f.repo(alice, "demo", true).await;
    assert_eq!(repo.full_name(), "alice/demo");
    assert_eq!(repo.default_branch, "main");
    assert!(f.core.store.exists(repo.id));
    let tree = f.state.git.tree(repo.id, "main", "").await.unwrap();
    assert_eq!(tree.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), vec!["README.md"]);
    let blob = f.state.git.blob(repo.id, "main", "README.md", 1000).await.unwrap();
    assert_eq!(String::from_utf8(blob.content).unwrap(), "# demo\n");
    // The first commit counts as a push: the repository is no longer empty and CI can see it.
    let again = f.state.repos.get(&f.actor(alice), repo.id).await.unwrap();
    assert!(!again.is_empty);
    assert_eq!(f.events("repo_created").await.len(), 1);
    assert_eq!(f.events("push").await.len(), 1);
    assert!(f.audit_actions().await.contains(&"repo.created".to_string()));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn without_auto_init_the_repo_is_empty(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let alice = f.user("alice").await;
    let repo = f.repo(alice, "empty", false).await;
    assert!(repo.is_empty);
    assert!(f.state.git.refs(repo.id, None).await.unwrap().is_empty());
    assert!(matches!(f.state.git.resolve(repo.id, "main").await, Err(Error::NotFound)));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn names_are_validated(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let alice = f.user("alice").await;
    let actor = f.actor(alice);
    for bad in ["", ".", "..", "a b", "x.git", "X.GIT", "slash/name", "semi;colon", &"n".repeat(101)] {
        let e = f.state.repos.create(&actor, new_repo(alice, bad), &f.client()).await.unwrap_err();
        assert!(matches!(e, Error::Validation(_)), "{bad:?} gave {e:?}");
    }
    for good in ["ok", "my-repo_1.2", "a", ".github", &"n".repeat(100)] {
        f.state.repos.create(&actor, new_repo(alice, good), &f.client()).await.unwrap_or_else(|e| panic!("{good:?}: {e:?}"));
    }
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn bad_default_branch_names_are_refused(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let alice = f.user("alice").await;
    for bad in ["has space", "-dash", "a..b", "x.lock", "refs/heads/x", "~tilde"] {
        let mut n = new_repo(alice, "r");
        n.default_branch = Some(bad.into());
        assert!(f.state.repos.create(&f.actor(alice), n, &f.client()).await.is_err(), "{bad:?}");
    }
    let mut n = new_repo(alice, "r");
    n.default_branch = Some("trunk".into());
    let repo = f.state.repos.create(&f.actor(alice), n, &f.client()).await.unwrap();
    assert_eq!(repo.default_branch, "trunk");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_name_is_unique_per_owner_and_case_insensitive(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let (alice, bob) = (f.user("alice").await, f.user("bob").await);
    f.repo(alice, "demo", false).await;
    let e = f.state.repos.create(&f.actor(alice), new_repo(alice, "DEMO"), &f.client()).await.unwrap_err();
    match e {
        Error::Validation(v) => assert!(v[0].message.as_deref().unwrap().contains("already exists")),
        other => panic!("{other:?}"),
    }
    f.repo(bob, "demo", false).await;
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn nobody_creates_repos_for_someone_else(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let (alice, bob) = (f.user("alice").await, f.user("bob").await);
    let e = f.state.repos.create(&f.actor(bob), new_repo(alice, "mine"), &f.client()).await.unwrap_err();
    assert!(matches!(e, Error::Forbidden(_)));
    assert_eq!(f.env.count("repositories").await, 0);
    // A token needs the repo scope, and anonymous callers get nothing.
    let ro = token_actor(alice, &["repo:read"]);
    assert!(matches!(f.state.repos.create(&ro, new_repo(alice, "x"), &f.client()).await, Err(Error::Forbidden(_))));
    assert!(f.state.repos.create(&Actor::Anonymous, new_repo(alice, "x"), &f.client()).await.is_err());
    let rw = token_actor(alice, &["repo"]);
    f.state.repos.create(&rw, new_repo(alice, "x"), &f.client()).await.unwrap();
}

fn token_actor(user: ch_core::UserId, scopes: &[&str]) -> Actor {
    ch_testkit::fixtures::token_actor(user, scopes)
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn org_repos_need_an_org_owner(pool: PgPool) {
    let f = Fixture::new(pool.clone()).await;
    let (alice, bob) = (f.user("alice").await, f.user("bob").await);
    let org = f.env.org("acme").await;
    for (u, role) in [(alice, "owner"), (bob, "member")] {
        sqlx::query("INSERT INTO org_members (org_id, user_id, role) VALUES ($1, $2, $3)")
            .bind(org.0)
            .bind(u.0)
            .bind(role)
            .execute(&pool)
            .await
            .unwrap();
    }
    assert!(matches!(f.state.repos.create(&f.actor(bob), new_repo(org, "tool"), &f.client()).await, Err(Error::Forbidden(_))));
    let repo = f.state.repos.create(&f.actor(alice), new_repo(org, "tool"), &f.client()).await.unwrap();
    assert_eq!(repo.full_name(), "acme/tool");
    // The org owner is admin; a plain member can read.
    assert!(f.state.repos.get(&f.actor(bob), repo.id).await.is_ok());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn private_repos_are_invisible_to_strangers(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let (alice, bob, carol) = (f.user("alice").await, f.user("bob").await, f.user("carol").await);
    let private = f.repo(alice, "secret", false).await;
    let public = f.public_repo(alice, "open").await;
    f.env.collaborator(private.id, bob, "read").await;
    let a = f.state.repos.get_by_name(&f.actor(alice), "alice", "secret").await.unwrap();
    assert_eq!(a.id, private.id);
    // Case does not matter, and a trailing .git is understood.
    assert!(f.state.repos.get_by_name(&f.actor(alice), "ALICE", "Secret.git").await.is_ok());
    assert!(f.state.repos.get_by_name(&f.actor(bob), "alice", "secret").await.is_ok());
    // Carol cannot tell it exists: same answer as a repo that was never made.
    assert!(matches!(f.state.repos.get_by_name(&f.actor(carol), "alice", "secret").await, Err(Error::NotFound)));
    assert!(matches!(f.state.repos.get_by_name(&f.actor(carol), "alice", "never-made").await, Err(Error::NotFound)));
    // Public means every signed-in account, and never anonymous.
    assert!(f.state.repos.get_by_name(&f.actor(carol), "alice", "open").await.is_ok());
    assert!(matches!(f.state.repos.get_by_name(&Actor::Anonymous, "alice", "open").await, Err(Error::NotFound)));
    let _ = public;
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn rename_leaves_a_redirect_and_a_new_repo_can_take_the_old_name(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let (alice, bob) = (f.user("alice").await, f.user("bob").await);
    let repo = f.repo(alice, "old", true).await;
    f.env.collaborator(repo.id, bob, "write").await;
    // A writer cannot rename: that is an admin power.
    assert!(matches!(f.state.repos.rename(&f.actor(bob), repo.id, "stolen", &f.client()).await, Err(Error::Forbidden(_))));
    let renamed = f.state.repos.rename(&f.actor(alice), repo.id, "new", &f.client()).await.unwrap();
    assert_eq!(renamed.name, "new");
    let via_old = f.state.repos.get_by_name(&f.actor(alice), "alice", "old").await.unwrap();
    assert_eq!(via_old.id, repo.id);
    assert_eq!(via_old.name, "new");
    // Git still finds the files: they are keyed by id, not by name.
    assert_eq!(f.state.git.tree(repo.id, "main", "").await.unwrap().len(), 1);
    // Someone creates a different repo named "old": it wins over the redirect.
    let other = f.repo(alice, "old", false).await;
    assert_ne!(other.id, repo.id);
    assert_eq!(f.state.repos.get_by_name(&f.actor(alice), "alice", "old").await.unwrap().id, other.id);
    assert!(f.audit_actions().await.contains(&"repo.renamed".to_string()));
    // Renaming onto an existing name fails.
    assert!(f.state.repos.rename(&f.actor(alice), repo.id, "old", &f.client()).await.is_err());
    assert!(f.state.repos.rename(&f.actor(alice), repo.id, "bad name", &f.client()).await.is_err());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn transfer_moves_ownership_and_redirects(pool: PgPool) {
    let f = Fixture::new(pool.clone()).await;
    let (alice, bob) = (f.user("alice").await, f.user("bob").await);
    let repo = f.repo(alice, "thing", true).await;
    // Bob cannot take it, and Alice cannot hand it over without Bob having somewhere to put it.
    assert!(f.state.repos.transfer(&f.actor(bob), repo.id, bob, &f.client()).await.is_err());
    // Alice may transfer to Bob only if she can create under Bob, which she cannot.
    assert!(matches!(f.state.repos.transfer(&f.actor(alice), repo.id, bob, &f.client()).await, Err(Error::Forbidden(_))));
    // To an org she owns, it works.
    let org = f.env.org("acme").await;
    sqlx::query("INSERT INTO org_members (org_id, user_id, role) VALUES ($1, $2, 'owner')")
        .bind(org.0)
        .bind(alice.0)
        .execute(&pool)
        .await
        .unwrap();
    let moved = f.state.repos.transfer(&f.actor(alice), repo.id, org, &f.client()).await.unwrap();
    assert_eq!(moved.full_name(), "acme/thing");
    assert_eq!(f.state.repos.get_by_name(&f.actor(alice), "alice", "thing").await.unwrap().id, repo.id);
    assert!(f.audit_actions().await.contains(&"repo.transferred".to_string()));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn delete_removes_the_row_the_files_and_says_so(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let (alice, bob) = (f.user("alice").await, f.user("bob").await);
    let repo = f.repo(alice, "doomed", true).await;
    f.env.collaborator(repo.id, bob, "write").await;
    assert!(matches!(f.state.repos.delete(&f.actor(bob), repo.id, &f.client()).await, Err(Error::Forbidden(_))));
    f.state.repos.delete(&f.actor(alice), repo.id, &f.client()).await.unwrap();
    assert!(matches!(f.state.repos.get(&f.actor(alice), repo.id).await, Err(Error::NotFound)));
    assert!(!f.core.store.exists(repo.id));
    assert_eq!(f.core.store.empty_trash(), 1);
    assert_eq!(f.events("repo_deleted").await.len(), 1);
    assert!(f.audit_actions().await.contains(&"repo.deleted".to_string()));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn forks_copy_their_objects_borrow_nothing_and_block_deleting_their_parent(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let (alice, bob, carol) = (f.user("alice").await, f.user("bob").await, f.user("carol").await);
    let root = f.public_repo(alice, "proj").await;
    f.commit(&root, alice, "main", "src/lib.rs", "fn root() {}\n", "Add lib").await;
    f.state.git.create_ref(&f.actor(alice), root.id, "refs/tags/v1", &f.branch_sha(&root, "main").await, &f.client()).await.unwrap();

    let fork = f.state.repos.fork(&f.actor(bob), root.id, bob, &f.client()).await.unwrap();
    assert_eq!(fork.full_name(), "bob/proj");
    assert_eq!(fork.fork_parent_id, Some(root.id));
    assert_eq!(fork.visibility, Visibility::Private, "a new fork is private");
    assert_eq!(f.branch_sha(&fork, "main").await, f.branch_sha(&root, "main").await, "refs are copied");
    assert_eq!(f.state.git.refs(fork.id, Some("refs/tags/")).await.unwrap().len(), 1);
    // A fork borrows nothing: what its readers can fetch is only what its own refs reach.
    assert!(!f.core.store.repo_path(fork.id).join("objects/info/alternates").exists(), "no alternates");
    let fsck = f.core.store.cmd(fork.id).args(["fsck", "--connectivity-only"]).run().await;
    assert!(fsck.is_ok(), "the fork is complete on its own: {fsck:?}");
    // Commits the root gains later are not in the fork.
    let later = f.commit(&root, alice, "main", "later.txt", "later\n", "Later").await;
    let has = f.core.store.cmd(fork.id).args(["cat-file", "-e", &format!("{later}^{{commit}}")]).run_any().await.unwrap();
    assert_ne!(has.code, 0, "the fork does not see the root's later commits");

    // A fork of a fork carries the intermediate fork's commits.
    f.commit(&fork, bob, "main", "bob.txt", "bob was here\n", "Bob's change").await;
    f.env.collaborator(fork.id, carol, "read").await;
    let grand = f.state.repos.fork(&f.actor(carol), fork.id, carol, &f.client()).await.unwrap();
    assert_eq!(f.branch_sha(&grand, "main").await, f.branch_sha(&fork, "main").await);
    assert_eq!(f.state.git.blob(grand.id, "main", "bob.txt", 100).await.unwrap().content, b"bob was here\n");

    // A repository with forks below it cannot be deleted, middle of a chain included.
    assert!(matches!(f.state.repos.delete(&f.actor(alice), root.id, &f.client()).await, Err(Error::Conflict(_))));
    assert!(matches!(f.state.repos.delete(&f.actor(bob), fork.id, &f.client()).await, Err(Error::Conflict(_))));
    f.state.repos.delete(&f.actor(carol), grand.id, &f.client()).await.unwrap();
    f.state.repos.delete(&f.actor(bob), fork.id, &f.client()).await.unwrap();
    f.state.repos.delete(&f.actor(alice), root.id, &f.client()).await.unwrap();
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn forking_needs_read_access_and_a_free_name(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let (alice, bob) = (f.user("alice").await, f.user("bob").await);
    let secret = f.repo(alice, "secret", true).await;
    assert!(matches!(f.state.repos.fork(&f.actor(bob), secret.id, bob, &f.client()).await, Err(Error::NotFound)));
    let open = f.public_repo(alice, "open").await;
    f.state.repos.fork(&f.actor(bob), open.id, bob, &f.client()).await.unwrap();
    assert!(matches!(f.state.repos.fork(&f.actor(bob), open.id, bob, &f.client()).await, Err(Error::Validation(_))));
    // A failed fork leaves no half-made repository behind.
    assert_eq!(f.env.count("repositories").await, 3);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn forking_an_empty_repo_works(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let (alice, bob) = (f.user("alice").await, f.user("bob").await);
    let r = f.public_repo(alice, "full").await;
    let e = f
        .state
        .repos
        .create(&f.actor(alice), NewRepo { visibility: Visibility::Public, ..new_repo(alice, "nothing") }, &f.client())
        .await
        .unwrap();
    let fork = f.state.repos.fork(&f.actor(bob), e.id, bob, &f.client()).await.unwrap();
    assert!(fork.is_empty);
    let _ = r;
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn collaborators(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let (alice, bob, carol) = (f.user("alice").await, f.user("bob").await, f.user("carol").await);
    let repo = f.repo(alice, "team", false).await;
    f.state.repos.set_collaborator(&f.actor(alice), repo.id, bob, Permission::Write, &f.client()).await.unwrap();
    f.state.repos.set_collaborator(&f.actor(alice), repo.id, carol, Permission::Read, &f.client()).await.unwrap();
    let list = f.state.repos.collaborators(&f.actor(alice), repo.id).await.unwrap();
    assert_eq!(
        list.iter().map(|c| (c.username.as_str(), c.permission)).collect::<Vec<_>>(),
        vec![("bob", Permission::Write), ("carol", Permission::Read)]
    );
    // Changing a level replaces it.
    f.state.repos.set_collaborator(&f.actor(alice), repo.id, carol, Permission::Admin, &f.client()).await.unwrap();
    assert_eq!(f.state.repos.collaborators(&f.actor(alice), repo.id).await.unwrap()[1].permission, Permission::Admin);
    // Only an admin manages them. Bob can write but not invite.
    assert!(matches!(
        f.state.repos.set_collaborator(&f.actor(bob), repo.id, bob, Permission::Admin, &f.client()).await,
        Err(Error::Forbidden(_))
    ));
    // The owner is not a collaborator, and an org is not a person.
    assert!(f.state.repos.set_collaborator(&f.actor(alice), repo.id, alice, Permission::Read, &f.client()).await.is_err());
    let org = f.env.org("acme").await;
    assert!(f.state.repos.set_collaborator(&f.actor(alice), repo.id, org, Permission::Read, &f.client()).await.is_err());
    // `None` removes; removing twice is a 404.
    f.state.repos.set_collaborator(&f.actor(alice), repo.id, bob, Permission::None, &f.client()).await.unwrap();
    assert!(matches!(f.state.repos.remove_collaborator(&f.actor(alice), repo.id, bob, &f.client()).await, Err(Error::NotFound)));
    assert!(matches!(f.state.repos.get(&f.actor(bob), repo.id).await, Err(Error::NotFound)), "bob lost access");
    let actions = f.audit_actions().await;
    assert!(actions.contains(&"repo.collaborator_added".to_string()) && actions.contains(&"repo.collaborator_removed".to_string()));
    assert_eq!(f.events("member_added").await.len(), 3);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn branch_protection_rules(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let (alice, bob) = (f.user("alice").await, f.user("bob").await);
    let repo = f.repo(alice, "guarded", true).await;
    f.env.collaborator(repo.id, bob, "write").await;
    let rule = BranchProtection {
        pattern: "main".into(),
        required_approvals: 1,
        required_checks: vec!["ci".into()],
        strict: true,
        block_force_push: true,
        block_deletion: true,
        ..Default::default()
    };
    // Write access is not enough: protections are a maintainer's business.
    assert!(matches!(f.state.repos.set_protection(&f.actor(bob), repo.id, rule.clone(), &f.client()).await, Err(Error::Forbidden(_))));
    f.state.repos.set_protection(&f.actor(alice), repo.id, rule.clone(), &f.client()).await.unwrap();
    let got = f.state.repos.protections(&f.actor(bob), repo.id).await.unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!((got[0].required_approvals, got[0].required_checks.clone(), got[0].strict), (1, vec!["ci".to_string()], true));
    // Setting the same pattern again replaces it.
    f.state.repos.set_protection(&f.actor(alice), repo.id, BranchProtection { required_approvals: 2, ..rule }, &f.client()).await.unwrap();
    let got = f.state.repos.protections(&f.actor(alice), repo.id).await.unwrap();
    assert_eq!((got.len(), got[0].required_approvals), (1, 2));
    assert!(
        f.state
            .repos
            .set_protection(&f.actor(alice), repo.id, BranchProtection { pattern: "has space".into(), ..Default::default() }, &f.client())
            .await
            .is_err()
    );
    assert!(
        f.state
            .repos
            .set_protection(&f.actor(alice), repo.id, BranchProtection { pattern: "".into(), ..Default::default() }, &f.client())
            .await
            .is_err()
    );
    f.state.repos.remove_protection(&f.actor(alice), repo.id, "main", &f.client()).await.unwrap();
    assert!(matches!(f.state.repos.remove_protection(&f.actor(alice), repo.id, "main", &f.client()).await, Err(Error::NotFound)));
    assert_eq!(f.audit_actions().await.iter().filter(|a| *a == "repo.branch_protection_changed").count(), 3);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn settings_topics_default_branch_and_archive(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let (alice, bob) = (f.user("alice").await, f.user("bob").await);
    let repo = f.repo(alice, "cfg", true).await;
    f.env.collaborator(repo.id, bob, "maintain").await;
    let upd = |u: RepoSettingsUpdate| u;
    // Topics are cleaned: lowercased, deduped, validated.
    let r = f
        .state
        .repos
        .update_settings(
            &f.actor(alice),
            repo.id,
            upd(RepoSettingsUpdate { topics: Some(vec!["Rust".into(), "rust".into(), "forge".into()]), ..Default::default() }),
            &f.client(),
        )
        .await
        .unwrap();
    assert_eq!(r.topics, vec!["forge", "rust"]);
    assert!(
        f.state
            .repos
            .update_settings(
                &f.actor(alice),
                repo.id,
                RepoSettingsUpdate { topics: Some(vec!["bad topic".into()]), ..Default::default() },
                &f.client()
            )
            .await
            .is_err()
    );
    // The default branch must exist.
    assert!(
        f.state
            .repos
            .update_settings(
                &f.actor(alice),
                repo.id,
                RepoSettingsUpdate { default_branch: Some("nope".into()), ..Default::default() },
                &f.client()
            )
            .await
            .is_err()
    );
    f.state.git.create_ref(&f.actor(alice), repo.id, "refs/heads/trunk", &f.branch_sha(&repo, "main").await, &f.client()).await.unwrap();
    let r = f
        .state
        .repos
        .update_settings(
            &f.actor(alice),
            repo.id,
            RepoSettingsUpdate { default_branch: Some("trunk".into()), ..Default::default() },
            &f.client(),
        )
        .await
        .unwrap();
    assert_eq!(r.default_branch, "trunk");
    let head = std::fs::read_to_string(f.core.store.repo_path(repo.id).join("HEAD")).unwrap();
    assert_eq!(head.trim(), "ref: refs/heads/trunk");
    // A maintainer changes settings but not visibility or archive state: those are admin's.
    assert!(
        f.state
            .repos
            .update_settings(
                &f.actor(bob),
                repo.id,
                RepoSettingsUpdate { description: Some("hi".into()), ..Default::default() },
                &f.client()
            )
            .await
            .is_ok()
    );
    assert!(matches!(
        f.state
            .repos
            .update_settings(
                &f.actor(bob),
                repo.id,
                RepoSettingsUpdate { visibility: Some(Visibility::Internal), ..Default::default() },
                &f.client()
            )
            .await,
        Err(Error::Forbidden(_))
    ));
    // Archiving blocks writes for everyone, then un-archiving restores them.
    f.state
        .repos
        .update_settings(&f.actor(alice), repo.id, RepoSettingsUpdate { archived: Some(true), ..Default::default() }, &f.client())
        .await
        .unwrap();
    let blocked = f
        .state
        .git
        .commit_file_change(
            &f.actor(alice),
            CommitFileChange {
                repo_id: repo.id,
                branch: "trunk".into(),
                new_branch: None,
                expected_sha: None,
                message: "x".into(),
                changes: vec![FileChange { path: "a".into(), content: Some(b"a".to_vec()), renamed_from: None }],
                author: Signature { name: "a".into(), email: "a@x".into(), at: chrono::Utc::now() },
            },
            &f.client(),
        )
        .await
        .unwrap_err();
    assert!(blocked.to_string().contains("archived"), "{blocked}");
    assert!(
        f.state
            .repos
            .update_settings(
                &f.actor(alice),
                repo.id,
                RepoSettingsUpdate { description: Some("x".into()), ..Default::default() },
                &f.client()
            )
            .await
            .is_err()
    );
    f.state
        .repos
        .update_settings(&f.actor(alice), repo.id, RepoSettingsUpdate { archived: Some(false), ..Default::default() }, &f.client())
        .await
        .unwrap();
    f.commit(&repo, alice, "trunk", "after.txt", "ok\n", "After").await;
    // Visibility changes are audited.
    f.state
        .repos
        .update_settings(
            &f.actor(alice),
            repo.id,
            RepoSettingsUpdate { visibility: Some(Visibility::Internal), ..Default::default() },
            &f.client(),
        )
        .await
        .unwrap();
    assert!(f.audit_actions().await.contains(&"repo.visibility_changed".to_string()));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn listing_what_a_person_can_see(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let (alice, bob) = (f.user("alice").await, f.user("bob").await);
    f.repo(alice, "alice-private", false).await;
    f.public_repo(alice, "alice-public").await;
    let shared = f.repo(alice, "alice-shared", false).await;
    f.env.collaborator(shared.id, bob, "read").await;
    f.repo(bob, "bob-private", false).await;
    let names = |v: Vec<Repo>| {
        let mut n: Vec<String> = v.into_iter().map(|r| r.full_name()).collect();
        n.sort();
        n
    };
    assert_eq!(
        names(f.state.repos.list_accessible(&f.actor(bob), 1, 30).await.unwrap()),
        vec!["alice/alice-public", "alice/alice-shared", "bob/bob-private"]
    );
    assert_eq!(
        names(f.state.repos.list_accessible(&f.actor(alice), 1, 30).await.unwrap()),
        vec!["alice/alice-private", "alice/alice-public", "alice/alice-shared"]
    );
    assert_eq!(f.state.repos.list_accessible(&f.actor(alice), 2, 2).await.unwrap().len(), 1, "page two of size two");
    assert!(f.state.repos.list_accessible(&Actor::Anonymous, 1, 30).await.unwrap().is_empty());
    // Per owner, filtered the same way.
    assert_eq!(names(f.state.repos.list_for_owner(&f.actor(bob), alice).await.unwrap()), vec!["alice/alice-public", "alice/alice-shared"]);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn workspace_switches_are_admin_only_and_audited(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let alice = f.user("alice").await;
    // Never set: the editor and the terminal are both on.
    assert_eq!(
        f.state.repos.workspace_settings().await.unwrap(),
        WorkspaceSettings { editor: true, terminal: true },
        "the editor and the terminal start on"
    );
    let settings = WorkspaceSettings { editor: false, terminal: false };
    // A plain user is refused.
    assert!(matches!(f.state.repos.set_workspace_settings(&f.actor(alice), settings, &f.client()).await, Err(Error::NotFound)));
    // An admin without two-factor is refused too.
    let no_2fa = Actor::User { id: alice, is_admin: true, has_2fa: false, via: AuthVia::Session };
    assert!(matches!(f.state.repos.set_workspace_settings(&no_2fa, settings, &f.client()).await, Err(Error::NotFound)));
    // An admin with two-factor can switch the editor off, and it is audited.
    let admin = Actor::User { id: alice, is_admin: true, has_2fa: true, via: AuthVia::Session };
    f.state.repos.set_workspace_settings(&admin, settings, &f.client()).await.unwrap();
    assert_eq!(f.state.repos.workspace_settings().await.unwrap(), settings);
    assert!(f.audit_actions().await.contains(&"instance.workspace_changed".to_string()));
    // The config file is the ceiling. No switch reads as on past it.
    let on = WorkspaceSettings { editor: true, terminal: true };
    let mut cfg = ch_core::Config::for_tests();
    cfg.workspace.enabled = false;
    assert!(!on.editor_on(&cfg));
    cfg.workspace.enabled = true;
    assert!(on.editor_on(&cfg));
    cfg.workspace.terminal = false;
    assert!(!on.terminal_on(&cfg));
    cfg.workspace.terminal = true;
    assert!(on.terminal_on(&cfg));
}
