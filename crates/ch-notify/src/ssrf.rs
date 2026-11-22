//! Keeping webhooks away from the inside of the network (ARCHITECTURE.md sections 7.6 and 8.5).
//!
//! A webhook is a request the server makes on someone else's say-so, so it must not reach
//! the host's loopback, the docker network, the cloud metadata address or anything else
//! private. Two checks: the URL when the hook is saved, and the addresses the name resolves
//! to on every delivery. The delivery then connects to the address that was checked, not to
//! the name, so a DNS answer that changes between the check and the connection (rebinding)
//! gets no second chance. `webhooks.allow_private` turns the address check off, for an
//! instance that really does want hooks to its own network.

use ch_core::error::{Error, FieldError, Result};
use std::net::{IpAddr, SocketAddr};

pub use ch_core::net::{is_public_ip, set_blocked_ips};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookTarget {
    pub url: url::Url,
    pub host: String,
    pub port: u16,
}

fn bad(msg: &str) -> Error {
    Error::Validation(vec![FieldError::custom("Hook", "url", msg)])
}

/// The checks that need no network: scheme, host, no credentials in the URL, length. A literal
/// private address is refused here too, so the mistake is caught when the hook is saved.
pub fn parse_target(raw: &str, allow_private: bool) -> Result<HookTarget> {
    if raw.len() > 2048 {
        return Err(bad("That URL is too long."));
    }
    let url = url::Url::parse(raw.trim()).map_err(|_| bad("That is not a valid URL."))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(bad("Use an http or https URL."));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(bad("Leave the username and password out of the URL."));
    }
    let host = match url.host() {
        Some(url::Host::Domain(d)) => d.to_string(),
        Some(url::Host::Ipv4(v4)) => v4.to_string(),
        Some(url::Host::Ipv6(v6)) => v6.to_string(),
        None => return Err(bad("That URL has no host.")),
    };
    let port = url.port_or_known_default().ok_or_else(|| bad("That URL has no port."))?;
    if !allow_private {
        if let Ok(ip) = host.parse::<IpAddr>()
            && !is_public_ip(ip)
        {
            return Err(bad("Webhooks cannot point at a private or loopback address."));
        }
        let lower = host.to_ascii_lowercase();
        if lower == "localhost" || lower.ends_with(".localhost") || lower.ends_with(".internal") || lower.ends_with(".local") {
            return Err(bad("Webhooks cannot point at a local name."));
        }
    }
    Ok(HookTarget { url, host, port })
}

/// The check for when a hook is saved: refuses a name that resolves to a private address. A
/// name that does not resolve right now is let through, since the host may simply be down; the
/// delivery checks again and records what happened.
pub async fn check_saved_target(target: &HookTarget, allow_private: bool) -> Result<()> {
    if allow_private {
        return Ok(());
    }
    match resolve_checked(target, false).await {
        Err(Error::Unavailable(m)) if m.contains("private address") => {
            Err(bad("That name points at a private address, which webhooks cannot reach."))
        }
        _ => Ok(()),
    }
}

