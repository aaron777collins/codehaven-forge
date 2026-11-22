//! Argon2id passwords (ARCHITECTURE.md sections 5.4 and 7.6).
//!
//! - 64 MiB, 3 passes, 1 lane, 16 byte salt. The parameters live in the PHC string, so they
//!   can be raised later and old hashes are upgraded at the next sign-in.
//! - At most 4 hashes run at once, so a flood of sign-in attempts cannot eat the CPU.
//! - When no usable account exists the server still verifies against a fixed dummy hash, so
//!   timing does not reveal which usernames exist.

use argon2::password_hash::{PasswordHash, PasswordHasher as _, PasswordVerifier, SaltString, rand_core::OsRng};
use argon2::{Algorithm, Argon2, Params, Version};
use ch_core::error::{Error, FieldError, Result};
use std::sync::Arc;
use tokio::sync::Semaphore;

pub const MIN_LENGTH: usize = 12;
pub const MAX_LENGTH: usize = 128;
const MAX_CONCURRENT: usize = 4;

#[derive(Debug, Clone, Copy)]
pub struct HashParams {
    pub memory_kib: u32,
    pub passes: u32,
    pub lanes: u32,
}

impl HashParams {
    pub const PRODUCTION: HashParams = HashParams { memory_kib: 64 * 1024, passes: 3, lanes: 1 };
    /// For tests only. Never reachable from the binary.
    pub const FAST_FOR_TESTS: HashParams = HashParams { memory_kib: 256, passes: 1, lanes: 1 };

    fn argon2(self) -> Result<Argon2<'static>> {
        let p = Params::new(self.memory_kib, self.passes, self.lanes, None)
            .map_err(|e| Error::internal(anyhow::anyhow!("bad argon2 params: {e}")))?;
        Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, p))
    }
}

#[derive(Clone)]
pub struct PasswordHasher {
    params: HashParams,
    sem: Arc<Semaphore>,
    dummy: Arc<String>,
}

impl PasswordHasher {
    pub fn new(params: HashParams) -> Result<Self> {
        let salt = SaltString::generate(&mut OsRng);
        let dummy = params
            .argon2()?
            .hash_password(b"codehaven dummy password for timing", &salt)
            .map_err(|e| Error::internal(anyhow::anyhow!("dummy hash failed: {e}")))?
            .to_string();
        Ok(Self { params, sem: Arc::new(Semaphore::new(MAX_CONCURRENT)), dummy: Arc::new(dummy) })
    }

    pub async fn hash(&self, password: &str) -> Result<String> {
        let _permit = self.sem.acquire().await.map_err(|_| Error::internal(anyhow::anyhow!("hasher closed")))?;
        let params = self.params;
        let password = password.to_owned();
        tokio::task::spawn_blocking(move || {
            let salt = SaltString::generate(&mut OsRng);
            params
                .argon2()?
                .hash_password(password.as_bytes(), &salt)
                .map(|h| h.to_string())
                .map_err(|e| Error::internal(anyhow::anyhow!("hashing failed: {e}")))
        })
        .await
        .map_err(|e| Error::internal(anyhow::anyhow!("hash task failed: {e}")))?
    }

    /// Checks a password. With `None` (no account, no password set, or an account that may
    /// not sign in) it verifies against the dummy hash and returns false, so the time taken
    /// is the same as for a real account.
    pub async fn verify(&self, stored: Option<&str>, password: &str) -> bool {
        let Ok(_permit) = self.sem.acquire().await else { return false };
        let real = stored.is_some();
        let hash = stored.map(str::to_owned).unwrap_or_else(|| (*self.dummy).clone());
        let password = password.to_owned();
        let ok = tokio::task::spawn_blocking(move || match PasswordHash::new(&hash) {
            Ok(parsed) => Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok(),
            Err(_) => false,
        })
        .await
        .unwrap_or(false);
        ok && real
    }

    /// True when a stored hash was made with weaker parameters than today's.
    pub fn needs_rehash(&self, stored: &str) -> bool {
        let Ok(parsed) = PasswordHash::new(stored) else { return true };
        let Ok(p) = Params::try_from(&parsed) else { return true };
        p.m_cost() < self.params.memory_kib || p.t_cost() < self.params.passes || p.p_cost() < self.params.lanes
    }
}

