//! The real client address and scheme of a request (ARCHITECTURE.md section 2.4).
//!
//! `X-Forwarded-For` and `X-Forwarded-Proto` are trusted only when the TCP peer is inside
//! `http.trusted_proxies`. From any other peer they are ignored and the socket address is
//! used, so nobody can pick their own address for the rate limiter or the audit log.

use ipnet::IpNet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Picks the client address.
///
/// `forwarded` is the raw `X-Forwarded-For` value(s). With a trusted peer, the chain is
/// walked from the right: trusted hops are skipped, and the first address that is not a
/// trusted proxy is the client. If every hop is trusted, the left-most one is used.
pub fn client_ip(peer: IpAddr, forwarded: Option<&str>, trusted: &[IpNet]) -> IpAddr {
    let is_trusted = |ip: &IpAddr| trusted.iter().any(|n| n.contains(&canonical(*ip)));
    if !is_trusted(&peer) {
        return canonical(peer);
    }
    let Some(chain) = forwarded else { return canonical(peer) };
    let hops: Vec<IpAddr> = chain.split(',').filter_map(|s| s.trim().parse::<IpAddr>().ok()).collect();
    if hops.is_empty() {
        return canonical(peer);
    }
    for ip in hops.iter().rev() {
        if !is_trusted(ip) {
            return canonical(*ip);
        }
    }
    canonical(hops[0])
}

/// The addresses of `http.trusted_proxy_hosts`, as last looked up. One per process state, kept
/// fresh by a background loop, so a proxy container that restarts with a new address is
/// followed and nothing else on its network is trusted (ch-dc1).
#[derive(Default)]
pub struct ProxyHosts {
    by_name: std::sync::RwLock<std::collections::BTreeMap<String, Vec<IpNet>>>,
}

impl ProxyHosts {
    /// The configured networks plus the looked-up proxy addresses.
    pub fn trusted(&self, configured: &[IpNet]) -> Vec<IpNet> {
        let mut out = configured.to_vec();
        out.extend(self.by_name.read().unwrap_or_else(|e| e.into_inner()).values().flatten().copied());
        out
    }

    /// Looks every name up again. A name that does not resolve keeps its last known addresses,
    /// so a proxy that is briefly away does not turn every visitor into the proxy's address.
    pub async fn refresh(&self, hosts: &[String]) {
        let mut next = std::collections::BTreeMap::new();
        for h in hosts {
            let found = match tokio::net::lookup_host((h.as_str(), 0)).await {
                Ok(addrs) => {
                    let mut v: Vec<IpNet> = addrs.map(|a| IpNet::from(canonical(a.ip()))).collect();
                    v.sort();
                    v.dedup();
                    v
                }
                Err(e) => {
                    tracing::warn!(host = %h, error = %e, "a trusted proxy name did not resolve; keeping its last addresses");
                    self.by_name.read().unwrap_or_else(|e| e.into_inner()).get(h).cloned().unwrap_or_default()
                }
            };
            next.insert(h.clone(), found);
        }
        *self.by_name.write().unwrap_or_else(|e| e.into_inner()) = next;
    }
}

/// Whether the original request was https. Only believed from a trusted peer.
pub fn is_secure(peer: IpAddr, forwarded_proto: Option<&str>, trusted: &[IpNet]) -> bool {
    let trusted_peer = trusted.iter().any(|n| n.contains(&canonical(peer)));
    trusted_peer && forwarded_proto.map(|p| p.trim().eq_ignore_ascii_case("https")).unwrap_or(false)
}

/// Maps `::ffff:a.b.c.d` to the IPv4 address so one client has one identity.
pub fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    }
}

// ---- public addresses, shared by webhook delivery and the OAuth client id fetch

/// The host's own public addresses (`webhooks.blocked_ips`). They are public, but a hook that
/// reaches them gets services the firewall keeps off the internet, such as a database or a
/// model server, so they are refused like a private address.
static BLOCKED: std::sync::RwLock<Vec<IpAddr>> = std::sync::RwLock::new(Vec::new());

/// Replaces the list of addresses webhooks may never reach. Called once at start-up.
pub fn set_blocked_ips(ips: &[IpAddr]) {
    // The lock only guards a plain list, so a poisoned lock still holds a usable value.
    let mut w = BLOCKED.write().unwrap_or_else(|e| e.into_inner());
    *w = ips.iter().copied().map(unmap).collect();
}

