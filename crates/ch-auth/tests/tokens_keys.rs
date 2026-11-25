//! Personal access tokens, SSH keys, job tokens and runner credentials.
#![allow(clippy::unwrap_used)]

mod common;
use common::keys::*;
use common::*;

use ch_core::model::*;
use ch_core::{Actor, AuthVia, Error, RepoId, UserId};
use ch_db::tokens::hash_token;
use sqlx::PgPool;

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn token_lifecycle(pool: PgPool) {
    let c = ctx(pool).await;
    let id = make_user(&c, "alice", false).await;
    let (_, me) = session(&c, "alice").await;

    let (rec, secret) = c.state.accounts.create_token(&me, id, "laptop", vec!["repo".into(), "user".into()], 0, &c1()).await.unwrap();
    assert!(secret.starts_with("chp_") && secret.len() == 44, "{secret}");
    assert_eq!(rec.prefix, secret[..8]);
    let days = (rec.expires_at - chrono::Utc::now()).num_days();
    assert!((89..=90).contains(&days), "default is 90 days, got {days}");
    // Only the hash is stored, and the secret is not in the audit log.
    let hits: i64 = sqlx::query_scalar("SELECT count(*) FROM personal_access_tokens WHERE token_hash = $1")
        .bind(hash_token(&secret))
        .fetch_one(&c.pool)
        .await
        .unwrap();
    assert_eq!(hits, 1);
    let leaked: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE details::text LIKE '%' || $1 || '%'")
        .bind(&secret[4..])
        .fetch_one(&c.pool)
        .await
        .unwrap();
    assert_eq!(leaked, 0);

    // Works as Bearer and as Basic with any username.
    let a = c.state.authn.from_bearer(&secret, &c1()).await.unwrap();
    assert!(matches!(&a, Actor::Token { user_id, scopes, .. } if *user_id == id && scopes.has("repo") && scopes.has("user")));
    for user in ["alice", "x-access-token", "whatever"] {
        assert!(matches!(c.state.authn.from_basic(user, &secret, &c1()).await.unwrap(), Actor::Token { .. }), "{user}");
    }
    // Last used is recorded.
    let toks = c.state.accounts.list_tokens(&me, id).await.unwrap();
    assert_eq!(toks.len(), 1);
    assert!(toks[0].last_used_at.is_some());
    // A token never carries site admin power.
    assert!(!a.is_site_admin());

    // Delete ends it.
    c.state.accounts.delete_token(&me, rec.id, &c1()).await.unwrap();
    assert!(c.state.authn.from_bearer(&secret, &c1()).await.is_err());
    assert!(c.state.accounts.list_tokens(&me, id).await.unwrap().is_empty());
    assert!(matches!(c.state.accounts.delete_token(&me, rec.id, &c1()).await, Err(Error::NotFound)));
    assert_eq!(count_audit(&c, "token.created").await, 1);
    assert_eq!(count_audit(&c, "token.deleted").await, 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn token_creation_rules(pool: PgPool) {
    let c = ctx(pool).await;
    let id = make_user(&c, "alice", false).await;
    let other = make_user(&c, "bob", false).await;
    let (_, me) = session(&c, "alice").await;
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();

    assert!(c.state.accounts.create_token(&me, id, "", s(&["repo"]), 30, &c1()).await.is_err(), "needs a name");
    assert!(c.state.accounts.create_token(&me, id, "x", vec![], 30, &c1()).await.is_err(), "needs a scope");
    assert!(c.state.accounts.create_token(&me, id, "x", s(&["root"]), 30, &c1()).await.is_err(), "unknown scope");
    assert!(c.state.accounts.create_token(&me, id, "x", s(&["site_admin"]), 30, &c1()).await.is_err(), "no admin scope exists");
    assert!(c.state.accounts.create_token(&me, id, "x", s(&["repo"]), 366, &c1()).await.is_err(), "a year at most");
    assert!(c.state.accounts.create_token(&me, id, "x", s(&["repo"]), 365, &c1()).await.is_ok());
    assert!(c.state.accounts.create_token(&me, id, "x", s(&["repo", "repo"]), 1, &c1()).await.is_ok(), "duplicates are folded");
    // Not for someone else, not signed out.
    assert!(c.state.accounts.create_token(&me, other, "x", s(&["repo"]), 30, &c1()).await.is_err());
    assert!(c.state.accounts.create_token(&Actor::Anonymous, id, "x", s(&["repo"]), 30, &c1()).await.is_err());
    // A token cannot mint tokens.
    let (_, secret) = c.state.accounts.create_token(&me, id, "boot", s(&["repo", "user"]), 30, &c1()).await.unwrap();
    let as_token = c.state.authn.from_bearer(&secret, &c1()).await.unwrap();
    assert!(matches!(c.state.accounts.create_token(&as_token, id, "child", s(&["repo"]), 30, &c1()).await, Err(Error::Forbidden(_))));
    // Nor delete another account's token, even as a site admin.
    let (_, victim) = c
        .state
        .accounts
        .create_token(
            &Actor::User { id: other, is_admin: false, has_2fa: false, via: AuthVia::Session },
            other,
            "theirs",
            s(&["repo"]),
            30,
            &c1(),
        )
        .await
        .unwrap();
    let theirs = c
        .state
        .accounts
        .list_tokens(&Actor::User { id: other, is_admin: false, has_2fa: false, via: AuthVia::Session }, other)
        .await
        .unwrap();
    assert!(c.state.accounts.delete_token(&me, theirs[0].id, &c1()).await.is_err());
    assert!(c.state.authn.from_bearer(&victim, &c1()).await.is_ok());
    assert!(c.state.accounts.list_tokens(&me, other).await.is_err());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn expired_and_made_up_tokens_fail(pool: PgPool) {
    let c = ctx(pool).await;
    let id = make_user(&c, "alice", false).await;
    let (_, me) = session(&c, "alice").await;
    let (_, secret) = c.state.accounts.create_token(&me, id, "short", vec!["repo".into()], 1, &c1()).await.unwrap();
    sqlx::query("UPDATE personal_access_tokens SET expires_at = now() - interval '1 second'").execute(&c.pool).await.unwrap();
    assert!(c.state.authn.from_bearer(&secret, &c1()).await.is_err());
    for bad in ["chp_".to_string() + &"a".repeat(40), "chp_short".into(), "nonsense".into(), String::new()] {
        assert!(c.state.authn.from_bearer(&bad, &c1()).await.is_err(), "{bad}");
        assert!(c.state.authn.from_basic("alice", &bad, &c1()).await.is_err(), "{bad}");
    }
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn failed_basic_and_bearer_are_limited_per_address(pool: PgPool) {
    let c = ctx(pool).await;
    let attacker = client("192.0.2.66");
    let bad = format!("chp_{}", "z".repeat(40));
    let mut limited_at = None;
    for i in 0..40 {
        match c.state.authn.from_bearer(&bad, &attacker).await {
            Err(Error::RateLimited { .. }) => {
                limited_at = Some(i);
                break;
            }
            Err(Error::Unauthorized(_)) => {}
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(limited_at, Some(30), "30 failures a minute, then refused");
    // Even a good credential from that address waits.
    let id = make_user(&c, "alice", false).await;
    let (_, me) = session(&c, "alice").await;
    let (_, secret) = c.state.accounts.create_token(&me, id, "t", vec!["repo".into()], 5, &c1()).await.unwrap();
    assert!(matches!(c.state.authn.from_bearer(&secret, &attacker).await, Err(Error::RateLimited { .. })));
    // Other addresses are fine.
    assert!(c.state.authn.from_bearer(&secret, &client("192.0.2.67")).await.is_ok());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn ssh_keys(pool: PgPool) {
    let c = ctx(pool).await;
    let id = make_user(&c, "alice", false).await;
    let bob = make_user(&c, "bob", false).await;
    let (_, me) = session(&c, "alice").await;
    let (_, bob_actor) = session(&c, "bob").await;

    let k = c.state.accounts.add_ssh_key(&me, id, "laptop", ED1, &c1()).await.unwrap();
    assert_eq!(k.key_type, "ssh-ed25519");
    assert!(k.fingerprint.starts_with("SHA256:"));
    assert!(matches!(c.state.authn.from_ssh_key(&k.fingerprint).await.unwrap(), Some(SshIdentity::User(u)) if u == id));
    assert!(c.state.authn.from_ssh_key("SHA256:nope").await.unwrap().is_none());

    // The same key twice, even for another account, is refused.
    assert!(c.state.accounts.add_ssh_key(&me, id, "again", ED1, &c1()).await.is_err());
    assert!(c.state.accounts.add_ssh_key(&bob_actor, bob, "stolen", ED1, &c1()).await.is_err());
    // Other types.
    assert!(c.state.accounts.add_ssh_key(&me, id, "second", ED2, &c1()).await.is_ok());
    assert_eq!(c.state.accounts.add_ssh_key(&me, id, "ecdsa", ECDSA, &c1()).await.unwrap().key_type, "ecdsa-sha2-nistp256");
    assert_eq!(c.state.accounts.add_ssh_key(&me, id, "rsa", RSA3072, &c1()).await.unwrap().key_type, "ssh-rsa");
    // Too small, junk, private, other people's accounts.
    assert!(c.state.accounts.add_ssh_key(&me, id, "small", RSA2048, &c1()).await.is_err());
    assert!(c.state.accounts.add_ssh_key(&me, id, "junk", "hello", &c1()).await.is_err());
    assert!(c.state.accounts.add_ssh_key(&me, id, "", ED2, &c1()).await.is_err(), "title needed");
    assert!(c.state.accounts.add_ssh_key(&me, bob, "for bob", ED2, &c1()).await.is_err());
    assert!(c.state.accounts.add_ssh_key(&Actor::Anonymous, id, "x", ED2, &c1()).await.is_err());

    // Only the person (or a site admin) sees the list with its titles and last use (ch-uph).
    // Nobody else can remove them.
    assert_eq!(c.state.accounts.list_ssh_keys(&me, id).await.unwrap().len(), 4);
    assert!(matches!(c.state.accounts.list_ssh_keys(&bob_actor, id).await, Err(Error::NotFound)));
    assert!(c.state.accounts.list_ssh_keys(&Actor::Anonymous, id).await.is_err());
    assert!(c.state.accounts.delete_ssh_key(&bob_actor, k.id, &c1()).await.is_err());
    c.state.accounts.delete_ssh_key(&me, k.id, &c1()).await.unwrap();
    assert!(c.state.authn.from_ssh_key(&k.fingerprint).await.unwrap().is_none(), "a removed key stops working at once");
    assert!(matches!(c.state.accounts.delete_ssh_key(&me, k.id, &c1()).await, Err(Error::NotFound)));
    assert_eq!(count_audit(&c, "ssh_key.added").await, 4);
    assert_eq!(count_audit(&c, "ssh_key.removed").await, 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_token_adds_a_key_only_with_the_power_the_key_gives(pool: PgPool) {
    // ch-fwm: an SSH key acts as the whole account in Git, so a token with only `user` must not
    // be able to turn itself into one. A push over SSH carries no token scopes, so the key also
    // changes workflow files: the token needs `workflow` as well (ch-s7r).
    let c = ctx(pool).await;
    let id = make_user(&c, "alice", false).await;
    let tok = |s: &[&str]| ch_testkit::fixtures::token_actor(id, s);
    for scopes in [&["user"][..], &["repo"], &["repo:read", "user"], &["user", "repo"], &["repo", "workflow"], &[]] {
        let r = c.state.accounts.add_ssh_key(&tok(scopes), id, "k", ED1, &c1()).await;
        assert!(matches!(r, Err(Error::Forbidden(_))), "{scopes:?}: {r:?}");
    }
    assert!(c.state.accounts.add_ssh_key(&tok(&["user", "repo", "workflow"]), id, "k", ED1, &c1()).await.is_ok());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn account_data_needs_the_user_scope_and_a_reset_ends_tokens(pool: PgPool) {
    // ch-p9j: a `repo:read` token cannot list token names or read email addresses, and an admin's
    // reset link revokes every token as well as every session.
    let c = ctx(pool).await;
    let id = make_user(&c, "alice", false).await;
    let (_, me) = session(&c, "alice").await;
    let (_, secret) = c.state.accounts.create_token(&me, id, "ci", vec!["repo:read".into()], 30, &c1()).await.unwrap();
    let narrow = c.state.authn.from_bearer(&secret, &c1()).await.unwrap();
    assert!(matches!(c.state.accounts.list_tokens(&narrow, id).await, Err(Error::Forbidden(_))));
    assert!(matches!(c.state.directory.emails(&narrow, id).await, Err(Error::Forbidden(_))));
    let user_tok = ch_testkit::fixtures::token_actor(id, &["user"]);
    assert!(c.state.accounts.list_tokens(&user_tok, id).await.is_ok());
    assert!(c.state.directory.emails(&user_tok, id).await.is_ok());

    c.state.accounts.issue_link(&Actor::System, id, LinkPurpose::ResetPassword, 60, &c1()).await.unwrap();
    assert!(c.state.authn.from_bearer(&secret, &c1()).await.is_err(), "a reset revokes the account's tokens");
    // A first password link leaves them alone.
    let (_, kept) = c.state.accounts.create_token(&me, id, "ci2", vec!["repo".into()], 30, &c1()).await.unwrap();
    c.state.accounts.issue_link(&Actor::System, id, LinkPurpose::SetPassword, 60, &c1()).await.unwrap();
    assert!(c.state.authn.from_bearer(&kept, &c1()).await.is_ok());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn deploy_keys_resolve_to_their_repo(pool: PgPool) {
    let c = ctx(pool).await;
    let owner = make_user(&c, "alice", false).await;
    let repo = c.env.repo(owner, "demo").await;
    let parsed = ch_auth::sshkeys::parse_public_key(ED2).unwrap();
    sqlx::query("INSERT INTO deploy_keys (repo_id, title, public_key, fingerprint, read_only) VALUES ($1, 'ci', $2, $3, true)")
        .bind(repo.0)
        .bind(&parsed.normalized)
        .bind(&parsed.fingerprint)
        .execute(&c.pool)
        .await
        .unwrap();
    match c.state.authn.from_ssh_key(&parsed.fingerprint).await.unwrap() {
        Some(SshIdentity::Deploy { repo_id, read_only }) => assert!(repo_id == repo && read_only),
        other => panic!("{other:?}"),
    }
}

async fn job_fixture(c: &Ctx, secret: &str, fork: bool) -> (RepoId, i64) {
    let owner = c.env.user(&format!("o{}", rand_suffix())).await;
    let repo = c.env.repo(owner, "demo").await;
    let run: i64 = sqlx::query_scalar("INSERT INTO workflow_runs (repo_id, run_number, event, head_sha, event_payload) VALUES ($1, 1, 'pull_request', 'abc', $2) RETURNING id")
        .bind(repo.0)
        // The shape ch-ci's join_payload stores: the event, then the run's meta. The fork flag is
        // meta.fork; a flat {"fork": true} is not what production writes (ch-x12).
        .bind(serde_json::json!({"event": {"action": "opened"}, "meta": {"workflow_path": ".codehaven/workflows/ci.yml", "workflow_name": "ci", "fork": fork}}))
        .fetch_one(&c.pool)
        .await
        .unwrap();
    let job: i64 = sqlx::query_scalar("INSERT INTO workflow_jobs (run_id, repo_id, job_key, name, token_hash, status) VALUES ($1, $2, 'build', 'build', $3, 'in_progress') RETURNING id")
        .bind(run)
        .bind(repo.0)
        .bind(hash_token(secret))
        .fetch_one(&c.pool)
        .await
        .unwrap();
    (repo, job)
}

fn rand_suffix() -> String {
    ch_db::tokens::random_base62(6).to_lowercase()
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn job_tokens_live_until_the_job_ends_and_fork_jobs_cannot_write(pool: PgPool) {
    let c = ctx(pool).await;
    let (repo, job) = job_fixture(&c, "chj_jobsecret0000000000000000000000000000", false).await;
    let a = c.state.authn.from_bearer("chj_jobsecret0000000000000000000000000000", &c1()).await.unwrap();
    match &a {
        Actor::Job { job_id, repo_id, perms } => {
            assert_eq!(job_id.0, job);
            assert_eq!(*repo_id, repo);
            assert!(perms.contents_read() && perms.runtime_write());
        }
        other => panic!("{other:?}"),
    }
    // `git clone` sends it as the Basic password.
    assert!(matches!(
        c.state.authn.from_basic("x-access-token", "chj_jobsecret0000000000000000000000000000", &c1()).await.unwrap(),
        Actor::Job { .. }
    ));
    sqlx::query("UPDATE workflow_jobs SET status = 'completed'").execute(&c.pool).await.unwrap();
    assert!(c.state.authn.from_bearer("chj_jobsecret0000000000000000000000000000", &c1()).await.is_err(), "dead when the job ends");

    let (_, _) = job_fixture(&c, "chj_forkjob000000000000000000000000000000", true).await;
    match c.state.authn.from_bearer("chj_forkjob000000000000000000000000000000", &c1()).await.unwrap() {
        Actor::Job { perms, .. } => assert!(perms.contents_read() && !perms.runtime_write()),
        other => panic!("{other:?}"),
    }
    // A flat top-level flag is ignored: only meta.fork counts, so the old wrong path cannot come back.
    let (_, flat_job) = job_fixture(&c, "chj_flatjob000000000000000000000000000000", false).await;
    sqlx::query("UPDATE workflow_runs SET event_payload = event_payload || '{\"fork\": true}'::jsonb WHERE id = (SELECT run_id FROM workflow_jobs WHERE id = $1)")
        .bind(flat_job)
        .execute(&c.pool)
        .await
        .unwrap();
    match c.state.authn.from_bearer("chj_flatjob000000000000000000000000000000", &c1()).await.unwrap() {
        Actor::Job { perms, .. } => assert!(perms.runtime_write()),
        other => panic!("{other:?}"),
    }
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn runner_credentials(pool: PgPool) {
    let c = ctx(pool).await;
    let rid: i64 = sqlx::query_scalar("INSERT INTO runners (name, credential_hash) VALUES ('r1', $1) RETURNING id")
        .bind(hash_token("chr_runnercred"))
        .fetch_one(&c.pool)
        .await
        .unwrap();
    assert!(matches!(c.state.authn.from_bearer("chr_runnercred", &c1()).await.unwrap(), Actor::Runner { runner_id } if runner_id.0 == rid));
    // A runner is not a user and reads nothing.
    let owner = c.env.user("o").await;
    let repo = c.env.public_repo(owner, "r").await;
    assert_eq!(
        c.state.authz.repo_permission(&Actor::Runner { runner_id: ch_core::RunnerId(rid) }, repo).await.unwrap(),
        ch_core::Permission::None
    );
    sqlx::query("UPDATE runners SET revoked_at = now()").execute(&c.pool).await.unwrap();
    assert!(c.state.authn.from_bearer("chr_runnercred", &c1()).await.is_err(), "a revoked runner stops at once");
    let _ = UserId(0);
}
