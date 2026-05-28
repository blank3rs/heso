//! Opt-in SSRF protection: a DNS resolver wrapper that refuses to
//! resolve to private, loopback, link-local, or cloud-metadata IPs.
//!
//! heso's default posture is "fetch any URL" — the primary use case is a
//! local CLI where reaching `localhost` is a feature, not a hole. But an
//! operator fronting heso as a hosted service that accepts URLs from
//! untrusted callers needs to refuse requests that resolve to internal
//! infrastructure (the classic server-side request forgery vector). That
//! protection is opt-in via [`HESO_BLOCK_PRIVATE_NETWORKS`] /
//! `--no-private-networks`; when off, this module is never installed.
//!
//! The check runs on the **resolved** address, post-DNS and pre-connect,
//! not on the hostname — so `localhost`, `127.0.0.1.nip.io`, and an
//! attacker-controlled domain with an A record pointing at `10.0.0.1`
//! are all caught at the same chokepoint.
//!
//! [`HESO_BLOCK_PRIVATE_NETWORKS`]: blocking_env_enabled

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

/// Environment variable an operator sets to refuse connections to
/// private/internal IPs across every network verb. Truthy values:
/// `1`, `true`, `yes`, `on` (case-insensitive). Checked once when the
/// [`crate::FetchEngine`]'s `reqwest::Client` is built, so a service
/// operator gets protection with no per-verb flag wiring.
pub const BLOCK_ENV_VAR: &str = "HESO_BLOCK_PRIVATE_NETWORKS";

/// Marker embedded in the resolver's error `Display`. The CLI walks the
/// `reqwest::Error` source chain for this string to distinguish a
/// deliberately-refused private address from an ordinary DNS failure
/// and emit the structured `private_network_blocked` envelope.
pub const BLOCK_ERROR_MARKER: &str = "private_network_blocked";

/// `true` when [`BLOCK_ENV_VAR`] is set to a truthy value.
pub fn blocking_env_enabled() -> bool {
    std::env::var(BLOCK_ENV_VAR)
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

/// `true` if `ip` belongs to a range heso refuses to connect to when
/// private-network blocking is on: loopback, RFC1918 private,
/// link-local (which covers the `169.254.169.254` cloud-metadata
/// address), unspecified, the CGNAT range `100.64.0.0/10` (which holds
/// Alibaba's `100.100.100.200` metadata endpoint), and the IPv4-mapped
/// IPv6 forms of all of the above.
pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_v4(v4),
        // An IPv4-mapped IPv6 address (`::ffff:a.b.c.d`) reaches the
        // same host as the bare v4 address, so classify it on the
        // unwrapped v4 to avoid a bypass.
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_blocked_v4(v4),
            None => is_blocked_v6(v6),
        },
    }
}

fn is_blocked_v4(ip: Ipv4Addr) -> bool {
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || is_cgnat_v4(ip)
}

fn is_blocked_v6(ip: Ipv6Addr) -> bool {
    ip.is_loopback()
        || ip.is_unspecified()
        // `fe80::/10` link-local. `Ipv6Addr::is_unicast_link_local` is
        // unstable, so test the prefix directly.
        || (ip.segments()[0] & 0xffc0) == 0xfe80
        // `fc00::/7` unique-local (the IPv6 analogue of RFC1918).
        || (ip.segments()[0] & 0xfe00) == 0xfc00
}

/// `100.64.0.0/10` — RFC 6598 carrier-grade NAT shared address space.
/// `std` doesn't classify it on stable, and Alibaba Cloud's metadata
/// endpoint `100.100.100.200` lives inside it.
fn is_cgnat_v4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    a == 100 && (64..=127).contains(&b)
}

/// If `url`'s host is a **literal** IP in a blocked range, return it.
///
/// reqwest only calls a custom [`Resolve`] for hostnames that need DNS;
/// a URL whose host is already an IP literal (`http://127.0.0.1/`,
/// `http://[::1]/`) skips resolution entirely and would bypass
/// [`PrivateNetworkGuard`]. The engine pre-flights every live request
/// through this check so literal-IP targets are refused at the same
/// policy as resolved ones.
pub fn blocked_literal_host_ip(url: &heso_core::Url) -> Option<IpAddr> {
    match url.host() {
        Some(url::Host::Ipv4(v4)) if is_blocked_v4(v4) => Some(IpAddr::V4(v4)),
        Some(url::Host::Ipv6(v6)) if is_blocked_ip(IpAddr::V6(v6)) => Some(IpAddr::V6(v6)),
        _ => None,
    }
}

/// Error returned when an address is in a blocked range — by
/// [`PrivateNetworkGuard`] after DNS, or by the engine's literal-IP
/// pre-flight ([`blocked_literal_host_ip`]). Its `Display` carries
/// [`BLOCK_ERROR_MARKER`] so the CLI can recognize it after it bubbles
/// up as a `reqwest::Error` or the crate's own error type.
#[derive(Debug)]
pub struct BlockedAddr {
    host: String,
    ip: IpAddr,
}

