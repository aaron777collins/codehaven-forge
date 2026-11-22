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
    pub ci: CiConfig,
    pub mcp: McpConfig,
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
    pub request_timeout_secs: u64,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CiConfig {
    pub log_retention_days: u32,
    pub registration_token_ttl_minutes: u32,
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
        Self { trusted_proxies: Vec::new(), request_timeout_secs: 60 }
    }
}
impl Default for DatabaseConfig {
    fn default() -> Self {
        Self { url: None, url_file: None, max_connections: 20 }
    }
}
impl Default for StorageConfig {
    fn default() -> Self {
        Self { data_dir: PathBuf::from("/data") }
    }
}
impl Default for SshConfig {
    fn default() -> Self {
        Self { bind: "0.0.0.0:2222".into(), external: "127.0.0.1:2222".into(), enabled: true }
    }
}
impl Default for InvitesConfig {
    fn default() -> Self {
        Self { org_owners_can_invite: false, default_expiry_hours: 24 * 7, min_expiry_hours: 1, max_expiry_hours: 24 * 30 }
    }
}
impl Default for CiConfig {
    fn default() -> Self {
        Self { log_retention_days: 90, registration_token_ttl_minutes: 60 }
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

impl Config {
    /// A configuration for tests: the defaults plus a loopback `external_url`, the one setting that
    /// has no default. Not for real use.
    #[doc(hidden)]
    pub fn for_tests() -> Config {
        let mut c = Config::default();
        c.server.external_url = "http://127.0.0.1:3200".into();
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
}
