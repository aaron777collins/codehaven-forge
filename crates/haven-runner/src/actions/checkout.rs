//! `checkout`: fetch the repository at the commit the run is for, using the job's token.
//!
//! Inputs: `ref`, `fetch-depth` (default 1, 0 means full history), `path`, `submodules`
//! (`true` or `recursive`), `persist-credentials` (default true).

use super::{ActionCtx, ActionOutput, inside_workspace};
use anyhow::{Result, anyhow, bail};
use base64::Engine;
use std::path::Path;
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// How to fetch and what to check out, worked out from the inputs and the run's context.
#[derive(Debug, PartialEq, Eq)]
pub struct Plan {
    /// Refspecs to fetch, tried in order until one yields the wanted commit.
    pub fetches: Vec<String>,
    /// `Some(branch)` to create a local branch at the fetched tip, `None` for a detached HEAD.
    pub branch: Option<String>,
    /// Commit to land on, when the run names one.
    pub sha: Option<String>,
}

fn is_sha(s: &str) -> bool {
    s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit())
}

pub fn plan(input_ref: Option<&str>, git_ref: &str, sha: &str) -> Plan {
    let from_full_ref = |r: &str| -> Plan {
        if let Some(b) = r.strip_prefix("refs/heads/") {
            Plan { fetches: vec![format!("+{r}:refs/remotes/origin/{b}")], branch: Some(b.to_string()), sha: None }
        } else if r.starts_with("refs/tags/") {
            Plan { fetches: vec![format!("+{r}:{r}")], branch: None, sha: None }
        } else {
            let local = format!("refs/remotes/{}", r.trim_start_matches("refs/"));
            Plan { fetches: vec![format!("+{r}:{local}")], branch: None, sha: None }
        }
    };
    match input_ref.map(str::trim).filter(|r| !r.is_empty()) {
        Some(r) if is_sha(r) => Plan { fetches: vec![format!("+{r}:refs/remotes/origin/__sha")], branch: None, sha: Some(r.to_string()) },
        Some(r) if r.starts_with("refs/") => from_full_ref(r),
        Some(r) => Plan {
            fetches: vec![format!("+refs/heads/{r}:refs/remotes/origin/{r}"), format!("+refs/tags/{r}:refs/tags/{r}")],
            branch: Some(r.to_string()),
            sha: None,
        },
        None => {
            // The run's own ref first (it is the cheapest), then the commit itself in case the
            // ref has moved on since the run was created.
            let mut p = from_full_ref(git_ref);
            if is_sha(sha) {
                p.sha = Some(sha.to_string());
                p.fetches.push(format!("+{sha}:refs/remotes/origin/__sha"));
            }
            p
        }
    }
}

/// `Authorization: basic ...` for the job token. The user name is a placeholder.
pub fn auth_header(token: &str) -> String {
    let b = base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}"));
    format!("AUTHORIZATION: basic {b}")
}

/// `https://host:port/`, the scope that confines the header to the one server, or `None` for
/// URLs where there is no server to authenticate to (a `file://` path in tests).
pub fn header_scope(clone_url: &str) -> Option<String> {
    let u = url::Url::parse(clone_url).ok()?;
    if !matches!(u.scheme(), "http" | "https") {
        return None;
    }
    let host = u.host_str()?;
    Some(match u.port() {
        Some(p) => format!("{}://{host}:{p}/", u.scheme()),
        None => format!("{}://{host}/", u.scheme()),
    })
}

