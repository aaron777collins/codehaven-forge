//! Test support shared by every stream: a database harness, fixture builders, temporary git
//! repositories, and the actors the permission tests need.
//!
//! Database tests use `#[sqlx::test(migrator = "ch_db::MIGRATOR")]`, which creates a fresh
//! database per test inside `pg-test`, so streams never see each other's data:
//!
//! ```ignore
//! #[sqlx::test(migrator = "ch_db::MIGRATOR")]
//! async fn my_test(pool: sqlx::PgPool) {
//!     let env = ch_testkit::TestEnv::new(pool).await;
//!     let alice = env.user("alice").await;
//!     let repo = env.repo(alice, "demo").await;
//! }
//! ```

pub mod fixtures;
pub mod git;

pub use fixtures::TestEnv;
pub use git::TempRepo;
