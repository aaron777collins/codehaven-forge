//! The server's ed25519 host key, kept in `data/secrets/ssh_host_ed25519` (mode 0600) so
//! clients see the same key across restarts. Made on first start.

use russh::keys::ssh_key::LineEnding;
use russh::keys::{Algorithm, PrivateKey};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// Loads the key, or creates it. A key file that other users can read is refused: whoever
/// holds it can impersonate this server.
pub fn load_or_create(path: &Path) -> Result<PrivateKey, String> {
    if path.exists() {
        let mode = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?.permissions().mode();
        if mode & 0o077 != 0 {
            return Err(format!(
                "{} has mode {:o}; it must not be readable by anyone but the server (chmod 600)",
                path.display(),
                mode & 0o777
            ));
        }
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        return PrivateKey::from_openssh(&text).map_err(|e| format!("{} is not a valid OpenSSH key: {e}", path.display()));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    }
    let key = PrivateKey::random(&mut rand10::rng(), Algorithm::Ed25519).map_err(|e| format!("generating a host key: {e}"))?;
    let pem = key.to_openssh(LineEnding::LF).map_err(|e| format!("encoding the host key: {e}"))?;
    // `create_new` so two processes starting together cannot both write; the loser reads.
    match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path) {
        Ok(mut f) => {
            f.write_all(pem.as_bytes()).map_err(|e| format!("writing {}: {e}", path.display()))?;
            f.sync_all().map_err(|e| format!("writing {}: {e}", path.display()))?;
            Ok(key)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => load_or_create(path),
        Err(e) => Err(format!("creating {}: {e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_then_reloads_the_same_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets/ssh_host_ed25519");
        let a = load_or_create(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let b = load_or_create(&path).unwrap();
        assert_eq!(a.public_key().to_bytes().unwrap(), b.public_key().to_bytes().unwrap());
    }

    #[test]
    fn refuses_a_key_others_can_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k");
        load_or_create(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let e = load_or_create(&path).unwrap_err();
        assert!(e.contains("chmod 600"), "{e}");
    }

    #[test]
    fn refuses_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k");
        std::fs::write(&path, "not a key").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(load_or_create(&path).is_err());
    }
}
