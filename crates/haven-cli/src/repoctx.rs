//! Which repository a command is about: `-R owner/name`, or the git remote of the current
//! directory.

use anyhow::{Result, anyhow, bail};
use std::path::Path;

/// `owner/name`, with an optional `https://host/` prefix and `.git` suffix.
pub fn parse_owner_name(s: &str) -> Result<(String, String)> {
    let s = s.trim().trim_end_matches('/');
    let path = match s.split_once("://") {
        Some((_, rest)) => rest.split_once('/').map(|(_, p)| p).unwrap_or(""),
        None => s,
    };
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.split('/').filter(|p| !p.is_empty());
    match (parts.next(), parts.next(), parts.next()) {
        (Some(o), Some(n), None) => Ok((o.to_string(), n.to_string())),
        _ => bail!("{s:?} is not a repository. Use owner/name."),
    }
}

/// The `(host, owner, name)` of a git remote URL in any of the three forms git accepts:
/// `https://host/o/r.git`, `ssh://git@host:2222/o/r.git` and `git@host:o/r.git`.
pub fn parse_remote(url: &str) -> Option<(String, String, String)> {
    let url = url.trim();
    let (host, path) = if let Some((_, rest)) = url.split_once("://") {
        let (authority, path) = rest.split_once('/')?;
        let authority = authority.rsplit('@').next()?;
        (authority.to_string(), path.to_string())
    } else if let Some((left, path)) = url.split_once(':') {
        // scp-like: user@host:owner/name.git
        let host = left.rsplit('@').next()?;
        (host.to_string(), path.to_string())
    } else {
        return None;
    };
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.split('/').filter(|p| !p.is_empty());
    let (o, n) = (parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    Some((host, o.to_string(), n.to_string()))
}

/// Reads `origin` (then `upstream`, then the first remote) from the repository in `cwd`.
/// When `host` is known, a remote on another host is skipped, so a clone of a GitHub
/// repository does not get mistaken for a Codehaven one.
pub fn from_git_remote(cwd: &Path, host: Option<&str>) -> Result<(String, String)> {
    let out =
        std::process::Command::new("git").arg("-C").arg(cwd).args(["remote", "-v"]).output().map_err(|e| anyhow!("cannot run git: {e}"))?;
    if !out.status.success() {
        bail!("this directory is not a git repository. Pass -R owner/name.");
    }
    let text = String::from_utf8_lossy(&out.stdout);
    pick_remote(&text, host).ok_or_else(|| anyhow!("no git remote points at this Codehaven host. Pass -R owner/name."))
}

/// The choosing logic of `from_git_remote`, on the text of `git remote -v`.
pub fn pick_remote(remote_v: &str, host: Option<&str>) -> Option<(String, String)> {
    let want = host.and_then(crate::config::host_port);
    let mut found: Vec<(String, String, String, String)> = Vec::new(); // name, host, owner, repo
    for line in remote_v.lines() {
        let mut it = line.split_whitespace();
        let (Some(name), Some(url)) = (it.next(), it.next()) else { continue };
        if let Some((h, o, r)) = parse_remote(url) {
            found.push((name.to_string(), h, o, r));
        }
    }
    let matches_host = |h: &str| match &want {
        // An ssh remote may carry the ssh port, so compare the host name alone as well.
        Some(w) => h == w || h.split(':').next() == w.split(':').next(),
        None => true,
    };
    for pref in ["origin", "upstream"] {
        if let Some((_, h, o, r)) = found.iter().find(|(n, h, ..)| n == pref && matches_host(h)) {
            let _ = h;
            return Some((o.clone(), r.clone()));
        }
    }
    found.into_iter().find(|(_, h, ..)| matches_host(h)).map(|(_, _, o, r)| (o, r))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_name_forms() {
        assert_eq!(parse_owner_name("alice/demo").unwrap(), ("alice".into(), "demo".into()));
        assert_eq!(parse_owner_name("https://h.example/alice/demo.git").unwrap(), ("alice".into(), "demo".into()));
        assert!(parse_owner_name("alice").is_err());
        assert!(parse_owner_name("a/b/c").is_err());
    }

    #[test]
    fn remote_forms() {
        assert_eq!(parse_remote("https://h.example/alice/demo.git"), Some(("h.example".into(), "alice".into(), "demo".into())));
        assert_eq!(parse_remote("ssh://git@h.example:2222/alice/demo.git"), Some(("h.example:2222".into(), "alice".into(), "demo".into())));
        assert_eq!(parse_remote("git@h.example:alice/demo.git"), Some(("h.example".into(), "alice".into(), "demo".into())));
        assert_eq!(parse_remote("https://user:tok@h.example/alice/demo"), Some(("h.example".into(), "alice".into(), "demo".into())));
        assert_eq!(parse_remote("/local/path/repo"), None);
    }

    #[test]
    fn origin_wins_and_foreign_hosts_are_skipped() {
        let v = "up\thttps://h.example/up/demo.git (fetch)\norigin\thttps://h.example/alice/demo.git (fetch)\n";
        assert_eq!(pick_remote(v, Some("https://h.example")), Some(("alice".into(), "demo".into())));
        let v = "origin\thttps://github.com/x/y.git (fetch)\nhaven\thttps://h.example/alice/demo.git (fetch)\n";
        assert_eq!(pick_remote(v, Some("https://h.example")), Some(("alice".into(), "demo".into())));
        assert_eq!(pick_remote(v, Some("https://other.example")), None);
    }
}
