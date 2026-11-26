//! Passkeys (WebAuthn) as a second factor beside TOTP (ch-wf0).
//!
//! The relying party is this site: its id is the host of the configured external address and its
//! origin is that address, so a passkey only ever answers for the site it was made on. The
//! ceremony is done by `webauthn-rs`; this module keeps the pieces around it: the relying party,
//! the state of a ceremony between its two requests (one row, read once, five minutes), and the
//! stored credentials.
//!
//! Only passkeys are asked for, never attestation, and never a private key: what is stored is
//! the public key, its flags and a counter.

use ch_core::error::{Error, Result};
use ch_core::ids::UserId;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sqlx::PgPool;
use url::Url;
use webauthn_rs::prelude::{Passkey, Uuid};
use webauthn_rs::{Webauthn, WebauthnBuilder};

/// How long the state of a ceremony waits for its second request.
const CEREMONY_MINUTES: i64 = 5;
/// Passkeys one account may hold.
pub const MAX_PASSKEYS: i64 = 10;

/// This site as a relying party, or nothing when the configured address cannot be one.
pub struct Rp {
    webauthn: Option<Webauthn>,
}

impl Rp {
    pub fn new(external_url: &str, site_name: &str) -> Self {
        let built = (|| -> std::result::Result<Webauthn, String> {
            let url = Url::parse(external_url).map_err(|e| format!("the external address is not a URL: {e}"))?;
            let host = url.host_str().ok_or("the external address has no host")?.to_string();
            if matches!(url.host(), Some(url::Host::Ipv4(_) | url::Host::Ipv6(_))) {
                return Err("an IP address cannot be a passkey's site".into());
            }
            let origin = Url::parse(&url.origin().ascii_serialization()).map_err(|e| e.to_string())?;
            WebauthnBuilder::new(&host, &origin).map_err(|e| e.to_string())?.rp_name(site_name).build().map_err(|e| e.to_string())
        })();
        match built {
            Ok(w) => Self { webauthn: Some(w) },
            Err(why) => {
                tracing::info!(%why, "passkeys are off: the external address cannot be a relying party");
                Self { webauthn: None }
            }
        }
    }

    pub fn available(&self) -> bool {
        self.webauthn.is_some()
    }

    pub fn get(&self) -> Result<&Webauthn> {
        self.webauthn.as_ref().ok_or_else(|| {
            Error::unavailable("Passkeys need this site to be reached by a domain name. Ask the administrator to set the external address.")
        })
    }
}

/// The user handle an authenticator stores for an account. Stable, and not the username.
pub fn user_handle(user: UserId) -> Uuid {
    Uuid::from_u64_pair(0x6368_7061_7373_6b65, user.0 as u64)
}

pub fn register_key(user: UserId) -> String {
    format!("reg:{}", user.0)
}

pub fn sign_in_key(session: i64) -> String {
    format!("auth:{session}")
}

/// Keeps the state of a ceremony for its second request, replacing an earlier one under the key.
pub async fn put_state<T: Serialize>(db: &PgPool, key: &str, state: &T) -> Result<()> {
    let json = serde_json::to_value(state).map_err(|e| Error::internal(anyhow::anyhow!("ceremony state: {e}")))?;
    sqlx::query("DELETE FROM webauthn_challenges WHERE expires_at < now()").execute(db).await?;
    sqlx::query(
        "INSERT INTO webauthn_challenges (key, state, expires_at) VALUES ($1, $2, now() + make_interval(mins => $3))
         ON CONFLICT (key) DO UPDATE SET state = EXCLUDED.state, expires_at = EXCLUDED.expires_at",
    )
    .bind(key)
    .bind(json)
    .bind(CEREMONY_MINUTES as i32)
    .execute(db)
    .await?;
    Ok(())
}

/// Reads and removes the state of a ceremony. A second reading, or one that waited too long,
/// finds nothing: a response cannot be replayed against the same challenge.
pub async fn take_state<T: DeserializeOwned>(db: &PgPool, key: &str) -> Result<Option<T>> {
    let row: Option<serde_json::Value> =
        sqlx::query_scalar("DELETE FROM webauthn_challenges WHERE key = $1 AND expires_at > now() RETURNING state")
            .bind(key)
            .fetch_optional(db)
            .await?;
    Ok(match row {
        Some(v) => serde_json::from_value(v).ok(),
        None => None,
    })
}

/// A passkey's name as the person typed it: trimmed, at most 60 characters, no control characters.
pub fn clean_name(name: &str) -> Result<String> {
    let n = name.split_whitespace().collect::<Vec<_>>().join(" ");
    if n.chars().any(char::is_control) || n.chars().count() > 60 {
        return Err(Error::invalid("Passkey", "name", "A name is at most 60 characters, with nothing unusual in it."));
    }
    Ok(if n.is_empty() { "Passkey".to_string() } else { n })
}

/// Every stored passkey of an account with its row id.
pub async fn load(db: &PgPool, user: UserId) -> Result<Vec<(i64, Passkey)>> {
    let rows: Vec<(i64, serde_json::Value)> =
        sqlx::query_as("SELECT id, passkey FROM webauthn_credentials WHERE user_id = $1 ORDER BY id").bind(user.0).fetch_all(db).await?;
    let mut out = Vec::with_capacity(rows.len());
    for (id, v) in rows {
        match serde_json::from_value::<Passkey>(v) {
            Ok(p) => out.push((id, p)),
            Err(e) => tracing::error!(credential = id, error = %e, "a stored passkey cannot be read"),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_relying_party_is_the_site_and_never_an_address() {
        assert!(Rp::new("https://code.example.com", "Codehaven").available());
        assert!(Rp::new("https://code.example.com:8443/", "Codehaven").available());
        assert!(Rp::new("http://localhost:3200", "Codehaven").available(), "localhost is a secure context");
        assert!(!Rp::new("http://192.0.2.10:3200", "Codehaven").available(), "an IP address cannot be a passkey's site");
        assert!(!Rp::new("http://[::1]:3200", "Codehaven").available());
        assert!(!Rp::new("not a url", "Codehaven").available());
        assert!(Rp::new("https://code.example.com", "Codehaven").get().is_ok());
        assert!(Rp::new("not a url", "Codehaven").get().is_err());
    }

    #[test]
    fn names_are_tidied_and_bounded() {
        assert_eq!(clean_name("  My   laptop ").unwrap(), "My laptop");
        assert_eq!(clean_name("   ").unwrap(), "Passkey");
        assert!(clean_name(&"x".repeat(61)).is_err());
        assert!(clean_name("bad\u{7}name").is_err());
        assert_ne!(user_handle(UserId(1)), user_handle(UserId(2)));
        assert_eq!(user_handle(UserId(7)), user_handle(UserId(7)));
    }
}
