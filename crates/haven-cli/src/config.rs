//! Where `haven` keeps its settings and tokens.
//!
//! - `<config dir>/hosts.toml` holds the default host and, per host, the account name. No
//!   secrets in it.
//! - Tokens go in the OS keyring (service `haven`, account = the host's base URL). When there
//!   is no keyring (a headless Linux box, a locked-down container) they go in
//!   `<config dir>/tokens.toml` with mode 0600 instead.
//! - `HAVEN_TOKEN` and `HAVEN_HOST` override both, which is how scripts and CI use `haven`.

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Where the config directory is: `HAVEN_CONFIG_DIR`, else `$XDG_CONFIG_HOME/haven`, else
/// `~/.config/haven` (`%APPDATA%\haven` on Windows).
pub fn default_config_dir() -> Result<PathBuf> {
    if let Some(d) = std::env::var_os("HAVEN_CONFIG_DIR").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(d));
    }
    if let Some(d) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(d).join("haven"));
    }
    #[cfg(windows)]
    if let Some(d) = std::env::var_os("APPDATA").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(d).join("haven"));
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .ok_or_else(|| anyhow!("cannot find a home directory; set HAVEN_CONFIG_DIR"))?;
    Ok(PathBuf::from(home).join(".config").join("haven"))
}

/// Turns what a person types (`code.example.com`, `https://code.example.com/`, `localhost:3200`)
/// into a base URL with no trailing slash. A bare host gets `https://`; `localhost` and
/// loopback addresses get `http://`, since nothing terminates TLS in front of a dev server.
pub fn normalize_host(input: &str) -> Result<String> {
    let s = input.trim().trim_end_matches('/');
    if s.is_empty() {
        bail!("the host is empty");
    }
    let with_scheme = if s.contains("://") {
        s.to_string()
    } else {
        let host_part = s.split(':').next().unwrap_or(s);
        let local = host_part == "localhost" || host_part == "127.0.0.1" || host_part == "[::1]" || host_part.ends_with(".localhost");
        format!("{}://{s}", if local { "http" } else { "https" })
    };
    let url = url::Url::parse(&with_scheme).with_context(|| format!("{input:?} is not a valid host"))?;
    if url.scheme() != "https" && url.scheme() != "http" {
        bail!("the host must be http or https, not {}", url.scheme());
    }
    let host = url.host_str().ok_or_else(|| anyhow!("{input:?} has no host name"))?;
    let mut base = format!("{}://{host}", url.scheme());
    if let Some(p) = url.port() {
        base.push_str(&format!(":{p}"));
    }
    Ok(base)
}

