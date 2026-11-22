//! One module per command group. Each exposes `run(ctx, cmd) -> Result<i32>`; the return is the
//! process exit code (0 unless the command's job is to report a result, like `run watch`).

use crate::{Ctx, cli::Command};
use anyhow::{Context, Result};

pub mod api;
pub mod auth;
pub mod issue;
pub mod pr;
pub mod release;
pub mod repo;
pub mod run;
pub mod workflow;

pub async fn dispatch(ctx: &Ctx, command: Command) -> Result<i32> {
    match command {
        Command::Auth(c) => auth::run(ctx, c).await,
        Command::Repo(c) => repo::run(ctx, c).await,
        Command::Issue(c) => issue::run(ctx, c).await,
        Command::Pr(c) => pr::run(ctx, c).await,
        Command::Run(c) => run::run(ctx, c).await,
        Command::Workflow(c) => workflow::run(ctx, c).await,
        Command::Release(c) => release::run(ctx, c).await,
        Command::Api(a) => api::run(ctx, a).await,
    }
}

/// A body given inline or from a file (`-` is standard input).
pub fn body_from(ctx: &Ctx, inline: Option<&str>, file: Option<&str>) -> Result<Option<String>> {
    match (inline, file) {
        (_, Some("-")) => Ok(Some(ctx.read_stdin()?)),
        (_, Some(path)) => Ok(Some(std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?)),
        (Some(b), None) => Ok(Some(b.to_string())),
        (None, None) => Ok(None),
    }
}

/// Runs `git` in the current directory, passing its output straight through.
pub fn git(ctx: &Ctx, args: &[&str]) -> Result<()> {
    let status = std::process::Command::new("git")
        .current_dir(&ctx.env.cwd)
        .args(args)
        .status()
        .map_err(|e| anyhow::anyhow!("cannot run git: {e}"))?;
    if !status.success() {
        anyhow::bail!("`git {}` failed", args.join(" "));
    }
    Ok(())
}

/// The output of a `git` command in the current directory, trimmed. `None` when it fails.
pub fn git_output(ctx: &Ctx, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git").current_dir(&ctx.env.cwd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
