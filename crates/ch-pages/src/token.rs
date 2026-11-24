//! The per-site viewing token (`docs/design/pages.md`, section 5).
//!
//! `v1.<repo id>.<user id>.<expiry>.<mac>`: an HMAC-SHA256 over the first four parts, keyed
//! from the master key under its own label, so no other token of Codehaven can stand in for
//! one and one can stand in for nothing else. It names one site and one person, carries
//! nothing of the session, and the server checks the person's access again on every request.

use base64::Engine;
use ch_core::{RepoId, UserId};
use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// One hour.
pub const TTL_SECS: i64 = 3600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grant {
    pub repo: RepoId,
    pub user: UserId,
    /// Unix seconds.
    pub exp: i64,
}

/// The key, derived from the 32 byte master key.
pub fn derive_key(master: &[u8; 32]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(master).unwrap_or_else(|_| unreachable!("hmac accepts any key length"));
    mac.update(b"codehaven-pages-token-v1");
    let mut out = [0u8; 32];
    out.copy_from_slice(&mac.finalize().into_bytes());
    out
}

fn mac(key: &[u8; 32], body: &str) -> Vec<u8> {
    let mut m = HmacSha256::new_from_slice(key).unwrap_or_else(|_| unreachable!("hmac accepts any key length"));
    m.update(b"pages|");
    m.update(body.as_bytes());
    m.finalize().into_bytes().to_vec()
}

pub fn mint(key: &[u8; 32], g: &Grant) -> String {
    let body = format!("v1.{}.{}.{}", g.repo.0, g.user.0, g.exp);
    let sig = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac(key, &body));
    format!("{body}.{sig}")
}

/// The grant, when `token` is exactly one this server made and it has not run out at `now`.
pub fn open(key: &[u8; 32], token: &str, now: i64) -> Option<Grant> {
    let mut parts = token.split('.');
    let (v, repo, user, exp, sig) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || v != "v1" {
        return None;
    }
    let num = |s: &str| -> Option<i64> {
        // Only the digits `mint` writes: no sign, no leading zero, so one grant has one spelling.
        if s.is_empty() || s.len() > 19 || !s.bytes().all(|b| b.is_ascii_digit()) || (s.len() > 1 && s.starts_with('0')) {
            return None;
        }
        s.parse().ok()
    };
    let (repo, user, exp) = (num(repo)?, num(user)?, num(exp)?);
    let given = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(sig).ok()?;
    let want = mac(key, &format!("v1.{repo}.{user}.{exp}"));
    if !ct_eq(&given, &want) || exp <= now {
        return None;
    }
    Some(Grant { repo: RepoId(repo), user: UserId(user), exp })
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    const K: [u8; 32] = [7u8; 32];

    #[test]
    fn a_minted_token_opens_until_it_runs_out() {
        let key = derive_key(&K);
        let g = Grant { repo: RepoId(12), user: UserId(3), exp: 1_000 };
        let t = mint(&key, &g);
        assert!(crate::path::token_shape_ok(&t), "{t}");
        assert_eq!(open(&key, &t, 999), Some(g));
        assert_eq!(open(&key, &t, 1_000), None, "expired at the second it names");
    }

    #[test]
    fn anything_changed_is_refused() {
        let key = derive_key(&K);
        let t = mint(&key, &Grant { repo: RepoId(12), user: UserId(3), exp: 1_000 });
        let swap = |from: &str, to: &str| t.replacen(from, to, 1);
        assert_eq!(open(&key, &swap("v1.12.", "v1.13."), 0), None, "another site");
        assert_eq!(open(&key, &swap(".3.", ".4."), 0), None, "another person");
        assert_eq!(open(&key, &swap(".1000.", ".9000."), 0), None, "a later expiry");
        assert_eq!(open(&key, &swap("v1.12.", "v1.012."), 0), None, "a second spelling");
        assert_eq!(open(&derive_key(&[8u8; 32]), &t, 0), None, "another server");
        assert_eq!(open(&key, &format!("{t}.x"), 0), None);
        assert_eq!(open(&key, "v1.12.3.1000", 0), None);
        assert_eq!(open(&key, "", 0), None);
    }

    #[test]
    fn the_key_is_not_the_master_key() {
        assert_ne!(derive_key(&K), K);
    }
}