/// Resolves the host and returns the addresses to connect to. With `allow_private` false,
/// every address the name resolves to must be public, so a name that resolves to both a public
/// and a private address is refused rather than gambled on.
pub async fn resolve_checked(target: &HookTarget, allow_private: bool) -> Result<Vec<SocketAddr>> {
    let host = target.host.trim_start_matches('[').trim_end_matches(']').to_string();
    let addrs: Vec<SocketAddr> = if let Ok(ip) = host.parse::<IpAddr>() {
        vec![SocketAddr::new(ip, target.port)]
    } else {
        tokio::net::lookup_host((host.as_str(), target.port))
            .await
            .map_err(|e| Error::Unavailable(format!("could not resolve {host}: {e}")))?
            .collect()
    };
    if addrs.is_empty() {
        return Err(Error::Unavailable(format!("{host} did not resolve")));
    }
    if !allow_private && let Some(a) = addrs.iter().find(|a| !is_public_ip(a.ip())) {
        return Err(Error::Unavailable(format!("{host} resolves to a private address ({}), which webhooks may not reach", a.ip())));
    }
    Ok(addrs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn public_addresses_pass() {
        for a in [
            "8.8.8.8",
            "1.1.1.1",
            "93.184.216.34",
            "172.15.255.255",
            "172.32.0.1",
            "100.63.255.255",
            "100.128.0.1",
            "2606:4700:4700::1111",
            "2001:4860:4860::8888",
        ] {
            assert!(is_public_ip(ip(a)), "{a}");
        }
    }

    #[test]
    fn private_loopback_and_special_addresses_are_refused() {
        for a in [
            "127.0.0.1",
            "127.255.255.254",
            "10.0.0.1",
            "10.255.255.255",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.0.1",
            "192.168.255.255",
            "169.254.169.254",
            "169.254.0.1",
            "100.64.0.1",
            "100.127.255.255",
            "0.0.0.0",
            "0.1.2.3",
            "255.255.255.255",
            "224.0.0.1",
            "239.1.1.1",
            "192.0.2.1",
            "198.51.100.1",
            "203.0.113.1",
            "198.18.0.1",
            "198.19.255.255",
            "240.0.0.1",
            "192.0.0.8",
            "::1",
            "::",
            "fe80::1",
            "fd00::1",
            "fc00::1",
            "ff02::1",
            "2001:db8::1",
            "2001::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::ffff:169.254.169.254",
            "64:ff9b::7f00:1",
            "64:ff9b::a9fe:a9fe",
            "2002:7f00:1::",
            "2002:a9fe:a9fe::1",
            "fec0::1",
        ] {
            assert!(!is_public_ip(ip(a)), "{a} must be refused");
        }
    }

    #[test]
    fn urls() {
        assert!(parse_target("https://example.org/hook", false).is_ok());
        assert!(parse_target("http://example.org:8080/hook?x=1", false).is_ok());
        let t = parse_target("https://example.org/hook", false).unwrap();
        assert_eq!((t.host.as_str(), t.port), ("example.org", 443));
        for bad in [
            "",
            "not a url",
            "ftp://example.org/",
            "file:///etc/passwd",
            "gopher://example.org",
            "javascript:alert(1)",
            "https://user:pw@example.org/",
            "https://user@example.org/",
            "http://127.0.0.1/",
            "http://127.0.0.1:2222/",
            "http://[::1]/",
            "http://10.1.2.3/hook",
            "http://169.254.169.254/latest/meta-data",
            "http://localhost/",
            "http://LOCALHOST:3200/",
            "http://app.localhost/",
            "http://db.internal/",
            "http://printer.local/",
            "http://0.0.0.0/",
            "http://[::ffff:127.0.0.1]/",
            &format!("https://example.org/{}", "a".repeat(2100)),
        ] {
            assert!(parse_target(bad, false).is_err(), "{bad:?}");
        }
        // Numeric tricks normalise to the dotted address, which is then refused.
        for tricky in ["http://2130706433/", "http://0x7f.0.0.1/", "http://127.1/", "http://0177.0.0.1/"] {
            assert!(parse_target(tricky, false).is_err(), "{tricky}");
        }
        // The instance owner can allow them.
        assert!(parse_target("http://127.0.0.1:9/", true).is_ok());
        assert!(parse_target("http://localhost/", true).is_ok());
    }

    #[test]
    fn the_hosts_own_public_address_is_refused() {
        // Only this address is listed, so the tests that run beside this one are not affected.
        let host = "8.8.4.4";
        assert!(is_public_ip(ip(host)), "public until it is listed");
        set_blocked_ips(&[ip(host)]);
        assert!(!is_public_ip(ip(host)));
        assert!(!is_public_ip(ip("::ffff:8.8.4.4")), "the IPv6 coat does not hide it");
        assert!(parse_target("http://8.8.4.4:11434/api/show", false).is_err());
        assert!(parse_target("http://8.8.4.5/", false).is_ok(), "a neighbour is not the host");
        // allow_private is the owner's switch for everything, this list included.
        assert!(parse_target("http://8.8.4.4/", true).is_ok());
        set_blocked_ips(&[]);
        assert!(is_public_ip(ip(host)));
    }

    #[tokio::test]
    async fn saving_refuses_a_name_that_resolves_private_but_lets_a_dead_name_through() {
        let t = parse_target("http://localhost:9/", true).unwrap();
        assert!(check_saved_target(&t, false).await.is_err());
        assert!(check_saved_target(&t, true).await.is_ok());
        let unresolvable = parse_target("http://no-such-host.invalid/hook", false).unwrap();
        assert!(check_saved_target(&unresolvable, false).await.is_ok(), "the host may only be down");
    }

    #[tokio::test]
    async fn resolution_refuses_private_answers() {
        let t = parse_target("http://127.0.0.1:9/", true).unwrap();
        assert!(resolve_checked(&t, false).await.is_err());
        assert_eq!(resolve_checked(&t, true).await.unwrap().len(), 1);
        // `localhost` resolves to loopback, so even though the name passed parse_target with
        // allow_private, the address check catches it when private is off.
        let t = parse_target("http://localhost:9/", true).unwrap();
        assert!(resolve_checked(&t, false).await.is_err());
        let t = parse_target("http://8.8.8.8/", false).unwrap();
        assert_eq!(resolve_checked(&t, false).await.unwrap()[0].ip(), ip("8.8.8.8"));
    }
}
