//! Client ID Metadata Documents: an MCP client names itself with an https URL, and the document
//! at that URL says what it is and where it may be sent back. Nothing signed out can create a
//! client row. A client id is only ever fetched for a signed-in person with two-factor on, from a
//! host the operator trusts, and only from public addresses.

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// What a client id document says that this server uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientDoc {
    pub client_id: String,
    pub client_name: String,
    pub redirect_uris: Vec<String>,
}

/// A fetched document and how long it may be remembered.
#[derive(Debug, Clone)]
pub struct Fetched {
    pub doc: ClientDoc,
    pub max_age: Option<Duration>,
}

/// Fetches and validates a client id document. A failure is a reason in plain words for the
/// person looking at the error page. Tests inject their own.
#[async_trait]
pub trait ClientDocFetcher: Send + Sync {
    async fn fetch(&self, client_id: &str) -> Result<Fetched, String>;
}

pub const MAX_DOC_BYTES: usize = 32 * 1024;
const MAX_REDIRECT_URIS: usize = 10;

/// Checks the shape of a client id and that its host is trusted. Returns the parsed URL.
pub fn check_client_id(raw: &str, trusted_hosts: &[String]) -> Result<url::Url, String> {
    let bad = |m: &str| Err(m.to_string());
    if raw.is_empty() || raw.len() > 512 {
        return bad("The client id is missing or too long.");
    }
    // Plain ASCII with no escapes, so no spelling of a dot segment or a second host survives.
    if !raw.bytes().all(|b| (0x21..0x7f).contains(&b))
        || raw.contains('%')
        || raw.contains('\\')
        || raw.contains('#')
        || raw.contains('?')
        || raw.contains('@')
    {
        return bad("The client id is not a plain https URL.");
    }
    // `url` drops a default port, so an explicit `:443` would look like none. It is refused as written.
    if raw.strip_prefix("https://").and_then(|r| r.split('/').next()).is_some_and(|authority| authority.contains(':')) {
        return bad("The client id must not name a port.");
    }
    let u = url::Url::parse(raw).map_err(|_| "The client id is not a valid URL.".to_string())?;
    if u.scheme() != "https" {
        return bad("The client id must be an https URL.");
    }
    if u.port().is_some() {
        return bad("The client id must use port 443.");
    }
    if !u.username().is_empty() || u.password().is_some() || u.query().is_some() || u.fragment().is_some() {
        return bad("The client id must not carry credentials, a query or a fragment.");
    }
    let path = u.path();
    if path.len() < 2 || path.ends_with('/') && path.len() == 1 {
        return bad("The client id needs a path.");
    }
    if path.split('/').any(|s| s == "." || s == "..") || raw.contains("/./") || raw.contains("/../") {
        return bad("The client id must not contain dot segments.");
    }
    let host = match u.host() {
        Some(url::Host::Domain(d)) => d.to_ascii_lowercase(),
        _ => return bad("The client id must name a host, not an address."),
    };
    if !trusted_hosts.contains(&host) {
        return bad("This server does not trust that host to describe an app. An operator can allow it in mcp.trusted_client_hosts.");
    }
    Ok(u)
}

/// Checks a fetched JSON document and extracts what is used. `client_id` must equal the URL
/// the document was fetched from exactly; the method must be `none`; there is no secret.
pub fn parse_doc(client_id: &str, body: &[u8]) -> Result<ClientDoc, String> {
    let v: serde_json::Value = serde_json::from_slice(body).map_err(|_| "The client document is not valid JSON.".to_string())?;
    let obj = v.as_object().ok_or("The client document is not a JSON object.")?;
    if obj.get("client_id").and_then(|x| x.as_str()) != Some(client_id) {
        return Err("The client document names a different client id.".into());
    }
    if obj.get("token_endpoint_auth_method").and_then(|x| x.as_str()) != Some("none") {
        return Err("The client document must use token_endpoint_auth_method none.".into());
    }
    if obj.contains_key("client_secret")
        || obj.contains_key("client_secret_expires_at")
        || obj.contains_key("jwks")
        || obj.contains_key("jwks_uri")
    {
        return Err("The client document must not carry a secret or keys.".into());
    }
    let uris = obj.get("redirect_uris").and_then(|x| x.as_array()).ok_or("The client document lists no redirect URIs.")?;
    if uris.is_empty() || uris.len() > MAX_REDIRECT_URIS {
        return Err("The client document must list between 1 and 10 redirect URIs.".into());
    }
    let mut redirect_uris = Vec::new();
    for u in uris {
        let s = u.as_str().ok_or("A redirect URI is not a string.")?;
        if s.len() > 512 {
            return Err("A redirect URI is too long.".into());
        }
        redirect_uris.push(s.to_string());
    }
    let name: String =
        obj.get("client_name").and_then(|x| x.as_str()).unwrap_or("").chars().filter(|c| !c.is_control()).take(100).collect();
    Ok(ClientDoc { client_id: client_id.to_string(), client_name: name.trim().to_string(), redirect_uris })
}