/// The rules for a new password: 12 to 128 characters, not the username or email, and not
/// easy to guess (zxcvbn score of 3 or more, which covers the common password lists,
/// keyboard patterns, dates and repeats).
pub fn check_policy(username: &str, email: &str, password: &str) -> Result<()> {
    let fail = |msg: &str| Err(Error::Validation(vec![FieldError::custom("Password", "password", msg)]));
    let len = password.chars().count();
    if len < MIN_LENGTH {
        return fail("Use at least 12 characters.");
    }
    if len > MAX_LENGTH {
        return fail("Use at most 128 characters.");
    }
    if password.chars().any(|c| c == '\0') {
        return fail("That password has a character we cannot store.");
    }
    let lower = password.to_lowercase();
    let local = email.split('@').next().unwrap_or("").to_lowercase();
    if (!username.is_empty() && lower.contains(&username.to_lowercase())) || (local.len() >= 4 && lower.contains(&local)) {
        return fail("Do not use your username or email in your password.");
    }
    let mut inputs: Vec<&str> = Vec::new();
    if !username.is_empty() {
        inputs.push(username);
    }
    if !local.is_empty() {
        inputs.push(&local);
    }
    let est = zxcvbn::zxcvbn(password, &inputs);
    if u8::from(est.score()) < 3 {
        return fail("That password is too easy to guess. Try a longer phrase, and avoid common words, names and patterns.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hasher() -> PasswordHasher {
        PasswordHasher::new(HashParams::FAST_FOR_TESTS).unwrap()
    }

    #[tokio::test]
    async fn hash_and_verify() {
        let h = hasher();
        let stored = h.hash("a very long passphrase").await.unwrap();
        assert!(stored.starts_with("$argon2id$"));
        assert!(h.verify(Some(&stored), "a very long passphrase").await);
        assert!(!h.verify(Some(&stored), "a very long passphras").await);
    }

    #[tokio::test]
    async fn two_hashes_of_one_password_differ() {
        let h = hasher();
        assert_ne!(h.hash("same password here").await.unwrap(), h.hash("same password here").await.unwrap());
    }

    #[tokio::test]
    async fn missing_account_never_verifies_but_still_does_the_work() {
        let h = hasher();
        assert!(!h.verify(None, "codehaven dummy password for timing").await);
        assert!(!h.verify(Some("not a phc string"), "x").await);
    }

    #[tokio::test]
    async fn rehash_when_parameters_are_weaker() {
        let weak = hasher();
        let stored = weak.hash("another long passphrase").await.unwrap();
        assert!(!weak.needs_rehash(&stored));
        let strong = PasswordHasher::new(HashParams { memory_kib: 1024, passes: 2, lanes: 1 }).unwrap();
        assert!(strong.needs_rehash(&stored));
        assert!(strong.needs_rehash("garbage"));
    }

    #[test]
    fn production_params_are_the_documented_ones() {
        let p = HashParams::PRODUCTION;
        assert_eq!((p.memory_kib, p.passes, p.lanes), (65536, 3, 1));
    }

    #[test]
    fn policy() {
        assert!(check_policy("alice", "a@b.co", "short").is_err());
        assert!(check_policy("alice", "a@b.co", "password1234").is_err(), "common");
        assert!(check_policy("alice", "a@b.co", "123456789012").is_err(), "sequence");
        assert!(check_policy("alice", "a@b.co", "aaaaaaaaaaaaaaaa").is_err(), "repeat");
        assert!(check_policy("alice", "a@b.co", "qwertyuiopasdf").is_err(), "keyboard");
        assert!(check_policy("alicesmith", "a@b.co", "my-alicesmith-secret-x").is_err(), "contains username");
        assert!(check_policy("alice", "a@b.co", &"x".repeat(200)).is_err(), "too long");
        assert!(check_policy("alice", "a@b.co", "violet harbour lantern 83 teapot").is_ok());
        assert!(check_policy("alice", "a@b.co", "Tr0ub4dor&3-lighthouse-quartz").is_ok());
    }
}
