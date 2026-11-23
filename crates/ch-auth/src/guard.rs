//! The request guard: CSRF, the `Origin` check, rate limits and the `X-RateLimit-*` headers
//! (ARCHITECTURE.md section 7.6).
//!
//! It runs inside the deny-by-default gate, so the caller is already resolved and sits in the
//! request extensions as an `Actor`. `crates/codehaven` wraps `Guard::handle` in a one line
//! middleware that passes the trusted client address.
//!
//! **CSRF.** Every unsafe request that does not carry an `Authorization` header (so it relies
//! on cookies) must pass two checks:
//! 1. `Origin`, or failing that `Referer`, equals `server.external_url`. A missing or `null`
//!    origin is refused.
//! 2. A token. It is the HMAC of a binding value under a key derived from the master key. The
//!    binding is the session cookie when there is one, and otherwise a random double-submit
//!    cookie (`__Host-ch_csrf`) that the guard sets on the first page a visitor loads, so the
//!    sign-in and invite forms work before any session exists. The token comes from the
//!    `X-CSRF-Token` header (htmx, `fetch`), the `_csrf` field of a urlencoded form, or the
//!    `_csrf` query parameter (multipart uploads).
//!
//! Pages read the token from the `CsrfToken` request extension.

use crate::ratelimit::{Limiter, Quota};
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use ch_core::access::{self, RouteKind};
use ch_core::identity::CsrfToken;
use ch_core::{Actor, Config};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::net::IpAddr;
use std::time::Duration;

/// The same cap as ch-web's form reader (FormData), so an edit the page allows is not refused here first.
const MAX_FORM_BYTES: usize = 4 * 1024 * 1024;
const CSRF_COOKIE_MAX_AGE: i64 = 7 * 24 * 3600;

pub struct Guard {
    key: [u8; 32],
    https: bool,
    origin: String,
    /// Whether the connector is on. With it off the MCP paths are ordinary pages.
    mcp: bool,
    /// Sign-in, two-factor, invite and password link pages: 10 a minute, burst 5, per address.
    auth_pages: Limiter,
    /// Everything else a signed-out visitor can reach: `public_access.requests_per_minute` per
    /// address, with a burst of `public_access.burst`.
    anonymous: Limiter,
    /// Signed-in API calls: 5,000 an hour per token or account.
    api: Quota,
}

impl Guard {
    pub fn new(config: &Config) -> Self {
        // The CSRF key is derived from the master key file so it survives restarts. If the
        // file cannot be read, a random key is used: forms open across a restart fail once.
        let key = std::fs::read(config.master_key_path())
            .ok()
            .filter(|b| b.len() == 32)
            .map(|b| Sha256::digest([b"ch-csrf-v1".as_slice(), &b].concat()).into())
            .unwrap_or_else(|| {
                let mut k = [0u8; 32];
                rand::Rng::fill(&mut rand::rng(), &mut k);
                k
            });
        Self::with_key(config, key)
    }

    pub fn with_key(config: &Config, key: [u8; 32]) -> Self {
        let origin = config.server.external_url.trim_end_matches('/').to_string();
        Self {
            key,
            https: config.is_https(),
            origin,
            mcp: config.mcp.enabled,
            auth_pages: Limiter::new(10, 5),
            // Every signed-out request, rejections and public reads alike (epic ch-4pn).
            anonymous: Limiter::new(config.public_access.requests_per_minute, config.public_access.burst),
            api: Quota::new(5000, Duration::from_secs(3600)),
        }
    }

    /// The token a page embeds for a signed-in session. For tools and tests that need to
    /// make a valid unsafe request without rendering a page first.
    pub fn token_for_session(&self, session_cookie: &str) -> String {
        self.token_for(&format!("s:{session_cookie}"))
    }

    fn token_for(&self, binding: &str) -> String {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&self.key).expect("hmac accepts any key length");
        mac.update(b"csrf-v1|");
        mac.update(binding.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    /// The token a page should embed for a request carrying these cookies. `None` for the
    /// binding when the visitor has neither cookie yet.
    fn binding(&self, headers: &HeaderMap) -> Option<String> {
        if let Some(s) = access::cookie_value(headers, access::session_cookie_name(self.https)) {
            return Some(format!("s:{s}"));
        }
        access::cookie_value(headers, access::csrf_cookie_name(self.https)).map(|c| format!("c:{c}"))
    }

    fn origin_ok(&self, headers: &HeaderMap) -> bool {
        if let Some(o) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
            return o == self.origin;
        }
        if let Some(r) = headers.get(header::REFERER).and_then(|v| v.to_str().ok()) {
            return r == self.origin
                || r.strip_prefix(&self.origin)
                    .is_some_and(|rest| rest.starts_with('/') || rest.starts_with('?') || rest.starts_with('#'));
        }
        false
    }

