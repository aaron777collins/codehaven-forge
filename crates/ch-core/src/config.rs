//! Configuration: `codehaven.toml`, overridden by `CODEHAVEN__SECTION__KEY` environment
//! variables. Secrets come from files (`*_file` keys), never from the TOML itself.

use figment::Figment;
use figment::providers::{Env, Format, Serialized, Toml};
use ipnet::IpNet;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub http: HttpConfig,
    pub database: DatabaseConfig,
    pub storage: StorageConfig,
    pub ssh: SshConfig,
    pub invites: InvitesConfig,
    pub mail: MailConfig,
    pub webhooks: WebhooksConfig,
    pub imports: ImportsConfig,
    pub ci: CiConfig,
    pub mcp: McpConfig,
    pub public_access: PublicAccessConfig,
    pub workspace: WorkspaceConfig,
    pub pages: PagesConfig,
    pub log: LogConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Name shown on the sign-in page.
    pub name: String,
    /// Public listener. Everything behind the gate.
    pub bind: String,
    /// Internal listener for `/readyz` and `/metrics`. Must stay on loopback.
    pub internal_bind: String,
    /// The one origin people use, for links, clone URLs, CSRF origin checks and HSTS. Required:
    /// there is no default.
    pub external_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HttpConfig {
    /// Peers allowed to set `X-Forwarded-For` and `X-Forwarded-Proto`. From any other
    /// peer those headers are ignored and the socket address is used.
    pub trusted_proxies: Vec<IpNet>,
    /// Host names whose addresses are trusted the same way, looked up again every 30 seconds:
    /// the reverse proxy's container name, so only that container is believed, not every
    /// neighbour on a shared Docker network (ch-dc1).
    pub trusted_proxy_hosts: Vec<String>,
    pub request_timeout_secs: u64,
    /// A release file upload may take as long as it needs, but the sender may stay silent this
    /// long at most between two chunks of the body.
    pub upload_idle_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DatabaseConfig {
    pub url: Option<String>,
    /// File holding the URL, so the password stays out of the TOML and the environment.
    pub url_file: Option<PathBuf>,
    pub max_connections: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    /// Holds `git/`, `blobs/`, `logs/`, `secrets/`.
    pub data_dir: PathBuf,
    /// Free space, in GiB, that uploads and pushes always leave on the data file system: an
    /// artifact, cache, release file, image layer or push that would go below it is refused, so
    /// the database and everything else on the disk keep room to work (ch-ctk.2).
    pub min_free_gib: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SshConfig {
    pub bind: String,
    /// What the UI shows people, for example `127.0.0.1:2222` when ssh is reached through a tunnel.
    pub external: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct InvitesConfig {
    pub org_owners_can_invite: bool,
    pub default_expiry_hours: u32,
    pub min_expiry_hours: u32,
    pub max_expiry_hours: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MailConfig {
    pub smtp: Option<SmtpConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password_file: Option<PathBuf>,
    pub from: String,
    /// `starttls` (the default: required, never falls back to plain text), `tls` (implicit TLS,
    /// as on port 465) or `none` (plain text, accepted only for a relay on localhost).
    #[serde(default)]
    pub tls: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct WebhooksConfig {
    /// Allow deliveries to private and loopback addresses. Off by default.
    pub allow_private: bool,
    /// Addresses webhooks may never reach although they are public: the host's own public
    /// addresses, where services the firewall hides from the internet still listen.
    pub blocked_ips: Vec<std::net::IpAddr>,
}

/// Importing a repository from a URL (ch-ejm). The server fetches it, so like a webhook it must
/// not reach the inside of the network.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ImportsConfig {
    /// Allow imports from private and loopback addresses, over http too. Off by default; for an
    /// instance that imports from its own network, and for tests.
    pub allow_private: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CiConfig {
    pub log_retention_days: u32,
    pub registration_token_ttl_minutes: u32,
    /// A queued job that no runner takes within this many minutes fails as timed out.
    pub queue_timeout_minutes: u32,
    /// A queued job whose labels no registered runner has fails after this many minutes.
    pub no_runner_minutes: u32,
}

/// The Claude connector: an MCP endpoint at `/mcp` with OAuth sign-in. Off unless turned on.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct McpConfig {
    pub enabled: bool,
    /// Hosts whose client id documents this instance will fetch. `claude.ai` is where Claude's
    /// apps publish theirs. Add a host to allow another MCP client.
    pub trusted_client_hosts: Vec<String>,
    /// Origins allowed to send a browser `Origin` header to `/mcp`, besides this instance's own.
    /// A request with no `Origin` header (the usual case for a server-side client) always passes.
    pub allowed_origins: Vec<String>,
}

/// Limits for signed-out visitors to public repositories (epic ch-4pn). Starting values, to be
/// measured on the server. "Per address" counts an IPv6 address by its /64.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PublicAccessConfig {
    /// Any signed-out request, rejections included, per address.
    pub requests_per_minute: u32,
    pub burst: u32,
    /// Public pages and API answers for the whole instance.
    pub instance_requests_per_minute: u32,
    /// Name lookups the gate runs at once. When full, the request gets the ordinary rejection.
    pub lookups_at_once: u32,
    /// Blame, compare, diffs and recursive trees.
    pub heavy_per_address: u32,
    pub heavy_instance: u32,
    /// Clones and fetches.
    pub git_per_address: u32,
    pub git_instance: u32,
    pub git_per_address_per_minute: u32,
    pub git_timeout_secs: u64,
    /// Zip and tar archives.
    pub archive_per_address: u32,
    pub archive_instance: u32,
    pub archive_per_address_per_minute: u32,
    pub archive_timeout_secs: u64,
    /// Raw files, blobs and release files.
    pub raw_per_address: u32,
    pub raw_instance: u32,
}

/// The browser workspace: the code editor at `/{owner}/{repo}/workspace`. These are the
/// ceiling. A site admin can turn each part off at `/admin/workspace`, never on past this.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceConfig {
    /// The editor, commits from the browser and the run panel.
    pub enabled: bool,
    /// The terminal that runs in the visitor's browser, in a sandboxed frame. Never on the server.
    pub terminal: bool,
    /// Where an optional Linux image for the terminal is installed. Empty means none.
    pub linux_image_dir: String,
}

/// Codehaven Pages (`docs/design/pages.md`). An empty `host` turns Pages off.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PagesConfig {
    /// The host sites are served from, for example `pages.example.com`. Never the app's host
    /// or a parent of it. Empty: no Pages.
    pub host: String,
    /// Files in one published copy.
    pub max_files: u32,
    /// Bytes in one published copy.
    pub max_bytes: u64,
    /// A reply whose body makes no progress for this long is ended and gives its place back.
    pub reply_stall_secs: u64,
    /// No reply's body takes longer than this.
    pub reply_deadline_secs: u64,
    /// Replies one client address may have open at once.
    pub replies_per_address: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LogConfig {
    /// `json` or `pretty`.
    pub format: String,
    /// An `EnvFilter` directive.
    pub level: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            name: "Codehaven".into(),
            bind: "0.0.0.0:3200".into(),
            internal_bind: "127.0.0.1:9300".into(),
            // No default: the address people open is the one thing only the operator knows.
            external_url: String::new(),
        }
    }
}
impl Default for HttpConfig {
    fn default() -> Self {
        Self { trusted_proxies: Vec::new(), trusted_proxy_hosts: Vec::new(), request_timeout_secs: 60, upload_idle_secs: 60 }
    }
}
impl Default for DatabaseConfig {
    fn default() -> Self {
        Self { url: None, url_file: None, max_connections: 20 }
    }
}
impl Default for StorageConfig {
    fn default() -> Self {
        Self { data_dir: PathBuf::from("/data"), min_free_gib: 10 }
    }
}
impl Default for SshConfig {
    fn default() -> Self {
        Self { bind: "0.0.0.0:2222".into(), external: "127.0.0.1:2222".into(), enabled: true }
    }
}
impl SshConfig {
    /// True when `external` names this machine or a private network, so only someone with a
    /// tunnel can use it: a loopback, private, link-local or unspecified address, or `localhost`.
    pub fn tunnel_only(&self) -> bool {
        // An unbracketed IPv6 literal such as `::1` has colons of its own, so it would be
        // split apart below: try the whole string first.
        if let Ok(ip) = self.external.parse::<std::net::IpAddr>() {
            return !crate::net::is_internet_address(ip);
        }
        let host = match self.external.rsplit_once(':') {
            // A bracketed IPv6 address keeps its colons inside the brackets.
            Some((h, p)) if !h.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => h,
            _ => self.external.as_str(),
        };
        let host = host.trim_start_matches('[').trim_end_matches(']');
        let lower = host.to_ascii_lowercase();
        if lower == "localhost" || lower.ends_with(".localhost") {
            return true;
        }
        match host.parse::<std::net::IpAddr>() {
            Ok(ip) => !crate::net::is_internet_address(ip),
            Err(_) => false,
        }
    }
}
impl Default for InvitesConfig {
    fn default() -> Self {
        Self { org_owners_can_invite: false, default_expiry_hours: 24 * 7, min_expiry_hours: 1, max_expiry_hours: 24 * 30 }
    }
}
impl Default for CiConfig {
    fn default() -> Self {
        Self { log_retention_days: 90, registration_token_ttl_minutes: 60, queue_timeout_minutes: 24 * 60, no_runner_minutes: 15 }
    }
}
impl Default for McpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            trusted_client_hosts: vec!["claude.ai".into()],
            allowed_origins: vec!["https://claude.ai".into(), "https://claude.com".into()],
        }
    }
}
impl Default for PublicAccessConfig {
    fn default() -> Self {
        Self {
            requests_per_minute: 240,
            burst: 120,
            instance_requests_per_minute: 600,
            lookups_at_once: 8,
            heavy_per_address: 2,
            heavy_instance: 4,
            git_per_address: 2,
            git_instance: 3,
            git_per_address_per_minute: 30,
            git_timeout_secs: 600,
            archive_per_address: 1,
            archive_instance: 2,
            archive_per_address_per_minute: 6,
            archive_timeout_secs: 120,
            raw_per_address: 4,
            raw_instance: 10,
        }
    }
}
impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self { enabled: true, terminal: true, linux_image_dir: String::new() }
    }
}
impl Default for PagesConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            max_files: 2000,
            max_bytes: 50 * 1024 * 1024,
            reply_stall_secs: 60,
            reply_deadline_secs: 600,
            replies_per_address: 32,
        }
    }
}
impl PagesConfig {
    /// The pages host, lowercase, when Pages is on.
    pub fn host(&self) -> Option<String> {
        let h = self.host.trim().to_ascii_lowercase();
        (!h.is_empty()).then_some(h)
    }
}
impl Default for LogConfig {
    fn default() -> Self {
        Self { format: "json".into(), level: "info".into() }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not read configuration: {0}")]
    Load(String),
    #[error("invalid configuration: {0}")]
    Invalid(String),
}

impl PublicAccessConfig {
    /// Refuses a zero anywhere, and an instance cap below its per-address cap.
    pub fn validate(&self) -> Result<(), ConfigError> {
        fn positive(name: &str, v: u64) -> Result<(), ConfigError> {
            if v == 0 {
                return Err(ConfigError::Invalid(format!("public_access.{name} must be more than 0")));
            }
            Ok(())
        }
        positive("requests_per_minute", self.requests_per_minute.into())?;
        positive("burst", self.burst.into())?;
        positive("instance_requests_per_minute", self.instance_requests_per_minute.into())?;
        positive("lookups_at_once", self.lookups_at_once.into())?;
        positive("heavy_per_address", self.heavy_per_address.into())?;
        positive("heavy_instance", self.heavy_instance.into())?;
        positive("git_per_address", self.git_per_address.into())?;
        positive("git_instance", self.git_instance.into())?;
        positive("git_per_address_per_minute", self.git_per_address_per_minute.into())?;
        positive("git_timeout_secs", self.git_timeout_secs)?;
        positive("archive_per_address", self.archive_per_address.into())?;
        positive("archive_instance", self.archive_instance.into())?;
        positive("archive_per_address_per_minute", self.archive_per_address_per_minute.into())?;
        positive("archive_timeout_secs", self.archive_timeout_secs)?;
        positive("raw_per_address", self.raw_per_address.into())?;
        positive("raw_instance", self.raw_instance.into())?;

        fn at_least(instance: (&str, u32), per_address: (&str, u32)) -> Result<(), ConfigError> {
            if instance.1 < per_address.1 {
                return Err(ConfigError::Invalid(format!("public_access.{} must be at least public_access.{}", instance.0, per_address.0)));
            }
            Ok(())
        }
        at_least(("heavy_instance", self.heavy_instance), ("heavy_per_address", self.heavy_per_address))?;
        at_least(("git_instance", self.git_instance), ("git_per_address", self.git_per_address))?;
        at_least(("archive_instance", self.archive_instance), ("archive_per_address", self.archive_per_address))?;
        at_least(("raw_instance", self.raw_instance), ("raw_per_address", self.raw_per_address))?;
        Ok(())
    }
}

impl Config {
    /// A configuration for tests: the defaults plus a loopback `external_url`, the one setting that
    /// has no default. Not for real use.
    #[doc(hidden)]
    pub fn for_tests() -> Config {
        let mut c = Config::default();
        c.server.external_url = "http://127.0.0.1:3200".into();
        // Test machines are not servers: a small floor still exercises the check.
        c.storage.min_free_gib = 1;
        c
    }

