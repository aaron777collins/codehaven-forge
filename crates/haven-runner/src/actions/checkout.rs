//! `checkout`: fetch the repository at the commit the run is for, using the job's token.
//!
//! Inputs: `ref`, `fetch-depth` (default 1, 0 means full history), `path`, `submodules`
//! (`true` or `recursive`), `persist-credentials` (default true).

use super::{ActionCtx, ActionOutput, inside_workspace};
use anyhow::{Result, anyhow, bail};
use base64::Engine;
use std::path::{Path, PathBuf};
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

/// Settings on every git command the checkout runs. The repository is out of the job's reach
/// (see `Stage`), so these are a second wall (ch-ctk.1): they outrank the repository's own
/// config, and `-c` reaches the git processes these start (submodules) too. No hooks and no
/// fsmonitor run, and no helper or ext transport is used. Submodule URLs come from the commit,
/// which may be a fork's, so a submodule may never be fetched from a local path (`file` is
/// allowed only for the runner's own top-level fetch, which tests use), and no attributes file
/// outside the repository is read. With global and system config off (see `git`), the only
/// config files git reads are the ones it writes itself in the private stage (the submodules'
/// repositories too, which git keeps under the stage's `.git/modules`), so no filter, include or
/// command can come from anything the job wrote. A server may not redirect a fetch elsewhere, so
/// a submodule URL cannot bounce the runner to another host (ch-3xn), and every object fetched is
/// checked, as the server checks every push.
pub const SAFE_CONFIG: &[&str] = &[
    "core.hooksPath=/dev/null",
    "core.fsmonitor=false",
    "core.untrackedCache=false",
    "core.attributesFile=/dev/null",
    "protocol.ext.allow=never",
    "protocol.file.allow=user",
    "credential.helper=",
    "safe.bareRepository=explicit",
    "http.followRedirects=false",
    "transfer.fsckObjects=true",
];

pub fn safe_config_args() -> Vec<String> {
    SAFE_CONFIG.iter().flat_map(|kv| ["-c".to_string(), (*kv).to_string()]).collect()
}

/// Where git works: a repository in the runner's private directory, which no job process can
/// reach (ch-ctk.1). The job can write anywhere in its own tree, the workspace included, and a
/// process it started in an earlier step keeps running while the checkout works. So no git
/// command of the runner's ever reads a config, hook, attribute or `.git` file there, or writes a
/// file there: git fetches and checks out here, and the finished tree, `.git` included, is then
/// moved into the workspace. From then on it is the job's, and the runner never runs git on it.
struct Stage {
    work: PathBuf,
    git_dir: PathBuf,
}

impl Stage {
    fn new(private: &Path) -> Result<Stage> {
        let work = private.join(format!("checkout-{:08x}", rand::random::<u32>()));
        std::fs::create_dir(&work).map_err(|e| anyhow!("cannot make the checkout's private directory: {e}"))?;
        Ok(Stage { git_dir: work.join(".git"), work })
    }

    /// `--git-dir` and `--work-tree` on every command, so git never looks anywhere else for a
    /// repository.
    fn args(&self) -> Vec<String> {
        vec![format!("--git-dir={}", self.git_dir.display()), format!("--work-tree={}", self.work.display())]
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.work);
    }
}

