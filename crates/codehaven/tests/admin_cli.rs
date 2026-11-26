//! The `codehaven admin` commands against a real database: the way the first admin is made, and
//! the recovery commands.

#![allow(clippy::unwrap_used)]

use ch_core::state::AppState;
use ch_core::{Actor, Config};
use codehaven::admin;
use codehaven::cli::{AdminCommand, CreateUser, InviteCommand};
use codehaven::wiring;
use sqlx::PgPool;
use std::sync::Arc;

async fn state(pool: PgPool) -> (AppState, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::for_tests();
    cfg.storage.data_dir = dir.path().to_path_buf();
    cfg.server.external_url = "https://code.example.com".into();
    (wiring::build_state(Arc::new(cfg), pool).await.unwrap(), dir)
}

async fn count(pool: &PgPool, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

fn create(username: &str, admin_flag: bool) -> AdminCommand {
    AdminCommand::CreateUser(CreateUser {
        username: username.into(),
        email: format!("{username}@example.org"),
        admin: admin_flag,
        can_run_ci: false,
        password_stdin: false,
    })
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn create_user_makes_an_account_with_no_password_and_a_one_hour_link(pool: PgPool) {
    let (state, _dir) = state(pool.clone()).await;
    assert_eq!(count(&pool, "SELECT count(*) FROM users WHERE kind = 'user'").await, 0);
    admin::run(&state, create("alice", true)).await.unwrap();
    let (admin_flag, has_pw): (bool, bool) =
        sqlx::query_as("SELECT is_admin, password_hash IS NOT NULL FROM users WHERE username = 'alice'").fetch_one(&pool).await.unwrap();
    assert!(admin_flag && !has_pw);
    let minutes: f64 = sqlx::query_scalar(
        "SELECT extract(epoch FROM (expires_at - now()))::float8 / 60 FROM one_time_links WHERE purpose = 'set_password'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!((58.0..=60.1).contains(&minutes), "{minutes}");
    // The same name twice is refused with a plain message.
    let err = admin::run(&state, create("alice", false)).await.unwrap_err().to_string();
    assert!(!err.is_empty() && !err.contains("23505"), "{err}");
    // Reset signs out everywhere and replaces the link.
    admin::run(&state, AdminCommand::ResetPassword { username: "alice".into() }).await.unwrap();
    assert_eq!(count(&pool, "SELECT count(*) FROM one_time_links WHERE purpose = 'reset_password' AND used_at IS NULL").await, 1);
    assert!(admin::run(&state, AdminCommand::ResetPassword { username: "nobody".into() }).await.is_err());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn invites_with_org_and_team_and_the_recovery_commands(pool: PgPool) {
    let (state, _dir) = state(pool.clone()).await;
    admin::run(&state, create("alice", true)).await.unwrap();
    let org: i64 =
        sqlx::query_scalar("INSERT INTO users (kind, username) VALUES ('org', 'haven') RETURNING id").fetch_one(&pool).await.unwrap();
    let team: i64 =
        sqlx::query_scalar("INSERT INTO teams (org_id, name) VALUES ($1, 'core') RETURNING id").bind(org).fetch_one(&pool).await.unwrap();

    let invite = |org: Option<&str>, team: Option<&str>, expires: &str| AdminCommand::Invite {
        command: InviteCommand::Create {
            email: Some("pal@example.org".into()),
            admin: false,
            can_run_ci: true,
            org: org.map(String::from),
            team: team.map(String::from),
            expires: expires.into(),
            note: Some("pal".into()),
        },
    };
    admin::run(&state, invite(Some("haven"), Some("core"), "2d")).await.unwrap();
    let (o, t, ci, hours): (Option<i64>, Option<i64>, bool, f64) =
        sqlx::query_as("SELECT org_id, team_id, grants_ci, extract(epoch FROM (expires_at - created_at))::float8 / 3600 FROM invites")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((o, t, ci), (Some(org), Some(team), true));
    assert!((47.9..=48.1).contains(&hours), "{hours}");
    assert!(admin::run(&state, invite(Some("nope"), None, "1d")).await.is_err(), "no such org");
    assert!(admin::run(&state, invite(Some("haven"), Some("nope"), "1d")).await.is_err(), "no such team");
    assert!(admin::run(&state, invite(None, None, "31d")).await.is_err(), "30 days at most");
    assert!(admin::run(&state, invite(None, None, "0h")).await.is_err(), "1 hour at least");
    admin::run(&state, invite(None, None, "1h")).await.unwrap();
    admin::run(&state, AdminCommand::Invite { command: InviteCommand::List }).await.unwrap();
    let id: i64 = sqlx::query_scalar("SELECT id FROM invites ORDER BY id LIMIT 1").fetch_one(&pool).await.unwrap();
    admin::run(&state, AdminCommand::Invite { command: InviteCommand::Revoke { id } }).await.unwrap();
    assert_eq!(count(&pool, "SELECT count(*) FROM invites WHERE revoked_at IS NOT NULL").await, 1);

    // Lockout, unlock, suspend, unsuspend, list.
    sqlx::query("INSERT INTO login_failures (key, locked_until) SELECT 'acct:' || id, now() + interval '1 hour' FROM users WHERE username = 'alice'").execute(&pool).await.unwrap();
    admin::run(&state, AdminCommand::Unlock { username: "alice".into() }).await.unwrap();
    assert_eq!(count(&pool, "SELECT count(*) FROM login_failures").await, 0);
    admin::run(&state, create("bob", false)).await.unwrap();
    admin::run(&state, AdminCommand::Suspend { username: "bob".into() }).await.unwrap();
    assert_eq!(count(&pool, "SELECT count(*) FROM users WHERE username = 'bob' AND suspended_at IS NOT NULL").await, 1);
    admin::run(&state, AdminCommand::Unsuspend { username: "bob".into() }).await.unwrap();
    assert_eq!(count(&pool, "SELECT count(*) FROM users WHERE username = 'bob' AND suspended_at IS NOT NULL").await, 0);
    admin::run(&state, AdminCommand::Users).await.unwrap();
    // Every command above is in the audit log as the CLI.
    let who: Vec<String> = sqlx::query_scalar("SELECT DISTINCT actor_label FROM audit_events ORDER BY 1").fetch_all(&pool).await.unwrap();
    assert_eq!(who, vec!["System (cli)"]);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn the_public_repositories_switch_is_audited_and_off_lowers_every_public_repository(pool: PgPool) {
    use codehaven::cli::PublicReposCommand;
    let (state, _dir) = state(pool.clone()).await;
    admin::run(&state, create("alice", false)).await.unwrap();
    let alice: i64 = count(&pool, "SELECT id FROM users WHERE username = 'alice'").await;
    sqlx::query("INSERT INTO repositories (owner_id, name, access_tier) VALUES ($1, 'a', 'internal'), ($1, 'b', 'internal')")
        .bind(alice)
        .execute(&pool)
        .await
        .unwrap();
    admin::run(&state, AdminCommand::PublicRepos { command: PublicReposCommand::Status }).await.unwrap();
    admin::run(&state, AdminCommand::PublicRepos { command: PublicReposCommand::On }).await.unwrap();
    assert_eq!(count(&pool, "SELECT count(*) FROM repositories WHERE access_tier = 'public'").await, 0, "on makes nothing public");
    sqlx::query("UPDATE repositories SET access_tier = 'public', public_since = now() WHERE name = 'a'").execute(&pool).await.unwrap();
    admin::run(&state, AdminCommand::PublicRepos { command: PublicReposCommand::Status }).await.unwrap();
    admin::run(&state, AdminCommand::PublicRepos { command: PublicReposCommand::Off }).await.unwrap();
    assert_eq!(count(&pool, "SELECT count(*) FROM repositories WHERE access_tier = 'public'").await, 0);
    assert_eq!(count(&pool, "SELECT count(*) FROM repositories WHERE name = 'a' AND access_tier = 'internal'").await, 1);
    assert_eq!(count(&pool, "SELECT count(*) FROM audit_events WHERE action = 'instance.public_repos_changed'").await, 2);
    assert_eq!(
        count(&pool, "SELECT count(*) FROM audit_events WHERE action = 'repo.visibility_changed' AND details->>'reason' IS NOT NULL").await,
        1,
        "each lowered repository is audited"
    );
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn runners_are_listed_and_revoked_from_the_cli(pool: PgPool) {
    let (state, _dir) = state(pool.clone()).await;
    // The credential hash is SHA-256 sized: the table checks it is 32 bytes.
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO runners (name, labels, credential_hash) VALUES ('old', ARRAY['linux'], sha256('old')) RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    admin::run(&state, AdminCommand::Runners).await.unwrap();
    admin::run(&state, AdminCommand::RevokeRunner { id }).await.unwrap();
    assert_eq!(count(&pool, &format!("SELECT count(*) FROM runners WHERE id = {id} AND revoked_at IS NOT NULL")).await, 1);
    assert_eq!(count(&pool, "SELECT count(*) FROM audit_events WHERE action = 'runner.removed'").await, 1);
    // A second time there is nothing to remove.
    assert!(admin::run(&state, AdminCommand::RevokeRunner { id }).await.is_err());
}

/// A bare repository with one commit on `main` and a tag, made with the system git.
fn bare_source(dir: &std::path::Path) -> std::path::PathBuf {
    let work = dir.join("work");
    let bare = dir.join("source.git");
    let git = |cwd: &std::path::Path, args: &[&str]| {
        let out = std::process::Command::new("git")
            .current_dir(cwd)
            .args(["-c", "user.name=T", "-c", "user.email=t@example.org", "-c", "init.defaultBranch=main", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q"]);
    std::fs::write(work.join("README.md"), "the source\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "first"]);
    git(&work, &["tag", "v1"]);
    git(dir, &["clone", "-q", "--bare", work.to_str().unwrap(), bare.to_str().unwrap()]);
    bare
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn repo_import_makes_a_private_repository_for_an_existing_account_and_audits_it(pool: PgPool) {
    let (state, dir) = state(pool.clone()).await;
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let bare = bare_source(&src);
    admin::run(&state, create("aaron", false)).await.unwrap();
    let import = |owner: &str, name: &str, from: &std::path::Path, run_ci: bool| AdminCommand::RepoImport {
        owner: owner.into(),
        name: name.into(),
        from: from.to_str().unwrap().into(),
        run_ci,
    };
    // An owner that does not exist, a path that is not a bare repository, a path that is not absolute: refused, nothing made.
    assert!(admin::run(&state, import("nobody", "codehaven", &bare, false)).await.is_err());
    assert!(
        admin::run(&state, import("aaron", "codehaven", &src.join("work"), false)).await.is_err(),
        "a working tree is not a bare repository"
    );
    assert!(admin::run(&state, import("aaron", "codehaven", std::path::Path::new("relative/x.git"), false)).await.is_err());
    assert_eq!(count(&pool, "SELECT count(*) FROM repositories").await, 0);
    // The import: private, owned by Aaron, with the branch and the tag, and the source untouched.
    admin::run(&state, import("aaron", "codehaven", &bare, false)).await.unwrap();
    let (tier, owner, empty, branch): (String, String, bool, String) = sqlx::query_as(
        "SELECT r.access_tier, u.username::text, r.is_empty, r.default_branch FROM repositories r JOIN users u ON u.id = r.owner_id WHERE r.name = 'codehaven'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((tier.as_str(), owner.as_str(), empty, branch.as_str()), ("private", "aaron", false, "main"));
    let repo = state.repos.get_by_name(&Actor::System, "aaron", "codehaven").await.unwrap();
    assert_eq!(state.git.blob(repo.id, "main", "README.md", 100).await.unwrap().content, b"the source\n");
    assert_eq!(state.git.refs(repo.id, Some("refs/tags/")).await.unwrap().len(), 1);
    // Both a person and a stranger see nothing of it.
    let stranger = Actor::Anonymous;
    assert!(matches!(state.repos.get(&stranger, repo.id).await, Err(ch_core::Error::NotFound)));
    // It is audited, naming where it came from, and a second import under the same name is refused whole.
    assert_eq!(count(&pool, "SELECT count(*) FROM audit_events WHERE action = 'repo.imported_from_server_path'").await, 1);
    assert!(admin::run(&state, import("aaron", "codehaven", &bare, false)).await.is_err());
    assert_eq!(count(&pool, "SELECT count(*) FROM repositories").await, 1);
    // With --run-ci the default branch is announced as a push by the owner, once.
    let before = count(&pool, "SELECT count(*) FROM outbox_events WHERE kind = 'push'").await;
    admin::run(&state, import("aaron", "second", &bare, true)).await.unwrap();
    assert_eq!(count(&pool, "SELECT count(*) FROM outbox_events WHERE kind = 'push'").await, before + 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn run_ci_starts_a_workflow_as_the_repository_owner(pool: PgPool) {
    use ch_core::model::*;
    let (state, _dir) = state(pool.clone()).await;
    admin::run(&state, create("alice", true)).await.unwrap();
    let alice: i64 = sqlx::query_scalar("SELECT id FROM users WHERE username = 'alice'").fetch_one(&pool).await.unwrap();
    let who = Actor::User { id: ch_core::UserId(alice), is_admin: false, has_2fa: true, via: ch_core::AuthVia::Session };
    let c = ClientInfo::local();
    let repo = state
        .repos
        .create(
            &who,
            NewRepo {
                owner_id: ch_core::UserId(alice),
                name: "app".into(),
                description: String::new(),
                visibility: Visibility::Private,
                default_branch: None,
                auto_init: true,
            },
            &c,
        )
        .await
        .unwrap();
    let expected = state.git.resolve(repo.id, "refs/heads/main").await.unwrap();
    state
        .git
        .commit_file_change(
            &who,
            CommitFileChange {
                repo_id: repo.id,
                branch: "main".into(),
                new_branch: None,
                expected_sha: Some(expected),
                changes: vec![FileChange {
                    path: ".codehaven/workflows/ci.yml".into(),
                    content: Some(
                        b"name: ci\non: workflow_dispatch\njobs:\n  a:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n"
                            .to_vec(),
                    ),
                    renamed_from: None,
                }],
                message: "workflow".into(),
                author: Signature { name: "A".into(), email: "a@example.org".into(), at: chrono::Utc::now() },
            },
            &c,
        )
        .await
        .unwrap();
    let run = |repo: &str, workflow: &str| AdminCommand::RunCi { repo: repo.into(), workflow: workflow.into(), branch: String::new() };
    // The owner may not run CI yet: refused in plain words, nothing created.
    let err = admin::run(&state, run("alice/app", "ci.yml")).await.unwrap_err().to_string();
    assert!(err.contains("Workflows are off"), "{err}");
    sqlx::query("UPDATE users SET can_run_ci = true WHERE id = $1").bind(alice).execute(&pool).await.unwrap();
    admin::run(&state, run("alice/app", "ci.yml")).await.unwrap();
    let (event, jobs): (String, i64) =
        sqlx::query_as("SELECT r.event, (SELECT count(*) FROM workflow_jobs j WHERE j.run_id = r.id) FROM workflow_runs r")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((event.as_str(), jobs), ("workflow_dispatch", 1));
    assert!(admin::run(&state, run("alice/app", "nope.yml")).await.is_err());
    assert!(admin::run(&state, run("alice/missing", "ci.yml")).await.is_err());
    assert!(admin::run(&state, run("not-a-repo", "ci.yml")).await.is_err());
}