/// `host[:port]` of a base URL, as git sees it in a credential request.
pub fn host_port(base: &str) -> Option<String> {
    let u = url::Url::parse(base).ok()?;
    let h = u.host_str()?;
    Some(match u.port() {
        Some(p) => format!("{h}:{p}"),
        None => h.to_string(),
    })
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Hosts {
    /// Base URL of the host used when `--host` and `HAVEN_HOST` are absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(default)]
    pub hosts: BTreeMap<String, HostEntry>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// `https` or `ssh`: how `repo clone` builds the remote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_protocol: Option<String>,
}

impl Hosts {
    pub fn load(dir: &Path) -> Result<Hosts> {
        let p = dir.join("hosts.toml");
        match std::fs::read_to_string(&p) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("reading {}", p.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Hosts::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", p.display())),
        }
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        write_private(&dir.join("hosts.toml"), &toml::to_string_pretty(self)?)
    }
}

/// Writes a file that only the current user can read.
pub fn write_private(path: &Path, content: &str) -> Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
        f.write_all(content.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// Where a token for a host is kept.
pub trait TokenStore: Send + Sync {
    fn get(&self, host: &str) -> Result<Option<String>>;
    fn set(&self, host: &str, token: &str) -> Result<()>;
    fn delete(&self, host: &str) -> Result<()>;
    /// Where it lives, for `auth status`.
    fn describe(&self) -> String;
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct TokenFile {
    #[serde(default)]
    tokens: BTreeMap<String, String>,
}

/// A 0600 TOML file. The fallback when there is no keyring, and the store the tests use.
pub struct FileStore {
    path: PathBuf,
}

impl FileStore {
    pub fn new(dir: &Path) -> FileStore {
        FileStore { path: dir.join("tokens.toml") }
    }

    fn read(&self) -> Result<TokenFile> {
        match std::fs::read_to_string(&self.path) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("reading {}", self.path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(TokenFile::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", self.path.display())),
        }
    }

    fn write(&self, f: &TokenFile) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        write_private(&self.path, &toml::to_string_pretty(f)?)
    }
}

impl TokenStore for FileStore {
    fn get(&self, host: &str) -> Result<Option<String>> {
        Ok(self.read()?.tokens.get(host).cloned())
    }
    fn set(&self, host: &str, token: &str) -> Result<()> {
        let mut f = self.read()?;
        f.tokens.insert(host.to_string(), token.to_string());
        self.write(&f)
    }
    fn delete(&self, host: &str) -> Result<()> {
        let mut f = self.read()?;
        if f.tokens.remove(host).is_some() {
            self.write(&f)?;
        }
        Ok(())
    }
    fn describe(&self) -> String {
        format!("file {}", self.path.display())
    }
}

const KEYRING_SERVICE: &str = "haven";

/// The OS keyring, with the file store behind it for when the keyring is unavailable.
pub struct KeyringStore {
    fallback: FileStore,
}

impl KeyringStore {
    pub fn new(dir: &Path) -> KeyringStore {
        KeyringStore { fallback: FileStore::new(dir) }
    }

    fn entry(host: &str) -> keyring::Result<keyring::Entry> {
        keyring::Entry::new(KEYRING_SERVICE, host)
    }
}

impl TokenStore for KeyringStore {
    fn get(&self, host: &str) -> Result<Option<String>> {
        if let Ok(e) = Self::entry(host) {
            match e.get_password() {
                Ok(p) => return Ok(Some(p)),
                Err(keyring::Error::NoEntry) => {}
                Err(_) => {}
            }
        }
        self.fallback.get(host)
    }

    fn set(&self, host: &str, token: &str) -> Result<()> {
        if let Ok(e) = Self::entry(host)
            && e.set_password(token).is_ok()
        {
            // Read it back: some headless backends accept a write they cannot keep.
            if matches!(e.get_password(), Ok(p) if p == token) {
                let _ = self.fallback.delete(host);
                return Ok(());
            }
        }
        self.fallback.set(host, token)
    }

    fn delete(&self, host: &str) -> Result<()> {
        if let Ok(e) = Self::entry(host) {
            let _ = e.delete_credential();
        }
        self.fallback.delete(host)
    }

    fn describe(&self) -> String {
        "the system keyring (or tokens.toml when there is none)".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_are_normalized() {
        assert_eq!(normalize_host("code.example.com").unwrap(), "https://code.example.com");
        assert_eq!(normalize_host("https://code.example.com/").unwrap(), "https://code.example.com");
        assert_eq!(normalize_host("localhost:3200").unwrap(), "http://localhost:3200");
        assert_eq!(normalize_host("http://127.0.0.1:3200/path?x=1").unwrap(), "http://127.0.0.1:3200");
        assert!(normalize_host("").is_err());
        assert!(normalize_host("ftp://x").is_err());
    }

    #[test]
    fn host_port_for_git() {
        assert_eq!(host_port("https://code.example.com").as_deref(), Some("code.example.com"));
        assert_eq!(host_port("http://localhost:3200").as_deref(), Some("localhost:3200"));
    }

    #[test]
    fn file_store_round_trips_and_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let s = FileStore::new(dir.path());
        assert_eq!(s.get("https://a").unwrap(), None);
        s.set("https://a", "tok1").unwrap();
        s.set("https://b", "tok2").unwrap();
        assert_eq!(s.get("https://a").unwrap().as_deref(), Some("tok1"));
        s.delete("https://a").unwrap();
        assert_eq!(s.get("https://a").unwrap(), None);
        assert_eq!(s.get("https://b").unwrap().as_deref(), Some("tok2"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("tokens.toml")).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn hosts_file_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Hosts::load(dir.path()).unwrap(), Hosts::default());
        let mut h = Hosts { default: Some("https://x".into()), ..Default::default() };
        h.hosts.insert("https://x".into(), HostEntry { user: Some("alice".into()), git_protocol: None });
        h.save(dir.path()).unwrap();
        assert_eq!(Hosts::load(dir.path()).unwrap(), h);
    }
}