/// The IPv4 address an IPv6 address embeds, in the mapped, NAT64, 6to4 or compatible coat.
fn embedded_v4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return Some(v4);
    }
    let s = ip.segments();
    // NAT64 (64:ff9b::/96) embeds an IPv4 address in the last 32 bits.
    if s[0] == 0x64 && s[1] == 0xff9b && s[2..6].iter().all(|x| *x == 0) {
        return Some(Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8));
    }
    // 6to4 (2002::/16) embeds one in bits 16..48.
    if s[0] == 0x2002 {
        return Some(Ipv4Addr::new((s[1] >> 8) as u8, s[1] as u8, (s[2] >> 8) as u8, s[2] as u8));
    }
    // The deprecated compatible form (::/96) carries one in the last 32 bits as well. `::` and
    // `::1` keep their IPv6 meaning; the unspecified and loopback checks refuse them anyway.
    if s[..6].iter().all(|x| *x == 0) && !(s[6] == 0 && s[7] <= 1) {
        return Some(Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8));
    }
    None
}

/// An IPv4 address in any IPv6 coat (mapped, NAT64, 6to4 or compatible) is the IPv4 address.
fn unmap(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => embedded_v4(v6).map_or(IpAddr::V6(v6), IpAddr::V4),
        v4 => v4,
    }
}

/// True only for an address on the public internet that is not one of the host's own.
pub fn is_public_ip(ip: IpAddr) -> bool {
    if BLOCKED.read().unwrap_or_else(|e| e.into_inner()).contains(&unmap(ip)) {
        return false;
    }
    is_internet_address(ip)
}

