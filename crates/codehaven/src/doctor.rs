//! `codehaven doctor` and `codehaven healthcheck`.

use ch_core::Config;
use ch_core::access::PUBLIC_ROUTES;
use std::path::Path;
use std::process::Command;

#[derive(Debug, PartialEq, Eq)]
pub enum Level {
    Pass,
    Warn,
    Fail,
}

pub struct Check {
    pub level: Level,
    pub name: &'static str,
    pub detail: String,
}

impl Check {
    fn pass(name: &'static str, detail: impl Into<String>) -> Self {
        Check { level: Level::Pass, name, detail: detail.into() }
    }
    fn warn(name: &'static str, detail: impl Into<String>) -> Self {
        Check { level: Level::Warn, name, detail: detail.into() }
    }
    fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Check { level: Level::Fail, name, detail: detail.into() }
    }
}

/// `git version 2.47.3` gives (2, 47). `merge-tree --write-tree` needs 2.38.
pub fn parse_git_version(out: &str) -> Option<(u32, u32)> {
    let v = out.trim().strip_prefix("git version ")?;
    let mut parts = v.split('.');
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

pub fn check_git() -> Check {
    match Command::new("git").arg("--version").output() {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout).to_string();
            match parse_git_version(&text) {
                Some(v) if v >= (2, 38) => Check::pass("git", text.trim()),
                Some(_) => Check::fail("git", format!("{} is too old, need 2.38 or newer", text.trim())),
                None => Check::warn("git", format!("could not read the version from `{}`", text.trim())),
            }
        }
        _ => Check::fail("git", "the git binary is not on PATH"),
    }
}

/// Creates the secrets directory private to its owner (mode 700), and tightens it if it
/// already exists with looser permissions.
pub fn create_secrets_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

pub fn check_dirs(config: &Config) -> Vec<Check> {
    let mut out = Vec::new();
    // The data root only has to exist: the server writes inside its subdirectories, and in the
    // container the root itself belongs to root.
    let root = &config.storage.data_dir;
    if root.is_dir() {
        out.push(Check::pass("data dir", format!("{} exists", root.display())));
    } else {
        out.push(Check::fail("data dir", format!("{} does not exist", root.display())));
    }
    for (name, dir) in [
        ("git dir", config.git_dir()),
        ("blobs dir", config.blobs_dir()),
        ("logs dir", config.logs_dir()),
        ("secrets dir", config.secrets_dir()),
    ] {
        let made = if name == "secrets dir" { create_secrets_dir(&dir) } else { std::fs::create_dir_all(&dir) };
        if let Err(e) = made {
            out.push(Check::fail(name, format!("{}: {e}", dir.display())));
            continue;
        }
        let probe = dir.join(".doctor");
        match std::fs::write(&probe, b"ok") {
            Ok(()) => {
                let _ = std::fs::remove_file(&probe);
                out.push(Check::pass(name, format!("{} is writable", dir.display())));
            }
            Err(e) => out.push(Check::fail(name, format!("{} is not writable: {e}", dir.display()))),
        }
    }
    out.extend(check_secrets_mode(&config.secrets_dir()));
    out
}

/// Checks the git hook scripts the server relies on: each of `pre-receive` and `post-receive`
/// must exist and stay executable, and a script in the hooks directory must actually run. Git
/// silently skips a hook it cannot execute, so without this check protection fails open.
pub fn check_hooks(config: &Config) -> Vec<Check> {
    let store = ch_git::store::Store::new(config.git_dir());
    // The directory is made when missing, so doctor also runs where the server never has;
    // anything that then cannot be written or run shows up as a probe failure.
    let _ = std::fs::create_dir_all(&store.hooks_dir);
    if let Err(msg) = store.exec_probe() {
        return vec![Check::fail("git hooks", msg)];
    }
    let problems = store.hook_script_problems();
    if problems.is_empty() {
        return vec![Check::pass("git hooks", "pre-receive and post-receive are executable and the hooks directory can run programs")];
    }
    // The server writes both scripts at start, so a fresh install simply has neither yet.
    if problems.iter().all(|p| p.contains("is missing")) {
        return vec![Check::pass("git hooks", "not written yet (the server writes them at start); this directory can run programs")];
    }
    problems.into_iter().map(|p| Check::fail("git hooks", p)).collect()
}

#[cfg(unix)]
fn check_secrets_mode(dir: &Path) -> Vec<Check> {
    use std::os::unix::fs::PermissionsExt;
    let mut out = Vec::new();
    if let Ok(m) = std::fs::metadata(dir) {
        let mode = m.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            out.push(Check::fail("secrets dir mode", format!("{} is mode {mode:o}, expected 700", dir.display())));
        } else {
            out.push(Check::pass("secrets dir mode", format!("{mode:o}")));
        }
    }
    let key = dir.join("master.key");
    if let Ok(m) = std::fs::metadata(&key) {
        let mode = m.permissions().mode() & 0o777;
        if mode & 0o077 != 0 || m.len() != 32 {
            out.push(Check::fail("master.key", format!("mode {mode:o}, {} bytes; expected 600 and 32 bytes", m.len())));
        } else {
            out.push(Check::pass("master.key", "mode 600, 32 bytes"));
        }
    }
    out
}