fn is_loopback_host(h: &str) -> bool {
    h == "localhost" || h == "127.0.0.1"
}

/// Whether `presented` may be used with a client whose document lists `registered`.
///
/// Exact string match. The one exception is `http` to `localhost` or `127.0.0.1`, where a
/// program on the same computer picks a free port: the port is ignored and nothing else is.
/// The two names are not interchangeable and `[::1]` is not accepted. A non-loopback redirect
/// must be https and on the same origin as the client id.
pub fn redirect_allowed(client_id: &url::Url, registered: &[String], presented: &str) -> bool {
    if presented.is_empty() || presented.len() > 512 || presented.bytes().any(|b| b < 0x21 || b == 0x7f || b == b'\\') {
        return false;
    }
    let Ok(p) = url::Url::parse(presented) else { return false };
    if !p.username().is_empty() || p.password().is_some() || p.fragment().is_some() || presented.contains('#') {
        return false;
    }
    if p.scheme() == "http" {
        // Only the two literal spellings. `url` would also turn `2130706433` or `127.1` into
        // 127.0.0.1, which is the same place but not what the app registered.
        if !(presented.starts_with("http://localhost") || presented.starts_with("http://127.0.0.1")) {
            return false;
        }
        let ph = match p.host() {
            Some(url::Host::Domain(d)) => d.to_ascii_lowercase(),
            Some(url::Host::Ipv4(a)) => a.to_string(),
            _ => return false,
        };
        if !is_loopback_host(&ph) {
            return false;
        }
        return registered.iter().any(|r| loopback_match(r, &p, &ph));
    }
    if p.scheme() != "https" {
        return false;
    }
    if p.origin() != client_id.origin() {
        return false;
    }
    registered.iter().any(|r| r == presented)
}

fn loopback_match(registered: &str, presented: &url::Url, presented_host: &str) -> bool {
    let Ok(r) = url::Url::parse(registered) else { return false };
    if r.scheme() != "http" {
        return false;
    }
    let rh = match r.host() {
        Some(url::Host::Domain(d)) => d.to_ascii_lowercase(),
        Some(url::Host::Ipv4(a)) => a.to_string(),
        _ => return false,
    };
    rh == presented_host
        && r.path() == presented.path()
        && r.query() == presented.query()
        && r.username().is_empty()
        && r.fragment().is_none()
}

/// True for an `http` redirect to this computer.
pub fn is_loopback_redirect(presented: &str) -> bool {
    url::Url::parse(presented).is_ok_and(|p| {
        p.scheme() == "http"
            && match p.host() {
                Some(url::Host::Domain(d)) => is_loopback_host(&d.to_ascii_lowercase()),
                Some(url::Host::Ipv4(a)) => a.to_string() == "127.0.0.1",
                _ => false,
            }
    })
}

