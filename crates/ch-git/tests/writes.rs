//! Writes that do not come from a client push: web edits, refs, protection and merges.
#![allow(clippy::unwrap_used)]

mod common;

use ch_core::model::*;
use ch_core::{Actor, Error, UserId};
use common::*;
use sqlx::PgPool;

fn sig(name: &str, secs: i64) -> Signature {
    Signature {
        name: name.into(),
        email: format!("{}@test.example", name.to_lowercase()),
        at: chrono::DateTime::from_timestamp(secs, 0).unwrap(),
    }
}

fn put(path: &str, content: &str) -> FileChange {
    FileChange { path: path.into(), content: Some(content.as_bytes().to_vec()), renamed_from: None }
}

fn req(repo: &Repo, branch: &str, changes: Vec<FileChange>, message: &str) -> CommitFileChange {
    CommitFileChange {
        repo_id: repo.id,
        branch: branch.into(),
        new_branch: None,
        expected_sha: None,
        changes,
        message: message.into(),
        author: sig("Alice", 1_700_000_000),
    }
}

async fn setup(pool: PgPool) -> (Fixture, UserId, Repo) {
    let f = Fixture::new(pool).await;
    let alice = f.user("alice").await;
    let repo = f.repo(alice, "demo", true).await;
    (f, alice, repo)
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_first_commit_into_an_empty_repo_makes_it_live(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let alice = f.user("alice").await;
    let repo = f.repo(alice, "blank", false).await;
    let sha = f
        .state
        .git
        .commit_file_change(&f.actor(alice), req(&repo, "main", vec![put("a/b/c.txt", "deep\n")], "Start"), &f.client())
        .await
        .unwrap();
    assert_eq!(f.branch_sha(&repo, "main").await, sha);
    assert!(!f.state.repos.get(&f.actor(alice), repo.id).await.unwrap().is_empty);
    assert_eq!(f.state.git.blob(repo.id, "main", "a/b/c.txt", 100).await.unwrap().content, b"deep\n");
    let push = &f.events("push").await[0];
    assert_eq!(push["git_ref"], "refs/heads/main");
    assert_eq!(push["before"], "0".repeat(40));
    assert_eq!(push["after"], sha);
    assert_eq!(push["pusher"], alice.0);
    assert_eq!(f.events("ref_created").await.len(), 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn edits_deletes_and_renames_in_one_commit(pool: PgPool) {
    let (f, alice, repo) = setup(pool).await;
    let git = &f.state.git;
    git.commit_file_change(
        &f.actor(alice),
        req(&repo, "main", vec![put("one.txt", "1\n"), put("dir/two.txt", "2\n"), put("three.txt", "3\n")], "Setup"),
        &f.client(),
    )
    .await
    .unwrap();
    git.commit_file_change(
        &f.actor(alice),
        req(
            &repo,
            "main",
            vec![
                put("one.txt", "one\n"),
                FileChange { path: "dir/two.txt".into(), content: None, renamed_from: None },
                FileChange { path: "moved/three.txt".into(), content: Some(b"3\n".to_vec()), renamed_from: Some("three.txt".into()) },
            ],
            "Mixed",
        ),
        &f.client(),
    )
    .await
    .unwrap();
    let names: Vec<String> = git.tree(repo.id, "main", "").await.unwrap().into_iter().map(|e| e.name).collect();
    assert_eq!(names, vec!["moved", "one.txt", "README.md"], "dir/ vanished with its last file");
    assert_eq!(git.blob(repo.id, "main", "one.txt", 10).await.unwrap().content, b"one\n");
    assert_eq!(git.blob(repo.id, "main", "moved/three.txt", 10).await.unwrap().content, b"3\n");
    let c = git.commit(repo.id, "main").await.unwrap();
    assert_eq!(c.author.name, "Alice");
    assert_eq!(c.message, "Mixed");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn bad_edits_are_refused_with_reasons(pool: PgPool) {
    let (f, alice, repo) = setup(pool).await;
    let git = &f.state.git;
    let a = f.actor(alice);
    let c = f.client();
    assert!(matches!(git.commit_file_change(&a, req(&repo, "main", vec![], "x"), &c).await, Err(Error::Validation(_))));
    assert!(
        matches!(git.commit_file_change(&a, req(&repo, "main", vec![put("a", "x")], "  "), &c).await, Err(Error::Validation(_))),
        "blank message"
    );
    for path in [".git/config", "x/.git/hooks/pre-receive", ".GIT/x", "a//b", "../up", ""] {
        assert!(
            matches!(git.commit_file_change(&a, req(&repo, "main", vec![put(path, "x")], "x"), &c).await, Err(Error::Validation(_))),
            "{path:?}"
        );
    }
    // No change at all is not a commit.
    assert!(matches!(
        git.commit_file_change(&a, req(&repo, "main", vec![put("README.md", "# demo\n")], "same"), &c).await,
        Err(Error::Validation(_))
    ));
    // A file and a folder cannot share a name.
    git.commit_file_change(&a, req(&repo, "main", vec![put("thing", "file\n")], "file"), &c).await.unwrap();
    assert!(matches!(
        git.commit_file_change(&a, req(&repo, "main", vec![put("thing/inner.txt", "x")], "dir"), &c).await,
        Err(Error::Conflict(_))
    ));
    // Unknown branch.
    assert!(matches!(git.commit_file_change(&a, req(&repo, "ghost", vec![put("a", "x")], "x"), &c).await, Err(Error::NotFound)));
    // No write access: Bob can see a public repo but not change it.
    let bob = f.user("bob").await;
    let open = f.public_repo(alice, "open").await;
    assert!(matches!(
        git.commit_file_change(&f.actor(bob), req(&open, "main", vec![put("a", "x")], "x"), &c).await,
        Err(Error::Forbidden(_))
    ));
    let hidden = f.repo(alice, "hidden", true).await;
    assert!(matches!(
        git.commit_file_change(&f.actor(bob), req(&hidden, "main", vec![put("a", "x")], "x"), &c).await,
        Err(Error::NotFound)
    ));
    assert!(git.commit_file_change(&Actor::Anonymous, req(&open, "main", vec![put("a", "x")], "x"), &c).await.is_err());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn expected_sha_and_new_branches(pool: PgPool) {
    let (f, alice, repo) = setup(pool).await;
    let git = &f.state.git;
    let base = f.branch_sha(&repo, "main").await;
    f.commit(&repo, alice, "main", "moved-on.txt", "x\n", "Someone else pushed").await;
    let mut stale = req(&repo, "main", vec![put("mine.txt", "m\n")], "Mine");
    stale.expected_sha = Some(base.clone());
    assert!(matches!(git.commit_file_change(&f.actor(alice), stale, &f.client()).await, Err(Error::Conflict(_))));
    // "Commit to a new branch": the source is untouched.
    let main_now = f.branch_sha(&repo, "main").await;
    let mut nb = req(&repo, "main", vec![put("mine.txt", "m\n")], "Mine");
    nb.new_branch = Some("my-change".into());
    let sha = git.commit_file_change(&f.actor(alice), nb.clone(), &f.client()).await.unwrap();
    assert_eq!(f.branch_sha(&repo, "my-change").await, sha);
    assert_eq!(f.branch_sha(&repo, "main").await, main_now);
    assert_eq!(git.commit(repo.id, "my-change").await.unwrap().parents, vec![main_now]);
    // The name is taken now.
    assert!(git.commit_file_change(&f.actor(alice), nb, &f.client()).await.is_err());
    assert!(f.events("ref_created").await.iter().any(|e| e["git_ref"] == "refs/heads/my-change"));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn concurrent_edits_never_lose_a_commit(pool: PgPool) {
    let (f, alice, repo) = setup(pool).await;
    let f = std::sync::Arc::new(f);
    let mut tasks = Vec::new();
    for i in 0..8 {
        let f = f.clone();
        let repo = repo.clone();
        tasks.push(tokio::spawn(async move {
            f.state
                .git
                .commit_file_change(
                    &f.actor(alice),
                    req(&repo, "main", vec![put(&format!("file-{i}.txt"), &format!("{i}\n"))], &format!("Edit {i}")),
                    &f.client(),
                )
                .await
        }));
    }
    let mut won = Vec::new();
    for t in tasks {
        match t.await.unwrap() {
            Ok(sha) => won.push(sha),
            Err(Error::Conflict(_)) => {}
            Err(e) => panic!("unexpected {e:?}"),
        }
    }
    assert!(!won.is_empty());
    // Every commit that reported success is in the history of the final branch.
    let head = f.branch_sha(&repo, "main").await;
    for sha in &won {
        let r = f.core.store.cmd(repo.id).args(["merge-base", "--is-ancestor", sha.as_str(), head.as_str()]).run().await;
        assert!(r.is_ok(), "{sha} was lost");
    }
    assert_eq!(f.state.git.log(repo.id, "main", None, 0, 100).await.unwrap().len(), 1 + won.len(), "no phantom commits either");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn protected_branches_refuse_direct_edits_but_not_new_branches(pool: PgPool) {
    let (f, alice, repo) = setup(pool).await;
    let git = &f.state.git;
    f.state
        .repos
        .set_protection(
            &f.actor(alice),
            repo.id,
            BranchProtection { pattern: "main".into(), block_force_push: true, block_deletion: true, ..Default::default() },
            &f.client(),
        )
        .await
        .unwrap();
    let e = git.commit_file_change(&f.actor(alice), req(&repo, "main", vec![put("a", "x")], "direct"), &f.client()).await.unwrap_err();
    assert!(matches!(&e, Error::Forbidden(m) if m.contains("protected")), "{e:?}");
    let mut nb = req(&repo, "main", vec![put("a", "x")], "via branch");
    nb.new_branch = Some("change".into());
    git.commit_file_change(&f.actor(alice), nb, &f.client()).await.unwrap();
    // With direct pushes allowed it goes through.
    f.state
        .repos
        .set_protection(
            &f.actor(alice),
            repo.id,
            BranchProtection {
                pattern: "main".into(),
                allow_direct_push: true,
                block_force_push: true,
                block_deletion: true,
                ..Default::default()
            },
            &f.client(),
        )
        .await
        .unwrap();
    git.commit_file_change(&f.actor(alice), req(&repo, "main", vec![put("b", "y")], "now allowed"), &f.client()).await.unwrap();
    // Deleting the default branch is refused whether or not it is protected.
    assert!(git.delete_ref(&f.actor(alice), repo.id, "refs/heads/main", &f.client()).await.is_err());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn creating_updating_and_deleting_refs(pool: PgPool) {
    let (f, alice, repo) = setup(pool).await;
    let git = &f.state.git;
    let a = f.actor(alice);
    let c = f.client();
    let c1 = f.branch_sha(&repo, "main").await;
    let c2 = f.commit(&repo, alice, "main", "x.txt", "x\n", "Two").await;
    git.create_ref(&a, repo.id, "refs/heads/topic", &c1, &c).await.unwrap();
    git.create_ref(&a, repo.id, "newbranch", &c2, &c).await.unwrap();
    assert_eq!(f.branch_sha(&repo, "newbranch").await, c2, "a short name means a branch");
    git.create_ref(&a, repo.id, "refs/tags/v1", &c1, &c).await.unwrap();
    assert!(matches!(git.create_ref(&a, repo.id, "refs/heads/topic", &c2, &c).await, Err(Error::Validation(_))), "already exists");
    for bad in ["refs/heads/has space", "refs/heads/a..b", "refs/heads/x.lock", "refs/heads/-x", "refs/pull/1/head", "refs/notes/x", "HEAD"]
    {
        assert!(git.create_ref(&a, repo.id, bad, &c1, &c).await.is_err(), "{bad}");
    }
    assert!(git.create_ref(&a, repo.id, "refs/heads/ghost", &"1".repeat(40), &c).await.is_err(), "no such commit");
    assert!(git.create_ref(&a, repo.id, "refs/heads/x", "not-a-sha", &c).await.is_err());
    // Fast-forward is fine, a rewind needs force.
    git.update_ref(&a, repo.id, "refs/heads/topic", &c2, false, &c).await.unwrap();
    assert!(matches!(git.update_ref(&a, repo.id, "refs/heads/topic", &c1, false, &c).await, Err(Error::Validation(_))));
    git.update_ref(&a, repo.id, "refs/heads/topic", &c1, true, &c).await.unwrap();
    assert_eq!(f.branch_sha(&repo, "topic").await, c1);
    assert!(matches!(git.update_ref(&a, repo.id, "refs/heads/ghost", &c1, true, &c).await, Err(Error::NotFound)));
    // Deleting.
    git.delete_ref(&a, repo.id, "refs/heads/topic", &c).await.unwrap();
    git.delete_ref(&a, repo.id, "refs/tags/v1", &c).await.unwrap();
    assert!(matches!(git.delete_ref(&a, repo.id, "refs/heads/topic", &c).await, Err(Error::NotFound)));
    let deleted = f.events("ref_deleted").await;
    assert_eq!(deleted.len(), 2);
    assert!(deleted.iter().all(|e| e["sha"].is_string()));
    // Only the server writes pull request refs.
    git.create_ref(&Actor::System, repo.id, "refs/pull/1/head", &c1, &c).await.unwrap();
    assert!(git.refs(repo.id, Some("refs/pull/")).await.unwrap().len() == 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn writes_need_write_access_and_respect_token_scopes(pool: PgPool) {
    let (f, alice, repo) = setup(pool).await;
    let bob = f.user("bob").await;
    f.env.collaborator(repo.id, bob, "read").await;
    let git = &f.state.git;
    let sha = f.branch_sha(&repo, "main").await;
    assert!(matches!(git.create_ref(&f.actor(bob), repo.id, "refs/heads/x", &sha, &f.client()).await, Err(Error::Forbidden(_))));
    let ro_token = ch_testkit::fixtures::token_actor(alice, &["repo:read"]);
    assert!(matches!(git.create_ref(&ro_token, repo.id, "refs/heads/x", &sha, &f.client()).await, Err(Error::Forbidden(_))));
    let rw_token = ch_testkit::fixtures::token_actor(alice, &["repo"]);
    git.create_ref(&rw_token, repo.id, "refs/heads/x", &sha, &f.client()).await.unwrap();
}

// ----------------------------------------------------------------------------- merging

struct Branches {
    f: Fixture,
    alice: UserId,
    repo: Repo,
}

/// `main` with two files, and `feature` with three commits on top of it (one of which
/// touches a shared file), as a pull request would have.
async fn pr_setup(pool: PgPool) -> Branches {
    let (f, alice, repo) = setup(pool).await;
    f.commit(&repo, alice, "main", "shared.txt", "line one\nline two\nline three\n", "Add shared").await;
    let base = f.branch_sha(&repo, "main").await;
    f.state.git.create_ref(&f.actor(alice), repo.id, "refs/heads/feature", &base, &f.client()).await.unwrap();
    for (i, (path, text)) in
        [("f1.txt", "one\n"), ("f2.txt", "two\n"), ("shared.txt", "line one\nline two\nline three\nfeature adds\n")].into_iter().enumerate()
    {
        let r = CommitFileChange {
            author: sig("Pat", 1_700_001_000 + i as i64 * 100),
            ..req(&repo, "feature", vec![put(path, text)], &format!("Feature step {}", i + 1))
        };
        f.state.git.commit_file_change(&f.actor(alice), r, &f.client()).await.unwrap();
    }
    Branches { f, alice, repo }
}

fn merge_req(b: &Branches, method: MergeMethod, expected: &str) -> MergeRequest {
    MergeRequest {
        repo_id: b.repo.id,
        base_ref: "main".into(),
        head_repo_id: b.repo.id,
        head_ref: "feature".into(),
        method,
        title: "Add the feature (#7)".into(),
        message: "Closes #3".into(),
        author: sig("Pat", 1_700_002_000),
        committer: sig("Maggie", 1_700_003_000),
        expected_base_sha: expected.into(),
        update_branch: false,
        expected_head_sha: None,
    }
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_merge_commit_has_two_parents_and_moves_the_base(pool: PgPool) {
    let b = pr_setup(pool).await;
    let base = b.f.branch_sha(&b.repo, "main").await;
    let head = b.f.branch_sha(&b.repo, "feature").await;
    let out = b.f.state.git.merge(&b.f.actor(b.alice), merge_req(&b, MergeMethod::Merge, &base), &b.f.client()).await.unwrap();
    let MergeOutcome::Merged { sha } = out else { panic!("{out:?}") };
    assert_eq!(b.f.branch_sha(&b.repo, "main").await, sha);
    let c = b.f.state.git.commit(b.repo.id, &sha).await.unwrap();
    assert_eq!(c.parents, vec![base, head]);
    assert_eq!(c.message, "Add the feature (#7)\n\nCloses #3");
    assert_eq!((c.author.name.as_str(), c.committer.name.as_str()), ("Pat", "Maggie"));
    for p in ["f1.txt", "f2.txt", "shared.txt"] {
        assert!(b.f.state.git.blob(b.repo.id, "main", p, 100).await.is_ok(), "{p}");
    }
    let pushes = b.f.events("push").await;
    let last = pushes.last().unwrap();
    assert_eq!((last["git_ref"].as_str(), last["after"].as_str()), (Some("refs/heads/main"), Some(sha.as_str())));
    assert_eq!(last["forced"], false);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_squash_is_one_commit_with_the_authors_name(pool: PgPool) {
    let b = pr_setup(pool).await;
    let base = b.f.branch_sha(&b.repo, "main").await;
    let MergeOutcome::Merged { sha } =
        b.f.state.git.merge(&b.f.actor(b.alice), merge_req(&b, MergeMethod::Squash, &base), &b.f.client()).await.unwrap()
    else {
        panic!()
    };
    let c = b.f.state.git.commit(b.repo.id, &sha).await.unwrap();
    assert_eq!(c.parents, vec![base]);
    assert_eq!((c.author.name.as_str(), c.committer.name.as_str()), ("Pat", "Maggie"));
    assert_eq!(b.f.state.git.log(b.repo.id, "main", None, 0, 100).await.unwrap().len(), 3, "three feature commits became one");
    assert!(b.f.state.git.blob(b.repo.id, "main", "f2.txt", 10).await.is_ok());
    // `feature` itself is untouched.
    assert_eq!(b.f.state.git.log(b.repo.id, "feature", None, 0, 100).await.unwrap().len(), 5);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_rebase_replays_each_commit_keeping_its_author(pool: PgPool) {
    let b = pr_setup(pool).await;
    // Main moves on so the replay is not a plain fast-forward.
    b.f.commit(&b.repo, b.alice, "main", "other.txt", "o\n", "Unrelated").await;
    let base = b.f.branch_sha(&b.repo, "main").await;
    let MergeOutcome::Merged { sha } =
        b.f.state.git.merge(&b.f.actor(b.alice), merge_req(&b, MergeMethod::Rebase, &base), &b.f.client()).await.unwrap()
    else {
        panic!()
    };
    let log = b.f.state.git.log(b.repo.id, "main", None, 0, 100).await.unwrap();
    assert_eq!(log[0].sha, sha);
    let steps: Vec<&str> = log.iter().take(4).map(|c| c.message.as_str()).collect();
    assert_eq!(steps, vec!["Feature step 3", "Feature step 2", "Feature step 1", "Unrelated"]);
    assert!(log.iter().take(3).all(|c| c.parents.len() == 1 && c.author.name == "Pat" && c.committer.name == "Maggie"));
    assert_eq!(log[2].author.at.timestamp(), 1_700_001_000, "author dates survive");
    assert_ne!(log[0].sha, b.f.branch_sha(&b.repo, "feature").await, "new commits, since the base differs");
    assert!(b.f.state.git.blob(b.repo.id, "main", "other.txt", 10).await.is_ok());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn conflicts_are_reported_by_path_and_change_nothing(pool: PgPool) {
    let b = pr_setup(pool).await;
    b.f.commit(&b.repo, b.alice, "main", "shared.txt", "line one\nline two\nline three\nmain adds\n", "Main edits the same spot").await;
    let base = b.f.branch_sha(&b.repo, "main").await;
    for m in [MergeMethod::Merge, MergeMethod::Squash, MergeMethod::Rebase] {
        let out = b.f.state.git.merge(&b.f.actor(b.alice), merge_req(&b, m, &base), &b.f.client()).await.unwrap();
        match out {
            MergeOutcome::Conflict { paths } => assert_eq!(paths, vec!["shared.txt"], "{m:?}"),
            other => panic!("{m:?}: {other:?}"),
        }
        assert_eq!(b.f.branch_sha(&b.repo, "main").await, base, "{m:?} left the base alone");
    }
    assert_eq!(b.f.state.git.test_merge(b.repo.id, "main", b.repo.id, "feature").await.unwrap(), MergeableState::Conflict);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_stale_base_is_reported_not_overwritten(pool: PgPool) {
    let b = pr_setup(pool).await;
    let stale = b.f.branch_sha(&b.repo, "main").await;
    let moved = b.f.commit(&b.repo, b.alice, "main", "later.txt", "l\n", "Pushed meanwhile").await;
    let out = b.f.state.git.merge(&b.f.actor(b.alice), merge_req(&b, MergeMethod::Merge, &stale), &b.f.client()).await.unwrap();
    match out {
        MergeOutcome::BaseMoved { current_sha } => assert_eq!(current_sha, moved),
        other => panic!("{other:?}"),
    }
    assert_eq!(b.f.branch_sha(&b.repo, "main").await, moved);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn test_merge_says_clean_conflict_or_unknown(pool: PgPool) {
    let b = pr_setup(pool).await;
    let g = &b.f.state.git;
    assert_eq!(g.test_merge(b.repo.id, "main", b.repo.id, "feature").await.unwrap(), MergeableState::Clean);
    assert!(matches!(g.test_merge(b.repo.id, "main", b.repo.id, "ghost").await, Err(Error::NotFound)));
    assert!(matches!(g.test_merge(b.repo.id, "ghost", b.repo.id, "feature").await, Err(Error::NotFound)));
    // Test merges write no refs.
    assert_eq!(g.refs(b.repo.id, None).await.unwrap().len(), 2);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn merging_needs_write_access_and_lands_on_protected_branches(pool: PgPool) {
    let b = pr_setup(pool).await;
    let bob = b.f.user("bob").await;
    b.f.env.collaborator(b.repo.id, bob, "read").await;
    let base = b.f.branch_sha(&b.repo, "main").await;
    assert!(matches!(
        b.f.state.git.merge(&b.f.actor(bob), merge_req(&b, MergeMethod::Merge, &base), &b.f.client()).await,
        Err(Error::Forbidden(_))
    ));
    // `main` is protected against direct pushes, yet the merge button works.
    b.f.state
        .repos
        .set_protection(
            &b.f.actor(b.alice),
            b.repo.id,
            BranchProtection { pattern: "main".into(), block_force_push: true, block_deletion: true, ..Default::default() },
            &b.f.client(),
        )
        .await
        .unwrap();
    assert!(
        b.f.state
            .git
            .commit_file_change(&b.f.actor(b.alice), req(&b.repo, "main", vec![put("z", "z")], "direct"), &b.f.client())
            .await
            .is_err()
    );
    let out = b.f.state.git.merge(&b.f.actor(b.alice), merge_req(&b, MergeMethod::Squash, &base), &b.f.client()).await.unwrap();
    assert!(matches!(out, MergeOutcome::Merged { .. }));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn an_update_branch_merge_obeys_the_direct_push_rule(pool: PgPool) {
    // ch-jnm: "update branch" writes into the pull request's head branch. That is a direct write,
    // not a gated merge, so a protected head branch refuses it.
    let b = pr_setup(pool).await;
    b.f.state
        .repos
        .set_protection(
            &b.f.actor(b.alice),
            b.repo.id,
            BranchProtection { pattern: "main".into(), block_force_push: true, block_deletion: true, ..Default::default() },
            &b.f.client(),
        )
        .await
        .unwrap();
    let base = b.f.branch_sha(&b.repo, "main").await;
    let req = MergeRequest { update_branch: true, ..merge_req(&b, MergeMethod::Merge, &base) };
    let e = b.f.state.git.merge(&b.f.actor(b.alice), req, &b.f.client()).await.unwrap_err();
    assert!(matches!(&e, Error::Forbidden(m) if m.contains("protected")), "{e:?}");
    assert_eq!(b.f.branch_sha(&b.repo, "main").await, base, "main did not move");
}

/// ch-x12: a commit the server writes on top of whatever a branch holds says so on its push
/// event, so CI does not count the person for the commits underneath it. A ref set to a commit
/// the person names, and a gated merge, are not built that way.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_push_says_when_the_server_built_the_commit_on_the_branch(pool: PgPool) {
    let b = pr_setup(pool).await;
    let (git, a, c) = (&b.f.state.git, b.f.actor(b.alice), b.f.client());
    let f = &b.f;
    let on = move |ref_name: &'static str| async move {
        f.events("push").await.into_iter().filter(|p| p["git_ref"] == ref_name).collect::<Vec<_>>()
    };
    let feature = on("refs/heads/feature").await;
    assert_eq!(feature.len(), 4, "{feature:?}");
    assert_eq!(feature[0]["server_built"], false, "a ref created at a commit the person named");
    assert!(feature[1..].iter().all(|p| p["server_built"] == true), "file edits are built on the branch: {feature:?}");

    // Update branch: main merged into feature, on top of whatever feature holds.
    b.f.commit(&b.repo, b.alice, "main", "m.txt", "m\n", "Main moves").await;
    let head = b.f.branch_sha(&b.repo, "feature").await;
    let up = MergeRequest {
        base_ref: "feature".into(),
        head_ref: "main".into(),
        update_branch: true,
        ..merge_req(&b, MergeMethod::Merge, &head)
    };
    let MergeOutcome::Merged { sha } = git.merge(&a, up, &c).await.unwrap() else { panic!("not merged") };
    let last = on("refs/heads/feature").await.pop().unwrap();
    assert_eq!((last["after"].as_str(), last["server_built"].as_bool()), (Some(sha.as_str()), Some(true)), "{last}");

    // A gated merge into the base, and a fast-forward to a named commit, are not.
    let base = b.f.branch_sha(&b.repo, "main").await;
    git.merge(&a, merge_req(&b, MergeMethod::Merge, &base), &c).await.unwrap();
    assert_eq!(on("refs/heads/main").await.pop().unwrap()["server_built"], false);
    git.create_ref(&a, b.repo.id, "refs/heads/topic", &head, &c).await.unwrap();
    git.update_ref(&a, b.repo.id, "refs/heads/topic", &sha, false, &c).await.unwrap();
    assert_eq!(on("refs/heads/topic").await.pop().unwrap()["server_built"], false);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn merging_something_already_merged_is_an_error_not_an_empty_commit(pool: PgPool) {
    let b = pr_setup(pool).await;
    let base = b.f.branch_sha(&b.repo, "main").await;
    let out = b.f.state.git.merge(&b.f.actor(b.alice), merge_req(&b, MergeMethod::Rebase, &base), &b.f.client()).await.unwrap();
    assert!(matches!(out, MergeOutcome::Merged { .. }));
    let now = b.f.branch_sha(&b.repo, "main").await;
    let again = b.f.state.git.merge(&b.f.actor(b.alice), merge_req(&b, MergeMethod::Rebase, &now), &b.f.client()).await;
    assert!(matches!(again, Err(Error::Validation(_))), "{again:?}");
    assert_eq!(b.f.branch_sha(&b.repo, "main").await, now);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_pull_request_from_a_fork_merges_into_the_root(pool: PgPool) {
    let (f, alice, root) = setup(pool).await;
    let bob = f.user("bob").await;
    let public = f.public_repo(alice, "proj").await;
    let fork = f.state.repos.fork(&f.actor(bob), public.id, bob, &f.client()).await.unwrap();
    f.state
        .git
        .commit_file_change(
            &f.actor(bob),
            CommitFileChange { author: sig("Bob", 1_700_001_000), ..req(&fork, "main", vec![put("from-bob.txt", "hi\n")], "Bob's patch") },
            &f.client(),
        )
        .await
        .unwrap();
    let base = f.branch_sha(&public, "main").await;
    // The fork's commit does not exist in the root yet; testing and merging must fetch it.
    assert_eq!(f.state.git.test_merge(public.id, "main", fork.id, "main").await.unwrap(), MergeableState::Clean);
    let out = f
        .state
        .git
        .merge(
            &f.actor(alice),
            MergeRequest {
                repo_id: public.id,
                base_ref: "main".into(),
                head_repo_id: fork.id,
                head_ref: "main".into(),
                method: MergeMethod::Squash,
                title: "Bob's patch".into(),
                message: String::new(),
                author: sig("Bob", 1_700_002_000),
                committer: sig("Alice", 1_700_002_001),
                expected_base_sha: base,
                update_branch: false,
                expected_head_sha: None,
            },
            &f.client(),
        )
        .await
        .unwrap();
    assert!(matches!(out, MergeOutcome::Merged { .. }), "{out:?}");
    assert_eq!(f.state.git.blob(public.id, "main", "from-bob.txt", 10).await.unwrap().content, b"hi\n");
    // `create_ref` for a pull request's head ref fetches from the fork network the same way.
    f.commit(&fork, bob, "main", "second.txt", "2\n", "Another").await;
    let sha = f.branch_sha(&fork, "main").await;
    f.state.git.create_ref(&Actor::System, public.id, "refs/pull/1/head", &sha, &f.client()).await.unwrap();
    assert_eq!(f.branch_sha(&public, "refs/pull/1/head").await, sha);
    let _ = root;
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn hostile_file_names_are_taken_literally_or_refused(pool: PgPool) {
    let (f, alice, repo) = setup(pool).await;
    let git = &f.state.git;
    let a = f.actor(alice);
    let c = f.client();
    // A name with quote characters is its own file and never overwrites README.md.
    git.commit_file_change(&a, req(&repo, "main", vec![put("\"README.md\"", "quoted\n")], "quoted"), &c).await.unwrap();
    assert_eq!(git.blob(repo.id, "main", "README.md", 100).await.unwrap().content, b"# demo\n");
    assert_eq!(git.blob(repo.id, "main", "\"README.md\"", 100).await.unwrap().content, b"quoted\n");
    // A quoted escape stays a backslash and a letter, never a newline.
    git.commit_file_change(&a, req(&repo, "main", vec![put("\"a\\nb\"", "esc\n")], "escape"), &c).await.unwrap();
    assert_eq!(git.blob(repo.id, "main", "\"a\\nb\"", 100).await.unwrap().content, b"esc\n");
    // Spellings that HFS+ or NTFS fold to .git are refused.
    for path in [".g\u{200c}it/config", ".GIT\u{200d}/hooks/x", "x/.git::$INDEX_ALLOCATION/y", "git~1/config"] {
        assert!(
            matches!(git.commit_file_change(&a, req(&repo, "main", vec![put(path, "x")], "x"), &c).await, Err(Error::Validation(_))),
            "{path:?}"
        );
    }
    // A new file whose name has a space and a type word in it is not a clash.
    git.commit_file_change(&a, req(&repo, "main", vec![put("absent tree", "ok\n"), put("x blob", "ok\n")], "spaces"), &c).await.unwrap();
    // Deleting a file and adding a folder of that name in one commit works.
    git.commit_file_change(&a, req(&repo, "main", vec![put("thing", "file\n")], "file"), &c).await.unwrap();
    let swap = vec![FileChange { path: "thing".into(), content: None, renamed_from: None }, put("thing/inner.txt", "inside\n")];
    git.commit_file_change(&a, req(&repo, "main", swap, "file to folder"), &c).await.unwrap();
    assert_eq!(git.blob(repo.id, "main", "thing/inner.txt", 100).await.unwrap().content, b"inside\n");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_symlink_cannot_be_renamed_into_a_file_with_new_content(pool: PgPool) {
    let (f, alice, repo) = setup(pool).await;
    // A real symlink in the repository, pushed over HTTP like any client would.
    let addr = f.serve_http().await;
    let c = Client::new();
    let work = c.path("work");
    c.git(c.dir.path(), &["clone", "-q", &http_url(addr, "alice", "pw-alice", "alice", "demo"), work.to_str().unwrap()]).await;
    std::os::unix::fs::symlink("/etc/passwd", work.join("link")).unwrap();
    c.commit_all(&work, "Add a symlink").await;
    c.git(&work, &["push", "-q", "origin", "HEAD:main"]).await;
    let git = &f.state.git;
    let a = f.actor(alice);
    let info = f.client();
    // The rename's source is a link, so its mode must not reach the new path as content.
    let rename = FileChange { path: "notalink".into(), content: Some(b"x".to_vec()), renamed_from: Some("link".into()) };
    assert!(
        matches!(git.commit_file_change(&a, req(&repo, "main", vec![rename], "Rename the link"), &info).await, Err(Error::Validation(_))),
        "a rename out of a symlink with new content is refused"
    );
    // A plain delete of the link still works.
    let delete = FileChange { path: "link".into(), content: None, renamed_from: None };
    git.commit_file_change(&a, req(&repo, "main", vec![delete], "Remove the link"), &info).await.unwrap();
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_refused_ref_name_copies_nothing_from_the_fork_network(pool: PgPool) {
    let (f, alice, _) = setup(pool).await;
    let bob = f.user("bob").await;
    let public = f.public_repo(alice, "proj").await;
    let fork = f.state.repos.fork(&f.actor(bob), public.id, bob, &f.client()).await.unwrap();
    f.commit(&fork, bob, "main", "only-in-fork.txt", "x\n", "Fork only").await;
    let sha = f.branch_sha(&fork, "main").await;
    // Alice may read the fork, but `refs/pull/` is not hers to write: refused before any fetch.
    for name in ["refs/pull/9/head", "refs/notes/x", "refs/remotes/o/main"] {
        assert!(
            matches!(f.state.git.create_ref(&f.actor(alice), public.id, name, &sha, &f.client()).await, Err(Error::Validation(_))),
            "{name}"
        );
        assert!(f.state.git.update_ref(&f.actor(alice), public.id, name, &sha, true, &f.client()).await.is_err(), "{name}");
    }
    assert!(f.state.git.commit(public.id, &sha).await.is_err(), "the fork's commit was not copied in");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_cancelled_commit_never_moves_a_branch_without_its_event(pool: PgPool) {
    let (f, alice, repo) = setup(pool).await;
    let f = std::sync::Arc::new(f);
    for (i, delay_ms) in (0..40u64).map(|d| d * 3).enumerate() {
        let before = f.branch_sha(&repo, "main").await;
        let pushes_before = f.events("push").await.len();
        let (f2, r2) = (f.clone(), repo.clone());
        let task = tokio::spawn(async move {
            let msg = format!("Try {i}");
            f2.state
                .git
                .commit_file_change(&f2.actor(alice), req(&r2, "main", vec![put("n.txt", &format!("{i}\n"))], &msg), &f2.client())
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        task.abort();
        let _ = task.await;
        // Whatever was started finishes on its own; give it a moment.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        let after = f.branch_sha(&repo, "main").await;
        let pushes = f.events("push").await;
        if after == before {
            assert_eq!(pushes.len(), pushes_before, "try {i}: no ref change, no event");
        } else {
            assert_eq!(pushes.len(), pushes_before + 1, "try {i}: a ref change has its event");
            assert_eq!(pushes.last().unwrap()["after"], after);
        }
    }
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_merge_takes_the_head_it_was_given_or_none(pool: PgPool) {
    // ch-oz8: the gate passed one head, then a push moved the branch. The merge must not take the
    // unreviewed commit in its place.
    let b = pr_setup(pool).await;
    let base = b.f.branch_sha(&b.repo, "main").await;
    let reviewed = b.f.branch_sha(&b.repo, "feature").await;
    b.f.commit(&b.repo, b.alice, "feature", "unreviewed.txt", "x\n", "Pushed after review").await;
    let req = MergeRequest { expected_head_sha: Some(reviewed.clone()), ..merge_req(&b, MergeMethod::Merge, &base) };
    let r = b.f.state.git.merge(&b.f.actor(b.alice), req, &b.f.client()).await;
    assert!(matches!(r, Err(Error::Conflict(_))), "{r:?}");
    assert_eq!(b.f.branch_sha(&b.repo, "main").await, base, "nothing landed");
    // With the head it really has, it merges.
    let now = b.f.branch_sha(&b.repo, "feature").await;
    let req = MergeRequest { expected_head_sha: Some(now), ..merge_req(&b, MergeMethod::Merge, &base) };
    assert!(matches!(b.f.state.git.merge(&b.f.actor(b.alice), req, &b.f.client()).await.unwrap(), MergeOutcome::Merged { .. }));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_person_never_copies_a_commit_in_from_another_repository(pool: PgPool) {
    // ch-1c5: only the server copies across a fork network, for a pull request it checked. A ref
    // made by a person names a commit the repository already has, even one the person can read
    // elsewhere.
    let (f, alice, _) = setup(pool).await;
    let bob = f.user("bob").await;
    let public = f.public_repo(alice, "proj").await;
    let fork = f.state.repos.fork(&f.actor(bob), public.id, bob, &f.client()).await.unwrap();
    f.commit(&fork, bob, "main", "only-in-fork.txt", "x\n", "Fork only").await;
    let sha = f.branch_sha(&fork, "main").await;
    let r = f.state.git.create_ref(&f.actor(alice), public.id, "refs/heads/from-fork", &sha, &f.client()).await;
    assert!(matches!(r, Err(Error::Validation(_))), "{r:?}");
    let r = f.state.git.update_ref(&f.actor(alice), public.id, "refs/heads/main", &sha, true, &f.client()).await;
    assert!(r.is_err(), "{r:?}");
    assert!(f.state.git.commit(public.id, &sha).await.is_err(), "the fork's commit was not copied in");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_token_changes_workflow_files_only_with_the_workflow_scope(pool: PgPool) {
    // ch-s7r: the workflow scope is what lets a token change what runs as CI.
    let (f, alice, repo) = setup(pool).await;
    let git = &f.state.git;
    let c = f.client();
    let repo_only = ch_testkit::fixtures::token_actor(alice, &["repo"]);
    let with_workflow = ch_testkit::fixtures::token_actor(alice, &["repo", "workflow"]);
    for path in [".codehaven/workflows/ci.yml", ".github/workflows/ci.yml"] {
        let r = git.commit_file_change(&repo_only, req(&repo, "main", vec![put(path, "name: ci\n")], "CI"), &c).await;
        assert!(matches!(&r, Err(Error::Forbidden(m)) if m.contains("workflow scope")), "{path}: {r:?}");
    }
    git.commit_file_change(&repo_only, req(&repo, "main", vec![put("docs/a.md", "a\n")], "Docs"), &c).await.unwrap();
    git.commit_file_change(&with_workflow, req(&repo, "main", vec![put(".codehaven/workflows/ci.yml", "name: ci\n")], "CI"), &c)
        .await
        .unwrap();
    // A new branch that only points at what is already there changes nothing, so it is fine.
    let sha = f.branch_sha(&repo, "main").await;
    git.create_ref(&repo_only, repo.id, "refs/heads/copy", &sha, &c).await.unwrap();
}
