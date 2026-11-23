//! The `codehaven admin` commands against a real database: the way the first admin is made, and
//! the recovery commands.

#![allow(clippy::unwrap_used)]

use ch_core::Config;
use ch_core::state::AppState;
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
    assert_eq!(count(&pool, "SELECT count(*) FROM users").await, 0);
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
