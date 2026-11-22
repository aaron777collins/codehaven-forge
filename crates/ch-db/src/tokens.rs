//! Random tokens and their hashes. Tokens, invite links, one-time links and session ids are
//! stored only as SHA-256 hashes (ARCHITECTURE.md section 5.4).

use rand::Rng;
use sha2::{Digest, Sha256};

const BASE62: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// `len` random base62 characters (about 5.95 bits each) from the OS-seeded CSPRNG.
pub fn random_base62(len: usize) -> String {
    let mut rng = rand::rng();
    (0..len).map(|_| BASE62[rng.random_range(0..62)] as char).collect()
}

/// A token with a recognisable prefix so secret scanners can match it: `chp_` for personal
/// access tokens (40 characters), `chi_` for invites (32), `cho_` for one-time links (32).
pub fn new_token(prefix: &str, len: usize) -> String {
    format!("{prefix}{}", random_base62(len))
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(sha256(data))
}

/// The value stored for a token. Never store or log the token itself.
pub fn hash_token(token: &str) -> Vec<u8> {
    sha256(token.as_bytes()).to_vec()
}

/// 256 random bits, hex encoded. For session ids and CSRF secrets.
pub fn random_hex_256() -> String {
    let mut rng = rand::rng();
    let mut b = [0u8; 32];
    rng.fill(&mut b);
    hex::encode(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_shape() {
        let t = new_token("chp_", 40);
        assert!(t.starts_with("chp_"));
        assert_eq!(t.len(), 44);
        assert!(t[4..].bytes().all(|b| b.is_ascii_alphanumeric()));
    }

    #[test]
    fn tokens_differ() {
        assert_ne!(new_token("chi_", 32), new_token("chi_", 32));
    }

    #[test]
    fn hash_is_stable_and_32_bytes() {
        assert_eq!(hash_token("abc"), hash_token("abc"));
        assert_ne!(hash_token("abc"), hash_token("abd"));
        assert_eq!(hash_token("abc").len(), 32);
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn session_ids_are_256_bits() {
        assert_eq!(random_hex_256().len(), 64);
    }
}