/// Percent used on the filesystem holding `path`, from `df`.
pub fn disk_percent(path: &Path) -> Option<u32> {
    let out = Command::new("df").arg("-P").arg(path).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().nth(1)?;
    let pct = line.split_whitespace().nth(4)?;
    pct.trim_end_matches('%').parse().ok()
}

pub fn check_disk(config: &Config) -> Check {
    match disk_percent(&config.storage.data_dir) {
        Some(p) if p >= 90 => Check::fail("disk", format!("{p}% used")),
        Some(p) if p >= 85 => Check::warn("disk", format!("{p}% used")),
        Some(p) => Check::pass("disk", format!("{p}% used")),
        None => Check::warn("disk", "could not read disk usage"),
    }
}

pub fn check_network_config(config: &Config) -> Vec<Check> {
    let mut out = Vec::new();
    out.push(Check::pass("external url", config.server.external_url.clone()));
    if config.is_https() && config.http.trusted_proxies.is_empty() {
        out.push(Check::warn(
            "trusted proxies",
            "none set: behind a reverse proxy every client would look like the proxy to the rate limiter and the audit log",
        ));
    } else {
        out.push(Check::pass("trusted proxies", format!("{} network(s)", config.http.trusted_proxies.len())));
    }
    out.push(Check::pass("internal listener", format!("{} (loopback only)", config.server.internal_bind)));
    out
}

/// Whether the Claude connector is on, and where it lives.
pub fn check_mcp(config: &Config) -> Check {
    if config.mcp.enabled {
        Check::pass(
            "connector",
            format!("on at {} (trusted client hosts: {})", config.mcp_resource(), config.mcp.trusted_client_hosts.join(", ")),
        )
    } else {
        Check::pass("connector", "off (set mcp.enabled to turn on the Claude connector)")
    }
}

/// The limits signed-out visitors get on public repositories.
pub fn check_public_access(config: &Config) -> Check {
    let p = &config.public_access;
    Check::pass(
        "public access limits",
        format!(
            "{} requests a minute per address; clones {} at once per address, {} for the instance; archives {} and {}; raw files {} and {}",
            p.requests_per_minute,
            p.git_per_address,
            p.git_instance,
            p.archive_per_address,
            p.archive_instance,
            p.raw_per_address,
            p.raw_instance
        ),
    )
}

pub fn check_public_routes() -> Check {
    let list = PUBLIC_ROUTES.iter().map(|r| format!("{} {}", r.methods.join("/"), r.pattern)).collect::<Vec<_>>().join(", ");
    Check::pass("public routes", format!("{} routes a signed-out visitor can reach: {list}", PUBLIC_ROUTES.len()))
}

pub async fn check_database(config: &Config) -> Vec<Check> {
    let url = match config.database_url() {
        Ok(u) => u,
        Err(e) => return vec![Check::fail("database", e.to_string())],
    };
    let pool = match ch_db::connect(&url, 2).await {
        Ok(p) => p,
        Err(e) => return vec![Check::fail("database", e.public_message())],
    };
    let mut out = vec![Check::pass("database", "connected")];
    match ch_db::migration_status(&pool).await {
        Ok((applied, known)) if applied == known => out.push(Check::pass("migrations", format!("{applied} applied"))),
        Ok((applied, known)) => out.push(Check::fail("migrations", format!("{applied} applied of {known}; run `codehaven migrate`"))),
        Err(e) => out.push(Check::fail("migrations", e.public_message())),
    }
    // `/mcp` and `/oauth/...` are connector paths. An older instance may already have an account
    // or organization with one of those names, whose profile page would then be shadowed.
    if let Ok(n) =
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM users WHERE lower(username::text) IN ('mcp', 'oauth')").fetch_one(&pool).await
    {
        out.push(if n == 0 {
            Check::pass("reserved names", "no account or organization is named mcp or oauth")
        } else {
            Check::warn("reserved names", "an account or organization is named mcp or oauth: rename it before turning the connector on")
        });
    }
    if let Ok(n) = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM users WHERE kind = 'user'").fetch_one(&pool).await {
        out.push(Check::pass(
            "accounts",
            if n == 0 {
                "none yet (the first admin is made with `codehaven admin create-user --admin`)".to_string()
            } else {
                format!("{n}")
            },
        ));
    }
    out
}

