//! Secrets at rest (ARCHITECTURE.md section 5.4).
//!
//! `data/secrets/master.key` is 32 random bytes, created on first start with mode 0600.
//! CI secrets, webhook secrets and TOTP secrets are sealed with AES-256-GCM, with a random
//! nonce per value and the row's identity as associated data, so a sealed value copied to
//! another row does not open.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use ch_core::{Error, Result};
use rand::Rng;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

const NONCE_LEN: usize = 12;

#[derive(Clone)]
pub struct MasterKey([u8; 32]);

impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MasterKey(..)")
    }
}

impl MasterKey {
    pub fn from_bytes(b: [u8; 32]) -> Self {
        MasterKey(b)
    }

    pub fn random() -> Self {
        let mut b = [0u8; 32];
        rand::rng().fill(&mut b);
        MasterKey(b)
    }

    /// Loads the key, creating it with mode 0600 when missing. Refuses a key file that is
    /// readable by anyone else, or that is not exactly 32 bytes.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(Error::internal)?;
            // The directory is private too.
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
        let key = MasterKey::random();
        match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path) {
            Ok(mut f) => {
                f.write_all(&key.0).map_err(Error::internal)?;
                f.sync_all().map_err(Error::internal)?;
                tracing::info!(path = %path.display(), "created master key");
                return Ok(key);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(Error::internal(e)),
        }
        // Another process may still be writing it. Wait briefly for 32 bytes.
        let mut last_len = 0;
        for _ in 0..50 {
            let meta = std::fs::metadata(path).map_err(Error::internal)?;
            last_len = meta.len();
            if last_len == 32 {
                if meta.permissions().mode() & 0o077 != 0 {
                    return Err(Error::internal(anyhow::anyhow!(
                        "{} is readable by other users (mode {:o}); run: chmod 600 {}",
                        path.display(),
                        meta.permissions().mode() & 0o777,
                        path.display()
                    )));
                }
                let bytes = std::fs::read(path).map_err(Error::internal)?;
                let arr: [u8; 32] = bytes.try_into().map_err(|_| Error::internal(anyhow::anyhow!("master key is not 32 bytes")))?;
                return Ok(MasterKey(arr));
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        Err(Error::internal(anyhow::anyhow!("{} has {last_len} bytes, expected 32", path.display())))
    }

    /// Returns `nonce || ciphertext`. `aad` binds the value to its place, for example
    /// `b"webhook:42"`.
    pub fn seal(&self, plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&self.0));
        let mut nonce = [0u8; NONCE_LEN];
        rand::rng().fill(&mut nonce);
        let ct = cipher
            .encrypt(Nonce::from_slice(&nonce), Payload { msg: plaintext, aad })
            .map_err(|_| Error::internal(anyhow::anyhow!("seal failed")))?;
        let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    pub fn open(&self, sealed: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
        if sealed.len() < NONCE_LEN + 16 {
            return Err(Error::internal(anyhow::anyhow!("sealed value too short")));
        }
        let (nonce, ct) = sealed.split_at(NONCE_LEN);
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&self.0));
        cipher
            .decrypt(Nonce::from_slice(nonce), Payload { msg: ct, aad })
            .map_err(|_| Error::internal(anyhow::anyhow!("could not open sealed value")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn round_trip() {
        let k = MasterKey::random();
        let sealed = k.seal(b"hunter2", b"secret:1").unwrap();
        assert_ne!(&sealed[NONCE_LEN..], b"hunter2");
        assert_eq!(k.open(&sealed, b"secret:1").unwrap(), b"hunter2");
    }

    #[test]
    fn wrong_aad_does_not_open() {
        let k = MasterKey::random();
        let sealed = k.seal(b"v", b"secret:1").unwrap();
        assert!(k.open(&sealed, b"secret:2").is_err());
    }

    #[test]
    fn wrong_key_does_not_open() {
        let sealed = MasterKey::random().seal(b"v", b"a").unwrap();
        assert!(MasterKey::random().open(&sealed, b"a").is_err());
    }

    #[test]
    fn tampering_is_detected() {
        let k = MasterKey::random();
        let mut sealed = k.seal(b"value", b"a").unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 1;
        assert!(k.open(&sealed, b"a").is_err());
        assert!(k.open(&[1, 2, 3], b"a").is_err());
    }

    #[test]
    fn nonce_is_random_per_value() {
        let k = MasterKey::random();
        assert_ne!(k.seal(b"v", b"a").unwrap(), k.seal(b"v", b"a").unwrap());
    }

    #[test]
    fn key_file_is_created_0600_and_reloaded() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("secrets").join("master.key");
        let a = MasterKey::load_or_create(&p).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(&p).unwrap().len(), 32);
        let b = MasterKey::load_or_create(&p).unwrap();
        let sealed = a.seal(b"x", b"y").unwrap();
        assert_eq!(b.open(&sealed, b"y").unwrap(), b"x");
    }

    #[test]
    fn world_readable_key_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("master.key");
        std::fs::write(&p, [7u8; 32]).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(MasterKey::load_or_create(&p).is_err());
    }

    #[test]
    fn debug_never_prints_the_key() {
        assert_eq!(format!("{:?}", MasterKey::from_bytes([9; 32])), "MasterKey(..)");
    }

    proptest! {
        #[test]
        fn any_plaintext_round_trips(data in proptest::collection::vec(any::<u8>(), 0..512), aad in proptest::collection::vec(any::<u8>(), 0..32)) {
            let k = MasterKey::from_bytes([3; 32]);
            let sealed = k.seal(&data, &aad).unwrap();
            prop_assert_eq!(k.open(&sealed, &aad).unwrap(), data);
        }
    }
}
