//! Importing a repository from a URL (ch-ejm, ROADMAP 4.1 M4, ARCHITECTURE 12.6).
//!
//! The server fetches from an address a person typed, so the fetch must not reach the host's
//! loopback, the docker network, the cloud metadata address or anything else private. The
//! same three steps as a webhook delivery:
//!
//! 1. The URL is checked before anything resolves: https only, no username or password, no
//!    local names, no literal private address.
//! 2. The name is resolved once, and every address it gives must be public.
//! 3. git connects to exactly that address (`http.curloptResolve` pins it), so a DNS answer that
//!    changes in between (rebinding) gets no second chance. Redirects are refused, so the
//!    server cannot be bounced to another host, and every protocol other than https is off, so
//!    no `file://`, `ext::` or ssh transport can be reached through a submodule or a trick URL.
//!
//! The fetch has a deadline and a size cap: a server that sends forever, or sends too much,
//! is cut off and the half-made repository removed. Only branches and tags are copied.

use crate::core::GitCore;
use crate::repos::check_repo_name;
use ch_core::error::{Error, FieldError, Result};
use ch_core::model::{ClientInfo, NewRepo, Repo};
use ch_core::net::is_public_ip;
use ch_core::{Actor, RepoId};
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

/// The most an import may write to disk: the same cap as one push over HTTPS.
pub const MAX_IMPORT_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// How long one import may take.
pub const IMPORT_DEADLINE: Duration = Duration::from_secs(15 * 60);
/// Imports running at once on this server. Each one is a long download and a lot of disk.
pub const MAX_CONCURRENT_IMPORTS: usize = 2;

/// Where an import comes from, after the checks that need no network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub url: url::Url,
    /// The host as git and libcurl will see it: a name, or an address (IPv6 without brackets).
    pub host: String,
    pub port: u16,
}

impl Source {
    /// Host and port only, never the path: what the audit log and messages may show.
    pub fn display(&self) -> String {
        match self.url.port() {
            Some(p) => format!("{}:{p}", self.url.host_str().unwrap_or("")),
            None => self.url.host_str().unwrap_or("").to_string(),
        }
    }
}

fn bad(msg: &str) -> Error {
    Error::Validation(vec![FieldError::custom("Import", "url", msg)])
}

/// The checks that need no network. `allow_private` (the `imports.allow_private` setting)
/// also allows plain http, for an instance that imports from its own network.
pub fn parse_source(raw: &str, allow_private: bool) -> Result<Source> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(bad("Give the address of the repository to import."));
    }
    if raw.len() > 2048 || raw.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(bad("That is not a valid address."));
    }
    let url = url::Url::parse(raw).map_err(|_| bad("That is not a valid address. Use one that starts with https://."))?;
    match url.scheme() {
        "https" => {}
        "http" if allow_private => {}
        _ => return Err(bad("Use an https:// address.")),
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(bad("Leave the username and password out of the address. Only repositories anyone can clone can be imported."));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(bad("Leave the ? and # parts out of the address."));
    }
    let host = match url.host() {
        Some(url::Host::Domain(d)) => d.to_ascii_lowercase(),
        Some(url::Host::Ipv4(v4)) => v4.to_string(),
        Some(url::Host::Ipv6(v6)) => v6.to_string(),
        None => return Err(bad("That address has no host.")),
    };
    let port = url.port_or_known_default().ok_or_else(|| bad("That address has no port."))?;
    if !allow_private {
        if let Ok(ip) = host.parse::<IpAddr>()
            && !is_public_ip(ip)
        {
            return Err(bad("Imports cannot come from a private or loopback address."));
        }
        if host == "localhost"
            || host.ends_with(".localhost")
            || host.ends_with(".internal")
            || host.ends_with(".local")
            || !host.contains(['.', ':'])
        {
            return Err(bad("Imports cannot come from a local name."));
        }
    }
    Ok(Source { url, host, port })
}

/// Resolves the host once. Every address must be public (unless `allow_private`), so a name
/// that resolves to both a public and a private address is refused rather than gambled on.
/// Returns the one address git will connect to.
pub async fn resolve(source: &Source, allow_private: bool) -> Result<SocketAddr> {
    let addrs: Vec<SocketAddr> = if let Ok(ip) = source.host.parse::<IpAddr>() {
        vec![SocketAddr::new(ip, source.port)]
    } else {
        let lookup = tokio::net::lookup_host((source.host.as_str(), source.port));
        match tokio::time::timeout(Duration::from_secs(10), lookup).await {
            Ok(Ok(a)) => a.collect(),
            _ => return Err(bad("That name could not be found.")),
        }
    };
    let Some(first) = addrs.first().copied() else { return Err(bad("That name could not be found.")) };
    if !allow_private && addrs.iter().any(|a| !is_public_ip(a.ip())) {
        return Err(bad("That name points at a private address, which imports cannot reach."));
    }
    Ok(first)
}

