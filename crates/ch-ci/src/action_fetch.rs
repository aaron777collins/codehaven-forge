//! Actions a step `uses:` from GitHub or from a repository here (ch-ctk.5).
//!
//! The runner asks the server, with its job token, for the actions a job names. The server checks
//! the site's policy before it fetches anything, resolves the ref to a commit, refuses a commit
//! that no branch or tag of the named repository reaches (GitHub serves a fork's commits under
//! the parent's name, which would get round an allow rule), takes a tarball of that commit into
//! the blob store, and pins the answer to the run. Later jobs and re-runs of failed jobs in the
//! same run get the same commit. The run page lists what was used.
//!
//! GitHub is reached only at the fixed address `https://github.com`, with git itself: no
//! redirects, no credentials, no hooks, no proxy from the environment, a time limit on every
//! command and size limits on what is kept. Private GitHub repositories are not supported.
//!
//! A repository here (`haven_owner/repo@ref`) is used when it is the job's own, or public, or has
//! the same owner and is at least as visible as the job's repository. A pull request from a fork
//! uses public ones only. Only branches and tags count, never `refs/pull/*`.

use crate::ctx::Ctx;
use crate::runs::JobRow;
use ch_api_types::runner as rn;
use ch_core::model::Visibility;
use ch_core::services::ArchiveFormat;
use ch_core::{Actor, Error, Permission, RepoId, Result};
use ch_workflow::uses::{Host, Policy, PolicyMode, Uses, Verdict, evaluate, is_full_sha, parse_uses};
use futures::StreamExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Where GitHub is. Tests point it at a local repository through [`crate::shared::Shared`].
pub const GITHUB: &str = "https://github.com";
/// The most distinct actions one run may name.
pub const MAX_PER_RUN: i64 = 100;
/// A packed action, compressed.
pub const MAX_ARCHIVE: u64 = 256 * 1024 * 1024;
/// An action's files, unpacked, and how many there may be.
pub const MAX_EXPANDED: u64 = 1024 * 1024 * 1024;
pub const MAX_ENTRIES: usize = 100_000;
const GIT_TIMEOUT: Duration = Duration::from_secs(120);

/// `mode, allow, block, require_sha, allow_codehaven_repos`.
type PolicyRow = (String, Vec<String>, Vec<String>, bool, bool);

/// The site's policy, from its one row.
pub async fn load_policy(ctx: &Ctx) -> Result<Policy> {
    let row: Option<PolicyRow> =
        sqlx::query_as("SELECT mode, allow, block, require_sha, allow_codehaven_repos FROM actions_policy WHERE id")
            .fetch_optional(&ctx.db)
            .await?;
    Ok(match row {
        Some((mode, allow, block, require_sha, allow_codehaven_repos)) => Policy {
            mode: match mode.as_str() {
                "none" => PolicyMode::None,
                "all" => PolicyMode::All,
                _ => PolicyMode::Selected,
            },
            allow,
            block,
            require_sha,
            allow_codehaven_repos,
        },
        None => Policy {
            mode: PolicyMode::Selected,
            allow: ch_workflow::uses::SEED_ALLOW.iter().map(|s| s.to_string()).collect(),
            block: Vec::new(),
            require_sha: false,
            allow_codehaven_repos: true,
        },
    })
}

/// What one `uses:` resolved to, or why it did not.
fn refused(uses: &str, why: impl Into<String>) -> rn::ActionResolution {
    rn::ActionResolution { uses: uses.to_string(), sha: None, archive: None, path: String::new(), error: Some(why.into()) }
}

