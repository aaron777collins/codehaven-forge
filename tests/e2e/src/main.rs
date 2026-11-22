//! `ch-e2e`: the end-to-end harness (ROADMAP.md section 6). It drives a running stack the way
//! a person would: real HTTP, real `git`, real `ssh`. Each step is a function, so a milestone
//! runs a prefix: M0 runs steps 0 and 1, M1 runs 0 to 9.
//!
//! ```text
//! ch-e2e                      run every step that exists
//! ch-e2e --steps 0-1          run steps 0 and 1
//! CH_E2E_URL=http://server:3200 ch-e2e
//! ```
//!
//! The harness image also holds the `codehaven` binary, so steps can run
//! `codehaven admin ...` against the same database the server uses, which is what an
//! operator does with `docker compose exec`.

mod ctx;
mod steps;

use anyhow::{Result, bail};
use std::time::Instant;

fn parse_range(s: &str) -> Result<(u32, u32)> {
    match s.split_once('-') {
        Some((a, b)) => Ok((a.parse()?, b.parse()?)),
        None => {
            let n = s.parse()?;
            Ok((n, n))
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut range = (0, u32::MAX);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--steps" => range = parse_range(&args.next().unwrap_or_default())?,
            other => bail!("unknown argument {other}"),
        }
    }
    let ctx = ctx::Ctx::from_env()?;
    // A test run starts on an empty stack. Refuse one that holds an account, so this harness can
    // never seed or change an instance people use. There is no flag that turns this off.
    let (ok, out, err) = ctx.codehaven(&["admin", "users"], None).await?;
    if !ok {
        bail!("could not list the accounts of the target, so not running: {}", err.trim());
    }
    let accounts = out.lines().filter(|l| !l.trim().is_empty()).count();
    if accounts > 0 {
        bail!("the target already holds {accounts} account(s). A test run must start on an empty stack. Refusing to run.");
    }
    let mut failed = 0;
    for step in steps::all() {
        if step.number < range.0 || step.number > range.1 {
            continue;
        }
        let started = Instant::now();
        match (step.run)(&ctx).await {
            Ok(()) => println!("step {:>2} PASS  {} ({} ms)", step.number, step.name, started.elapsed().as_millis()),
            Err(e) => {
                failed += 1;
                println!("step {:>2} FAIL  {}\n         {e:#}", step.number, step.name);
                // Later steps build on earlier ones, so stop at the first failure.
                break;
            }
        }
    }
    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}
