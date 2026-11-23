//! The workspace key and the draft key (browser workspace spec, sections 5 and 8).
//!
//! Both come from the instance master key through HKDF-SHA256 with their own label, so neither
//! can be turned into the other or into a CSRF token.
//!
//! - The workspace key binds a tab to one session, one user and one repository id. It grants
//!   nothing. It makes a tab stop working when any of the three changes, and a data route
//!   cannot be opened by navigation, which cannot send the header.
//! - The draft key encrypts drafts in the browser. It is a function of user and repository
//!   only, so drafts survive a new session, and only `boot` hands it out.

use crate::csrf::ct_eq;
use ch_core::{Config, RepoId, UserId};
use hmac::{Hmac, Mac};
use rand::Rng;
use sha2::Sha256;
use std::sync::OnceLock;

type HmacSha256 = Hmac<Sha256>;

static BIND: OnceLock<[u8; 32]> = OnceLock::new();
static DRAFT: OnceLock<[u8; 32]> = OnceLock::new();

fn hmac(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).unwrap_or_else(|_| unreachable!("hmac accepts any key length"));
    for p in parts {
        mac.update(p);
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&mac.finalize().into_bytes());
    out
}

/// HKDF-SHA256 (RFC 5869) with an empty salt and 32 bytes of output: one expand block.
pub fn hkdf32(ikm: &[u8], info: &[u8]) -> [u8; 32] {
    let prk = hmac(&[0u8; 32], &[ikm]);
    hmac(&prk, &[info, &[1u8]])
}

fn master(config: &Config) -> [u8; 32] {
    let mut base = [0u8; 32];
    match std::fs::read(config.master_key_path()) {
        Ok(b) if b.len() == 32 => base.copy_from_slice(&b),
        // Tests and the demo have no master key: random for the life of the process.
        _ => rand::rng().fill(&mut base),
    }
    base
}

fn bind_key(config: &Config) -> &'static [u8; 32] {
    BIND.get_or_init(|| hkdf32(&master(config), b"workspace-binding-v1"))
}

fn draft_root(config: &Config) -> &'static [u8; 32] {
    DRAFT.get_or_init(|| hkdf32(&master(config), b"workspace-draft-v1"))
}

/// Length-prefixed, so no two different inputs give the same bytes.
fn framed(session: &str, user: UserId, repo: RepoId) -> Vec<u8> {
    let mut v = Vec::with_capacity(session.len() + 24);
    v.extend_from_slice(&(session.len() as u64).to_be_bytes());
    v.extend_from_slice(session.as_bytes());
    v.extend_from_slice(&user.0.to_be_bytes());
    v.extend_from_slice(&repo.0.to_be_bytes());
    v
}

/// The key a tab sends as `X-Workspace-Key`.
pub fn workspace_key(config: &Config, session_cookie: &str, user: UserId, repo: RepoId) -> String {
    hex::encode(hmac(bind_key(config), &[&framed(session_cookie, user, repo)]))
}

/// True when `supplied` is the key for this session, user and repository.
pub fn workspace_key_valid(config: &Config, session_cookie: &str, user: UserId, repo: RepoId, supplied: &str) -> bool {
    !supplied.is_empty() && ct_eq(&workspace_key(config, session_cookie, user, repo), supplied)
}

/// The raw AES-256 key for this person's drafts of this repository.
pub fn draft_key(config: &Config, user: UserId, repo: RepoId) -> [u8; 32] {
    let mut v = Vec::with_capacity(16);
    v.extend_from_slice(&user.0.to_be_bytes());
    v.extend_from_slice(&repo.0.to_be_bytes());
    hmac(draft_root(config), &[&v])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hkdf_matches_rfc_5869_case_3() {
        // RFC 5869 test case 3: SHA-256, empty salt and info. Our helper takes `info`, so check
        // the extract and one expand block against the published OKM's first 32 bytes.
        let ikm = [0x0bu8; 22];
        let okm = hkdf32(&ikm, b"");
        assert_eq!(hex::encode(okm), "8da4e775a563c18f715f802a063c5a31b8a11f5c5ee1879ec3454e5f3c738d2d");
    }

    #[test]
    fn keys_bind_session_user_and_repo() {
        let c = Config::for_tests();
        let k = workspace_key(&c, "s1", UserId(1), RepoId(2));
        assert!(workspace_key_valid(&c, "s1", UserId(1), RepoId(2), &k));
        assert!(!workspace_key_valid(&c, "s2", UserId(1), RepoId(2), &k), "another session");
        assert!(!workspace_key_valid(&c, "s1", UserId(3), RepoId(2), &k), "another user");
        assert!(!workspace_key_valid(&c, "s1", UserId(1), RepoId(4), &k), "another repository");
        assert!(!workspace_key_valid(&c, "s1", UserId(1), RepoId(2), ""), "no key");
        // Framing: moving bytes between the session and the ids changes the key.
        assert_ne!(workspace_key(&c, "s1", UserId(1), RepoId(2)), workspace_key(&c, "s", UserId(1), RepoId(2)));
        assert_ne!(draft_key(&c, UserId(1), RepoId(2)), draft_key(&c, UserId(2), RepoId(1)));
        assert_eq!(draft_key(&c, UserId(1), RepoId(2)), draft_key(&c, UserId(1), RepoId(2)));
    }
}