    fn set_csrf_cookie(&self, value: &str) -> String {
        let name = access::csrf_cookie_name(self.https);
        let secure = if self.https { "; Secure" } else { "" };
        format!("{name}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={CSRF_COOKIE_MAX_AGE}{secure}")
    }

    /// Wraps one request. `ip` is the trusted client address.
    pub async fn handle(&self, mut req: Request, next: Next, ip: IpAddr) -> Response {
        let method = req.method().clone();
        let path = req.uri().path().to_owned();
        let kind = access::classify_for(&path, self.mcp);
        let actor = req.extensions().get::<Actor>().cloned().unwrap_or(Actor::Anonymous);
        let unsafe_method = !matches!(method, Method::GET | Method::HEAD | Method::OPTIONS);

        // ---- rate limits
        let mut quota_state = None;
        // The machine routes have their own, larger buckets: everyone using Claude arrives from a
        // few addresses, and they must not share the signed-out budget of the pages.
        if actor.is_anonymous() && !matches!(kind, RouteKind::OAuth | RouteKind::Mcp) {
            if is_auth_page(&method, &path)
                && let Err(wait) = self.auth_pages.hit(&ip.to_string())
            {
                return too_many(wait, kind);
            }
            // An IPv6 host counts by its /64, so it cannot take a fresh budget per address.
            if !path.starts_with("/static/")
                && let Err(wait) = self.anonymous.hit(&access::address_key(ip))
            {
                return too_many(wait, kind);
            }
        } else if kind == RouteKind::Api {
            let key = match &actor {
                Actor::Token { token_id, .. } => format!("t:{token_id}"),
                Actor::User { id, .. } => format!("u:{id}"),
                Actor::Job { job_id, .. } => format!("j:{job_id}"),
                _ => format!("ip:{ip}"),
            };
            match self.api.hit(&key) {
                Ok(s) => quota_state = Some(s),
                Err(s) => {
                    let wait = (s.reset - chrono::Utc::now().timestamp()).max(1) as u64;
                    let mut r = too_many(wait, kind);
                    add_quota_headers(r.headers_mut(), s.limit, 0, s.reset);
                    return r;
                }
            }
        }

        // ---- CSRF
        let page_or_api = matches!(kind, RouteKind::Page | RouteKind::Api);
        let has_auth_header = req.headers().contains_key(header::AUTHORIZATION);
        let mut new_cookie: Option<String> = None;
        if page_or_api {
            let binding = match self.binding(req.headers()) {
                Some(b) => b,
                None => {
                    let fresh = ch_db::tokens::random_hex_256();
                    new_cookie = Some(fresh.clone());
                    format!("c:{fresh}")
                }
            };
            let expected = self.token_for(&binding);
            // A credential in `Authorization` means no cookies are in play, so there is nothing
            // to forge. An unrecognised one does not count: it leaves the caller anonymous.
            if unsafe_method && !(has_auth_header && !actor.is_anonymous()) {
                if !self.origin_ok(req.headers()) {
                    return forbidden(kind, "Request blocked: the origin does not match this server.");
                }
                let (parts, body) = req.into_parts();
                let mut supplied = parts.headers.get("x-csrf-token").and_then(|v| v.to_str().ok()).map(str::to_owned);
                let mut body = body;
                if supplied.is_none() {
                    let ctype = parts.headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
                    if ctype.starts_with("application/x-www-form-urlencoded") {
                        match axum::body::to_bytes(body, MAX_FORM_BYTES).await {
                            Ok(bytes) => {
                                supplied = form_field(&bytes, "_csrf");
                                body = Body::from(bytes);
                            }
                            Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, "That form is too large.").into_response(),
                        }
                    } else if let Some(q) = parts.uri.query() {
                        supplied = form_field(q.as_bytes(), "_csrf");
                    }
                }
                let good = supplied.as_deref().is_some_and(|s| constant_eq(s.as_bytes(), expected.as_bytes()));
                req = Request::from_parts(parts, body);
                if !good {
                    return forbidden(
                        kind,
                        "Request blocked: the security token is missing or out of date. Reload the page and try again.",
                    );
                }
            }
            req.extensions_mut().insert(CsrfToken(expected));
        }

