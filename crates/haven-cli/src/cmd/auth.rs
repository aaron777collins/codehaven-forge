//! `haven auth`: sign in with a personal access token, and the git credential helper.

use crate::cli::{AuthCmd, LoginArgs};
use crate::config::{self, HostEntry};
use crate::{ApiClient, Ctx};
use anyhow::{Result, bail};
use ch_api_types::users::User;
use std::io::Write;

pub async fn run(ctx: &Ctx, cmd: AuthCmd) -> Result<i32> {
    match cmd {
        AuthCmd::Login(a) => login(ctx, a).await,
        AuthCmd::Logout => logout(ctx),
        AuthCmd::Status => status(ctx).await,
        AuthCmd::Token => token(ctx),
        AuthCmd::GitCredential { operation } => git_credential(ctx, &operation),
        AuthCmd::SetupGit => setup_git(ctx),
    }
}

async fn login(ctx: &Ctx, a: LoginArgs) -> Result<i32> {
    let host = match ctx.host_flag.as_deref() {
        Some(h) => config::normalize_host(h)?,
        None => match ctx.hosts()?.default {
            Some(h) => h,
            None => bail!("which host? Run `haven auth login --host <host>`."),
        },
    };
    let token = if a.with_token {
        ctx.read_stdin()?.trim().to_string()
    } else if let Some(t) = &ctx.env.token {
        t.clone()
    } else if ctx.env.interactive {
        ctx.print(format!("Paste a personal access token for {host} (made under Settings, Tokens): "));
        let t = rpassword::read_password()?;
        t.trim().to_string()
    } else {
        bail!("no token. Pipe one in with --with-token, or set HAVEN_TOKEN.");
    };
    if token.is_empty() {
        bail!("the token is empty");
    }
    let client = ApiClient::new(&host, Some(token.clone()))?;
    let me: User = client.get("/user").await?;
    ctx.env.store.set(&host, &token)?;
    let mut hosts = ctx.hosts()?;
    hosts.hosts.entry(host.clone()).or_insert_with(HostEntry::default).user = Some(me.login.clone());
    hosts.default = Some(host.clone());
    hosts.save(&ctx.env.config_dir)?;
    ctx.println(format!("Signed in to {host} as {}.", me.login));
    Ok(0)
}

fn logout(ctx: &Ctx) -> Result<i32> {
    let host = ctx.host()?;
    ctx.env.store.delete(&host)?;
    let mut hosts = ctx.hosts()?;
    hosts.hosts.remove(&host);
    if hosts.default.as_deref() == Some(host.as_str()) {
        hosts.default = hosts.hosts.keys().next().cloned();
    }
    hosts.save(&ctx.env.config_dir)?;
    ctx.println(format!("Signed out of {host}."));
    Ok(0)
}

async fn status(ctx: &Ctx) -> Result<i32> {
    let host = ctx.host()?;
    let Some(token) = ctx.token_for(&host)? else {
        ctx.println(format!("Not signed in to {host}."));
        return Ok(1);
    };
    match ApiClient::new(&host, Some(token))?.get::<User>("/user").await {
        Ok(me) => {
            ctx.println(format!("Signed in to {host} as {}.", me.login));
            ctx.println(format!("Token kept in {}.", ctx.env.store.describe()));
            Ok(0)
        }
        Err(e) => {
            ctx.println(format!("The stored token for {host} does not work: {e}"));
            Ok(1)
        }
    }
}

fn token(ctx: &Ctx) -> Result<i32> {
    let host = ctx.host()?;
    match ctx.token_for(&host)? {
        Some(t) => {
            ctx.println(t);
            Ok(0)
        }
        None => bail!("not signed in to {host}"),
    }
}

/// The git credential helper protocol: key=value lines in, key=value lines out. Only `get`
/// answers, and only for the host you signed in to, so a push to another host never gets this
/// token.
fn git_credential(ctx: &Ctx, op: &str) -> Result<i32> {
    if op != "get" {
        return Ok(0);
    }
    let input = ctx.read_stdin()?;
    let mut protocol = String::new();
    let mut host = String::new();
    for line in input.lines() {
        match line.split_once('=') {
            Some(("protocol", v)) => protocol = v.to_string(),
            Some(("host", v)) => host = v.to_string(),
            _ => {}
        }
    }
    let Ok(base) = ctx.host() else { return Ok(0) };
    let Some(want) = config::host_port(&base) else { return Ok(0) };
    if host != want || (protocol != "https" && protocol != "http") {
        return Ok(0);
    }
    let Some(token) = ctx.token_for(&base)? else { return Ok(0) };
    let user = ctx.hosts()?.hosts.get(&base).and_then(|h| h.user.clone()).unwrap_or_else(|| "haven".to_string());
    ctx.println(format!("username={user}\npassword={token}"));
    Ok(0)
}

fn setup_git(ctx: &Ctx) -> Result<i32> {
    let base = ctx.host()?;
    let key = format!("credential.{base}.helper");
    let status = std::process::Command::new("git").args(["config", "--global", &key, "!haven auth git-credential"]).status()?;
    if !status.success() {
        bail!("git config failed");
    }
    let _ = std::io::stdout().flush();
    ctx.println(format!("git will now ask haven for {base} credentials."));
    Ok(0)
}