/// The `-c` settings and arguments for the pinned fetch into `GIT_DIR`. Kept apart so a test can
/// check every guard is there.
pub fn fetch_args(source: &Source, addr: SocketAddr) -> Vec<String> {
    let ip = match addr.ip() {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => format!("[{v6}]"),
    };
    let host = if source.host.contains(':') { format!("[{}]", source.host) } else { source.host.clone() };
    let mut a: Vec<String> = Vec::new();
    for c in [
        // Only https (or http, where the instance allows private imports): no file://, ext::,
        // ssh or git:// through anything the remote says.
        "protocol.allow=never".to_string(),
        "protocol.https.allow=always".to_string(),
        format!("protocol.http.allow={}", if source.url.scheme() == "http" { "always" } else { "never" }),
        // Exactly the address that was checked, and nowhere else.
        format!("http.curloptResolve={host}:{}:{ip}", source.port),
        "http.followRedirects=false".to_string(),
        "http.proxy=".to_string(),
        "credential.helper=".to_string(),
        // A server that stalls is cut off rather than holding a slot for the whole deadline.
        "http.lowSpeedLimit=1024".to_string(),
        "http.lowSpeedTime=60".to_string(),
        "fetch.fsckObjects=true".to_string(),
        "transfer.fsckObjects=true".to_string(),
    ] {
        a.push("-c".into());
        a.push(c);
    }
    a.extend(["fetch", "--quiet", "--no-tags", "--no-write-fetch-head", "--no-recurse-submodules", "--"].map(String::from));
    a.push(source.url.to_string());
    a.push("+refs/heads/*:refs/heads/*".into());
    a.push("+refs/tags/*:refs/tags/*".into());
    a
}

/// Bytes under a directory, for the size cap. Unreadable entries count as nothing.
pub fn dir_size(path: &std::path::Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&p) else { continue };
        for e in rd.flatten() {
            match e.metadata() {
                Ok(m) if m.is_dir() => stack.push(e.path()),
                Ok(m) => total += m.len(),
                Err(_) => {}
            }
        }
    }
    total
}

static IMPORTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(MAX_CONCURRENT_IMPORTS);

impl GitCore {
    /// Makes a new repository and fills it from a URL anyone can clone. Created like any new
    /// repository (private or internal, never public), and removed again if the fetch fails.
    pub(crate) async fn do_import_repo(&self, actor: &Actor, new: NewRepo, url: &str, client: &ClientInfo) -> Result<Repo> {
        actor.deny_app("import a repository")?;
        if actor.user_id().is_none() && !actor.is_system() {
            return Err(Error::forbidden("Sign in to import a repository."));
        }
        let allow = self.cfg.imports.allow_private;
        let source = parse_source(url, allow)?;
        self.may_create_under(actor, new.owner_id).await?;
        check_repo_name(&new.name)?;
        let _slot = IMPORTS
            .try_acquire()
            .map_err(|_| Error::unavailable("Other imports are running on this server. Try again in a few minutes."))?;
        let addr = resolve(&source, allow).await?;
        let repo = self.do_create_repo(actor, NewRepo { auto_init: false, ..new }, client).await?;
        match self.fill_import(repo.id, &source, addr).await {
            Ok(()) => {
                tracing::info!(repo = repo.id.0, from = %source.display(), "repository imported");
                self.repo_row(repo.id).await
            }
            Err(e) => {
                let _ = sqlx::query("DELETE FROM repositories WHERE id = $1").bind(repo.id.0).execute(&self.db).await;
                let _ = self.store.trash(repo.id);
                Err(e)
            }
        }
    }

