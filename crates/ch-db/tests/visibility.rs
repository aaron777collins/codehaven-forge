//! The three visibility tiers in the schema (epic ch-4pn): the migration over a copy of a
//! seeded old schema, and the triggers that keep every write, old binary or new, from making
//! anything public by accident.

#![allow(clippy::unwrap_used, clippy::type_complexity)]

use sqlx::PgPool;
use sqlx::migrate::Migrator;
use std::borrow::Cow;

/// The version that adds the tiers.
const TIERS: i64 = 121;

/// The migrations before the tiers: the schema an instance has before this release.
fn old_schema() -> Migrator {
    Migrator {
        migrations: Cow::Owned(ch_db::MIGRATOR.iter().filter(|m| m.version < TIERS).cloned().collect()),
        ignore_missing: ch_db::MIGRATOR.ignore_missing,
        locking: ch_db::MIGRATOR.locking,
        no_tx: ch_db::MIGRATOR.no_tx,
    }
}

async fn user(pool: &PgPool, name: &str) -> i64 {
    sqlx::query_scalar("INSERT INTO users (username) VALUES ($1) RETURNING id").bind(name).fetch_one(pool).await.unwrap()
}

/// A repository the way the old binary writes one: only `visibility`.
async fn old_repo(pool: &PgPool, owner: i64, name: &str, visibility: &str, parent: Option<i64>) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO repositories (owner_id, name, visibility, fork_parent_id, fork_root_id) VALUES ($1, $2, $3, $4, $4) RETURNING id",
    )
    .bind(owner)
    .bind(name)
    .bind(visibility)
    .bind(parent)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn tier(pool: &PgPool, id: i64) -> (String, String) {
    sqlx::query_as("SELECT access_tier, visibility FROM repositories WHERE id = $1").bind(id).fetch_one(pool).await.unwrap()
}

async fn count(pool: &PgPool, table: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT count(*) FROM {table}")).fetch_one(pool).await.unwrap()
}

fn is_check_violation<T: std::fmt::Debug>(r: Result<T, sqlx::Error>) -> bool {
    matches!(r, Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("23514"))
}