/// Resolves every action a job asks for. Each answer either names a commit and an archive the
/// job may download, or says in a sentence why not; a refusal never fails the others.
pub async fn resolve_for_job(ctx: &Ctx, job: &JobRow, asked: &[String]) -> Result<Vec<rn::ActionResolution>> {
    if asked.len() > MAX_PER_RUN as usize {
        return Err(Error::invalid("Actions", "actions", format!("A job may name at most {MAX_PER_RUN} actions.")));
    }
    let policy = load_policy(ctx).await?;
    let run = crate::runs::fetch_run(ctx, job.run_id).await?;
    let fork = crate::spec::split_payload(&run.event_payload).1.fork;
    let who = Who { fork, actor: run.actor_id.map(ch_core::UserId) };
    let mut out = Vec::with_capacity(asked.len());
    for uses in asked {
        out.push(match resolve_one(ctx, job, &policy, &who, uses).await {
            Ok(r) => r,
            Err(Error::Validation(v)) => refused(uses, v.first().and_then(|f| f.message.clone()).unwrap_or_default()),
            Err(Error::NotFound) => refused(uses, format!("{uses} was not found.")),
            Err(Error::Forbidden(m)) => refused(uses, m),
            Err(e) => return Err(e),
        });
    }
    Ok(out)
}

fn host_key(h: &Host) -> &'static str {
    match h {
        Host::GitHub => "github",
        Host::Haven => "haven",
    }
}

async fn resolve_one(ctx: &Ctx, job: &JobRow, policy: &Policy, who: &Who, uses: &str) -> Result<rn::ActionResolution> {
    let parsed = parse_uses(uses).map_err(|e| Error::invalid("Actions", "uses", e.0))?;
    if let Verdict::Deny(why) = evaluate(policy, &parsed) {
        return Err(Error::forbidden(why));
    }
    let Uses::Repo { host, owner, repo, path, git_ref } = parsed else {
        return Err(Error::invalid("Actions", "uses", format!("{uses} is not fetched by the server.")));
    };
    let name = format!("{owner}/{repo}");
    let hk = host_key(&host);

    // One answer per run: a later job, and a re-run of a failed job, get the same commit.
    let pinned: Option<(String, String)> =
        sqlx::query_as("SELECT sha, archive_sha256 FROM action_resolutions WHERE run_id = $1 AND host = $2 AND name = $3 AND git_ref = $4")
            .bind(job.run_id)
            .bind(hk)
            .bind(&name)
            .bind(&git_ref)
            .fetch_optional(&ctx.db)
            .await?;
    if let Some((sha, archive)) = pinned {
        return Ok(rn::ActionResolution { uses: uses.to_string(), sha: Some(sha), archive: Some(archive), path, error: None });
    }
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM action_resolutions WHERE run_id = $1").bind(job.run_id).fetch_one(&ctx.db).await?;
    if count >= MAX_PER_RUN {
        return Err(Error::invalid("Actions", "uses", format!("A run may use at most {MAX_PER_RUN} different actions.")));
    }

    let (sha, archive) = match host {
        Host::GitHub => github(ctx, &owner, &repo, &git_ref).await?,
        Host::Haven => haven(ctx, job, who, &owner, &repo, &git_ref).await?,
    };
    // Two jobs of a run may ask at once: the first answer stays.
    let (sha, archive): (String, String) = sqlx::query_as(
        "INSERT INTO action_resolutions (run_id, host, name, git_ref, sha, archive_sha256) VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (run_id, host, name, git_ref) DO UPDATE SET run_id = action_resolutions.run_id
         RETURNING sha, archive_sha256",
    )
    .bind(job.run_id)
    .bind(hk)
    .bind(&name)
    .bind(&git_ref)
    .bind(&sha)
    .bind(&archive)
    .fetch_one(&ctx.db)
    .await?;
    Ok(rn::ActionResolution { uses: uses.to_string(), sha: Some(sha), archive: Some(archive), path, error: None })
}

/// A stored archive of this commit, when one was made before.
async fn known_archive(ctx: &Ctx, host: &str, name: &str, sha: &str) -> Result<Option<String>> {
    let row: Option<String> = sqlx::query_scalar(
        "UPDATE action_archives SET last_used_at = now() WHERE host = $1 AND name = $2 AND sha = $3 RETURNING blob_sha256",
    )
    .bind(host)
    .bind(name)
    .bind(sha)
    .fetch_optional(&ctx.db)
    .await?;
    match row {
        Some(b) if ctx.blobs.size(&b).await?.is_some() => Ok(Some(b)),
        _ => Ok(None),
    }
}