async fn git(ctx: &ActionCtx<'_>, stage: &Stage, global: &[String], args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.current_dir(&stage.work).args(safe_config_args()).args(global);
    // `init` makes the repository, so it takes the path instead.
    if args.first() != Some(&"init") {
        cmd.args(stage.args());
    }
    cmd.args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        // No global or system config: the old global file lived in the job's directory, where
        // the job could write hooks and commands into it.
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_EXEC_PATH")
        .env_remove("GIT_TEMPLATE_DIR")
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS")
        .env_remove("GIT_SSH")
        .env_remove("GIT_SSH_COMMAND")
        .env_remove("GIT_PROXY_COMMAND")
        .env_remove("GIT_EXTERNAL_DIFF")
        .env_remove("GIT_ALLOW_PROTOCOL")
        // Unset means "from the user": the runner's own fetch. git sets it to 0 for submodules.
        .env_remove("GIT_PROTOCOL_FROM_USER")
        .env_remove("GIT_CONFIG_SYSTEM")
        .env_remove("XDG_CONFIG_HOME")
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
    // No link on the way from the job root, so the checkout lands in the workspace and nowhere
    // else (ch-ctk.1).
    crate::safefs::create_dir_all(&ctx.dirs.root, &dir, 0o755).map_err(|e| anyhow!("cannot use {sub:?} for the checkout: {e}"))?;
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
    // A new repository every time, out of the job's reach. A .git an earlier step left in the
    // workspace is never read, and the move below replaces it.
    let stage = Stage::new(&ctx.dirs.private)?;
    let work = stage.work.to_string_lossy().into_owned();
    git(ctx, &stage, &[], &["init", "-q", &work]).await?;
    git(ctx, &stage, &[], &["remote", "add", "origin", &repo.clone_url]).await?;

    let depth_arg = format!("--depth={depth}");
    let mut landed: Option<String> = None;
    let mut last_err = None;
    for spec in &p.fetches {
        let mut args: Vec<&str> = vec!["-c", "protocol.version=2", "fetch", "-q", "--no-tags", "--prune", "--no-recurse-submodules"];
        if depth > 0 {
            args.push(&depth_arg);
        }
        args.extend(["origin", spec]);
        match git(ctx, &stage, &auth_args, &args).await {
            Ok(_) => {
                // Land on the right commit: the one the run names, or whatever the ref points at.
                let tip_ref = spec.split(':').nth(1).unwrap_or("FETCH_HEAD");
                let tip = git(ctx, &stage, &[], &["rev-parse", &format!("{tip_ref}^{{commit}}")]).await.ok().map(|s| s.trim().to_string());
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
            git(ctx, &stage, &[], &["checkout", "-q", "--force", "-B", b, &commit]).await?;
        }
        _ => {
            git(ctx, &stage, &[], &["checkout", "-q", "--force", "--detach", &commit]).await?;
        }
    }

    if let Some(mode) = submodules.filter(|m| m == "true" || m == "recursive") {
        let mut a = vec!["submodule", "sync"];
        if mode == "recursive" {
            a.push("--recursive");
        }
        git(ctx, &stage, &auth_args, &a).await?;
        let mut a = vec!["-c", "protocol.version=2", "submodule", "update", "--init", "--force", "-q"];
        if mode == "recursive" {
            a.push("--recursive");
        }
        let da = depth_arg.as_str();
        if depth > 0 {
            a.push("--depth");
            a.push(da.trim_start_matches("--depth="));
        }
        git(ctx, &stage, &auth_args, &a).await?;
    }

    if persist && let Some(s) = &scope {
        git(ctx, &stage, &[], &["config", "--local", &format!("http.{s}.extraheader"), &header]).await?;
    }
    // Hand the finished tree over. Nothing the job did in the workspace meanwhile was read; a
    // link it left or swaps in at any name is replaced, never followed.
    crate::safefs::move_into(&ctx.dirs.root, &stage.work, &dir)
        .map_err(|e| anyhow!("cannot move the checkout into {sub:?}: {e}. Did a step change that directory while the checkout ran?"))?;
    drop(stage);
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
    fn every_git_call_turns_hooks_and_fsmonitor_off() {
        let a = safe_config_args();
        for want in [
            "core.hooksPath=/dev/null",
            "core.fsmonitor=false",
            "protocol.ext.allow=never",
            // Pinned, not left to the git version's default: a fork's .gitmodules must not fetch
            // a local path such as the runner's home.
            "protocol.file.allow=user",
            "core.attributesFile=/dev/null",
            // ch-3xn: no redirect to another host, and fetched objects are checked.
            "http.followRedirects=false",
            "transfer.fsckObjects=true",
        ] {
            let i = a.iter().position(|x| x == want).unwrap_or_else(|| panic!("{want} is missing"));
            assert_eq!(a[i - 1], "-c");
        }
    }

    #[test]
    fn the_header_is_scoped_to_the_server() {
        assert_eq!(header_scope("https://h.example/o/r.git").as_deref(), Some("https://h.example/"));
        assert_eq!(header_scope("http://server:3200/o/r.git").as_deref(), Some("http://server:3200/"));
        assert_eq!(header_scope("file:///tmp/x"), None);
        assert_eq!(header_scope("/tmp/x"), None);
    }
}