#[sqlx::test(migrations = false)]
async fn the_migration_turns_old_public_into_internal_and_makes_nothing_public(pool: PgPool) {
    old_schema().run(&pool).await.unwrap();
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let open = old_repo(&pool, alice, "open", "public", None).await;
    let secret = old_repo(&pool, alice, "secret", "private", None).await;
    // A fork that was more visible than its private parent, and its own public child.
    let leaky = old_repo(&pool, bob, "leaky", "public", Some(secret)).await;
    let leaky_child = old_repo(&pool, alice, "leaky-child", "public", Some(leaky)).await;
    // A fork of a public parent keeps its reach.
    let fork = old_repo(&pool, bob, "open-fork", "public", Some(open)).await;
    let private_fork = old_repo(&pool, bob, "open-private-fork", "private", Some(open)).await;
    sqlx::query("INSERT INTO repo_redirects (old_owner, old_name, repo_id) VALUES ('alice', 'old-open', $1), ('alice', 'old-secret', $2)")
        .bind(open)
        .bind(secret)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO commit_statuses (repo_id, sha, state) VALUES ($1, 'abc', 'success'), ($2, 'def', 'failure')")
        .bind(open)
        .bind(secret)
        .execute(&pool)
        .await
        .unwrap();
    let before = (count(&pool, "repositories").await, count(&pool, "repo_redirects").await, count(&pool, "commit_statuses").await);

    ch_db::MIGRATOR.run(&pool).await.unwrap();

    let after = (count(&pool, "repositories").await, count(&pool, "repo_redirects").await, count(&pool, "commit_statuses").await);
    assert_eq!(before, after, "no row is added or lost");
    let public: i64 = sqlx::query_scalar("SELECT count(*) FROM repositories WHERE access_tier = 'public'").fetch_one(&pool).await.unwrap();
    assert_eq!(public, 0, "nothing becomes public");
    assert_eq!(tier(&pool, open).await, ("internal".into(), "public".into()), "old public means every signed-in account");
    assert_eq!(tier(&pool, secret).await, ("private".into(), "private".into()));
    assert_eq!(tier(&pool, leaky).await, ("private".into(), "private".into()), "a fork above its private parent is capped");
    assert_eq!(tier(&pool, leaky_child).await, ("private".into(), "private".into()), "the whole chain settles");
    assert_eq!(tier(&pool, fork).await, ("internal".into(), "public".into()));
    assert_eq!(tier(&pool, private_fork).await, ("private".into(), "private".into()));
    let switch: bool = sqlx::query_scalar("SELECT allow_public_repos FROM visibility_settings").fetch_one(&pool).await.unwrap();
    assert!(!switch, "public repositories start off");
    let policy: Vec<String> = sqlx::query_scalar("SELECT DISTINCT contribution_policy FROM repositories").fetch_all(&pool).await.unwrap();
    assert_eq!(policy, vec!["everyone".to_string()], "contribution stays as it was");
    let audiences: Vec<(String, String)> =
        sqlx::query_as("SELECT old_name::text, audience FROM repo_redirects ORDER BY old_name").fetch_all(&pool).await.unwrap();
    assert_eq!(audiences, vec![("old-open".into(), "internal".into()), ("old-secret".into(), "private".into())]);
    let recorded: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT sha, recorded_tier FROM commit_statuses ORDER BY sha").fetch_all(&pool).await.unwrap();
    assert_eq!(recorded, vec![("abc".into(), Some("internal".into())), ("def".into(), Some("private".into()))]);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn an_old_binary_can_write_private_or_internal_and_never_public(pool: PgPool) {
    let alice = user(&pool, "alice").await;
    let r = old_repo(&pool, alice, "a", "public", None).await;
    assert_eq!(tier(&pool, r).await.0, "internal", "an old insert of public is internal");
    sqlx::query("UPDATE visibility_settings SET allow_public_repos = true").execute(&pool).await.unwrap();
    // Even with the switch on, an old binary's public to private to public ends internal.
    sqlx::query("UPDATE repositories SET visibility = 'private' WHERE id = $1").bind(r).execute(&pool).await.unwrap();
    assert_eq!(tier(&pool, r).await, ("private".into(), "private".into()));
    sqlx::query("UPDATE repositories SET visibility = 'public' WHERE id = $1").bind(r).execute(&pool).await.unwrap();
    assert_eq!(tier(&pool, r).await, ("internal".into(), "public".into()));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn nothing_is_created_public_and_public_needs_the_switch_and_a_date(pool: PgPool) {
    let alice = user(&pool, "alice").await;
    let insert_public =
        sqlx::query("INSERT INTO repositories (owner_id, name, access_tier, public_since) VALUES ($1, 'p', 'public', now())")
            .bind(alice)
            .execute(&pool)
            .await;
    assert!(is_check_violation(insert_public), "a repository cannot be created public");
    let r = old_repo(&pool, alice, "r", "private", None).await;
    let raise = |since: bool| {
        let pool = pool.clone();
        async move {
            sqlx::query(if since {
                "UPDATE repositories SET access_tier = 'public', public_since = now() WHERE id = $1"
            } else {
                "UPDATE repositories SET access_tier = 'public' WHERE id = $1"
            })
            .bind(r)
            .execute(&pool)
            .await
        }
    };
    assert!(is_check_violation(raise(true).await), "the switch is off");
    sqlx::query("UPDATE visibility_settings SET allow_public_repos = true").execute(&pool).await.unwrap();
    assert!(is_check_violation(raise(false).await), "public needs public_since");
    raise(true).await.unwrap();
    assert_eq!(tier(&pool, r).await, ("public".into(), "public".into()));
    // Narrowing clears the public fields.
    sqlx::query("UPDATE repositories SET access_tier = 'internal' WHERE id = $1").bind(r).execute(&pool).await.unwrap();
    let since: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT public_since FROM repositories WHERE id = $1").bind(r).fetch_one(&pool).await.unwrap();
    assert!(since.is_none());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn turning_the_switch_off_lowers_every_public_repository_and_on_raises_none(pool: PgPool) {
    let alice = user(&pool, "alice").await;
    sqlx::query("UPDATE visibility_settings SET allow_public_repos = true").execute(&pool).await.unwrap();
    let a = old_repo(&pool, alice, "a", "private", None).await;
    let b = old_repo(&pool, alice, "b", "private", None).await;
    for id in [a, b] {
        sqlx::query("UPDATE repositories SET access_tier = 'public', public_since = now() WHERE id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query("UPDATE visibility_settings SET allow_public_repos = false").execute(&pool).await.unwrap();
    assert_eq!(tier(&pool, a).await.0, "internal");
    assert_eq!(tier(&pool, b).await.0, "internal");
    sqlx::query("UPDATE visibility_settings SET allow_public_repos = true").execute(&pool).await.unwrap();
    assert_eq!(tier(&pool, a).await.0, "internal", "on again does not republish");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_fork_is_never_above_its_parent_and_lowering_a_parent_lowers_its_forks(pool: PgPool) {
    let alice = user(&pool, "alice").await;
    sqlx::query("UPDATE visibility_settings SET allow_public_repos = true").execute(&pool).await.unwrap();
    let root = old_repo(&pool, alice, "root", "public", None).await;
    let fork = old_repo(&pool, alice, "fork", "public", Some(root)).await;
    let grandchild = old_repo(&pool, alice, "grandchild", "public", Some(fork)).await;
    // Raising a fork above its parent lands at the parent's tier.
    sqlx::query("UPDATE repositories SET access_tier = 'public', public_since = now() WHERE id = $1")
        .bind(fork)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(tier(&pool, fork).await.0, "internal", "capped at the parent");
    sqlx::query("UPDATE repositories SET access_tier = 'private' WHERE id = $1").bind(root).execute(&pool).await.unwrap();
    assert_eq!(tier(&pool, fork).await.0, "private", "lowering a parent lowers its fork");
    assert_eq!(tier(&pool, grandchild).await.0, "private", "and the fork's own forks");
    sqlx::query("UPDATE repositories SET access_tier = 'internal' WHERE id = $1").bind(root).execute(&pool).await.unwrap();
    assert_eq!(tier(&pool, fork).await.0, "private", "raising a parent raises none");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn redirects_and_ci_records_keep_the_tier_they_were_made_at(pool: PgPool) {
    let alice = user(&pool, "alice").await;
    let r = old_repo(&pool, alice, "r", "private", None).await;
    sqlx::query("INSERT INTO repo_redirects (old_owner, old_name, repo_id) VALUES ('alice', 'was-private', $1)")
        .bind(r)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO commit_statuses (repo_id, sha, state) VALUES ($1, 'one', 'success')").bind(r).execute(&pool).await.unwrap();
    sqlx::query("UPDATE visibility_settings SET allow_public_repos = true").execute(&pool).await.unwrap();
    sqlx::query("UPDATE repositories SET access_tier = 'public', public_since = now() WHERE id = $1").bind(r).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO repo_redirects (old_owner, old_name, repo_id) VALUES ('alice', 'was-public', $1)")
        .bind(r)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO commit_statuses (repo_id, sha, state) VALUES ($1, 'two', 'success')").bind(r).execute(&pool).await.unwrap();
    let audiences: Vec<(String, String)> =
        sqlx::query_as("SELECT old_name::text, audience FROM repo_redirects ORDER BY old_name").fetch_all(&pool).await.unwrap();
    assert_eq!(audiences, vec![("was-private".into(), "private".into()), ("was-public".into(), "public".into())]);
    let recorded: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT sha, recorded_tier FROM commit_statuses ORDER BY sha").fetch_all(&pool).await.unwrap();
    assert_eq!(recorded, vec![("one".into(), Some("private".into())), ("two".into(), Some("public".into()))]);
}