async fn remember_archive(ctx: &Ctx, host: &str, name: &str, sha: &str, blob: &ch_core::services::BlobRef) -> Result<()> {
    sqlx::query(
        "INSERT INTO action_archives (host, name, sha, blob_sha256, size) VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (host, name, sha) DO UPDATE SET blob_sha256 = EXCLUDED.blob_sha256, size = EXCLUDED.size, last_used_at = now()",
    )
    .bind(host)
    .bind(name)
    .bind(sha)
    .bind(&blob.sha256)
    .bind(blob.size as i64)
    .execute(&ctx.db)
    .await?;
    Ok(())
}

// ------------------------------------------------------------------------------------ GitHub

/// A scratch directory under the data directory, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    async fn new(ctx: &Ctx) -> Result<Self> {
        let n: u64 = rand_u64();
        let dir = ctx.config.storage.data_dir.join("tmp").join(format!("action-{n:016x}"));
        tokio::fs::create_dir_all(&dir).await.map_err(Error::internal)?;
        Ok(Scratch(dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn rand_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    h.finish()
}

/// git with nothing from the machine or the user: no config files, no hooks, no credential
/// helper, no prompts, no redirects, and only https (or, for tests, the file protocol the
/// configured base uses).
fn git(dir: &Path, allow_file: bool) -> tokio::process::Command {
    let mut c = tokio::process::Command::new("git");
    c.env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "/bin/false")
        .env("SSH_ASKPASS", "/bin/false")
        .arg("-c")
        .arg("protocol.allow=never")
        .arg("-c")
        .arg("protocol.https.allow=always")
        .arg("-c")
        .arg(format!("protocol.file.allow={}", if allow_file { "always" } else { "never" }))
        .arg("-c")
        .arg("http.followRedirects=false")
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .arg("-c")
        .arg("credential.helper=")
        .arg("-c")
        .arg("core.fsmonitor=false")
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    c
}

async fn run(mut c: tokio::process::Command, what: &str) -> Result<Vec<u8>> {
    c.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    let out = tokio::time::timeout(GIT_TIMEOUT, c.output())
        .await
        .map_err(|_| Error::invalid("Actions", "uses", format!("Fetching {what} took too long.")))?
        .map_err(Error::internal)?;
    if !out.status.success() {
        tracing::info!(what, stderr = %String::from_utf8_lossy(&out.stderr).chars().take(400).collect::<String>(), "action fetch failed");
        return Err(Error::invalid("Actions", "uses", format!("{what} could not be fetched from GitHub.")));
    }
    Ok(out.stdout)
}

/// Which commit `git_ref` names on the remote: a tag (peeled) before a branch. A full SHA is
/// returned as it is, with `false` meaning it is not the tip of a branch or tag.
fn pick_ref(advertised: &str, git_ref: &str) -> Option<(String, bool)> {
    let mut tag = None;
    let mut peeled = None;
    let mut branch = None;
    let mut tips = Vec::new();
    for line in advertised.lines() {
        let Some((sha, name)) = line.split_once('\t') else { continue };
        if !is_full_sha(sha) {
            continue;
        }
        let sha = sha.to_ascii_lowercase();
        if name == format!("refs/tags/{git_ref}") {
            tag = Some(sha.clone());
        } else if name == format!("refs/tags/{git_ref}^{{}}") {
            peeled = Some(sha.clone());
        } else if name == format!("refs/heads/{git_ref}") {
            branch = Some(sha.clone());
        }
        if name.starts_with("refs/heads/") || name.starts_with("refs/tags/") {
            tips.push(sha);
        }
    }
    if let Some(s) = peeled.or(tag).or(branch) {
        return Some((s, true));
    }
    if is_full_sha(git_ref) {
        let want = git_ref.to_ascii_lowercase();
        let is_tip = tips.contains(&want);
        return Some((want, is_tip));
    }
    None
}

