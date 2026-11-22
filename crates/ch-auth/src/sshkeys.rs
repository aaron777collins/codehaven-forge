//! SSH public key parsing and fingerprints. ed25519, ECDSA and RSA of 3072 bits or more are
//! accepted. DSA is refused. The fingerprint is `SHA256:` plus base64, which is what the
//! SSH server computes from the key a client offers.

use ch_core::error::{Error, FieldError, Result};
use ssh_key::{Algorithm, HashAlg, PublicKey};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedKey {
    pub key_type: String,
    pub fingerprint: String,
    /// The key in OpenSSH one line form, which is what is stored.
    pub normalized: String,
}

fn bad(msg: &str) -> Error {
    Error::Validation(vec![FieldError::custom("PublicKey", "key", msg)])
}

pub fn parse_public_key(text: &str) -> Result<ParsedKey> {
    let text = text.trim();
    if text.is_empty() || text.len() > 8192 || text.contains('\n') {
        return Err(bad("Paste one public key on a single line."));
    }
    if text.contains("PRIVATE KEY") {
        return Err(bad("That is a private key. Paste the public key, which ends in .pub."));
    }
    let key = PublicKey::from_openssh(text).map_err(|_| bad("That is not a valid OpenSSH public key."))?;
    let key_type = match key.algorithm() {
        Algorithm::Ed25519 => "ssh-ed25519".to_string(),
        Algorithm::SkEd25519 => "sk-ssh-ed25519@openssh.com".to_string(),
        Algorithm::Ecdsa { curve } => format!("ecdsa-sha2-{}", curve.as_str()),
        Algorithm::SkEcdsaSha2NistP256 => "sk-ecdsa-sha2-nistp256@openssh.com".to_string(),
        Algorithm::Rsa { .. } => {
            let bits = key
                .key_data()
                .rsa()
                .and_then(|r| r.n.as_positive_bytes())
                .map(|b| b.len() * 8 - b.first().map_or(0, |f| f.leading_zeros() as usize))
                .unwrap_or(0);
            if bits < 3072 {
                return Err(bad("RSA keys need at least 3072 bits. Use an ed25519 key: ssh-keygen -t ed25519"));
            }
            "ssh-rsa".to_string()
        }
        _ => return Err(bad("That key type is not supported. Use ed25519, ECDSA or RSA of 3072 bits or more.")),
    };
    let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
    let normalized = key.to_openssh().map_err(|_| bad("Could not read that key."))?;
    Ok(ParsedKey { key_type, fingerprint, normalized })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Generated with `ssh-keygen -t ed25519`. The private half was discarded.
    const ED25519: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFb6NT8qYavZlutmABtenBoJjXu0zs0KW7M8CidCX/T+ test@codehaven";

    #[test]
    fn accepts_ed25519_and_computes_a_sha256_fingerprint() {
        let k = parse_public_key(ED25519).unwrap();
        assert_eq!(k.key_type, "ssh-ed25519");
        assert!(k.fingerprint.starts_with("SHA256:"));
        assert_eq!(k.fingerprint.len(), "SHA256:".len() + 43);
        assert!(k.normalized.starts_with("ssh-ed25519 "));
    }

    #[test]
    fn same_key_same_fingerprint_even_with_another_comment() {
        let a = parse_public_key(ED25519).unwrap();
        let b = parse_public_key(&ED25519.replace("test@codehaven", "other comment")).unwrap();
        assert_eq!(a.fingerprint, b.fingerprint);
    }

    #[test]
    fn refuses_junk_private_keys_and_multiple_lines() {
        assert!(parse_public_key("").is_err());
        assert!(parse_public_key("not a key").is_err());
        assert!(parse_public_key("ssh-ed25519 AAAA").is_err());
        assert!(parse_public_key("-----BEGIN OPENSSH PRIVATE KEY-----").is_err());
        assert!(parse_public_key(&format!("{ED25519}\n{ED25519}")).is_err());
    }
}
