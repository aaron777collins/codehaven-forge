//! Outgoing email over SMTP (ch-bfa).
//!
//! Off unless `[mail.smtp]` is set: then `build` returns `None` and the state keeps the
//! placeholder, whose `enabled` is false. TLS is required by default. Plain text is allowed only
//! to a relay on this machine, when the operator asks for it by name. The password is read from
//! a file once at start and never logged.

use async_trait::async_trait;
use ch_core::config::SmtpConfig;
use ch_core::services::Mail;
use ch_core::{Error, Result};
use lettre::message::Mailbox;
use lettre::message::header::ContentType;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use std::time::Duration;

/// How the connection to the relay is protected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tls {
    StartTls,
    Implicit,
    None,
}

/// Reads the `tls` setting. Plain text only for a relay on this machine.
pub fn tls_mode(cfg: &SmtpConfig) -> anyhow::Result<Tls> {
    let mode = match cfg.tls.as_deref().map(str::trim) {
        None | Some("") | Some("starttls") => Tls::StartTls,
        Some("tls") => Tls::Implicit,
        Some("none") => Tls::None,
        Some(other) => anyhow::bail!("mail.smtp.tls must be starttls, tls or none, not {other:?}"),
    };
    if mode == Tls::None && !matches!(cfg.host.as_str(), "localhost" | "127.0.0.1" | "::1") {
        anyhow::bail!("mail.smtp.tls = \"none\" is allowed only for a relay on localhost");
    }
    Ok(mode)
}

pub struct SmtpMail {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
}

impl SmtpMail {
    /// `None` when mail is not configured.
    pub fn build(cfg: Option<&SmtpConfig>) -> anyhow::Result<Option<Self>> {
        let Some(cfg) = cfg else { return Ok(None) };
        let from: Mailbox = cfg.from.parse().map_err(|e| anyhow::anyhow!("mail.smtp.from is not an address: {e}"))?;
        let builder = match tls_mode(cfg)? {
            Tls::StartTls => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&cfg.host)?,
            Tls::Implicit => AsyncSmtpTransport::<Tokio1Executor>::relay(&cfg.host)?,
            Tls::None => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&cfg.host),
        };
        let mut builder = builder.port(cfg.port).timeout(Some(Duration::from_secs(20)));
        if let Some(user) = cfg.username.as_deref().filter(|u| !u.is_empty()) {
            let password = match &cfg.password_file {
                Some(path) => std::fs::read_to_string(path)
                    .map_err(|e| anyhow::anyhow!("cannot read mail.smtp.password_file: {e}"))?
                    .trim_end_matches(['\r', '\n'])
                    .to_string(),
                None => anyhow::bail!("mail.smtp.username is set, so mail.smtp.password_file is needed"),
            };
            builder = builder.credentials(Credentials::new(user.to_string(), password));
        }
        Ok(Some(SmtpMail { transport: builder.build(), from }))
    }
}

/// One plain-text message. Addresses are parsed, so a line break cannot add a header.
pub fn message(from: &Mailbox, to: &str, subject: &str, body: &str) -> Result<Message> {
    let to: Mailbox = to.parse().map_err(|_| Error::invalid("Mail", "to", "That is not an email address."))?;
    Message::builder()
        .from(from.clone())
        .to(to)
        .subject(subject.replace(['\r', '\n'], " "))
        .header(ContentType::TEXT_PLAIN)
        .body(body.to_string())
        .map_err(|e| Error::Internal(anyhow::anyhow!("could not build a mail: {e}")))
}

#[async_trait]
impl Mail for SmtpMail {
    fn enabled(&self) -> bool {
        true
    }
    async fn send(&self, to: &str, subject: &str, body: &str) -> Result<()> {
        let msg = message(&self.from, to, subject, body)?;
        self.transport.send(msg).await.map_err(|e| {
            // The relay's answer, never the message or a credential.
            tracing::warn!(error = %e, "mail was not accepted by the relay");
            Error::unavailable("Mail could not be sent right now.")
        })?;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn cfg(host: &str, tls: Option<&str>) -> SmtpConfig {
        SmtpConfig {
            host: host.into(),
            port: 587,
            username: None,
            password_file: None,
            from: "Codehaven <code@example.test>".into(),
            tls: tls.map(String::from),
        }
    }

    #[test]
    fn tls_is_required_unless_the_relay_is_local_and_named() {
        assert_eq!(tls_mode(&cfg("smtp.example.test", None)).unwrap(), Tls::StartTls);
        assert_eq!(tls_mode(&cfg("smtp.example.test", Some("tls"))).unwrap(), Tls::Implicit);
        assert!(tls_mode(&cfg("smtp.example.test", Some("none"))).is_err());
        assert_eq!(tls_mode(&cfg("localhost", Some("none"))).unwrap(), Tls::None);
        assert!(tls_mode(&cfg("smtp.example.test", Some("ssl"))).is_err());
    }

    #[test]
    fn no_config_means_no_mail() {
        assert!(SmtpMail::build(None).unwrap().is_none());
    }

    #[test]
    fn a_username_needs_a_password_file() {
        let mut c = cfg("smtp.example.test", None);
        c.username = Some("relay".into());
        assert!(SmtpMail::build(Some(&c)).is_err());
    }

    #[test]
    fn a_line_break_cannot_add_a_header() {
        let from: Mailbox = "Codehaven <code@example.test>".parse().unwrap();
        assert!(message(&from, "a@example.test\r\nBcc: x@example.test", "s", "b").is_err());
        let m = message(&from, "a@example.test", "Line\r\nBcc: x@example.test", "b").unwrap();
        let raw = String::from_utf8(m.formatted()).unwrap();
        assert!(!raw.contains("\r\nBcc:"), "{raw}");
    }
}