async fn github(ctx: &Ctx, owner: &str, repo: &str, git_ref: &str) -> Result<(String, String)> {
    let base = ctx.shared.github_base().unwrap_or_else(|| GITHUB.to_string());
    let allow_file = base.starts_with('/') || base.starts_with("file://");
    let url = format!("{}/{owner}/{repo}{}", base.trim_end_matches('/'), if allow_file { "" } else { ".git" });
    let name = format!("{owner}/{repo}");
    let scratch = Scratch::new(ctx).await?;

    // Short SHAs are never guessed at: they are refused, like any name that is not a ref.
    let mut ls = git(&scratch.0, allow_file);
    ls.args(["ls-remote", &url]);
    let advertised = String::from_utf8_lossy(&run(ls, &name).await?).to_string();
    let Some((sha, is_tip)) = pick_ref(&advertised, git_ref) else {
        return Err(Error::invalid(
            "Actions",
            "uses",
            format!("{name} has no tag or branch named {git_ref}. Use a tag, a branch or a full commit SHA."),
        ));
    };

    if let Some(blob) = known_archive(ctx, "github", &name, &sha).await? {
        if is_tip || reachable_on_github(&scratch.0, allow_file, &url, &sha, &name).await? {
            return Ok((sha, blob));
        }
        return Err(not_in_repo(&name, &sha));
    }
    if !is_tip && !reachable_on_github(&scratch.0, allow_file, &url, &sha, &name).await? {
        return Err(not_in_repo(&name, &sha));
    }

    // Only this commit's files, then a tarball of exactly it.
    let repo_dir = scratch.0.join("content.git");
    let mut init = git(&scratch.0, allow_file);
    init.args(["init", "--bare", "--quiet"]).arg(&repo_dir);
    run(init, &name).await?;
    let mut fetch = git(&repo_dir, allow_file);
    fetch.args(["fetch", "--quiet", "--no-tags", "--depth=1", &url, &sha]);
    run(fetch, &name).await?;
    check_tree(&repo_dir, allow_file, &sha, &name).await?;
    let blob = archive_dir(ctx, &repo_dir, allow_file, &sha, &name).await?;
    remember_archive(ctx, "github", &name, &sha, &blob).await?;
    Ok((sha, blob.sha256))
}

fn not_in_repo(name: &str, sha: &str) -> Error {
    Error::forbidden(format!(
        "{sha} is not on a branch or tag of {name}, so it may be a fork's commit. Use a tag, a branch or a commit on one of them."
    ))
}

/// True when a branch or tag of the named repository reaches `sha`. Fetches only commits.
async fn reachable_on_github(dir: &Path, allow_file: bool, url: &str, sha: &str, name: &str) -> Result<bool> {
    let repo_dir = dir.join("reach.git");
    let mut init = git(dir, allow_file);
    init.args(["init", "--bare", "--quiet"]).arg(&repo_dir);
    run(init, name).await?;
    let mut fetch = git(&repo_dir, allow_file);
    fetch.args(["fetch", "--quiet", "--no-tags", "--filter=tree:0", url, "+refs/heads/*:refs/heads/*", "+refs/tags/*:refs/tags/*"]);
    run(fetch, name).await?;
    let mut contains = git(&repo_dir, allow_file);
    contains.args(["for-each-ref", "--count=1", "--format=%(refname)", "--contains", sha, "refs/heads", "refs/tags"]);
    match run(contains, name).await {
        Ok(out) => Ok(!String::from_utf8_lossy(&out).trim().is_empty()),
        // An unknown commit: no ref reaches it.
        Err(_) => Ok(false),
    }
}