impl BlockedAddr {
    /// Build a blocked-address error for a literal-IP host refused by
    /// the engine's pre-flight check.
    pub fn new(host: impl Into<String>, ip: IpAddr) -> Self {
        Self {
            host: host.into(),
            ip,
        }
    }
}

impl std::fmt::Display for BlockedAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{BLOCK_ERROR_MARKER}: {} resolves to {} (private/loopback/metadata range)",
            self.host, self.ip
        )
    }
}

impl std::error::Error for BlockedAddr {}

/// A [`Resolve`] wrapper that delegates name resolution to the system
/// resolver (`getaddrinfo` via [`tokio::net::lookup_host`]) and then
/// rejects the whole resolution if **any** resolved address is in a
/// blocked range. Installed on the `reqwest::Client` only when
/// private-network blocking is enabled.
#[derive(Debug, Default)]
pub struct PrivateNetworkGuard;

impl Resolve for PrivateNetworkGuard {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            // Port 0: reqwest overwrites it with the URL's port (or the
            // scheme default) per the `Resolve` contract, so the value
            // we pass here is irrelevant to the eventual connection.
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?
                .collect();
            if let Some(blocked) = addrs.iter().find(|a| is_blocked_ip(a.ip())) {
                return Err(Box::new(BlockedAddr {
                    host,
                    ip: blocked.ip(),
                }) as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn ip(s: &str) -> IpAddr {
        IpAddr::from_str(s).unwrap()
    }

    #[test]
    fn blocks_loopback() {
        assert!(is_blocked_ip(ip("127.0.0.1")));
        assert!(is_blocked_ip(ip("127.0.0.53")));
        assert!(is_blocked_ip(ip("::1")));
    }

    #[test]
    fn blocks_rfc1918_private() {
        assert!(is_blocked_ip(ip("10.0.0.1")));
        assert!(is_blocked_ip(ip("172.16.5.4")));
        assert!(is_blocked_ip(ip("172.31.255.255")));
        assert!(is_blocked_ip(ip("192.168.1.1")));
    }

    #[test]
    fn blocks_link_local_and_metadata() {
        assert!(is_blocked_ip(ip("169.254.0.1")));
        // AWS / GCP / Azure instance-metadata endpoint.
        assert!(is_blocked_ip(ip("169.254.169.254")));
        assert!(is_blocked_ip(ip("fe80::1")));
    }

    #[test]
    fn blocks_unspecified() {
        assert!(is_blocked_ip(ip("0.0.0.0")));
        assert!(is_blocked_ip(ip("::")));
    }

    #[test]
    fn blocks_cgnat_and_alibaba_metadata() {
        assert!(is_blocked_ip(ip("100.64.0.0")));
        // Alibaba Cloud instance-metadata endpoint.
        assert!(is_blocked_ip(ip("100.100.100.200")));
        assert!(is_blocked_ip(ip("100.127.255.255")));
        // Just outside the /10 — public.
        assert!(!is_blocked_ip(ip("100.128.0.1")));
        assert!(!is_blocked_ip(ip("100.63.255.255")));
    }

    #[test]
    fn blocks_ipv6_unique_local() {
        assert!(is_blocked_ip(ip("fc00::1")));
        assert!(is_blocked_ip(ip("fd12:3456::1")));
    }

    #[test]
    fn blocks_ipv4_mapped_loopback() {
        assert!(is_blocked_ip(ip("::ffff:127.0.0.1")));
        assert!(is_blocked_ip(ip("::ffff:10.0.0.1")));
        assert!(is_blocked_ip(ip("::ffff:169.254.169.254")));
    }

    #[test]
    fn allows_public_addresses() {
        assert!(!is_blocked_ip(ip("1.1.1.1")));
        assert!(!is_blocked_ip(ip("8.8.8.8")));
        assert!(!is_blocked_ip(ip("93.184.216.34"))); // example.com
        assert!(!is_blocked_ip(ip("2606:4700:4700::1111"))); // cloudflare v6
    }

    #[test]
    fn env_truthy_parsing() {
        // The parser only inspects the value; exercise it directly to
        // avoid mutating process-global env from a test.
        for v in ["1", "true", "TRUE", "Yes", "on"] {
            assert!(matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            ));
        }
        for v in ["0", "false", "no", "", "off"] {
            assert!(!matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            ));
        }
    }

    #[test]
    fn blocked_addr_display_carries_marker() {
        let e = BlockedAddr {
            host: "metadata.example".to_owned(),
            ip: ip("169.254.169.254"),
        };
        assert!(e.to_string().contains(BLOCK_ERROR_MARKER));
    }
}
