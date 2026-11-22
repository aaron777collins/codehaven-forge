//! `cargo xtask`: the repo's task runner. Runs inside the builder container
//! (see `deploy/dev/slot.sh`), so `DATABASE_URL` points at the throwaway `pg-test`.
//!
//! ```text
//! cargo xtask fmt         formatting check
//! cargo xtask lint        clippy with warnings denied
//! cargo xtask test [-p crate ...]   nextest, whole workspace by default
//! cargo xtask db-prepare  migrate pg-test, then refresh .sqlx/ (run after any query or migration change)
//! cargo xtask db-check    fail when .sqlx/ is stale
//! cargo xtask portable    fail when a name, address or path of one deployment is in the tree
//! cargo xtask ci          portable, fmt, lint, db-check, build, test: the gate every stream runs before a merge
//! ```

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::process::{Command, Stdio};

#[derive(Parser)]
#[command(name = "xtask")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Fmt,
    Lint {
        /// Limit to these crates.
        #[arg(short, long)]
        package: Vec<String>,
    },
    Test {
        #[arg(short, long)]
        package: Vec<String>,
        /// Extra arguments for nextest, after `--`.
        #[arg(last = true)]
        rest: Vec<String>,
    },
    DbPrepare,
    DbCheck,
    Portable,
    Ci,
}

fn run(program: &str, args: &[&str], envs: &[(&str, &str)]) -> Result<()> {
    eprintln!("$ {program} {}", args.join(" "));
    let status = Command::new(program)
        .args(args)
        .envs(envs.iter().copied())
        .stdin(Stdio::null())
        .status()
        .with_context(|| format!("starting {program}"))?;
    if !status.success() {
        bail!("{program} {} failed with {status}", args.join(" "));
    }
    Ok(())
}

fn pkg_args(packages: &[String]) -> Vec<String> {
    if packages.is_empty() {
        vec!["--workspace".to_string()]
    } else {
        packages.iter().flat_map(|p| ["-p".to_string(), p.clone()]).collect()
    }
}

fn cargo(base: &[&str], extra: &[String], tail: &[&str], envs: &[(&str, &str)]) -> Result<()> {
    let mut args: Vec<&str> = base.to_vec();
    args.extend(extra.iter().map(String::as_str));
    args.extend_from_slice(tail);
    run("cargo", &args, envs)
}

/// The portability guard: first thing in `ci`, because it is cheap and a leak is easy to fix early.
fn portable() -> Result<()> {
    run("python3", &["deploy/check-portable.py", "--self-test"], &[])?;
    run("python3", &["deploy/check-portable.py"], &[])
}

fn migrate_test_db() -> Result<()> {
    if std::env::var("DATABASE_URL").is_err() {
        bail!("DATABASE_URL is not set. Run this inside the builder: make ci, or deploy/dev/slot.sh <stream> \"cargo xtask ...\"");
    }
    // `create` is a no-op when the database exists.
    run("sqlx", &["database", "create"], &[])?;
    run("sqlx", &["migrate", "run", "--source", "crates/ch-db/migrations"], &[])
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("xtask: {e:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    match Cli::parse().command {
        Cmd::Fmt => run("cargo", &["fmt", "--all", "--", "--check"], &[]),
        Cmd::Lint { package } => cargo(&["clippy"], &pkg_args(&package), &["--all-targets", "--", "-D", "warnings"], &[]),
        Cmd::Test { package, rest } => {
            let mut tail: Vec<&str> = vec![];
            tail.extend(rest.iter().map(String::as_str));
            cargo(&["nextest", "run"], &pkg_args(&package), &tail, &[])
        }
        Cmd::DbPrepare => {
            migrate_test_db()?;
            // The macros check queries against the live database while preparing.
            run("cargo", &["sqlx", "prepare", "--workspace", "--", "--all-targets"], &[("SQLX_OFFLINE", "false")])
        }
        Cmd::DbCheck => {
            migrate_test_db()?;
            run("cargo", &["sqlx", "prepare", "--check", "--workspace", "--", "--all-targets"], &[("SQLX_OFFLINE", "false")])
        }
        Cmd::Portable => portable(),
        Cmd::Ci => {
            portable()?;
            run("cargo", &["fmt", "--all", "--", "--check"], &[])?;
            migrate_test_db()?;
            run("cargo", &["sqlx", "prepare", "--check", "--workspace", "--", "--all-targets"], &[("SQLX_OFFLINE", "false")])?;
            run("cargo", &["clippy", "--workspace", "--all-targets", "--", "-D", "warnings"], &[])?;
            run("cargo", &["build", "--workspace"], &[])?;
            run("cargo", &["nextest", "run", "--workspace"], &[])
        }
    }
}