/// Refuses a tree that is too big to unpack, before any of it is kept.
async fn check_tree(repo_dir: &Path, allow_file: bool, sha: &str, name: &str) -> Result<()> {
    let mut ls = git(repo_dir, allow_file);
    ls.args(["ls-tree", "-r", "-l", "-z", sha]);
    let out = run(ls, name).await?;
    let mut entries = 0usize;
    let mut total = 0u64;
    for rec in out.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        entries += 1;
        // `<mode> <type> <object> <size>\t<path>`; a submodule has `-` for its size.
        let head = rec.split(|b| *b == b'\t').next().unwrap_or_default();
        let size = String::from_utf8_lossy(head).split_whitespace().nth(3).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        total = total.saturating_add(size);
        if entries > MAX_ENTRIES || total > MAX_EXPANDED {
            return Err(Error::invalid("Actions", "uses", format!("{name} is too big to use as an action.")));
        }
    }
    Ok(())
}

async fn archive_dir(ctx: &Ctx, repo_dir: &Path, allow_file: bool, sha: &str, name: &str) -> Result<ch_core::services::BlobRef> {
    let out = repo_dir.join("action.tar.gz");
    let mut a = git(repo_dir, allow_file);
    a.args(["archive", "--format=tar.gz", "--prefix=action/", "-o"]).arg(&out).arg(sha);
    run(a, name).await?;
    let size = tokio::fs::metadata(&out).await.map_err(Error::internal)?.len();
    if size > MAX_ARCHIVE {
        return Err(Error::invalid("Actions", "uses", format!("{name} is too big to use as an action.")));
    }
    let file = tokio::fs::File::open(&out).await.map_err(Error::internal)?;
    ctx.blobs.put_stream(tokio_util::io::ReaderStream::new(file).boxed(), MAX_ARCHIVE).await
}

// ------------------------------------------------------------------------------------ here

fn rank(v: Visibility) -> u8 {
    match v {
        Visibility::Private => 0,
        Visibility::Internal => 1,
        Visibility::Public => 2,
    }
}

/// Who a run acts for: a pull request from a fork, and the person who started it (none for a
/// scheduled run).
pub(crate) struct Who {
    pub fork: bool,
    pub actor: Option<ch_core::UserId>,
}

/// May a run in `caller` use code from `target` on this instance? It is the caller's own
/// repository, or public, or has the same owner and is at least as visible as the caller and is
/// readable by whoever started the run: a member of an organization does not get a repository's
/// code by naming it in a workflow when they cannot read it themselves. A pull request from a fork
/// runs code nobody here vouched for, so it gets public repositories only; so does a scheduled run,
/// which has no person behind it. Used for actions and for reusable workflows.
pub(crate) async fn may_use_repo(ctx: &Ctx, caller: &ch_core::model::Repo, who: &Who, target: &ch_core::model::Repo) -> Result<bool> {
    let public_now = ctx.authz.repo_permission(&Actor::Anonymous, target.id).await? >= Permission::View;
    let own = target.id == caller.id;
    Ok(if who.fork {
        public_now
    } else {
        own || public_now || {
            let same = target.owner_id == caller.owner_id && rank(target.visibility) >= rank(caller.visibility);
            let readable = match who.actor {
                Some(u) => {
                    let person = Actor::User { id: u, is_admin: false, has_2fa: false, via: ch_core::AuthVia::Session };
                    ctx.authz.repo_permission(&person, target.id).await? >= Permission::Read
                }
                None => false,
            };
            same && readable
        }
    })
}

/// The commit a branch, tag or full SHA names in `target`, never one only a fork's pull request
/// has: `refs/pull/*` and bare commits that no branch or tag reaches do not count.
pub(crate) async fn commit_of_ref(ctx: &Ctx, target: RepoId, full: &str, git_ref: &str) -> Result<String> {
    if is_full_sha(git_ref) {
        let sha = git_ref.to_ascii_lowercase();
        if !ctx.git.reachable(target, &sha).await.unwrap_or(false) {
            return Err(not_in_repo(full, &sha));
        }
        return Ok(sha);
    }
    for r in [format!("refs/tags/{git_ref}"), format!("refs/heads/{git_ref}")] {
        if ctx.source.exact_ref(target, &r).await?.is_some() {
            return ctx.source.resolve(target, &format!("{r}^{{commit}}")).await;
        }
    }
    Err(Error::invalid(
        "Actions",
        "uses",
        format!("{full} has no tag or branch named {git_ref}. Use a tag, a branch or a full commit SHA."),
    ))
}