        if let Some(s) = quota_state {
            req.extensions_mut().insert(access::ApiQuota { limit: s.limit, remaining: s.remaining, reset: s.reset });
        }
        let mut resp = next.run(req).await;
        if let Some(s) = quota_state {
            add_quota_headers(resp.headers_mut(), s.limit, s.remaining, s.reset);
        }
        if let Some(c) = new_cookie
            && let Ok(v) = HeaderValue::from_str(&self.set_csrf_cookie(&c))
        {
            resp.headers_mut().append(header::SET_COOKIE, v);
        }
        resp
    }
}

fn is_auth_page(method: &Method, path: &str) -> bool {
    if *method == Method::POST {
        return path == "/login" || path == "/login/two-factor" || path.starts_with("/invite/") || path.starts_with("/password/");
    }
    // Looking up a token is a guess at a secret, so a GET of an invite or link page counts too.
    path.starts_with("/invite/") || path.starts_with("/password/")
}

fn add_quota_headers(h: &mut HeaderMap, limit: u32, remaining: u32, reset: i64) {
    for (name, v) in [
        ("x-ratelimit-limit", limit.to_string()),
        ("x-ratelimit-remaining", remaining.to_string()),
        ("x-ratelimit-reset", reset.to_string()),
        ("x-ratelimit-used", (limit - remaining).to_string()),
    ] {
        if let Ok(v) = HeaderValue::from_str(&v) {
            h.insert(axum::http::HeaderName::from_static(name), v);
        }
    }
}

fn too_many(wait: u64, kind: RouteKind) -> Response {
    let mut r = match kind {
        RouteKind::Page => (StatusCode::TOO_MANY_REQUESTS, "Too many requests. Wait a moment and try again.\n").into_response(),
        _ => ch_core::Error::RateLimited { retry_after_secs: wait }.into_response(),
    };
    r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from(wait));
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

fn forbidden(kind: RouteKind, msg: &'static str) -> Response {
    let mut r = match kind {
        RouteKind::Api => {
            (StatusCode::FORBIDDEN, [(header::CONTENT_TYPE, "application/json; charset=utf-8")], format!("{{\"message\":\"{msg}\"}}"))
                .into_response()
        }
        _ => (StatusCode::FORBIDDEN, format!("{msg}\n")).into_response(),
    };
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    subtle::ConstantTimeEq::ct_eq(a, b).into()
}

/// One field of an `application/x-www-form-urlencoded` body or query string.
pub fn form_field(data: &[u8], name: &str) -> Option<String> {
    for pair in data.split(|b| *b == b'&') {
        let mut it = pair.splitn(2, |b| *b == b'=');
        let k = it.next()?;
        let v = it.next().unwrap_or(b"");
        if percent_decode(k) == name.as_bytes() {
            return String::from_utf8(percent_decode(v)).ok();
        }
    }
    None
}

fn percent_decode(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        match s[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < s.len() => {
                let h = std::str::from_utf8(&s[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
                match h {
                    Some(b) => {
                        out.push(b);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_fields() {
        assert_eq!(form_field(b"a=1&_csrf=abc123&b=2", "_csrf").as_deref(), Some("abc123"));
        assert_eq!(form_field(b"a=1", "_csrf"), None);
        assert_eq!(form_field(b"_csrf=a%2Bb+c", "_csrf").as_deref(), Some("a+b c"));
        assert_eq!(form_field(b"%5Fcsrf=x", "_csrf").as_deref(), Some("x"));
        assert_eq!(form_field(b"_csrf=%zz", "_csrf").as_deref(), Some("%zz"));
        assert_eq!(form_field(b"_csrf=%2", "_csrf").as_deref(), Some("%2"));
    }

    #[test]
    fn auth_page_set() {
        assert!(is_auth_page(&Method::POST, "/login"));
        assert!(is_auth_page(&Method::POST, "/login/two-factor"));
        assert!(is_auth_page(&Method::GET, "/invite/chi_x"));
        assert!(is_auth_page(&Method::GET, "/password/cho_x"));
        assert!(!is_auth_page(&Method::GET, "/login"));
        assert!(!is_auth_page(&Method::POST, "/settings"));
    }
}
