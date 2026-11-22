//! CSRF tokens, signed cookies and the Origin check (ARCHITECTURE.md section 7.6).
//!
//! - A signed in form carries a token that is an HMAC of the session cookie, so it needs no
//!   storage and dies with the session.
//! - A signed out form (sign in, invite, password link) uses a signed double-submit cookie:
//!   the form field must equal the cookie and the cookie must carry our signature.
//! - Every unsafe request must also come from our own origin.
//! - The key is derived from the instance master key. Without one (tests, demo) it is random
//!   for the life of the process.

use ch_core::Config;
use hmac::{Hmac, Mac};
use rand::Rng;
use sha2::Sha256;
use std::sync::OnceLock;

type HmacSha256 = Hmac<Sha256>;

static KEY: OnceLock<[u8; 32]> = OnceLock::new();

/// The signing key, derived once per process.
pub fn key(config: &Config) -> &'static [u8; 32] {
    KEY.get_or_init(|| {
        let mut base = [0u8; 32];
        match std::fs::read(config.master_key_path()) {
            Ok(b) if b.len() == 32 => base.copy_from_slice(&b),
            _ => rand::rng().fill(&mut base),
        }
        let mut mac = HmacSha256::new_from_slice(&base).unwrap_or_else(|_| unreachable!("hmac accepts any key length"));
        mac.update(b"codehaven-web-v1");
        let mut out = [0u8; 32];
        out.copy_from_slice(&mac.finalize().into_bytes());
        out
    })
}

pub fn mac_hex(key: &[u8; 32], msg: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(key).unwrap_or_else(|_| unreachable!("hmac accepts any key length"));
    mac.update(msg.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Compares without stopping at the first different byte.
pub fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The token a signed in form carries.
pub fn session_token(key: &[u8; 32], session_cookie: &str) -> String {
    mac_hex(key, &format!("s|{session_cookie}"))
}

/// A fresh signed value for the signed out double-submit cookie.
pub fn new_anon_value(key: &[u8; 32]) -> String {
    let mut nonce = [0u8; 16];
    rand::rng().fill(&mut nonce);
    let nonce = hex::encode(nonce);
    format!("{nonce}.{}", mac_hex(key, &format!("c|{nonce}")))
}

pub fn anon_value_valid(key: &[u8; 32], value: &str) -> bool {
    match value.split_once('.') {
        Some((nonce, sig)) if nonce.len() == 32 => ct_eq(sig, &mac_hex(key, &format!("c|{nonce}"))),
        _ => false,
    }
}

/// Signs a short message for a cookie the browser must not be able to forge (flash notices).
pub fn sign(key: &[u8; 32], purpose: &str, text: &str) -> String {
    use base64::Engine;
    let body = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(text);
    format!("{body}.{}", mac_hex(key, &format!("{purpose}|{body}")))
}

pub fn unsign(key: &[u8; 32], purpose: &str, value: &str) -> Option<String> {
    use base64::Engine;
    let (body, sig) = value.split_once('.')?;
    if !ct_eq(sig, &mac_hex(key, &format!("{purpose}|{body}"))) {
        return None;
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(body).ok()?;
    String::from_utf8(bytes).ok()
}

/// The origin part of an absolute URL: `https://host`.
pub fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let host = rest.split(['/', '?', '#']).next()?;
    if host.is_empty() || !(scheme == "http" || scheme == "https") {
        return None;
    }
    Some(format!("{scheme}://{host}"))
}

/// An unsafe request must name our own origin in `Origin`, or failing that `Referer`.
pub fn origin_ok(config: &Config, origin: Option<&str>, referer: Option<&str>) -> bool {
    let Some(expected) = origin_of(&config.server.external_url) else { return false };
    match (origin, referer) {
        (Some(o), _) => o == expected,
        (None, Some(r)) => origin_of(r).is_some_and(|o| o == expected),
        (None, None) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        let mut c = Config::for_tests();
        c.server.external_url = "https://forge.example".into();
        c
    }

    #[test]
    fn anon_values_verify_and_forgeries_do_not() {
        let k = key(&cfg());
        let v = new_anon_value(k);
        assert!(anon_value_valid(k, &v));
        assert!(!anon_value_valid(k, "00000000000000000000000000000000.deadbeef"));
        assert!(!anon_value_valid(k, "nodot"));
        assert!(!anon_value_valid(k, &format!("{v}x")));
        assert_ne!(v, new_anon_value(k));
    }

    #[test]
    fn session_tokens_are_bound_to_the_session() {
        let k = key(&cfg());
        let a = session_token(k, "session-a");
        assert_eq!(a, session_token(k, "session-a"));
        assert_ne!(a, session_token(k, "session-b"));
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn signed_messages_round_trip_and_reject_tampering() {
        let k = key(&cfg());
        let s = sign(k, "flash", "ok:Saved");
        assert_eq!(unsign(k, "flash", &s).as_deref(), Some("ok:Saved"));
        assert_eq!(unsign(k, "other", &s), None);
        let tampered = s.replacen('.', "x.", 1);
        assert_eq!(unsign(k, "flash", &tampered), None);
    }

    #[test]
    fn origin_rules() {
        let c = cfg();
        assert!(origin_ok(&c, Some("https://forge.example"), None));
        assert!(!origin_ok(&c, Some("https://evil.example"), None));
        assert!(!origin_ok(&c, Some("http://forge.example"), None));
        assert!(origin_ok(&c, None, Some("https://forge.example/some/page?x=1")));
        assert!(!origin_ok(&c, None, Some("https://forge.example.evil.com/")));
        assert!(!origin_ok(&c, None, None));
        // An explicit Origin wins over a friendly Referer.
        assert!(!origin_ok(&c, Some("https://evil.example"), Some("https://forge.example/")));
    }

    #[test]
    fn constant_time_compare() {
        assert!(ct_eq("abc", "abc"));
        assert!(!ct_eq("abc", "abd"));
        assert!(!ct_eq("abc", "abcd"));
    }
}