/// `host` or `host:port` of a redirect, for display.
pub fn redirect_host(presented: &str) -> String {
    url::Url::parse(presented)
        .ok()
        .map(|u| match (u.host_str(), u.port()) {
            (Some(h), Some(p)) => format!("{h}:{p}"),
            (Some(h), None) => h.to_string(),
            _ => String::new(),
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------- the real fetcher

/// Fetches over https from public addresses only. The connection is pinned to an address that
/// was checked, so a DNS answer that changes in between gets no second chance. No redirects, no
/// proxy, 5 seconds, 32 KiB.
pub struct HttpFetcher;

#[async_trait]
impl ClientDocFetcher for HttpFetcher {
    async fn fetch(&self, client_id: &str) -> Result<Fetched, String> {
        let url = url::Url::parse(client_id).map_err(|_| "bad client id".to_string())?;
        let host = url.host_str().ok_or("bad client id")?.to_string();
        let work = async {
            let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host.as_str(), 443))
                .await
                .map_err(|_| "The client's host did not resolve.".to_string())?
                .collect();
            if addrs.is_empty() {
                return Err("The client's host did not resolve.".to_string());
            }
            // Every answer must be public, so a name that also points inside the network is refused.
            if addrs.iter().any(|a| !ch_core::net::is_public_ip(a.ip())) {
                return Err("The client's host does not resolve to a public address.".to_string());
            }
            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .timeout(Duration::from_secs(5))
                .user_agent("Codehaven")
                .resolve(&host, addrs[0])
                .build()
                .map_err(|_| "Could not start the request.".to_string())?;
            let mut resp = client
                .get(url.clone())
                .header("accept", "application/json")
                .send()
                .await
                .map_err(|_| "The client document could not be fetched.".to_string())?;
            if resp.status() != reqwest::StatusCode::OK {
                return Err("The client document could not be fetched.".to_string());
            }
            let ctype = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
            if !(ctype.starts_with("application/json") || ctype.contains("+json")) {
                return Err("The client document is not JSON.".to_string());
            }
            let max_age = resp
                .headers()
                .get("cache-control")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.split(',').find_map(|p| p.trim().strip_prefix("max-age=").and_then(|n| n.parse::<u64>().ok())))
                .map(Duration::from_secs);
            let mut body: Vec<u8> = Vec::new();
            while let Some(chunk) = resp.chunk().await.map_err(|_| "The client document could not be read.".to_string())? {
                body.extend_from_slice(&chunk);
                if body.len() > MAX_DOC_BYTES {
                    return Err("The client document is too large.".to_string());
                }
            }
            let doc = parse_doc(client_id, &body)?;
            Ok(Fetched { doc, max_age })
        };
        match tokio::time::timeout(Duration::from_secs(6), work).await {
            Ok(r) => r,
            Err(_) => Err("The client document took too long.".to_string()),
        }
    }
}

// ---------------------------------------------------------------- cache

/// Good documents for 5 minutes to an hour, at most 100 of them. Errors are never cached.
pub struct DocCache {
    map: Mutex<HashMap<String, (Instant, Instant, ClientDoc)>>,
}

impl Default for DocCache {
    fn default() -> Self {
        Self::new()
    }
}