    /// The pinned fetch, watched for size, then the default branch.
    async fn fill_import(&self, id: RepoId, source: &Source, addr: SocketAddr) -> Result<()> {
        let dir = self.store.repo_path(id);
        let cmd = self.git(id).args(fetch_args(source, addr)).timeout(IMPORT_DEADLINE);
        let fetch = self.exec(cmd);
        tokio::pin!(fetch);
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        loop {
            tokio::select! {
                r = &mut fetch => {
                    if let Err(e) = r {
                        // The remote's own words stay in the log: they can name paths the person
                        // did not type, and the address is theirs to know already.
                        tracing::info!(repo = id.0, from = %source.display(), error = %e, "import fetch failed");
                        return Err(bad(&format!(
                            "The import from {} did not work. Check the address is a git repository anyone can clone over https, then try again.",
                            source.display()
                        )));
                    }
                    break;
                }
                _ = tick.tick() => {
                    let d = dir.clone();
                    let size = tokio::task::spawn_blocking(move || dir_size(&d)).await.unwrap_or(0);
                    if size > MAX_IMPORT_BYTES {
                        // Dropping the fetch kills git.
                        return Err(bad("That repository is larger than 2 GB, the most an import can bring in."));
                    }
                }
            }
        }
        self.open_on_default_branch(id).await.map(|_| ())
    }

    /// Points HEAD at the branch an import opens on and marks the repository as not empty.
    /// Returns the branch, or `None` when nothing came across (an empty repository, as if made by hand).
    async fn open_on_default_branch(&self, id: RepoId) -> Result<Option<String>> {
        let out = self.exec(self.git(id).args(["for-each-ref", "--format=%(refname:strip=2)", "refs/heads/"])).await?;
        let branches: Vec<String> = out.stdout_str().lines().map(str::to_string).filter(|b| !b.is_empty()).collect();
        let Some(branch) = pick_default(&branches, &self.repo_row(id).await?.default_branch) else {
            return Ok(None);
        };
        self.exec(self.git(id).args(["symbolic-ref", "HEAD"]).arg(format!("refs/heads/{branch}"))).await?;
        sqlx::query("UPDATE repositories SET default_branch = $2, is_empty = false, pushed_at = now(), updated_at = now() WHERE id = $1")
            .bind(id.0)
            .bind(&branch)
            .execute(&self.db)
            .await?;
        Ok(Some(branch))
    }

    /// A private repository for an existing account from a bare repository on this server's disk
    /// (`codehaven admin repo-import`). The system actor only. Branches and tags come across and
    /// nothing else; the source is never changed. With `run_ci` the default branch is announced
    /// as a push by the owner, so the repository's workflows run once.
    pub(crate) async fn do_import_local(&self, actor: &Actor, new: NewRepo, path: &str, run_ci: bool, client: &ClientInfo) -> Result<Repo> {
        if !actor.is_system() {
            return Err(Error::forbidden("Only the server's own administration may import from a path."));
        }
        check_repo_name(&new.name)?;
        let src = std::path::Path::new(path);
        if !src.is_absolute() || !src.is_dir() {
            return Err(Error::Validation(vec![FieldError::custom(
                "Import",
                "path",
                "Give the absolute path of a bare git repository on this server.",
            )]));
        }
        let bare = self
            .exec_any(self.store.bare_cmd().args(["rev-parse", "--is-bare-repository"]).git_dir(src.to_path_buf()))
            .await
            .map(|o| o.code == 0 && o.stdout_str().trim() == "true")
            .unwrap_or(false);
        if !bare {
            return Err(Error::Validation(vec![FieldError::custom("Import", "path", "That path is not a bare git repository.")]));
        }
        let owner = new.owner_id;
        let repo = self
            .do_create_repo(actor, NewRepo { auto_init: false, visibility: ch_core::model::Visibility::Private, ..new }, client)
            .await?;
        let fetched = self
            .exec(
                self.git(repo.id)
                    .args([
                        "-c",
                        "protocol.allow=never",
                        "-c",
                        "protocol.file.allow=always",
                        "fetch",
                        "--quiet",
                        "--no-tags",
                        "--no-write-fetch-head",
                    ])
                    .arg(src)
                    .args(["+refs/heads/*:refs/heads/*", "+refs/tags/*:refs/tags/*"])
                    .timeout(IMPORT_DEADLINE),
            )
            .await;
        let branch = match fetched {
            Ok(_) => self.open_on_default_branch(repo.id).await,
            Err(e) => Err(Error::from(e)),
        };
        let branch = match branch {
            Ok(b) => b,
            Err(e) => {
                let _ = sqlx::query("DELETE FROM repositories WHERE id = $1").bind(repo.id.0).execute(&self.db).await;
                let _ = self.store.trash(repo.id);
                tracing::info!(error = %e, "import from a server path failed");
                return Err(Error::Validation(vec![FieldError::custom(
                    "Import",
                    "path",
                    "The import did not work. Is that a complete bare repository?",
                )]));
            }
        };
        let mut tx = self.db.begin().await?;
        self.audit
            .record(
                &mut tx,
                ch_core::audit::AuditEvent::new(
                    actor,
                    client,
                    ch_core::audit::action::REPO_IMPORTED_LOCAL,
                    ch_core::audit::Outcome::Success,
                )
                .target("repository", repo.id.0)
                .details(serde_json::json!({ "full_name": repo.full_name(), "from": path, "run_ci": run_ci })),
            )
            .await?;
        if run_ci && let Some(branch) = &branch {
            let after = self.read_resolve(repo.id, &format!("refs/heads/{branch}")).await?;
            self.events
                .emit(
                    &mut tx,
                    ch_core::events::DomainEvent::Push {
                        repo_id: repo.id,
                        git_ref: format!("refs/heads/{branch}"),
                        before: "0".repeat(40),
                        after,
                        pusher: Some(owner),
                        forced: false,
                        app: None,
                        server_built: false,
                    },
                )
                .await?;
        }
        tx.commit().await?;
        self.repo_row(repo.id).await
    }
}

