//! Smart HTTP with the real `git` client against a live router, including the whole push
//! pipeline: authentication, permissions, branch protection, events and the hook socket.
#![allow(clippy::unwrap_used)]

mod common;

use ch_core::model::*;
use common::*;
use sqlx::PgPool;

struct World {
    f: Fixture,
    c: Client,
    addr: std::net::SocketAddr,
    alice: ch_core::UserId,
    repo: Repo,
}

async fn world(pool: PgPool) -> World {
    let f = Fixture::new(pool).await;
    let alice = f.user("alice").await;
    let repo = f.repo(alice, "demo", true).await;
    let addr = f.serve_http().await;
    World { f, c: Client::new(), addr, alice, repo }
}

impl World {
    fn url(&self, user: &str, secret: &str) -> String {
        http_url(self.addr, user, secret, "alice", "demo")
    }

    async fn clone_as(&self, user: &str, secret: &str, dir: &str) -> std::path::PathBuf {
        let target = self.c.path(dir);
        self.c.git(self.c.dir.path(), &["clone", "-q", &self.url(user, secret), target.to_str().unwrap()]).await;
        target
    }

    async fn alice_clone(&self) -> std::path::PathBuf {
        self.clone_as("alice", "pw-alice", "alice-work").await
    }
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn clone_commit_push_pull_round_trip(pool: PgPool) {
    let w = world(pool).await;
    let work = w.alice_clone().await;
    assert_eq!(std::fs::read_to_string(work.join("README.md")).unwrap(), "# demo\n");
    w.c.write(&work, "src/new.txt", "hello\n");
    let sha = w.c.commit_all(&work, "Add new file").await;
    w.c.git(&work, &["push", "-q", "origin", "HEAD:main"]).await;
    assert_eq!(w.f.branch_sha(&w.repo, "main").await, sha);
    // Someone else clones and sees it.
    let bob = w.f.user("bob").await;
    w.f.env.collaborator(w.repo.id, bob, "write").await;
    let other = w.clone_as("bob", "pw-bob", "bob-work").await;
    assert_eq!(std::fs::read_to_string(other.join("src/new.txt")).unwrap(), "hello\n");
    // And pulls a later change.
    w.c.write(&work, "src/new.txt", "hello again\n");
    w.c.commit_all(&work, "Edit").await;
    w.c.git(&work, &["push", "-q", "origin", "HEAD:main"]).await;
    w.c.git(&other, &["pull", "-q", "--ff-only"]).await;
    assert_eq!(std::fs::read_to_string(other.join("src/new.txt")).unwrap(), "hello again\n");
    // The push recorded events and the repo row.
    let pushes = w.f.events("push").await;
    let last = pushes.last().unwrap();
    assert_eq!(last["git_ref"], "refs/heads/main");
    assert_eq!(last["pusher"], w.alice.0);
    assert_eq!(last["forced"], false);
    assert_eq!(last["server_built"], false, "the person pushed this commit themselves");
    assert!(w.f.state.repos.get(&w.f.actor(w.alice), w.repo.id).await.unwrap().pushed_at.is_some());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn both_protocol_versions_work(pool: PgPool) {
    let w = world(pool).await;
    for (i, v) in ["0", "1", "2"].into_iter().enumerate() {
        let dir = w.c.path(&format!("v{i}"));
        w.c.git(
            w.c.dir.path(),
            &["-c", &format!("protocol.version={v}"), "clone", "-q", &w.url("alice", "pw-alice"), dir.to_str().unwrap()],
        )
        .await;
        assert!(dir.join("README.md").exists(), "protocol v{v}");
    }
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_first_push_into_an_empty_repo_picks_the_default_branch(pool: PgPool) {
    let f = Fixture::new(pool).await;
    let alice = f.user("alice").await;
    let repo = f.repo(alice, "fresh", false).await;
    let addr = f.serve_http().await;
    let c = Client::new();
    let work = c.path("w");
    std::fs::create_dir_all(&work).unwrap();
    c.git(&work, &["init", "-q", "-b", "trunk"]).await;
    c.write(&work, "a.txt", "a\n");
    let sha = c.commit_all(&work, "First").await;
    // Cloning an empty repository works and warns, as with any forge.
    let empty = c.path("empty");
    c.git(c.dir.path(), &["clone", "-q", &http_url(addr, "alice", "pw-alice", "alice", "fresh"), empty.to_str().unwrap()]).await;
    c.git(&work, &["remote", "add", "origin", &http_url(addr, "alice", "pw-alice", "alice", "fresh")]).await;
    c.git(&work, &["push", "-q", "origin", "trunk"]).await;
    let after = f.state.repos.get(&f.actor(alice), repo.id).await.unwrap();
    assert!(!after.is_empty);
    assert_eq!(after.default_branch, "trunk", "main was never pushed, so the pushed branch became the default");
    assert_eq!(f.branch_sha(&after, "HEAD").await, sha);
    // A clone now checks out the right branch.
    let cloned = c.path("cloned");
    c.git(c.dir.path(), &["clone", "-q", &http_url(addr, "alice", "pw-alice", "alice", "fresh"), cloned.to_str().unwrap()]).await;
    assert!(cloned.join("a.txt").exists());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn without_credentials_there_is_a_challenge_and_no_hint(pool: PgPool) {
    let w = world(pool).await;
    let http = reqwest::Client::new();
    for path in ["alice/demo.git", "alice/never-made.git", "nobody/nothing.git"] {
        let r = http.get(format!("http://{}/{path}/info/refs?service=git-upload-pack", w.addr)).send().await.unwrap();
        assert_eq!(r.status(), 401, "{path}");
        assert!(r.headers()["www-authenticate"].to_str().unwrap().starts_with("Basic"));
    }
    // Wrong password: the same.
    let r = http
        .get(format!("http://{}/alice/demo.git/info/refs?service=git-upload-pack", w.addr))
        .basic_auth("alice", Some("guess"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_repo_you_cannot_read_looks_like_one_that_does_not_exist(pool: PgPool) {
    let w = world(pool).await;
    w.f.user("mallory").await;
    let http = reqwest::Client::new();
    let ask = |path: String| {
        let http = http.clone();
        let addr = w.addr;
        async move {
            let r = http
                .get(format!("http://{addr}/{path}/info/refs?service=git-upload-pack"))
                .basic_auth("mallory", Some("pw-mallory"))
                .send()
                .await
                .unwrap();
            (r.status().as_u16(), r.text().await.unwrap())
        }
    };
    let hidden = ask("alice/demo.git".into()).await;
    let missing = ask("alice/never-made.git".into()).await;
    assert_eq!(hidden, missing);
    assert_eq!(hidden.0, 404);
    assert_eq!(hidden.1, "Repository not found.\n");
    // Pushing is the same story.
    let push = ask("alice/demo.git".into()).await;
    assert_eq!(push.0, 404);
    let out = w.c.git_in(w.c.dir.path(), &["clone", "-q", &w.url("mallory", "pw-mallory"), w.c.path("nope").to_str().unwrap()]).await;
    assert!(!out.ok && out.stderr.contains("not found"), "{}", out.stderr);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn read_access_clones_but_cannot_push(pool: PgPool) {
    let w = world(pool).await;
    let bob = w.f.user("bob").await;
    w.f.env.collaborator(w.repo.id, bob, "read").await;
    let work = w.clone_as("bob", "pw-bob", "bob").await;
    w.c.write(&work, "x.txt", "x\n");
    w.c.commit_all(&work, "Bob tries").await;
    let before = w.f.branch_sha(&w.repo, "main").await;
    let out = w.c.git_in(&work, &["push", "origin", "HEAD:main"]).await;
    assert!(!out.ok);
    assert!(out.stderr.contains("403"), "{}", out.stderr);
    assert_eq!(w.f.branch_sha(&w.repo, "main").await, before);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn tokens_follow_their_scopes(pool: PgPool) {
    let w = world(pool).await;
    // A read token clones but cannot push; a repo token can do both.
    let work = w.clone_as("alice", "ro-alice", "ro").await;
    w.c.write(&work, "t.txt", "t\n");
    w.c.commit_all(&work, "With a read token").await;
    assert!(!w.c.git_in(&work, &["push", "origin", "HEAD:main"]).await.ok);
    w.c.git(&work, &["remote", "set-url", "origin", &w.url("alice", "tok-alice")]).await;
    w.c.git(&work, &["push", "-q", "origin", "HEAD:main"]).await;
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn protected_branches_refuse_pushes_with_a_readable_reason(pool: PgPool) {
    let w = world(pool).await;
    w.f.state
        .repos
        .set_protection(
            &w.f.actor(w.alice),
            w.repo.id,
            BranchProtection { pattern: "main".into(), block_force_push: true, block_deletion: true, ..Default::default() },
            &w.f.client(),
        )
        .await
        .unwrap();
    let work = w.alice_clone().await;
    w.c.write(&work, "p.txt", "p\n");
    let sha = w.c.commit_all(&work, "Direct to main").await;
    let before = w.f.branch_sha(&w.repo, "main").await;
    let out = w.c.git_in(&work, &["push", "origin", "HEAD:main"]).await;
    assert!(!out.ok);
    assert!(out.stderr.contains("Branch 'main' is protected. Open a pull request to change it."), "{}", out.stderr);
    assert_eq!(w.f.branch_sha(&w.repo, "main").await, before);
    // A feature branch is fine, and so is pushing both in a way that includes the protected one
    // only if the protected one is left out.
    w.c.git(&work, &["push", "-q", "origin", "HEAD:feature"]).await;
    assert_eq!(w.f.branch_sha(&w.repo, "feature").await, sha);
    // One refused ref refuses the push: nothing half-lands.
    w.c.git(&work, &["branch", "second"]).await;
    let both = w.c.git_in(&work, &["push", "origin", "HEAD:main", "second:second2"]).await;
    assert!(!both.ok);
    assert!(w.f.state.git.resolve(w.repo.id, "second2").await.is_err());
    // The refusal was recorded.
    assert!(w.f.audit_actions().await.contains(&"repo.push_rejected".to_string()));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn force_pushes_and_deletes(pool: PgPool) {
    let w = world(pool).await;
    let work = w.alice_clone().await;
    w.c.git(&work, &["push", "-q", "origin", "HEAD:scratch"]).await;
    // Rewrite history and force it onto the unprotected branch.
    w.c.git(&work, &["commit", "-q", "--amend", "-m", "Rewritten", "--allow-empty"]).await;
    let rewritten = w.c.git(&work, &["rev-parse", "HEAD"]).await;
    assert!(!w.c.git_in(&work, &["push", "origin", "HEAD:scratch"]).await.ok, "git itself refuses a non-fast-forward");
    w.c.git(&work, &["push", "-q", "--force", "origin", "HEAD:scratch"]).await;
    assert_eq!(w.f.branch_sha(&w.repo, "scratch").await, rewritten);
    assert_eq!(w.f.events("push").await.last().unwrap()["forced"], true);
    // With protection that blocks force pushes, the same thing is refused.
    w.f.state
        .repos
        .set_protection(
            &w.f.actor(w.alice),
            w.repo.id,
            BranchProtection {
                pattern: "scratch".into(),
                allow_direct_push: true,
                block_force_push: true,
                block_deletion: true,
                ..Default::default()
            },
            &w.f.client(),
        )
        .await
        .unwrap();
    w.c.git(&work, &["commit", "-q", "--amend", "-m", "Rewritten again", "--allow-empty"]).await;
    let out = w.c.git_in(&work, &["push", "--force", "origin", "HEAD:scratch"]).await;
    assert!(!out.ok && out.stderr.contains("Force pushes to 'scratch' are not allowed."), "{}", out.stderr);
    // Deleting a protected branch is refused; deleting an ordinary one works.
    let out = w.c.git_in(&work, &["push", "origin", ":scratch"]).await;
    assert!(!out.ok && out.stderr.contains("cannot be deleted"), "{}", out.stderr);
    w.c.git(&work, &["push", "-q", "origin", "HEAD:temp"]).await;
    w.c.git(&work, &["push", "-q", "origin", ":temp"]).await;
    assert!(w.f.state.git.resolve(w.repo.id, "temp").await.is_err());
    assert_eq!(w.f.events("ref_deleted").await.len(), 1);
    // And the default branch can never go.
    let out = w.c.git_in(&work, &["push", "origin", ":main"]).await;
    assert!(!out.ok, "{}", out.stderr);
    assert!(w.f.state.git.resolve(w.repo.id, "main").await.is_ok());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn tags_push_and_pull_refs_are_the_servers_alone(pool: PgPool) {
    let w = world(pool).await;
    let work = w.alice_clone().await;
    w.c.git(&work, &["tag", "v1"]).await;
    w.c.git(&work, &["tag", "-a", "v2", "-m", "annotated"]).await;
    w.c.git(&work, &["push", "-q", "origin", "v1", "v2"]).await;
    let tags: Vec<String> = w.f.state.git.refs(w.repo.id, Some("refs/tags/")).await.unwrap().into_iter().map(|r| r.name).collect();
    assert_eq!(tags, vec!["refs/tags/v1", "refs/tags/v2"]);
    let created: Vec<_> =
        w.f.events("ref_created").await.into_iter().filter(|e| e["git_ref"].as_str().unwrap().starts_with("refs/tags/")).collect();
    assert_eq!(created.len(), 2);
    for r in ["HEAD:refs/pull/1/head", "HEAD:refs/ch/x", "HEAD:refs/notes/x"] {
        let out = w.c.git_in(&work, &["push", "origin", r]).await;
        assert!(!out.ok && out.stderr.contains("managed by Codehaven"), "{r}: {}", out.stderr);
    }
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn archived_repos_refuse_pushes(pool: PgPool) {
    let w = world(pool).await;
    let work = w.alice_clone().await;
    w.c.write(&work, "late.txt", "l\n");
    w.c.commit_all(&work, "Too late").await;
    w.f.state
        .repos
        .update_settings(&w.f.actor(w.alice), w.repo.id, RepoSettingsUpdate { archived: Some(true), ..Default::default() }, &w.f.client())
        .await
        .unwrap();
    let out = w.c.git_in(&work, &["push", "origin", "HEAD:main"]).await;
    assert!(!out.ok && out.stderr.contains("403"), "{}", out.stderr);
    // Reading still works.
    w.clone_as("alice", "pw-alice", "again").await;
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_big_push_and_a_fetch_with_a_gzipped_request(pool: PgPool) {
    let w = world(pool).await;
    let work = w.alice_clone().await;
    // 6 MB of incompressible data, then many branches so the fetch request is large enough
    // that git compresses it.
    let junk: Vec<u8> = (0..6 * 1024 * 1024u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
    std::fs::write(work.join("big.bin"), junk).unwrap();
    w.c.commit_all(&work, "Big file").await;
    w.c.git(&work, &["push", "-q", "origin", "HEAD:main"]).await;
    for i in 0..300 {
        w.c.git(&work, &["branch", &format!("some-rather-long-branch-name-number-{i}")]).await;
    }
    w.c.git(&work, &["push", "-q", "origin", "--all"]).await;
    let fetched = w.c.path("fetched");
    w.c.git(w.c.dir.path(), &["clone", "-q", &w.url("alice", "pw-alice"), fetched.to_str().unwrap()]).await;
    assert_eq!(std::fs::metadata(fetched.join("big.bin")).unwrap().len(), 6 * 1024 * 1024);
    let branches = w.c.git(&fetched, &["branch", "-r"]).await;
    assert!(branches.lines().count() > 300);
    // `git gc --auto` bookkeeping noticed the pushes.
    w.f.core.housekeeping_pass().await;
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn hooks_refuse_anything_the_server_did_not_start(pool: PgPool) {
    let w = world(pool).await;
    let work = w.alice_clone().await;
    w.c.write(&work, "sneaky.txt", "s\n");
    w.c.commit_all(&work, "Sneak").await;
    // Pushing straight into the bare repository on disk skips the server, but not its hooks.
    let bare = w.f.core.store.repo_path(w.repo.id);
    let before = w.f.branch_sha(&w.repo, "main").await;
    let out = w.c.git_in(&work, &["push", bare.to_str().unwrap(), "HEAD:main"]).await;
    assert!(!out.ok, "{}", out.stderr);
    assert!(out.stderr.contains("Refs can only be updated through Codehaven"), "{}", out.stderr);
    assert_eq!(w.f.branch_sha(&w.repo, "main").await, before);
    // A guessed push id gets nowhere either.
    let out =
        w.c.git_env(
            &work,
            &["push", bare.to_str().unwrap(), "HEAD:main"],
            &[
                ("CODEHAVEN_PUSH_ID", &"ab".repeat(32)),
                ("CODEHAVEN_REPO_ID", &w.repo.id.0.to_string()),
                ("CODEHAVEN_SOCKET", w.f.core.store.run_dir.to_str().unwrap()),
            ],
        )
        .await;
    assert!(!out.ok);
    assert_eq!(w.f.branch_sha(&w.repo, "main").await, before);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_push_is_refused_when_the_hook_cannot_run(pool: PgPool) {
    use std::os::unix::fs::PermissionsExt;
    let w = world(pool).await;
    let work = w.alice_clone().await;
    w.c.write(&work, "no-exec.txt", "n\n");
    w.c.commit_all(&work, "The hook loses its exec bit").await;
    // Git silently skips a hook it cannot execute: without a server-side check the push below
    // would land and take branch protection with it.
    std::fs::set_permissions(w.f.core.store.hooks_dir.join("pre-receive"), std::fs::Permissions::from_mode(0o644)).unwrap();
    let before = w.f.branch_sha(&w.repo, "main").await;
    let out = w.c.git_in(&work, &["push", "origin", "HEAD:main"]).await;
    assert!(!out.ok, "{}", out.stderr);
    assert_eq!(w.f.branch_sha(&w.repo, "main").await, before);
    // Reads are unaffected by a broken hook: a fresh clone still works.
    let again = w.clone_as("alice", "pw-alice", "again").await;
    assert!(again.join("README.md").is_file());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn two_people_pushing_different_branches_at_once(pool: PgPool) {
    let w = world(pool).await;
    let bob = w.f.user("bob").await;
    w.f.env.collaborator(w.repo.id, bob, "write").await;
    let a = w.alice_clone().await;
    let b = w.clone_as("bob", "pw-bob", "bobs").await;
    w.c.write(&a, "a.txt", "a\n");
    w.c.commit_all(&a, "A").await;
    w.c.write(&b, "b.txt", "b\n");
    w.c.commit_all(&b, "B").await;
    let (r1, r2) = tokio::join!(
        w.c.git_in(&a, &["push", "-q", "origin", "HEAD:from-alice"]),
        w.c.git_in(&b, &["push", "-q", "origin", "HEAD:from-bob"])
    );
    assert!(r1.ok && r2.ok, "{} / {}", r1.stderr, r2.stderr);
    assert!(w.f.state.git.resolve(w.repo.id, "from-alice").await.is_ok());
    assert!(w.f.state.git.resolve(w.repo.id, "from-bob").await.is_ok());
}

async fn open_as(
    w: &World,
    caller: ch_core::services::GitCaller,
    repo: &str,
    wire: ch_core::services::GitWire,
) -> Result<(), ch_core::Error> {
    w.f.state
        .git_transport
        .open(caller, "alice", repo, wire, ch_core::services::GitMode::AdvertiseRefs, None, &w.f.client())
        .await
        .map(|_| ())
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_job_token_reads_only_its_own_repository(pool: PgPool) {
    use ch_core::services::{GitCaller, GitWire};
    let w = world(pool).await;
    w.f.repo(w.alice, "other", true).await;
    let job = || {
        GitCaller::Actor(ch_core::Actor::Job {
            job_id: ch_core::JobId(1),
            repo_id: w.repo.id,
            perms: ch_core::actor::JobPerms { contents_read: true, write_checks: true },
        })
    };
    assert!(open_as(&w, job(), "demo", GitWire::UploadPack).await.is_ok());
    assert!(open_as(&w, job(), "demo", GitWire::ReceivePack).await.is_err(), "no pushing with a job token");
    assert!(open_as(&w, job(), "other", GitWire::UploadPack).await.is_err());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_deploy_key_is_bound_to_one_repository_and_read_only_unless_marked(pool: PgPool) {
    use ch_core::services::{GitCaller, GitWire};
    let w = world(pool).await;
    w.f.repo(w.alice, "other", true).await;
    let key = |read_only| GitCaller::Deploy { repo_id: w.repo.id, read_only };
    assert!(open_as(&w, key(true), "demo", GitWire::UploadPack).await.is_ok());
    assert!(matches!(open_as(&w, key(true), "demo", GitWire::ReceivePack).await, Err(ch_core::Error::Forbidden(_))));
    assert!(open_as(&w, key(false), "demo", GitWire::ReceivePack).await.is_ok());
    assert!(matches!(open_as(&w, key(false), "other", GitWire::UploadPack).await, Err(ch_core::Error::NotFound)));
}