async fn haven(ctx: &Ctx, job: &JobRow, who: &Who, owner: &str, repo: &str, git_ref: &str) -> Result<(String, String)> {
    let full = format!("haven_{owner}/{repo}");
    let missing = || Error::invalid("Actions", "uses", format!("{full} was not found on this instance."));
    let target = match ctx.repos.get_by_name(&Actor::System, owner, repo).await {
        Ok(r) => r,
        Err(Error::NotFound) => return Err(missing()),
        Err(e) => return Err(e),
    };
    let caller = ctx.repos.get(&Actor::System, RepoId(job.repo_id)).await?;
    // A repository the job may not use looks exactly like one that does not exist.
    if !may_use_repo(ctx, &caller, who, &target).await? {
        return Err(missing());
    }
    // Only a branch or tag of that repository: never refs/pull, never a commit only a fork has.
    let sha = commit_of_ref(ctx, target.id, &full, git_ref).await?;
    let key = format!("{owner}/{repo}").to_ascii_lowercase();
    if let Some(blob) = known_archive(ctx, "haven", &key, &sha).await? {
        return Ok((sha, blob));
    }
    let stream = ctx.git.archive(target.id, &sha, ArchiveFormat::TarGz).await?;
    let blob = ctx.blobs.put_stream(stream.boxed(), MAX_ARCHIVE).await?;
    remember_archive(ctx, "haven", &key, &sha, &blob).await?;
    Ok((sha, blob.sha256))
}

/// May this job download the archive? Only one its own run resolved.
pub async fn archive_for_job(ctx: &Ctx, job: &JobRow, archive: &str) -> Result<()> {
    let ok: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM action_resolutions WHERE run_id = $1 AND archive_sha256 = $2)")
        .bind(job.run_id)
        .bind(archive)
        .fetch_one(&ctx.db)
        .await?;
    if ok { Ok(()) } else { Err(Error::NotFound) }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADV: &str = "1111111111111111111111111111111111111111\tHEAD
2222222222222222222222222222222222222222\trefs/heads/main
3333333333333333333333333333333333333333\trefs/heads/v1
4444444444444444444444444444444444444444\trefs/tags/v1
5555555555555555555555555555555555555555\trefs/tags/v1^{}
6666666666666666666666666666666666666666\trefs/pull/1/head
7777777777777777777777777777777777777777\trefs/tags/v2
";

    #[test]
    fn a_tag_wins_over_a_branch_and_is_peeled() {
        assert_eq!(pick_ref(ADV, "v1"), Some(("5555555555555555555555555555555555555555".into(), true)));
        assert_eq!(pick_ref(ADV, "v2"), Some(("7777777777777777777777777777777777777777".into(), true)));
        assert_eq!(pick_ref(ADV, "main"), Some(("2222222222222222222222222222222222222222".into(), true)));
    }

    #[test]
    fn a_sha_is_a_tip_only_on_a_branch_or_tag() {
        assert_eq!(pick_ref(ADV, "2222222222222222222222222222222222222222").map(|p| p.1), Some(true));
        // A pull request's head is not a branch of the repository.
        assert_eq!(pick_ref(ADV, "6666666666666666666666666666666666666666").map(|p| p.1), Some(false));
        // Short SHAs and unknown names resolve to nothing.
        assert_eq!(pick_ref(ADV, "2222222"), None);
        assert_eq!(pick_ref(ADV, "nope"), None);
        // A ref name is matched whole, never as a suffix.
        assert_eq!(pick_ref("8888888888888888888888888888888888888888\trefs/tags/x/v1\n", "v1"), None);
    }
}