/// The branch a new import opens on: the one asked for if it came across, else `main`, else
/// `master`, else the first. `None` when nothing came across.
pub fn pick_default(branches: &[String], wanted: &str) -> Option<String> {
    [wanted, "main", "master"]
        .iter()
        .find(|w| branches.iter().any(|b| b == *w))
        .map(|w| (*w).to_string())
        .or_else(|| branches.first().cloned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(u: &str) -> bool {
        parse_source(u, false).is_err()
    }

    #[test]
    fn only_plain_public_https_addresses_pass_the_first_check() {
        assert!(parse_source("https://git.example.org/team/tool.git", false).is_ok());
        assert!(parse_source("https://git.example.org:8443/a/b", false).is_ok());
        for u in [
            "",
            "http://git.example.org/a/b",
            "ssh://git@git.example.org/a/b",
            "git://git.example.org/a/b",
            "file:///etc",
            "ext::sh -c id",
            "https://user:pw@git.example.org/a/b",
            "https://token@git.example.org/a/b",
            "https://localhost/a",
            "https://a.localhost/a",
            "https://metadata.google.internal/a",
            "https://printer.local/a",
            "https://intranet/a",
            "https://127.0.0.1/a",
            "https://10.0.0.5/a",
            "https://169.254.169.254/latest",
            "https://[::1]/a",
            "https://[fd00::1]/a",
            "https://0.0.0.0/a",
            "https://git.example.org/a?x=1",
            "https://git.example.org/a#frag",
            "https://git.example.org/a b",
        ] {
            assert!(refused(u), "{u}");
        }
        // The instance can allow its own network, and then plain http too.
        assert!(parse_source("http://127.0.0.1:3000/a/b.git", true).is_ok());
        assert!(parse_source("ftp://127.0.0.1/a", true).is_err());
    }

    #[test]
    fn the_fetch_is_pinned_and_fenced() {
        let s = parse_source("https://git.example.org/a/b.git", false).unwrap();
        let a = fetch_args(&s, "203.0.113.34:443".parse().unwrap()).join(" ");
        for want in [
            "protocol.allow=never",
            "protocol.https.allow=always",
            "protocol.http.allow=never",
            "http.curloptResolve=git.example.org:443:203.0.113.34",
            "http.followRedirects=false",
            "http.proxy=",
            "--no-recurse-submodules",
            "+refs/heads/*:refs/heads/*",
        ] {
            assert!(a.contains(want), "{want} missing from {a}");
        }
        assert!(a.ends_with("-- https://git.example.org/a/b.git +refs/heads/*:refs/heads/* +refs/tags/*:refs/tags/*"), "{a}");
        let v6 = fetch_args(&s, "[2001:db8::1]:443".parse().unwrap()).join(" ");
        assert!(v6.contains("http.curloptResolve=git.example.org:443:[2001:db8::1]"), "{v6}");
    }

    #[test]
    fn the_display_never_shows_the_path() {
        let s = parse_source("https://git.example.org/secret-team/plan.git", false).unwrap();
        assert_eq!(s.display(), "git.example.org");
    }
}