impl DocCache {
    pub fn new() -> Self {
        Self { map: Mutex::new(HashMap::new()) }
    }
    pub fn get(&self, client_id: &str) -> Option<ClientDoc> {
        let mut m = self.map.lock().unwrap_or_else(|e| e.into_inner());
        match m.get(client_id) {
            Some((_, until, doc)) if *until > Instant::now() => Some(doc.clone()),
            Some(_) => {
                m.remove(client_id);
                None
            }
            None => None,
        }
    }
    pub fn put(&self, fetched: &Fetched) {
        let ttl = fetched.max_age.unwrap_or(Duration::from_secs(300)).clamp(Duration::from_secs(300), Duration::from_secs(3600));
        let now = Instant::now();
        let mut m = self.map.lock().unwrap_or_else(|e| e.into_inner());
        if m.len() >= 100 {
            m.retain(|_, (_, until, _)| *until > now);
            if m.len() >= 100
                && let Some(oldest) = m.iter().min_by_key(|(_, (at, ..))| *at).map(|(k, _)| k.clone())
            {
                m.remove(&oldest);
            }
        }
        m.insert(fetched.doc.client_id.clone(), (now, now + ttl, fetched.doc.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trusted() -> Vec<String> {
        vec!["claude.ai".into()]
    }
    const CLAUDE: &str = "https://claude.ai/oauth/mcp-oauth-client-metadata";
    const CODE: &str = "https://claude.ai/oauth/claude-code-client-metadata";

    #[test]
    fn client_id_shape() {
        assert!(check_client_id(CLAUDE, &trusted()).is_ok());
        assert!(check_client_id(CODE, &trusted()).is_ok());
        for bad in [
            "http://claude.ai/oauth/x",
            "https://claude.ai",
            "https://claude.ai/",
            "https://claude.ai:8443/oauth/x",
            "https://claude.ai:443/oauth/x",
            "https://user\u{40}claude.ai/oauth/x",
            "https://claude.ai/oauth/x?a=1",
            "https://claude.ai/oauth/x#f",
            "https://claude.ai/oauth/../x",
            "https://claude.ai/oauth/./x",
            "https://claude.ai/oauth/%2e%2e/x",
            "https://evil.example/oauth/x",
            "https://claude.ai.evil.example/oauth/x",
            "https://sub.claude.ai/oauth/x",
            "https://127.0.0.1/oauth/x",
            "https://[::1]/oauth/x",
            "https://claude.ai\\\u{40}evil.example/x",
            "",
        ] {
            assert!(check_client_id(bad, &trusted()).is_err(), "{bad}");
        }
        assert!(check_client_id("https://other.example/c", &["other.example".into()]).is_ok());
        assert!(check_client_id(&format!("https://claude.ai/{}", "a".repeat(600)), &trusted()).is_err());
    }

    #[test]
    fn real_documents_parse() {
        let a = br#"{"client_id":"https://claude.ai/oauth/mcp-oauth-client-metadata","client_name":"Claude","client_uri":"https://claude.ai","redirect_uris":["https://claude.ai/api/mcp/auth_callback"],"grant_types":["authorization_code","refresh_token","urn:ietf:params:oauth:grant-type:jwt-bearer"],"response_types":["code"],"token_endpoint_auth_method":"none"}"#;
        let d = parse_doc(CLAUDE, a).unwrap();
        assert_eq!(d.client_name, "Claude");
        assert_eq!(d.redirect_uris, vec!["https://claude.ai/api/mcp/auth_callback".to_string()]);
    }

    #[test]
    fn document_checks() {
        let ok = |extra: &str| {
            format!(
                r#"{{"client_id":"{CLAUDE}","client_name":"X","redirect_uris":["https://claude.ai/cb"],"token_endpoint_auth_method":"none"{extra}}}"#
            )
        };
        assert!(parse_doc(CLAUDE, ok("").as_bytes()).is_ok());
        assert!(parse_doc(CLAUDE, ok(r#","client_secret":"s""#).as_bytes()).is_err(), "secret");
        assert!(parse_doc(CLAUDE, ok(r#","jwks_uri":"https://x""#).as_bytes()).is_err(), "keys");
        assert!(parse_doc(CODE, ok("").as_bytes()).is_err(), "id mismatch");
        let basic = ok("").replace("\"none\"", "\"client_secret_basic\"");
        assert!(parse_doc(CLAUDE, basic.as_bytes()).is_err(), "method");
        let none = format!(r#"{{"client_id":"{CLAUDE}","redirect_uris":["https://claude.ai/cb"]}}"#);
        assert!(parse_doc(CLAUDE, none.as_bytes()).is_err(), "method missing");
        let many = format!(
            r#"{{"client_id":"{CLAUDE}","token_endpoint_auth_method":"none","redirect_uris":[{}]}}"#,
            (0..11).map(|i| format!("\"https://claude.ai/{i}\"")).collect::<Vec<_>>().join(",")
        );
        assert!(parse_doc(CLAUDE, many.as_bytes()).is_err(), "eleven uris");
        assert!(parse_doc(CLAUDE, b"[]").is_err());
        assert!(parse_doc(CLAUDE, b"not json").is_err());
        let nm = format!(
            r#"{{"client_id":"{CLAUDE}","token_endpoint_auth_method":"none","redirect_uris":["https://claude.ai/cb"],"client_name":"A\u0007B<script>"}}"#
        );
        assert_eq!(
            parse_doc(CLAUDE, nm.as_bytes()).unwrap().client_name,
            "AB<script>",
            "control characters are dropped, markup is left for escaping"
        );
    }

    #[test]
    fn redirect_table() {
        let id = url::Url::parse(CLAUDE).unwrap();
        let cc = url::Url::parse(CODE).unwrap();
        let web = vec!["https://claude.ai/api/mcp/auth_callback".to_string()];
        let loop_ = vec!["http://localhost/callback".to_string(), "http://127.0.0.1/callback".to_string()];
        // exact
        assert!(redirect_allowed(&id, &web, "https://claude.ai/api/mcp/auth_callback"));
        // port only, on loopback
        assert!(redirect_allowed(&cc, &loop_, "http://localhost:54321/callback"));
        assert!(redirect_allowed(&cc, &loop_, "http://127.0.0.1:8080/callback"));
        assert!(redirect_allowed(&cc, &loop_, "http://localhost/callback"));
        // host swap: 127.0.0.1 is registered as 127.0.0.1 only
        assert!(!redirect_allowed(&cc, &["http://localhost/callback".to_string()], "http://127.0.0.1:1/callback"));
        assert!(!redirect_allowed(&cc, &["http://127.0.0.1/callback".to_string()], "http://localhost:1/callback"));
        // path, query, userinfo, ipv6, subdomain, encoding
        for bad in [
            "http://localhost:1/other",
            "http://localhost:1/callback/",
            "http://localhost:1/callback?x=1",
            "http://user\u{40}localhost:1/callback",
            "http://[::1]:1/callback",
            "http://localhost.evil.example/callback",
            "http://evil.localhost:1/callback",
            "http://localhost:1/%63allback",
            "http://localhost:1/callback#f",
            "https://localhost:1/callback",
            "http://0.0.0.0:1/callback",
            "http://127.0.0.2:1/callback",
            "http://2130706433:1/callback",
            "http://localhost:99999/callback",
            "",
        ] {
            assert!(!redirect_allowed(&cc, &loop_, bad), "{bad}");
        }
        for bad in [
            "https://claude.ai/api/mcp/auth_callback/",
            "https://claude.ai/api/mcp/auth_callback?x=1",
            "https://claude.ai:8443/api/mcp/auth_callback",
            "https://sub.claude.ai/api/mcp/auth_callback",
            "http://claude.ai/api/mcp/auth_callback",
            "https://claude.ai/api/mcp/auth_callback#x",
            "https://evil.example/api/mcp/auth_callback",
            "https://claude.ai\u{40}evil.example/api/mcp/auth_callback",
            "https://claude.ai/api/mcp/auth_callbac%6b",
        ] {
            assert!(!redirect_allowed(&id, &web, bad), "{bad}");
        }
        // a document that lists a redirect on another origin does not widen anything
        assert!(!redirect_allowed(&id, &["https://evil.example/cb".to_string()], "https://evil.example/cb"));
        // loopback never matches a web client's list
        assert!(!redirect_allowed(&id, &web, "http://localhost:1/api/mcp/auth_callback"));
    }

    #[test]
    fn loopback_and_host_display() {
        assert!(is_loopback_redirect("http://localhost:5/callback"));
        assert!(is_loopback_redirect("http://127.0.0.1:5/callback"));
        assert!(!is_loopback_redirect("https://claude.ai/api/mcp/auth_callback"));
        assert_eq!(redirect_host("http://localhost:5/callback"), "localhost:5");
        assert_eq!(redirect_host("https://claude.ai/api/mcp/auth_callback"), "claude.ai");
    }

    #[test]
    fn cache_keeps_good_documents_and_evicts() {
        let c = DocCache::new();
        let f = Fetched {
            doc: ClientDoc { client_id: "https://claude.ai/a".into(), client_name: "n".into(), redirect_uris: vec![] },
            max_age: None,
        };
        assert!(c.get("https://claude.ai/a").is_none());
        c.put(&f);
        assert!(c.get("https://claude.ai/a").is_some());
        for i in 0..150 {
            c.put(&Fetched {
                doc: ClientDoc { client_id: format!("https://claude.ai/{i}"), client_name: String::new(), redirect_uris: vec![] },
                max_age: None,
            });
        }
        assert!(c.map.lock().unwrap().len() <= 100);
    }
}