async fn git(ctx: &ActionCtx<'_>, dir: &Path, global: &[String], args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .args(global)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_GLOBAL", ctx.dirs.runner.join("gitconfig"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let shown = args.join(" ");
    ctx.info(format!("[command]git {shown}")).await;
    let mut child = cmd.spawn().map_err(|e| anyhow!("cannot run git: {e}"))?;
    let mut so = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
    let mut se = child.stderr.take().ok_or_else(|| anyhow!("no stderr"))?;
    let read_all = async {
        let (mut a, mut b) = (Vec::new(), Vec::new());
        let (ra, rb) = tokio::join!(so.read_to_end(&mut a), se.read_to_end(&mut b));
        ra?;
        rb?;
        let status = child.wait().await?;
        Ok::<_, std::io::Error>((a, b, status))
    };
    let (out, err, status) = tokio::select! {
        r = read_all => r.map_err(|e| anyhow!("git failed: {e}"))?,
        _ = ctx.cancel.cancelled() => bail!("cancelled"),
    };
    let out = String::from_utf8_lossy(&out).into_owned();
    let err = String::from_utf8_lossy(&err).into_owned();
    for l in err.lines().chain(out.lines()).filter(|l| !l.trim().is_empty()) {
        ctx.info(l).await;
    }
    if !status.success() {
        bail!(
            "git {} failed with {}",
            args.first().unwrap_or(&""),
            status.code().map(|c| format!("exit code {c}")).unwrap_or_else(|| "a signal".into())
        );
    }
    Ok(out)
}