/// Runs every check and prints a report. Returns false when anything failed.
pub async fn run(config: &Config) -> bool {
    let mut checks = vec![check_git()];
    checks.extend(check_dirs(config));
    checks.extend(check_hooks(config));
    checks.push(check_disk(config));
    checks.extend(check_network_config(config));
    checks.push(check_mcp(config));
    checks.push(check_public_access(config));
    checks.push(check_public_routes());
    checks.extend(check_database(config).await);
    let mut ok = true;
    for c in &checks {
        let tag = match c.level {
            Level::Pass => "ok  ",
            Level::Warn => "warn",
            Level::Fail => {
                ok = false;
                "FAIL"
            }
        };
        println!("[{tag}] {:<18} {}", c.name, c.detail);
    }
    println!("{}", if ok { "doctor: all checks passed" } else { "doctor: something needs fixing" });
    ok
}

/// GETs `/readyz` on the internal listener with a bare TCP request. No HTTP client needed.
pub async fn healthcheck(config: &Config) -> bool {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let addr = &config.server.internal_bind;
    let attempt = async {
        let mut s = tokio::net::TcpStream::connect(addr).await?;
        s.write_all(format!("GET /readyz HTTP/1.0\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes()).await?;
        let mut buf = String::new();
        s.read_to_string(&mut buf).await?;
        Ok::<_, std::io::Error>(buf)
    };
    match tokio::time::timeout(std::time::Duration::from_secs(3), attempt).await {
        Ok(Ok(resp)) => resp.starts_with("HTTP/1.0 200") || resp.starts_with("HTTP/1.1 200"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_version_parsing() {
        assert_eq!(parse_git_version("git version 2.47.3\n"), Some((2, 47)));
        assert_eq!(parse_git_version("git version 2.39.5 (Apple Git-154)"), Some((2, 39)));
        assert_eq!(parse_git_version("hello"), None);
    }

    #[test]
    fn public_routes_check_lists_every_route() {
        let c = check_public_routes();
        assert!(c.detail.contains(&PUBLIC_ROUTES.len().to_string()));
        assert!(c.detail.contains("/healthz"));
    }

    #[test]
    fn public_access_line_names_the_limits() {
        let cfg = Config::for_tests();
        let c = check_public_access(&cfg);
        assert!(c.detail.contains("240 requests a minute per address"), "{}", c.detail);
        assert!(c.detail.contains("clones 2 at once per address, 3 for the instance"), "{}", c.detail);
    }

    #[test]
    fn untrusted_https_without_proxies_warns() {
        let mut cfg = Config::for_tests();
        cfg.server.external_url = "https://x.example".into();
        let checks = check_network_config(&cfg);
        assert!(checks.iter().any(|c| c.name == "trusted proxies" && c.level == Level::Warn));
    }

    #[test]
    fn dirs_are_created_and_checked() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::for_tests();
        cfg.storage.data_dir = dir.path().to_path_buf();
        let checks = check_dirs(&cfg);
        assert!(checks.iter().all(|c| c.level != Level::Fail), "{:?}", checks.iter().map(|c| (&c.name, &c.detail)).collect::<Vec<_>>());
        assert!(dir.path().join("git").is_dir());
    }

    #[test]
    fn hooks_check_fails_when_a_hook_is_not_executable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::for_tests();
        cfg.storage.data_dir = dir.path().to_path_buf();
        let store = ch_git::store::Store::new(cfg.git_dir());
        store.init_dirs().unwrap();
        store.install_hooks(Path::new("/usr/bin/true")).unwrap();
        std::fs::set_permissions(store.hooks_dir.join("pre-receive"), std::fs::Permissions::from_mode(0o644)).unwrap();
        let checks = check_hooks(&cfg);
        assert!(
            checks.iter().any(|c| c.name == "git hooks" && c.level == Level::Fail && c.detail.contains("pre-receive")),
            "{:?}",
            checks.iter().map(|c| (&c.name, &c.level, &c.detail)).collect::<Vec<_>>()
        );
    }

    #[test]
    fn hooks_check_passes_after_install() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::for_tests();
        cfg.storage.data_dir = dir.path().to_path_buf();
        let store = ch_git::store::Store::new(cfg.git_dir());
        store.init_dirs().unwrap();
        store.install_hooks(Path::new("/usr/bin/true")).unwrap();
        assert!(check_hooks(&cfg).iter().filter(|c| c.name == "git hooks").all(|c| c.level == Level::Pass));
    }

    #[test]
    fn hooks_check_does_not_fail_before_the_server_ever_started() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::for_tests();
        cfg.storage.data_dir = dir.path().to_path_buf();
        let store = ch_git::store::Store::new(cfg.git_dir());
        store.init_dirs().unwrap();
        assert!(check_hooks(&cfg).iter().all(|c| c.level != Level::Fail));
    }
}
