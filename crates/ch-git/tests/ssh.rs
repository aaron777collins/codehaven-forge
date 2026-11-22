//! Git over SSH end to end: the `ch-ssh` server in front of this crate's transport, driven by
//! the real `ssh` and `git` clients.
#![allow(clippy::unwrap_used)]

mod common;

use ch_core::model::*;
use common::*;
use sqlx::PgPool;
use std::path::PathBuf;

struct Ssh {
    f: Fixture,
    c: Client,
    port: u16,
    alice: ch_core::UserId,
    repo: Repo,
}

struct Key {
    path: PathBuf,
    fingerprint: String,
}

impl Ssh {
    async fn new(pool: PgPool) -> Ssh {
        let f = Fixture::new(pool).await;
        let alice = f.user("alice").await;
        let repo = f.repo(alice, "demo", true).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = f.state.clone();
        let cancel = f.cancel.clone();
        tokio::spawn(async move {
            if let Err(e) = ch_ssh::serve_on(state, listener, cancel).await {
                eprintln!("ssh server stopped: {e}");
            }
        });
        Ssh { f, c: Client::new(), port, alice, repo }
    }

    /// A fresh ed25519 key pair on disk, with its fingerprint.
    async fn key(&self, name: &str) -> Key {
        let path = self.c.path(name);
        let out =
            tokio::process::Command::new("ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(&path).output().await.unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let fp = tokio::process::Command::new("ssh-keygen").arg("-lf").arg(path.with_extension("pub")).output().await.unwrap();
        let text = String::from_utf8_lossy(&fp.stdout).to_string();
        let fingerprint = text.split_whitespace().nth(1).unwrap().to_string();
        assert!(fingerprint.starts_with("SHA256:"), "{text}");
        Key { path, fingerprint }
    }

    fn register(&self, key: &Key, who: SshIdentity) {
        self.f.authn.keys.lock().unwrap().insert(key.fingerprint.clone(), who);
    }

    fn ssh_command(&self, key: &Key) -> String {
        format!(
            "ssh -i {} -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o BatchMode=yes -o LogLevel=ERROR -o ConnectTimeout=10",
            key.path.display()
        )
    }

    fn url(&self, repo: &str) -> String {
        format!("ssh://git@127.0.0.1:{}/alice/{repo}.git", self.port)
    }

    async fn git(&self, key: &Key, cwd: &std::path::Path, args: &[&str]) -> GitOut {
        self.c.git_env(cwd, args, &[("GIT_SSH_COMMAND", &self.ssh_command(key)), ("GIT_SSH_VARIANT", "ssh")]).await
    }

    async fn ssh_raw(&self, key: &Key, extra: &[&str]) -> (bool, String, String) {
        let mut c = tokio::process::Command::new("ssh");
        c.args(["-i"]).arg(&key.path).args([
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "BatchMode=yes",
            "-o",
            "LogLevel=ERROR",
            "-o",
            "ConnectTimeout=10",
            "-p",
        ]);
        c.arg(self.port.to_string()).arg("git@127.0.0.1").args(extra);
        let out = tokio::time::timeout(std::time::Duration::from_secs(30), c.output()).await.unwrap().unwrap();
        (out.status.success(), String::from_utf8_lossy(&out.stdout).to_string(), String::from_utf8_lossy(&out.stderr).to_string())
    }
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn clone_push_and_fetch_over_ssh(pool: PgPool) {
    let s = Ssh::new(pool).await;
    let key = s.key("alice_key").await;
    s.register(&key, SshIdentity::User(s.alice));
    let work = s.c.path("work");
    let out = s.git(&key, s.c.dir.path(), &["clone", "-q", &s.url("demo"), work.to_str().unwrap()]).await;
    assert!(out.ok, "{}", out.stderr);
    assert_eq!(std::fs::read_to_string(work.join("README.md")).unwrap(), "# demo\n");
    s.c.write(&work, "over-ssh.txt", "pushed\n");
    let sha = s.c.commit_all(&work, "Over SSH").await;
    let out = s.git(&key, &work, &["push", "-q", "origin", "HEAD:main"]).await;
    assert!(out.ok, "{}", out.stderr);
    assert_eq!(s.f.branch_sha(&s.repo, "main").await, sha);
    // The same pipeline as HTTP: events, with Alice as the pusher.
    let push = s.f.events("push").await.pop().unwrap();
    assert_eq!((push["git_ref"].as_str(), push["pusher"].as_i64()), (Some("refs/heads/main"), Some(s.alice.0)));
    let again = s.git(&key, &work, &["fetch", "-q", "origin"]).await;
    assert!(again.ok, "{}", again.stderr);
    // ls-remote speaks protocol v2 by default and must work too.
    let refs = s.git(&key, &work, &["ls-remote", "origin"]).await;
    assert!(refs.stdout.contains("refs/heads/main"), "{}", refs.stdout);
    // Archives over SSH.
    let arch = s.git(&key, &work, &["archive", &format!("--remote={}", s.url("demo")), "HEAD", "README.md"]).await;
    assert!(arch.ok || arch.stderr.contains("not"), "{}", arch.stderr);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn protection_and_permissions_are_the_same_as_over_http(pool: PgPool) {
    let s = Ssh::new(pool).await;
    let key = s.key("alice_key").await;
    s.register(&key, SshIdentity::User(s.alice));
    s.f.state
        .repos
        .set_protection(
            &s.f.actor(s.alice),
            s.repo.id,
            BranchProtection { pattern: "main".into(), block_force_push: true, block_deletion: true, ..Default::default() },
            &s.f.client(),
        )
        .await
        .unwrap();
    let work = s.c.path("work");
    assert!(s.git(&key, s.c.dir.path(), &["clone", "-q", &s.url("demo"), work.to_str().unwrap()]).await.ok);
    s.c.write(&work, "x.txt", "x\n");
    s.c.commit_all(&work, "Direct").await;
    let before = s.f.branch_sha(&s.repo, "main").await;
    let out = s.git(&key, &work, &["push", "origin", "HEAD:main"]).await;
    assert!(!out.ok);
    assert!(out.stderr.contains("Branch 'main' is protected. Open a pull request to change it."), "{}", out.stderr);
    assert_eq!(s.f.branch_sha(&s.repo, "main").await, before);
    assert!(s.git(&key, &work, &["push", "-q", "origin", "HEAD:topic"]).await.ok);

    // Bob has a key but only read access.
    let bob = s.f.user("bob").await;
    s.f.env.collaborator(s.repo.id, bob, "read").await;
    let bob_key = s.key("bob_key").await;
    s.register(&bob_key, SshIdentity::User(bob));
    let bw = s.c.path("bobs");
    assert!(s.git(&bob_key, s.c.dir.path(), &["clone", "-q", &s.url("demo"), bw.to_str().unwrap()]).await.ok);
    s.c.write(&bw, "b.txt", "b\n");
    s.c.commit_all(&bw, "Bob").await;
    let out = s.git(&bob_key, &bw, &["push", "origin", "HEAD:bob-branch"]).await;
    assert!(!out.ok && out.stderr.contains("write access"), "{}", out.stderr);
    // Carol has a key and no access: the repository does not exist, as far as she can tell.
    let carol = s.f.user("carol").await;
    let ck = s.key("carol_key").await;
    s.register(&ck, SshIdentity::User(carol));
    let out = s.git(&ck, s.c.dir.path(), &["clone", "-q", &s.url("demo"), s.c.path("c").to_str().unwrap()]).await;
    assert!(!out.ok && out.stderr.contains("Repository not found."), "{}", out.stderr);
    let out = s.git(&ck, s.c.dir.path(), &["clone", "-q", &s.url("never-made"), s.c.path("d").to_str().unwrap()]).await;
    assert!(!out.ok && out.stderr.contains("Repository not found."), "{}", out.stderr);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn unknown_keys_get_nothing_and_shells_get_a_polite_no(pool: PgPool) {
    let s = Ssh::new(pool).await;
    let stranger = s.key("stranger").await;
    let out = s.git(&stranger, s.c.dir.path(), &["clone", "-q", &s.url("demo"), s.c.path("x").to_str().unwrap()]).await;
    assert!(!out.ok && out.stderr.contains("Permission denied"), "{}", out.stderr);

    let key = s.key("alice_key").await;
    s.register(&key, SshIdentity::User(s.alice));
    // No command: a shell was asked for, and it is refused with a message.
    let (ok, stdout, stderr) = s.ssh_raw(&key, &["-T"]).await;
    assert!(!ok);
    let text = format!("{stdout}{stderr}");
    assert!(text.contains("does not give shell access"), "{text}");
    // A command that is not git.
    for cmd in ["ls -la", "cat /etc/passwd", "git-upload-pack '/alice/demo.git; id'", "scp -t /tmp"] {
        let (ok, stdout, stderr) = s.ssh_raw(&key, &[cmd]).await;
        assert!(!ok, "{cmd}");
        let text = format!("{stdout}{stderr}");
        assert!(text.contains("only runs git commands") || text.contains("owner/name"), "{cmd}: {text}");
        assert!(!text.contains("root:"), "no shell ran");
    }
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn deploy_keys_are_bound_to_one_repository(pool: PgPool) {
    let s = Ssh::new(pool).await;
    s.f.repo(s.alice, "other", true).await;
    let ro = s.key("deploy_ro").await;
    s.register(&ro, SshIdentity::Deploy { repo_id: s.repo.id, read_only: true });
    let rw = s.key("deploy_rw").await;
    s.register(&rw, SshIdentity::Deploy { repo_id: s.repo.id, read_only: false });

    let work = s.c.path("w");
    assert!(s.git(&ro, s.c.dir.path(), &["clone", "-q", &s.url("demo"), work.to_str().unwrap()]).await.ok);
    s.c.write(&work, "d.txt", "d\n");
    let sha = s.c.commit_all(&work, "From a deploy key").await;
    let out = s.git(&ro, &work, &["push", "origin", "HEAD:deploy"]).await;
    assert!(!out.ok && out.stderr.contains("read-only"), "{}", out.stderr);
    // The writable key pushes. There is no user behind it, so no pusher is recorded.
    let out = s.git(&rw, &work, &["push", "-q", "origin", "HEAD:deploy"]).await;
    assert!(out.ok, "{}", out.stderr);
    assert_eq!(s.f.branch_sha(&s.repo, "deploy").await, sha);
    assert!(s.f.events("push").await.pop().unwrap()["pusher"].is_null());
    // Another repository is off limits to both.
    for k in [&ro, &rw] {
        let out = s.git(k, s.c.dir.path(), &["clone", "-q", &s.url("other"), s.c.path("o").to_str().unwrap()]).await;
        assert!(!out.ok && out.stderr.contains("Repository not found."), "{}", out.stderr);
    }
    // A deploy key cannot reach protected branches by direct push either.
    s.f.state
        .repos
        .set_protection(
            &s.f.actor(s.alice),
            s.repo.id,
            BranchProtection { pattern: "main".into(), block_force_push: true, block_deletion: true, ..Default::default() },
            &s.f.client(),
        )
        .await
        .unwrap();
    let out = s.git(&rw, &work, &["push", "origin", "HEAD:main"]).await;
    assert!(!out.ok && out.stderr.contains("protected"), "{}", out.stderr);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_removed_key_stops_working_at_once(pool: PgPool) {
    let s = Ssh::new(pool).await;
    let key = s.key("alice_key").await;
    s.register(&key, SshIdentity::User(s.alice));
    let out = s.git(&key, s.c.dir.path(), &["ls-remote", &s.url("demo")]).await;
    assert!(out.ok, "{}", out.stderr);
    s.f.authn.keys.lock().unwrap().clear();
    let out = s.git(&key, s.c.dir.path(), &["ls-remote", &s.url("demo")]).await;
    assert!(!out.ok && out.stderr.contains("Permission denied"), "{}", out.stderr);
}