pub async fn run(ctx: &ActionCtx<'_>) -> Result<ActionOutput> {
    let repo = &ctx.payload.repo;
    let token = &ctx.payload.token;
    let sub = ctx.input("path").unwrap_or(".");
    let dir = inside_workspace(&ctx.dirs.workspace, sub)?;
    std::fs::create_dir_all(&dir)?;
    let depth: u32 = match ctx.input("fetch-depth") {
        None => 1,
        Some(d) => d.trim().parse().map_err(|_| anyhow!("fetch-depth must be a whole number, not {d:?}"))?,
    };
    let persist = ctx.flag("persist-credentials", true);
    let submodules = ctx.input("submodules").map(|s| s.trim().to_ascii_lowercase());
    let p = plan(ctx.input("ref"), &ctx.payload.context.git_ref, &ctx.payload.context.sha);

    let scope = header_scope(&repo.clone_url);
    let header = auth_header(token);
    ctx.log.masker().add(token);
    // The header holds the token in another encoding, which the token mask would miss.
    ctx.log.masker().add(header.trim_start_matches("AUTHORIZATION: basic "));
    let auth_args: Vec<String> = match &scope {
        Some(s) => vec!["-c".into(), format!("http.{s}.extraheader={header}")],
        None => vec![],
    };

    ctx.info(format!("Syncing repository: {}", repo.full_name)).await;
    if !dir.join(".git").exists() {
        git(ctx, &dir, &[], &["init", "-q", "."]).await?;
    }
    if git(ctx, &dir, &[], &["remote", "add", "origin", &repo.clone_url]).await.is_err() {
        git(ctx, &dir, &[], &["remote", "set-url", "origin", &repo.clone_url]).await?;
    }

    let depth_arg = format!("--depth={depth}");
    let mut landed: Option<String> = None;
    let mut last_err = None;
    for spec in &p.fetches {
        let mut args: Vec<&str> = vec!["-c", "protocol.version=2", "fetch", "-q", "--no-tags", "--prune", "--no-recurse-submodules"];
        if depth > 0 {
            args.push(&depth_arg);
        }
        args.extend(["origin", spec]);
        match git(ctx, &dir, &auth_args, &args).await {
            Ok(_) => {
                // Land on the right commit: the one the run names, or whatever the ref points at.
                let tip_ref = spec.split(':').nth(1).unwrap_or("FETCH_HEAD");
                let tip = git(ctx, &dir, &[], &["rev-parse", &format!("{tip_ref}^{{commit}}")]).await.ok().map(|s| s.trim().to_string());
                match (&p.sha, tip) {
                    (Some(want), Some(have)) if *want != have => {
                        ctx.info(format!("{tip_ref} is at {have}, not {want}. Trying the commit itself.")).await;
                        continue;
                    }
                    (_, Some(have)) => {
                        landed = Some(have);
                        break;
                    }
                    (_, None) => continue,
                }
            }
            Err(e) => last_err = Some(e),
        }
    }
    let Some(commit) = landed else {
        return Err(last_err
            .unwrap_or_else(|| anyhow!("could not fetch {} from {}", p.sha.as_deref().unwrap_or("the requested ref"), repo.full_name)));
    };

    match &p.branch {
        Some(b) if p.sha.as_deref().is_none_or(|s| s == commit) && !b.is_empty() => {
            git(ctx, &dir, &[], &["checkout", "-q", "--force", "-B", b, &commit]).await?;
        }
        _ => {
            git(ctx, &dir, &[], &["checkout", "-q", "--force", "--detach", &commit]).await?;
        }
    }

    if let Some(mode) = submodules.filter(|m| m == "true" || m == "recursive") {
        let mut a = vec!["submodule", "sync"];
        if mode == "recursive" {
            a.push("--recursive");
        }
        git(ctx, &dir, &auth_args, &a).await?;
        let mut a = vec!["-c", "protocol.version=2", "submodule", "update", "--init", "--force", "-q"];
        if mode == "recursive" {
            a.push("--recursive");
        }
        let da = depth_arg.as_str();
        if depth > 0 {
            a.push("--depth");
            a.push(da.trim_start_matches("--depth="));
        }
        git(ctx, &dir, &auth_args, &a).await?;
    }

    if persist && let Some(s) = &scope {
        git(ctx, &dir, &[], &["config", "--local", &format!("http.{s}.extraheader"), &header]).await?;
    }
    ctx.info(format!("Checked out {commit}")).await;
    let mut out = ActionOutput::default();
    out.outputs.insert("commit".into(), commit);
    out.outputs.insert("ref".into(), ctx.payload.context.git_ref.clone());
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn default_plan_follows_the_runs_branch() {
        let p = plan(None, "refs/heads/main", SHA);
        assert_eq!(p.branch.as_deref(), Some("main"));
        assert_eq!(p.sha.as_deref(), Some(SHA));
        assert_eq!(p.fetches[0], "+refs/heads/main:refs/remotes/origin/main");
        assert_eq!(p.fetches[1], format!("+{SHA}:refs/remotes/origin/__sha"), "falls back to the commit itself");
    }

    #[test]
    fn tags_and_pull_requests_check_out_detached() {
        let p = plan(None, "refs/tags/v1.0", SHA);
        assert_eq!(p.branch, None);
        assert_eq!(p.fetches[0], "+refs/tags/v1.0:refs/tags/v1.0");
        let p = plan(None, "refs/pull/7/merge", SHA);
        assert_eq!(p.branch, None);
        assert_eq!(p.fetches[0], "+refs/pull/7/merge:refs/remotes/pull/7/merge");
    }

    #[test]
    fn explicit_refs() {
        let p = plan(Some(SHA), "refs/heads/main", "x");
        assert_eq!(p.sha.as_deref(), Some(SHA));
        assert_eq!(p.branch, None);
        let p = plan(Some("dev"), "refs/heads/main", SHA);
        assert_eq!(p.branch.as_deref(), Some("dev"));
        assert_eq!(p.fetches.len(), 2, "a bare name may be a branch or a tag");
        let p = plan(Some("refs/heads/release/1"), "refs/heads/main", SHA);
        assert_eq!(p.branch.as_deref(), Some("release/1"));
        let p = plan(Some("  "), "refs/heads/main", SHA);
        assert_eq!(p.branch.as_deref(), Some("main"), "a blank ref input means the default");
    }

    #[test]
    fn the_auth_header_carries_the_token_in_basic_form() {
        let h = auth_header("tok123");
        let b64 = h.strip_prefix("AUTHORIZATION: basic ").unwrap();
        let raw = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
        assert_eq!(raw, b"x-access-token:tok123");
    }

    #[test]
    fn the_header_is_scoped_to_the_server() {
        assert_eq!(header_scope("https://h.example/o/r.git").as_deref(), Some("https://h.example/"));
        assert_eq!(header_scope("http://server:3200/o/r.git").as_deref(), Some("http://server:3200/"));
        assert_eq!(header_scope("file:///tmp/x"), None);
        assert_eq!(header_scope("/tmp/x"), None);
    }
}