    /// Defaults, then the TOML file if it exists, then `CODEHAVEN__*` variables.
    pub fn load(file: Option<&Path>) -> Result<Config, ConfigError> {
        let mut fig = Figment::from(Serialized::defaults(Config::default()));
        if let Some(path) = file
            && path.exists()
        {
            fig = fig.merge(Toml::file(path));
        }
        fig = fig.merge(Env::prefixed("CODEHAVEN__").split("__"));
        let cfg: Config = fig.extract().map_err(|e| ConfigError::Load(e.to_string()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Checks that cannot be expressed in types. Fails loudly at startup instead of
    /// running with something unsafe.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let url = self.external_url_parts()?;
        if url.scheme != "http" && url.scheme != "https" {
            return Err(ConfigError::Invalid("server.external_url must start with http:// or https://".into()));
        }
        let internal: std::net::SocketAddr =
            self.server.internal_bind.parse().map_err(|_| ConfigError::Invalid("server.internal_bind must be an ip:port".into()))?;
        if !internal.ip().is_loopback() {
            return Err(ConfigError::Invalid("server.internal_bind must be a loopback address".into()));
        }
        let _: std::net::SocketAddr =
            self.server.bind.parse().map_err(|_| ConfigError::Invalid("server.bind must be an ip:port".into()))?;
        for h in &self.http.trusted_proxy_hosts {
            let plain = !h.is_empty()
                && h.len() <= 253
                && h.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.' || b == b'_');
            if !plain {
                return Err(ConfigError::Invalid(format!(
                    "http.trusted_proxy_hosts holds {h:?}: write plain lowercase host names such as caddy"
                )));
            }
        }
        for net in &self.http.trusted_proxies {
            if net.prefix_len() == 0 {
                return Err(ConfigError::Invalid(
                    "http.trusted_proxies must not contain 0.0.0.0/0 or ::/0: anyone could then fake their address".into(),
                ));
            }
        }
        if self.mcp.enabled {
            // Claude connects from the public internet and sends tokens, so the instance must be
            // reached over https. Plain http is for a developer's own machine only.
            let host = url.host.split(':').next().unwrap_or("");
            let loopback = matches!(host, "localhost" | "127.0.0.1" | "[::1]");
            if url.scheme != "https" && !(loopback && url.scheme == "http") {
                return Err(ConfigError::Invalid(
                    "mcp.enabled needs an https server.external_url (plain http is accepted only for localhost). Set server.external_url to the https address people open, or turn mcp.enabled off".into(),
                ));
            }
            for h in &self.mcp.trusted_client_hosts {
                if h.is_empty() || h.contains('/') || h.contains(':') || h.contains('*') || h != &h.to_ascii_lowercase() {
                    return Err(ConfigError::Invalid(format!(
                        "mcp.trusted_client_hosts holds {h:?}: write plain lowercase host names such as claude.ai"
                    )));
                }
            }
        }
        if self.invites.min_expiry_hours == 0 || self.invites.min_expiry_hours > self.invites.max_expiry_hours {
            return Err(ConfigError::Invalid("invites expiry bounds are inconsistent".into()));
        }
        if self.invites.default_expiry_hours < self.invites.min_expiry_hours
            || self.invites.default_expiry_hours > self.invites.max_expiry_hours
        {
            return Err(ConfigError::Invalid("invites.default_expiry_hours is outside the allowed range".into()));
        }
        self.public_access.validate()?;
        if let Some(h) = self.pages.host() {
            let app = url.host.split(':').next().unwrap_or("");
            if !crate::pages::valid_pages_host(&h, app) {
                return Err(ConfigError::Invalid(format!(
                    "pages.host is {h:?}: write a host name such as pages.example.com, with no port, that is neither the app's host nor a parent of it"
                )));
            }
            if self.pages.max_files == 0 || self.pages.max_bytes == 0 {
                return Err(ConfigError::Invalid("pages.max_files and pages.max_bytes must be more than 0".into()));
            }
            let p = &self.pages;
            if p.reply_stall_secs == 0 || p.reply_deadline_secs == 0 || p.replies_per_address == 0 {
                return Err(ConfigError::Invalid(
                    "pages.reply_stall_secs, pages.reply_deadline_secs and pages.replies_per_address must be more than 0".into(),
                ));
            }
        }
        Ok(())
    }

    pub fn external_url_parts(&self) -> Result<ExternalUrl, ConfigError> {
        let u = &self.server.external_url;
        if u.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "Set server.external_url (CODEHAVEN__SERVER__EXTERNAL_URL) to the URL people open, for example https://code.example.com"
                    .into(),
            ));
        }
        let (scheme, rest) = u.split_once("://").ok_or_else(|| ConfigError::Invalid("server.external_url needs a scheme".into()))?;
        if rest.is_empty() || rest.contains('/') {
            return Err(ConfigError::Invalid("server.external_url must be an origin with no path and no trailing slash".into()));
        }
        Ok(ExternalUrl { scheme: scheme.to_string(), host: rest.to_string() })
    }

