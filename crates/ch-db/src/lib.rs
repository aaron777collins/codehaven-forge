//! Postgres access for Codehaven: the pool, migrations, the audit log, the domain event
//! outbox, the job queue, the content-addressed blob store and secret sealing.
//!
//! The foundation stream owns `migrations/`. After M0 a schema change is a small pull
//! request to foundation with the migration and the refreshed `.sqlx/` data.

pub mod audit;
pub mod blobs;
pub mod ci_auth;
pub mod jobs;
pub mod outbox;
pub mod sealing;
pub mod tokens;

use ch_core::{Error, Result};
use sqlx::migrate::Migrator;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{ConnectOptions, PgPool};
use std::str::FromStr;
use std::time::Duration;

pub use audit::PgAudit;
pub use blobs::FsBlobStore;
pub use outbox::PgEventBus;

/// The full v1 schema. Other crates use it in tests: `#[sqlx::test(migrator = "ch_db::MIGRATOR")]`.
pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// Opens the pool. Statement logging stays off so no query text with values reaches logs.
pub async fn connect(url: &str, max_connections: u32) -> Result<PgPool> {
    let opts = PgConnectOptions::from_str(url)
        .map_err(|e| Error::internal(anyhow::anyhow!("bad database url: {e}")))?
        .log_statements(tracing::log::LevelFilter::Off)
        .log_slow_statements(tracing::log::LevelFilter::Warn, Duration::from_secs(2));
    PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(Duration::from_secs(10))
        .connect_with(opts)
        .await
        .map_err(|e| Error::internal(anyhow::anyhow!("could not connect to the database: {e}")))
}

/// The checksum migration 0003 had when it was first applied. A comment in that file was later
/// made neutral, which changes the checksum of the file, so a database that applied the first
/// version must still be accepted. See [`migrate`].
const LEGACY_V3_CHECKSUM_HEX: &str = "c996ffb0412b30bd07abc867560a74cbe532e37a433b40dd10a135ead351fa794c1168966db26fcc6c26653acadc074a";

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2).filter_map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()).collect()
}

/// A runtime copy of [`MIGRATOR`] that carries the original checksum for version 3 and nothing else
/// changed. Used only for a database that recorded that original checksum.
fn legacy_migrator() -> Migrator {
    let migrations: Vec<sqlx::migrate::Migration> = MIGRATOR
        .iter()
        .cloned()
        .map(|mut m| {
            if m.version == 3 {
                m.checksum = std::borrow::Cow::Owned(unhex(LEGACY_V3_CHECKSUM_HEX));
            }
            m
        })
        .collect();
    Migrator {
        migrations: std::borrow::Cow::Owned(migrations),
        ignore_missing: MIGRATOR.ignore_missing,
        locking: MIGRATOR.locking,
        no_tx: MIGRATOR.no_tx,
    }
}

/// The checksum recorded for migration `version`, if the table and the row exist.
async fn applied_checksum(pool: &PgPool, version: i64) -> Option<Vec<u8>> {
    sqlx::query_scalar::<_, Vec<u8>>("SELECT checksum FROM _sqlx_migrations WHERE version = $1")
        .bind(version)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
}

/// Applies every pending migration.
///
/// An instance that applied migration 0003 before its comment was made neutral recorded the old
/// checksum. sqlx would refuse to start on it, so for exactly that checksum the run uses a copy of
/// the migrator that expects the old value for version 3. `_sqlx_migrations` is never edited, so
/// an older binary still starts on the same database.
pub async fn migrate(pool: &PgPool) -> Result<()> {
    let legacy = applied_checksum(pool, 3).await.is_some_and(|c| hex(&c) == LEGACY_V3_CHECKSUM_HEX);
    let result = if legacy { legacy_migrator().run(pool).await } else { MIGRATOR.run(pool).await };
    result.map_err(|e| Error::internal(anyhow::anyhow!("migration failed: {e}")))
}

/// How many migrations are applied, and how many this binary knows about.
pub async fn migration_status(pool: &PgPool) -> Result<(usize, usize)> {
    let known = MIGRATOR.iter().count();
    let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE success").fetch_one(pool).await.unwrap_or(0);
    Ok((applied as usize, known))
}

/// Runs `f` in a transaction, committing on `Ok` and rolling back on `Err`.
pub async fn with_tx<T, F>(pool: &PgPool, f: F) -> Result<T>
where
    F: for<'t> FnOnce(&'t mut sqlx::Transaction<'static, sqlx::Postgres>) -> futures::future::BoxFuture<'t, Result<T>>,
{
    let mut tx = pool.begin().await?;
    match f(&mut tx).await {
        Ok(v) => {
            tx.commit().await?;
            Ok(v)
        }
        Err(e) => {
            // Dropping the transaction rolls it back. Do it explicitly to surface errors in logs.
            if let Err(re) = tx.rollback().await {
                tracing::warn!(error = %re, "rollback failed");
            }
            Err(e)
        }
    }
}
