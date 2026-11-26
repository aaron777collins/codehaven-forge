//! The schema's own guarantees: the constraints that keep money-equivalent data (accounts,
//! invites, permissions) from going wrong even if application code has a bug.

#![allow(clippy::unwrap_used, clippy::type_complexity)]

use sqlx::PgPool;

async fn user(pool: &PgPool, name: &str) -> i64 {
    sqlx::query_scalar("INSERT INTO users (username) VALUES ($1) RETURNING id").bind(name).fetch_one(pool).await.unwrap()
}

async fn repo(pool: &PgPool, owner: i64, name: &str) -> i64 {
    sqlx::query_scalar("INSERT INTO repositories (owner_id, name) VALUES ($1, $2) RETURNING id")
        .bind(owner)
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn is_violation(r: Result<impl std::fmt::Debug, sqlx::Error>, code: &str) -> bool {
    match r {
        Err(sqlx::Error::Database(e)) => e.code().as_deref() == Some(code),
        _ => false,
    }
}

const UNIQUE: &str = "23505";
const CHECK: &str = "23514";
const FK: &str = "23503";

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn fresh_database_has_zero_accounts(pool: PgPool) {
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE kind <> 'bot'").fetch_one(&pool).await.unwrap();
    assert_eq!(n, 0, "a new instance has no accounts and no seeded admin");
    // The one row it has is the account workflow jobs write as: it cannot sign in.
    let bot: (String, bool, bool) = sqlx::query_as("SELECT username::text, password_hash IS NULL, is_admin FROM users WHERE kind = 'bot'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(bot, ("codehaven-actions".to_string(), true, false));
    let invites: i64 = sqlx::query_scalar("SELECT count(*) FROM invites").fetch_one(&pool).await.unwrap();
    assert_eq!(invites, 0);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn usernames_are_case_insensitive_and_unique(pool: PgPool) {
    user(&pool, "Alice").await;
    assert!(is_violation(sqlx::query("INSERT INTO users (username) VALUES ('alice')").execute(&pool).await, UNIQUE));
    // Lookups ignore case too.
    let id: Option<i64> = sqlx::query_scalar("SELECT id FROM users WHERE username = 'ALICE'").fetch_optional(&pool).await.unwrap();
    assert!(id.is_some());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn usernames_must_be_safe_for_urls(pool: PgPool) {
    for bad in ["", "a b", "-a", "a-", "a/b", "../x", "a.b", "x".repeat(40).as_str(), "a\nb"] {
        let r = sqlx::query("INSERT INTO users (username) VALUES ($1)").bind(bad).execute(&pool).await;
        assert!(is_violation(r, CHECK), "{bad:?} should be refused");
    }
    for good in ["a", "alice", "a-b", "A1", &"x".repeat(39)] {
        sqlx::query("INSERT INTO users (username) VALUES ($1)").bind(good).execute(&pool).await.unwrap_or_else(|e| panic!("{good:?}: {e}"));
    }
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_user_has_at_most_one_primary_email_and_emails_are_unique(pool: PgPool) {
    let a = user(&pool, "alice").await;
    let b = user(&pool, "bob").await;
    sqlx::query("INSERT INTO user_emails (user_id, email, is_primary) VALUES ($1, 'a@x.io', true)").bind(a).execute(&pool).await.unwrap();
    assert!(is_violation(
        sqlx::query("INSERT INTO user_emails (user_id, email, is_primary) VALUES ($1, 'a2@x.io', true)").bind(a).execute(&pool).await,
        UNIQUE
    ));
    assert!(is_violation(
        sqlx::query("INSERT INTO user_emails (user_id, email) VALUES ($1, 'A@X.IO')").bind(b).execute(&pool).await,
        UNIQUE
    ));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn invite_tokens_are_stored_hashed_with_fixed_length(pool: PgPool) {
    // Anything but a 32 byte hash is refused, so a raw token can never be stored by mistake.
    let r = sqlx::query("INSERT INTO invites (token_hash, expires_at) VALUES ('\\x6368695f736563726574', now() + interval '1 day')")
        .execute(&pool)
        .await;
    assert!(is_violation(r, CHECK));
    sqlx::query("INSERT INTO invites (token_hash, expires_at) VALUES (sha256('chi_secret'), now() + interval '1 day')")
        .execute(&pool)
        .await
        .unwrap();
    let dup = sqlx::query("INSERT INTO invites (token_hash, expires_at) VALUES (sha256('chi_secret'), now() + interval '1 day')")
        .execute(&pool)
        .await;
    assert!(is_violation(dup, UNIQUE));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn invite_cannot_expire_before_it_exists(pool: PgPool) {
    let r =
        sqlx::query("INSERT INTO invites (token_hash, expires_at) VALUES (sha256('x'), now() - interval '1 hour')").execute(&pool).await;
    assert!(is_violation(r, CHECK));
}

// The single-use guarantee from ARCHITECTURE.md 7.4, at the SQL level: 20 racing accepts of
// one link claim it once.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn racing_invite_claims_succeed_exactly_once(pool: PgPool) {
    sqlx::query("INSERT INTO invites (token_hash, expires_at) VALUES (sha256('chi_race'), now() + interval '1 day')")
        .execute(&pool)
        .await
        .unwrap();
    let mut handles = vec![];
    for _ in 0..20 {
        let pool = pool.clone();
        handles.push(tokio::spawn(async move {
            let mut tx = pool.begin().await.unwrap();
            let claimed: Option<i64> = sqlx::query_scalar(
                "UPDATE invites SET used_at = now() WHERE token_hash = sha256('chi_race') AND used_at IS NULL AND revoked_at IS NULL AND expires_at > now() RETURNING id",
            )
            .fetch_optional(&mut *tx)
            .await
            .unwrap();
            tx.commit().await.unwrap();
            claimed.is_some()
        }));
    }
    let mut wins = 0;
    for h in handles {
        if h.await.unwrap() {
            wins += 1;
        }
    }
    assert_eq!(wins, 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn session_and_token_hashes_must_be_32_bytes(pool: PgPool) {
    let u = user(&pool, "alice").await;
    let r = sqlx::query(
        "INSERT INTO sessions (token_hash, user_id, csrf_secret, expires_at) VALUES ('\\x00', $1, 'x', now() + interval '1 day')",
    )
    .bind(u)
    .execute(&pool)
    .await;
    assert!(is_violation(r, CHECK));
    let r = sqlx::query("INSERT INTO personal_access_tokens (user_id, name, token_hash, prefix, scopes, expires_at) VALUES ($1, 'n', '\\x00', 'chp_abcd', '{repo}', now() + interval '1 day')").bind(u).execute(&pool).await;
    assert!(is_violation(r, CHECK));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn deleting_a_user_removes_their_credentials_but_keeps_the_audit_trail(pool: PgPool) {
    let u = user(&pool, "alice").await;
    sqlx::query(
        "INSERT INTO sessions (token_hash, user_id, csrf_secret, expires_at) VALUES (sha256('s'), $1, 'x', now() + interval '1 day')",
    )
    .bind(u)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO ssh_keys (user_id, title, public_key, key_type, fingerprint) VALUES ($1, 't', 'k', 'ed25519', 'SHA256:abc')")
        .bind(u)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO audit_events (actor_kind, actor_id, actor_label, action, outcome) VALUES ('user', $1, 'user:1', 'auth.sign_in', 'success')").bind(u).execute(&pool).await.unwrap();
    sqlx::query("DELETE FROM users WHERE id = $1").bind(u).execute(&pool).await.unwrap();
    for t in ["sessions", "ssh_keys"] {
        let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {t}")).fetch_one(&pool).await.unwrap();
        assert_eq!(n, 0, "{t} should cascade");
    }
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events").fetch_one(&pool).await.unwrap();
    assert_eq!(n, 1, "audit history outlives the account");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn repo_names_are_unique_per_owner_and_case_insensitive(pool: PgPool) {
    let a = user(&pool, "alice").await;
    let b = user(&pool, "bob").await;
    repo(&pool, a, "Demo").await;
    assert!(is_violation(
        sqlx::query("INSERT INTO repositories (owner_id, name) VALUES ($1, 'demo')").bind(a).execute(&pool).await,
        UNIQUE
    ));
    repo(&pool, b, "demo").await;
    for bad in ["", "a b", "a/b", "..", ".", "a\\b"] {
        let r = sqlx::query("INSERT INTO repositories (owner_id, name) VALUES ($1, $2)").bind(a).bind(bad).execute(&pool).await;
        assert!(is_violation(r, CHECK), "{bad:?}");
    }
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn visibility_is_public_or_private_only(pool: PgPool) {
    let a = user(&pool, "alice").await;
    let r = sqlx::query("INSERT INTO repositories (owner_id, name, visibility) VALUES ($1, 'x', 'anonymous')").bind(a).execute(&pool).await;
    assert!(is_violation(r, CHECK), "the legacy column takes only public or private");
    let r =
        sqlx::query("INSERT INTO repositories (owner_id, name, access_tier) VALUES ($1, 'z', 'anonymous')").bind(a).execute(&pool).await;
    assert!(is_violation(r, CHECK), "the tier is private, internal or public");
    let v: String = sqlx::query_scalar("INSERT INTO repositories (owner_id, name) VALUES ($1, 'y') RETURNING visibility")
        .bind(a)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(v, "private", "repositories default to private");
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn permissions_are_a_closed_set(pool: PgPool) {
    let a = user(&pool, "alice").await;
    let b = user(&pool, "bob").await;
    let r = repo(&pool, a, "demo").await;
    let bad = sqlx::query("INSERT INTO repo_collaborators (repo_id, user_id, permission) VALUES ($1, $2, 'root')")
        .bind(r)
        .bind(b)
        .execute(&pool)
        .await;
    assert!(is_violation(bad, CHECK));
    sqlx::query("INSERT INTO repo_collaborators (repo_id, user_id, permission) VALUES ($1, $2, 'write')")
        .bind(r)
        .bind(b)
        .execute(&pool)
        .await
        .unwrap();
    assert!(is_violation(
        sqlx::query("INSERT INTO repo_collaborators (repo_id, user_id, permission) VALUES ($1, $2, 'read')")
            .bind(r)
            .bind(b)
            .execute(&pool)
            .await,
        UNIQUE
    ));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn issue_numbers_are_unique_per_repo_and_prs_share_them(pool: PgPool) {
    let a = user(&pool, "alice").await;
    let r1 = repo(&pool, a, "one").await;
    let r2 = repo(&pool, a, "two").await;
    sqlx::query("INSERT INTO issues (repo_id, number, author_id, title) VALUES ($1, 1, $2, 'i')")
        .bind(r1)
        .bind(a)
        .execute(&pool)
        .await
        .unwrap();
    assert!(is_violation(
        sqlx::query("INSERT INTO issues (repo_id, number, author_id, title, is_pull) VALUES ($1, 1, $2, 'p', true)")
            .bind(r1)
            .bind(a)
            .execute(&pool)
            .await,
        UNIQUE
    ));
    sqlx::query("INSERT INTO issues (repo_id, number, author_id, title) VALUES ($1, 1, $2, 'other repo')")
        .bind(r2)
        .bind(a)
        .execute(&pool)
        .await
        .unwrap();
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn next_number_takes_each_number_once_under_concurrency(pool: PgPool) {
    let a = user(&pool, "alice").await;
    let r = repo(&pool, a, "demo").await;
    let mut handles = vec![];
    for _ in 0..25 {
        let pool = pool.clone();
        handles.push(tokio::spawn(async move {
            sqlx::query_scalar::<_, i64>("UPDATE repositories SET next_number = next_number + 1 WHERE id = $1 RETURNING next_number - 1")
                .bind(r)
                .fetch_one(&pool)
                .await
                .unwrap()
        }));
    }
    let mut got = vec![];
    for h in handles {
        got.push(h.await.unwrap());
    }
    got.sort();
    assert_eq!(got, (1..=25).collect::<Vec<_>>());
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn review_request_names_exactly_one_target(pool: PgPool) {
    let a = user(&pool, "alice").await;
    let r = repo(&pool, a, "demo").await;
    let issue: i64 =
        sqlx::query_scalar("INSERT INTO issues (repo_id, number, author_id, title, is_pull) VALUES ($1, 1, $2, 'p', true) RETURNING id")
            .bind(r)
            .bind(a)
            .fetch_one(&pool)
            .await
            .unwrap();
    sqlx::query(
        "INSERT INTO pull_requests (issue_id, repo_id, head_ref, head_sha, base_ref, base_sha) VALUES ($1, $2, 'f', 'a', 'main', 'b')",
    )
    .bind(issue)
    .bind(r)
    .execute(&pool)
    .await
    .unwrap();
    let none = sqlx::query("INSERT INTO review_requests (pull_id) VALUES ($1)").bind(issue).execute(&pool).await;
    assert!(is_violation(none, CHECK));
    sqlx::query("INSERT INTO review_requests (pull_id, user_id) VALUES ($1, $2)").bind(issue).bind(a).execute(&pool).await.unwrap();
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn secrets_have_exactly_one_scope_and_unique_names(pool: PgPool) {
    let a = user(&pool, "alice").await;
    let r = repo(&pool, a, "demo").await;
    let both = sqlx::query("INSERT INTO secrets (scope, repo_id, org_id, name, value_sealed) VALUES ('repo', $1, $2, 'TOKEN', 'x')")
        .bind(r)
        .bind(a)
        .execute(&pool)
        .await;
    assert!(is_violation(both, CHECK));
    let lower = sqlx::query("INSERT INTO secrets (scope, repo_id, name, value_sealed) VALUES ('repo', $1, 'token', 'x')")
        .bind(r)
        .execute(&pool)
        .await;
    assert!(is_violation(lower, CHECK), "secret names are upper case");
    sqlx::query("INSERT INTO secrets (scope, repo_id, name, value_sealed) VALUES ('repo', $1, 'TOKEN', 'x')")
        .bind(r)
        .execute(&pool)
        .await
        .unwrap();
    assert!(is_violation(
        sqlx::query("INSERT INTO secrets (scope, repo_id, name, value_sealed) VALUES ('repo', $1, 'TOKEN', 'y')")
            .bind(r)
            .execute(&pool)
            .await,
        UNIQUE
    ));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn runner_credentials_and_registration_tokens_are_hashes(pool: PgPool) {
    let r = sqlx::query("INSERT INTO runners (name, credential_hash) VALUES ('r', '\\x01')").execute(&pool).await;
    assert!(is_violation(r, CHECK));
    let r = sqlx::query("INSERT INTO runner_registration_tokens (token_hash, expires_at) VALUES ('\\x01', now() + interval '1 hour')")
        .execute(&pool)
        .await;
    assert!(is_violation(r, CHECK));
    sqlx::query("INSERT INTO runner_registration_tokens (token_hash, expires_at) VALUES (sha256('t'), now() + interval '1 hour')")
        .execute(&pool)
        .await
        .unwrap();
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn foreign_keys_refuse_orphans(pool: PgPool) {
    let r = sqlx::query("INSERT INTO repositories (owner_id, name) VALUES (999999, 'x')").execute(&pool).await;
    assert!(is_violation(r, FK));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn full_text_search_columns_are_generated(pool: PgPool) {
    let a = user(&pool, "alice").await;
    sqlx::query("INSERT INTO repositories (owner_id, name, description, topics) VALUES ($1, 'harbour', 'a small forge', '{rust,git}')")
        .bind(a)
        .execute(&pool)
        .await
        .unwrap();
    let hits: i64 = sqlx::query_scalar("SELECT count(*) FROM repositories WHERE search @@ plainto_tsquery('simple', 'forge')")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(hits, 1);
    let hits: i64 = sqlx::query_scalar("SELECT count(*) FROM repositories WHERE search @@ plainto_tsquery('simple', 'rust')")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(hits, 1, "topics are searchable");
    // Trigram search for the quick finder.
    let hits: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE username::text % 'alise'").fetch_one(&pool).await.unwrap();
    assert_eq!(hits, 1);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn migrations_are_all_recorded(pool: PgPool) {
    let (applied, known) = ch_db::migration_status(&pool).await.unwrap();
    assert_eq!(applied, known);
    assert!(known >= 8);
}

// ---- migration 0003 keeps working on a database that applied its first version ----

/// The text migration 0003 had before a comment in it was made neutral. Rebuilt from the file as
/// it is now, so this test does not carry the old wording itself.
fn first_version_of_0003() -> String {
    let now = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/migrations/0003_access.sql")).unwrap();
    let neutral = "-- NULL when made by the CLI.";
    assert!(now.contains(neutral), "0003 changed: update this test");
    let host_number = 3;
    now.replacen(neutral, &format!("-- NULL when made by the CLI on dev{host_number}."), 1)
}

/// A migrator that is the current one with the first version of 0003, as an instance that was
/// set up before the comment changed applied it.
fn migrator_as_first_applied() -> sqlx::migrate::Migrator {
    let old = first_version_of_0003();
    let migrations: Vec<sqlx::migrate::Migration> = ch_db::MIGRATOR
        .iter()
        .cloned()
        .map(|m| {
            if m.version == 3 {
                sqlx::migrate::Migration::new(m.version, m.description.clone(), m.migration_type, old.clone().into(), m.no_tx)
            } else {
                m
            }
        })
        .collect();
    sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(migrations),
        ignore_missing: ch_db::MIGRATOR.ignore_missing,
        locking: ch_db::MIGRATOR.locking,
        no_tx: ch_db::MIGRATOR.no_tx,
    }
}

async fn checksum_of(pool: &PgPool, version: i64) -> Vec<u8> {
    sqlx::query_scalar("SELECT checksum FROM _sqlx_migrations WHERE version = $1").bind(version).fetch_one(pool).await.unwrap()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[sqlx::test(migrations = false)]
async fn a_database_that_applied_the_first_0003_still_migrates(pool: PgPool) {
    migrator_as_first_applied().run(&pool).await.unwrap();
    let before = checksum_of(&pool, 3).await;
    assert_eq!(
        hex(&before),
        "c996ffb0412b30bd07abc867560a74cbe532e37a433b40dd10a135ead351fa794c1168966db26fcc6c26653acadc074a",
        "the rebuilt first version must hash to the checksum live recorded"
    );
    let alice = user(&pool, "alice").await;
    ch_db::migrate(&pool).await.expect("an instance on the first 0003 must still start");
    assert_eq!(checksum_of(&pool, 3).await, before, "_sqlx_migrations is never edited");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE id = $1").bind(alice).fetch_one(&pool).await.unwrap();
    assert_eq!(n, 1, "no row is lost");
    // Running it again is still fine.
    ch_db::migrate(&pool).await.unwrap();
}

#[sqlx::test(migrations = false)]
async fn a_new_database_records_the_current_0003(pool: PgPool) {
    ch_db::migrate(&pool).await.unwrap();
    let current = ch_db::MIGRATOR.iter().find(|m| m.version == 3).unwrap().checksum.to_vec();
    assert_eq!(checksum_of(&pool, 3).await, current);
}

#[sqlx::test(migrations = false)]
async fn any_other_0003_checksum_is_still_refused(pool: PgPool) {
    ch_db::migrate(&pool).await.unwrap();
    sqlx::query("UPDATE _sqlx_migrations SET checksum = '\\x00' WHERE version = 3").execute(&pool).await.unwrap();
    assert!(ch_db::migrate(&pool).await.is_err(), "a changed migration must not be waved through");
}

// ---- migration 0120 (connected apps) over a seeded copy of the previous schema ----

/// The previous release's schema: every migration before 0120, with the first version of 0003 as
/// an instance set up before its comment was made neutral recorded it.
fn migrator_before_0120() -> sqlx::migrate::Migrator {
    let all = migrator_as_first_applied();
    let migrations: Vec<sqlx::migrate::Migration> = all.iter().filter(|m| m.version < 120).cloned().collect();
    assert!(migrations.iter().any(|m| m.version == 111), "the previous release ends at 0111");
    sqlx::migrate::Migrator { migrations: std::borrow::Cow::Owned(migrations), ignore_missing: false, locking: true, no_tx: false }
}

/// Every table with its row count and a fingerprint of its contents, so "nothing changed" is a
/// comparison and not a hope. `_sqlx_migrations` is left out: it is meant to gain a row.
async fn fingerprint(pool: &PgPool) -> std::collections::BTreeMap<String, (i64, String)> {
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT tablename::text FROM pg_tables WHERE schemaname = 'public' AND tablename <> '_sqlx_migrations' ORDER BY 1",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let mut out = std::collections::BTreeMap::new();
    for t in tables {
        // Test-only: the table name comes from the catalog.
        let (n, h): (i64, String) =
            sqlx::query_as(&format!("SELECT count(*), COALESCE(md5(string_agg(x::text, '|' ORDER BY x::text)), '') FROM {t} x"))
                .fetch_one(pool)
                .await
                .unwrap();
        out.insert(t, (n, h));
    }
    out
}

#[sqlx::test(migrations = false)]
async fn the_oauth_migration_changes_no_existing_row_and_adds_only_empty_tables(pool: PgPool) {
    migrator_before_0120().run(&pool).await.unwrap();
    // Seed what an instance in use holds.
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    sqlx::query("INSERT INTO user_emails (user_id, email, is_primary, verified_at) VALUES ($1, 'alice@example.org', true, now())")
        .bind(alice)
        .execute(&pool)
        .await
        .unwrap();
    let r1 = repo(&pool, alice, "one").await;
    repo(&pool, bob, "two").await;
    sqlx::query("INSERT INTO repo_collaborators (repo_id, user_id, permission) VALUES ($1, $2, 'write')")
        .bind(r1)
        .bind(bob)
        .execute(&pool)
        .await
        .unwrap();
    for n in 1..=3 {
        sqlx::query("INSERT INTO issues (repo_id, number, author_id, title, body) VALUES ($1, $2, $3, $4, 'body')")
            .bind(r1)
            .bind(n as i64)
            .bind(alice)
            .bind(format!("Issue {n}"))
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO personal_access_tokens (user_id, name, token_hash, prefix, scopes, expires_at) VALUES ($1, 'ci', $2, 'chp_abcd', ARRAY['repo'], now() + interval '30 days')")
        .bind(alice)
        .bind(vec![7u8; 32])
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO sessions (token_hash, user_id, csrf_secret, expires_at) VALUES ($1, $2, $3, now() + interval '1 day')")
        .bind(vec![8u8; 32])
        .bind(alice)
        .bind(vec![9u8; 32])
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO audit_events (actor_kind, actor_id, actor_label, action, outcome, details) VALUES ('user', $1, 'user:1', 'auth.sign_in', 'success', '{}'::jsonb)").bind(alice).execute(&pool).await.unwrap();
    let before = fingerprint(&pool).await;
    assert!(before.values().filter(|(n, _)| *n > 0).count() >= 8, "the seed touched many tables: {before:?}");
    let checksum_before = checksum_of(&pool, 3).await;
    let applied_before: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations").fetch_one(&pool).await.unwrap();

    // Upgrade to 0120 exactly. Later migrations have their own tests over a seeded copy (see
    // `visibility.rs` for 0121), and the full upgrade runs at the end of this test.
    let to_0120 = || {
        let all = migrator_as_first_applied();
        let migrations: Vec<sqlx::migrate::Migration> = all.iter().filter(|m| m.version <= 120).cloned().collect();
        sqlx::migrate::Migrator { migrations: std::borrow::Cow::Owned(migrations), ignore_missing: false, locking: true, no_tx: false }
    };
    to_0120().run(&pool).await.expect("the upgrade must run on an instance with data");

    let after = fingerprint(&pool).await;
    for (table, was) in &before {
        assert_eq!(after.get(table), Some(was), "{table} changed: a migration that only adds tables must leave every old row alone");
    }
    let added: Vec<&String> = after.keys().filter(|k| !before.contains_key(*k)).collect();
    assert_eq!(
        added.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        ["oauth_codes", "oauth_grant_repos", "oauth_grants", "oauth_requests", "oauth_tokens"],
        "exactly the five new tables"
    );
    for t in ["oauth_grants", "oauth_grant_repos", "oauth_requests", "oauth_codes", "oauth_tokens"] {
        assert_eq!(after[t].0, 0, "{t} starts empty");
    }
    assert_eq!(checksum_of(&pool, 3).await, checksum_before, "the first-applied 0003 checksum is kept");
    let applied_after: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations").fetch_one(&pool).await.unwrap();
    assert_eq!(applied_after, applied_before + 1);
    // Running again is a no-op.
    to_0120().run(&pool).await.unwrap();
    assert_eq!(fingerprint(&pool).await, after);
    // And the current binary's migrator takes it the rest of the way, keeping the old 0003.
    ch_db::migrate(&pool).await.expect("the full upgrade runs on top");
    assert_eq!(checksum_of(&pool, 3).await, checksum_before);
    // The old binary's queries still work on the new schema (rollback runs it without --migrate).
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM issues i JOIN repositories r ON r.id = i.repo_id JOIN users u ON u.id = r.owner_id")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(n, 3);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn grant_rows_obey_their_constraints(pool: PgPool) {
    let u = user(&pool, "alice").await;
    let r = repo(&pool, u, "one").await;
    let ins = |scopes: &'static str, read_all: bool, write_all: bool, wf_all: bool| {
        let pool = pool.clone();
        async move {
            sqlx::query(&format!(
                "INSERT INTO oauth_grants (user_id, client_id, client_host, client_name, redirect_uri, resource, scopes, read_all, write_all, workflows_all, expires_at)
                 VALUES ($1, 'https://claude.ai/c', 'claude.ai', 'Claude', 'https://claude.ai/cb', 'https://code.example.com/mcp', {scopes}, $2, $3, $4, now() + interval '90 days') RETURNING id"
            ))
            .bind(u)
            .bind(read_all)
            .bind(write_all)
            .bind(wf_all)
            .fetch_one(&pool)
            .await
        }
    };
    ins("ARRAY['read']::text[]", true, false, false).await.unwrap();
    ins("ARRAY['read','write']::text[]", true, true, false).await.unwrap();
    ins("ARRAY['read','write','workflows']::text[]", true, true, true).await.unwrap();
    // Read is required, only the three scopes exist, write needs read-all, workflows need write.
    assert!(is_violation(ins("ARRAY['write']::text[]", true, false, false).await, CHECK), "read is always granted");
    assert!(is_violation(ins("ARRAY['read','admin']::text[]", true, false, false).await, CHECK), "no other scope");
    assert!(is_violation(ins("ARRAY['read','write']::text[]", false, true, false).await, CHECK), "write all needs read all");
    assert!(is_violation(ins("ARRAY['read','workflows']::text[]", true, false, false).await, CHECK), "workflows need write");
    assert!(is_violation(ins("ARRAY['read','write']::text[]", true, true, true).await, CHECK), "workflows all needs the workflows scope");
    // Per-repository rows: workflows need write, one row per repository and grant, and a deleted
    // repository takes its rows with it.
    let g: i64 = sqlx::query_scalar("SELECT id FROM oauth_grants LIMIT 1").fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO oauth_grant_repos (grant_id, repo_id, can_write) VALUES ($1, $2, true)")
        .bind(g)
        .bind(r)
        .execute(&pool)
        .await
        .unwrap();
    assert!(is_violation(
        sqlx::query("INSERT INTO oauth_grant_repos (grant_id, repo_id) VALUES ($1, $2)").bind(g).bind(r).execute(&pool).await,
        UNIQUE
    ));
    let r2 = repo(&pool, u, "two").await;
    assert!(is_violation(
        sqlx::query("INSERT INTO oauth_grant_repos (grant_id, repo_id, can_write, can_workflows) VALUES ($1, $2, false, true)")
            .bind(g)
            .bind(r2)
            .execute(&pool)
            .await,
        CHECK
    ));
    sqlx::query("DELETE FROM repositories WHERE id = $1").bind(r).execute(&pool).await.unwrap();
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM oauth_grant_repos").fetch_one(&pool).await.unwrap();
    assert_eq!(left, 0);
    // Deleting the account takes its grants.
    sqlx::query("DELETE FROM users WHERE id = $1").bind(u).execute(&pool).await.unwrap();
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM oauth_grants").fetch_one(&pool).await.unwrap();
    assert_eq!(left, 0);
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn token_and_code_hashes_must_be_32_bytes_and_unique(pool: PgPool) {
    let u = user(&pool, "alice").await;
    let g: i64 = sqlx::query_scalar(
        "INSERT INTO oauth_grants (user_id, client_id, client_host, client_name, redirect_uri, resource, scopes, expires_at)
         VALUES ($1, 'c', 'h', 'n', 'r', 'x', ARRAY['read'], now() + interval '1 day') RETURNING id",
    )
    .bind(u)
    .fetch_one(&pool)
    .await
    .unwrap();
    let tok = |h: Vec<u8>, kind: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query("INSERT INTO oauth_tokens (grant_id, kind, token_hash, expires_at) VALUES ($1, $2, $3, now() + interval '1 hour')")
                .bind(g)
                .bind(kind)
                .bind(h)
                .execute(&pool)
                .await
        }
    };
    tok(vec![1; 32], "access").await.unwrap();
    assert!(is_violation(tok(vec![1; 32], "refresh").await, UNIQUE));
    assert!(is_violation(tok(vec![2; 31], "access").await, CHECK));
    assert!(is_violation(tok(vec![3; 32], "session").await, CHECK));
    assert!(is_violation(
        sqlx::query(
            "INSERT INTO oauth_codes (code_hash, grant_id, redirect_uri, code_challenge, expires_at) VALUES ($1, $2, 'r', 'c', now())"
        )
        .bind(vec![1u8; 5])
        .bind(g)
        .execute(&pool)
        .await,
        CHECK
    ));
}

#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn a_pull_request_from_a_fork_stays_one_after_the_fork_is_deleted(pool: PgPool) {
    // Migration 0122: CI must tell a branch of the same repository (head_repo_id NULL) from a
    // fork that was deleted (head_repo_id set to NULL by ON DELETE SET NULL).
    let alice = user(&pool, "alice").await;
    let bob = user(&pool, "bob").await;
    let base = repo(&pool, alice, "demo").await;
    let fork = repo(&pool, bob, "demo").await;
    let mut pulls = Vec::new();
    for (n, head) in [(1_i64, None), (2, Some(base)), (3, Some(fork))] {
        let issue: i64 = sqlx::query_scalar(
            "INSERT INTO issues (repo_id, number, is_pull, author_id, title) VALUES ($1, $2, true, $3, 't') RETURNING id",
        )
        .bind(base)
        .bind(n)
        .bind(bob)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO pull_requests (issue_id, repo_id, head_repo_id, head_ref, head_sha, base_ref, base_sha) VALUES ($1, $2, $3, 'f', 'abc', 'main', 'abc')")
            .bind(issue)
            .bind(base)
            .bind(head)
            .execute(&pool)
            .await
            .unwrap();
        pulls.push(issue);
    }
    let flags = |pool: PgPool| async move {
        sqlx::query_scalar::<_, bool>("SELECT from_fork FROM pull_requests ORDER BY issue_id").fetch_all(&pool).await.unwrap()
    };
    assert_eq!(flags(pool.clone()).await, vec![false, false, true]);
    sqlx::query("DELETE FROM repositories WHERE id = $1").bind(fork).execute(&pool).await.unwrap();
    let head: Option<i64> =
        sqlx::query_scalar("SELECT head_repo_id FROM pull_requests WHERE issue_id = $1").bind(pulls[2]).fetch_one(&pool).await.unwrap();
    assert_eq!(head, None);
    assert_eq!(flags(pool.clone()).await, vec![false, false, true], "the flag outlives the fork");
    // Nothing clears it, not even a direct write.
    sqlx::query("UPDATE pull_requests SET from_fork = false").execute(&pool).await.unwrap();
    assert_eq!(flags(pool.clone()).await, vec![false, false, true]);
}

/// ch-c7c: the image before migration 0139 pins and unpins with `UPDATE issues SET pinned = ...`
/// and nothing else. After a rollback that must still work, and still leave the CHECK true.
#[sqlx::test(migrator = "ch_db::MIGRATOR")]
async fn an_older_writer_can_pin_and_unpin_without_a_position(pool: PgPool) {
    let u = user(&pool, "alice").await;
    let r = repo(&pool, u, "demo").await;
    for n in 1..=3_i64 {
        sqlx::query("INSERT INTO issues (repo_id, number, author_id, title) VALUES ($1, $2, $3, 'i')")
            .bind(r)
            .bind(n)
            .bind(u)
            .execute(&pool)
            .await
            .unwrap();
    }
    // What the C9 image writes: the flag alone.
    for n in [1_i64, 2] {
        sqlx::query("UPDATE issues SET pinned = true WHERE repo_id = $1 AND number = $2").bind(r).bind(n).execute(&pool).await.unwrap();
    }
    let rows: Vec<(i64, Option<i32>)> =
        sqlx::query_as("SELECT number, pin_position FROM issues WHERE repo_id = $1 AND pinned ORDER BY number")
            .bind(r)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(rows, vec![(1, Some(0)), (2, Some(1))], "an older pin goes last");
    sqlx::query("UPDATE issues SET pinned = false WHERE repo_id = $1 AND number = 1").bind(r).execute(&pool).await.unwrap();
    let gone: Option<i32> =
        sqlx::query_scalar("SELECT pin_position FROM issues WHERE repo_id = $1 AND number = 1").bind(r).fetch_one(&pool).await.unwrap();
    assert_eq!(gone, None, "an unpinned issue has no position");
    // This version's own writes are untouched by the trigger.
    sqlx::query("UPDATE issues SET pinned = true, pin_position = 7 WHERE repo_id = $1 AND number = 3")
        .bind(r)
        .execute(&pool)
        .await
        .unwrap();
    let p: Option<i32> =
        sqlx::query_scalar("SELECT pin_position FROM issues WHERE repo_id = $1 AND number = 3").bind(r).fetch_one(&pool).await.unwrap();
    assert_eq!(p, Some(7));
}