    /// The one origin people use, with no trailing slash. Every URL the connector publishes comes
    /// from this and never from the `Host` header.
    pub fn origin(&self) -> &str {
        self.server.external_url.trim_end_matches('/')
    }

    /// The MCP resource identifier: the URL of the endpoint itself.
    pub fn mcp_resource(&self) -> String {
        format!("{}/mcp", self.origin())
    }

    /// True when the public origin is https, so cookies are `Secure` and HSTS is sent.
    pub fn is_https(&self) -> bool {
        self.server.external_url.starts_with("https://")
    }

    /// The database URL, read from `url_file` when set.
    pub fn database_url(&self) -> Result<String, ConfigError> {
        if let Some(f) = &self.database.url_file {
            let s = std::fs::read_to_string(f).map_err(|e| ConfigError::Load(format!("database.url_file {}: {e}", f.display())))?;
            return Ok(s.trim().to_string());
        }
        self.database.url.clone().ok_or_else(|| ConfigError::Invalid("set database.url_file or database.url".into()))
    }

    pub fn git_dir(&self) -> PathBuf {
        self.storage.data_dir.join("git")
    }
    pub fn blobs_dir(&self) -> PathBuf {
        self.storage.data_dir.join("blobs")
    }
    pub fn logs_dir(&self) -> PathBuf {
        self.storage.data_dir.join("logs")
    }
    pub fn secrets_dir(&self) -> PathBuf {
        self.storage.data_dir.join("secrets")
    }
    pub fn master_key_path(&self) -> PathBuf {
        self.secrets_dir().join("master.key")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalUrl {
    pub scheme: String,
    pub host: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_is_off_by_default_and_needs_https() {
        assert!(!Config::default().mcp.enabled);
        assert_eq!(Config::default().mcp.trusted_client_hosts, vec!["claude.ai".to_string()]);
        let mut c = Config::default();
        c.server.external_url = "http://code.example.com".into();
        c.mcp.enabled = true;
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("mcp.enabled") && err.contains("https"), "{err}");
        c.server.external_url = "https://code.example.com".into();
        c.validate().unwrap();
        assert_eq!(c.mcp_resource(), "https://code.example.com/mcp");
        c.server.external_url = "http://localhost:3200".into();
        c.validate().unwrap();
        c.server.external_url = "https://code.example.com".into();
        c.mcp.trusted_client_hosts = vec!["Claude.AI".into()];
        assert!(c.validate().is_err(), "host names are lowercase");
    }

    #[test]
    fn defaults_validate() {
        Config::for_tests().validate().unwrap();
    }

    #[test]
    fn a_missing_external_url_fails_with_the_setting_named() {
        let err = Config::default().validate().unwrap_err().to_string();
        assert!(err.contains("server.external_url"), "{err}");
        assert!(err.contains("CODEHAVEN__SERVER__EXTERNAL_URL"), "{err}");
        assert!(err.contains("https://code.example.com"), "{err}");
        let mut blank = Config::default();
        blank.server.external_url = "   ".into();
        assert!(blank.validate().is_err());
    }

    #[test]
    fn load_without_an_external_url_fails_clearly() {
        let dir = std::env::temp_dir().join(format!("ch-cfg-nourl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("codehaven.toml");
        std::fs::write(&f, "[server]\nname = \"Haven\"\n").unwrap();
        // The test runner may have CODEHAVEN__ variables set: only this one is under test.
        if std::env::var("CODEHAVEN__SERVER__EXTERNAL_URL").is_err() {
            let err = Config::load(Some(&f)).unwrap_err().to_string();
            assert!(err.contains("CODEHAVEN__SERVER__EXTERNAL_URL"), "{err}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_open_trusted_proxy() {
        let mut c = Config::for_tests();
        c.http.trusted_proxies = vec!["0.0.0.0/0".parse().unwrap()];
        assert!(c.validate().is_err());
    }

    #[test]
    fn internal_listener_must_be_loopback() {
        let mut c = Config::for_tests();
        c.server.internal_bind = "0.0.0.0:9300".into();
        assert!(c.validate().is_err());
        c.server.internal_bind = "127.0.0.1:9300".into();
        c.validate().unwrap();
    }

    #[test]
    fn external_url_must_be_an_origin() {
        let mut c = Config::default();
        c.server.external_url = "https://code.example.com".into();
        c.validate().unwrap();
        assert!(c.is_https());
        c.server.external_url = "https://code.example.com/".into();
        assert!(c.validate().is_err());
        c.server.external_url = "https://code.example.com/sub".into();
        assert!(c.validate().is_err());
        c.server.external_url = "ftp://x".into();
        assert!(c.validate().is_err());
        c.server.external_url = "nonsense".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn pages_host_is_off_by_default_and_never_the_app() {
        let mut c = Config::for_tests();
        assert_eq!(c.pages.host(), None);
        c.server.external_url = "https://code.example.com".into();
        c.pages.host = "pages.example.com".into();
        assert!(c.validate().is_ok());
        assert_eq!(c.pages.host().as_deref(), Some("pages.example.com"));
        c.pages.host = "code.example.com".into();
        assert!(c.validate().unwrap_err().to_string().contains("pages.host"));
        c.pages.host = "example.com".into();
        assert!(c.validate().is_err(), "a parent of the app's host would share its cookies");
        c.pages.host = "pages.example.com".into();
        c.pages.reply_stall_secs = 0;
        assert!(c.validate().unwrap_err().to_string().contains("pages.reply_stall_secs"), "a zero stall limit would cut every reply");
    }

    #[test]
    fn toml_then_env_override() {
        let dir = std::env::temp_dir().join(format!("ch-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("codehaven.toml");
        std::fs::write(
            &f,
            "[server]\nname = \"Haven\"\nexternal_url = \"https://x.example\"\n[http]\ntrusted_proxies = [\"10.0.0.0/8\"]\n",
        )
        .unwrap();
        let c = Config::load(Some(&f)).unwrap();
        assert_eq!(c.server.name, "Haven");
        assert!(c.is_https());
        assert_eq!(c.http.trusted_proxies.len(), 1);
        // Secrets file wins over inline URL.
        let pw = dir.join("dburl");
        std::fs::write(&pw, "postgres://u:p@h/db\n").unwrap();
        let mut c2 = c.clone();
        c2.database.url_file = Some(pw);
        assert_eq!(c2.database_url().unwrap(), "postgres://u:p@h/db");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_database_is_an_error() {
        assert!(Config::default().database_url().is_err());
    }

    #[test]
    fn public_access_defaults() {
        let c = PublicAccessConfig::default();
        assert_eq!(c.requests_per_minute, 240);
        assert_eq!(c.burst, 120);
        assert_eq!(c.instance_requests_per_minute, 600);
        assert_eq!(c.lookups_at_once, 8);
        assert_eq!(c.heavy_per_address, 2);
        assert_eq!(c.heavy_instance, 4);
        assert_eq!(c.git_per_address, 2);
        assert_eq!(c.git_instance, 3);
        assert_eq!(c.git_per_address_per_minute, 30);
        assert_eq!(c.git_timeout_secs, 600);
        assert_eq!(c.archive_per_address, 1);
        assert_eq!(c.archive_instance, 2);
        assert_eq!(c.archive_per_address_per_minute, 6);
        assert_eq!(c.archive_timeout_secs, 120);
        assert_eq!(c.raw_per_address, 4);
        assert_eq!(c.raw_instance, 10);
        assert!(c.validate().is_ok());
    }

    #[test]
    fn public_access_override_from_toml() {
        let dir = std::env::temp_dir().join(format!("ch-cfg-public-access-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("codehaven.toml");
        std::fs::write(&f, "[server]\nexternal_url = \"https://code.example.com\"\n[public_access]\ngit_instance = 5\n").unwrap();
        let c = Config::load(Some(&f)).unwrap();
        assert_eq!(c.public_access.git_instance, 5);
        assert_eq!(c.public_access.git_per_address, 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn public_access_refuses_zero() {
        let c = PublicAccessConfig { git_instance: 0, ..Default::default() };
        assert_eq!(c.validate().unwrap_err().to_string(), "invalid configuration: public_access.git_instance must be more than 0");
    }

    #[test]
    fn public_access_refuses_instance_below_address() {
        let c = PublicAccessConfig { archive_instance: 1, archive_per_address: 2, ..Default::default() };
        assert_eq!(
            c.validate().unwrap_err().to_string(),
            "invalid configuration: public_access.archive_instance must be at least public_access.archive_per_address"
        );
    }

    #[test]
    fn trusted_proxy_hosts_are_plain_names() {
        let mut c = Config::for_tests();
        c.http.trusted_proxy_hosts = vec!["caddy".into(), "proxy_1.internal".into()];
        assert!(c.validate().is_ok());
        for bad in ["", "Caddy", "caddy:80", "http://caddy", "a b"] {
            c.http.trusted_proxy_hosts = vec![bad.into()];
            assert!(c.validate().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn ssh_behind_a_tunnel_is_recognised() {
        let s = |e: &str| SshConfig { external: e.into(), ..SshConfig::default() };
        for e in ["127.0.0.1:2222", "localhost:22", "10.0.0.5:22", "192.168.0.1", "[::1]:2222", "::1", "0.0.0.0:2222"] {
            assert!(s(e).tunnel_only(), "{e}");
        }
        for e in ["code.example.com:22", "code.example.com", "8.8.8.8:2222", "[2001:4860:4860::8888]:22"] {
            assert!(!s(e).tunnel_only(), "{e}");
        }
    }
}