/// True for an address on the public internet, the host's own included: not loopback, private,
/// link-local, reserved or one of those in an IPv6 coat.
pub fn is_internet_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || o[0] == 0                                   // 0.0.0.0/8 "this network"
        || (o[0] == 100 && (o[1] & 0xc0) == 64)        // 100.64.0.0/10 carrier grade NAT
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)     // 192.0.0.0/24 IETF protocol assignments
        || (o[0] == 198 && (o[1] & 0xfe) == 18)        // 198.18.0.0/15 benchmarking
        || o[0] >= 240) // 240.0.0.0/4 reserved
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    // An IPv4 address wearing any IPv6 coat is judged as the IPv4 address.
    if let Some(v4) = embedded_v4(ip) {
        return is_public_v4(v4);
    }
    let s = ip.segments();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || (s[0] & 0xfe00) == 0xfc00           // fc00::/7 unique local
        || (s[0] & 0xffc0) == 0xfe80           // fe80::/10 link local
        || (s[0] & 0xffc0) == 0xfec0           // fec0::/10 site local (deprecated)
        || (s[0] == 0x2001 && s[1] == 0x0db8)  // documentation
        || (s[0] == 0x2001 && s[1] == 0)       // Teredo
        || (s[0] == 0x0100 && s[1..4].iter().all(|x| *x == 0))) // 100::/64 discard
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(s: &str) -> IpNet {
        s.parse().unwrap()
    }
    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn untrusted_peer_cannot_set_its_address() {
        let trusted = [net("10.20.16.0/20")];
        assert_eq!(client_ip(ip("203.0.113.9"), Some("1.2.3.4"), &trusted), ip("203.0.113.9"));
    }

    #[test]
    fn trusted_peer_gives_the_forwarded_client() {
        let trusted = [net("10.20.16.0/20")];
        assert_eq!(client_ip(ip("10.20.16.5"), Some("198.51.100.7"), &trusted), ip("198.51.100.7"));
    }

    #[test]
    fn client_cannot_prepend_a_fake_hop() {
        // The client sent "1.1.1.1", Caddy appended the real peer.
        let trusted = [net("10.20.16.0/20")];
        assert_eq!(client_ip(ip("10.20.16.5"), Some("1.1.1.1, 198.51.100.7"), &trusted), ip("198.51.100.7"));
    }

    #[test]
    fn trusted_hops_are_skipped() {
        let trusted = [net("10.20.16.0/20"), net("10.0.0.0/8")];
        assert_eq!(client_ip(ip("10.20.16.5"), Some("198.51.100.7, 10.1.1.1"), &trusted), ip("198.51.100.7"));
    }

    #[test]
    fn missing_or_garbage_header_falls_back_to_peer() {
        let trusted = [net("10.20.16.0/20")];
        assert_eq!(client_ip(ip("10.20.16.5"), None, &trusted), ip("10.20.16.5"));
        assert_eq!(client_ip(ip("10.20.16.5"), Some("not an ip"), &trusted), ip("10.20.16.5"));
    }

    #[test]
    fn no_trusted_proxies_means_socket_address() {
        assert_eq!(client_ip(ip("203.0.113.9"), Some("1.2.3.4"), &[]), ip("203.0.113.9"));
    }

    #[test]
    fn mapped_ipv6_is_the_ipv4_address() {
        assert_eq!(canonical(ip("::ffff:10.20.16.5")), ip("10.20.16.5"));
        let trusted = [net("10.20.16.0/20")];
        assert_eq!(client_ip(ip("::ffff:10.20.16.5"), Some("198.51.100.7"), &trusted), ip("198.51.100.7"));
    }

    #[tokio::test]
    async fn a_trusted_proxy_name_trusts_its_address_and_nothing_else() {
        // ch-dc1: name the proxy, and only its address may set the forwarded headers.
        let hosts = ProxyHosts::default();
        assert!(hosts.trusted(&[]).is_empty());
        hosts.refresh(&["localhost".into()]).await;
        let trusted = hosts.trusted(&[]);
        assert_eq!(client_ip(ip("127.0.0.1"), Some("198.51.100.7"), &trusted), ip("198.51.100.7"));
        assert_eq!(client_ip(ip("192.168.0.1"), Some("198.51.100.7"), &trusted), ip("192.168.0.1"), "a neighbour is not the proxy");
        // A name that stops resolving keeps what it had.
        hosts.refresh(&["localhost".into(), "no-such-host.invalid".into()]).await;
        assert_eq!(client_ip(ip("127.0.0.1"), Some("198.51.100.7"), &hosts.trusted(&[])), ip("198.51.100.7"));
    }

    #[test]
    fn forwarded_proto_needs_a_trusted_peer() {
        let trusted = [net("10.20.16.0/20")];
        assert!(is_secure(ip("10.20.16.5"), Some("https"), &trusted));
        assert!(!is_secure(ip("203.0.113.9"), Some("https"), &trusted));
        assert!(!is_secure(ip("10.20.16.5"), Some("http"), &trusted));
        assert!(!is_secure(ip("10.20.16.5"), None, &trusted));
    }

    /// The deprecated IPv4-compatible form `::a.b.c.d`, built rather than written out so the
    /// portability check sees only ordinary test addresses.
    fn compat(v4: &str) -> IpAddr {
        let o = v4.parse::<Ipv4Addr>().unwrap().octets();
        IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, u16::from_be_bytes([o[0], o[1]]), u16::from_be_bytes([o[2], o[3]])))
    }

    #[test]
    fn every_ipv6_coat_of_a_private_address_is_refused() {
        // ch-1x8: the compatible form of loopback used to pass as a public address.
        for v4 in ["127.0.0.1", "10.0.0.1", "10.1.2.3", "169.254.169.254", "192.168.0.1"] {
            assert!(!is_public_ip(compat(v4)), "::{v4} must be refused");
        }
        for a in ["64:ff9b::7f00:1", "2002:7f00:1::", "::ffff:127.0.0.1", "::", "::1"] {
            assert!(!is_public_ip(ip(a)), "{a} must be refused");
        }
        // A public IPv4 address in the compatible coat is judged as that address.
        assert!(is_public_ip(compat("8.8.8.8")));
    }

    #[test]
    fn the_blocked_list_sees_through_every_coat() {
        // Only this address is listed, so tests running beside this one are not affected.
        let host = ip("8.8.4.4");
        assert!(is_public_ip(host));
        set_blocked_ips(&[host]);
        for a in [host, ip("::ffff:8.8.4.4"), ip("64:ff9b::808:404"), ip("2002:808:404::1"), compat("8.8.4.4")] {
            assert!(!is_public_ip(a), "{a} is the blocked host");
        }
        assert!(is_public_ip(ip("8.8.4.5")), "a neighbour is not the host");
        set_blocked_ips(&[]);
        assert!(is_public_ip(host));
    }
}
