//! Fail-closed guard against server-side request forgery (SSRF) for the
//! host-managed remote applet backend and the model helper.
//!
//! Both backends previously accepted any HTTPS host and used the resolved
//! address as-is, so a program could reach private, link-local or cloud
//! metadata destinations, and a hostname that resolved to a public address at
//! check time could resolve to a private one at connect time (DNS rebinding).
//!
//! This module is deliberately dependency-free (standard library only) and
//! has no network access: it classifies endpoint hosts and resolved socket
//! addresses. Callers enforce the verdict at two layers:
//!
//! - by name, before any resolution or socket use, for literal IP endpoints
//!   and loopback names;
//! - at connect time, by filtering the resolver output, so a rebinding
//!   hostname whose address set contains a blocked address is refused even
//!   when the name itself looks benign.
//!
//! Hostname checks alone are not sufficient, and neither layer claims success
//! on its own: the literal check cannot see rebinding, and the resolver
//! filter cannot produce a fail-fast refusal for a literal. Both layers
//! preserve explicit policy grants (an origin listed in `network_origins`)
//! and the explicit loopback-fixture mode (`allow_loopback_http_fixture`, or
//! the model helper's loopback endpoints).

use std::collections::BTreeSet;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// A refused network destination. The message names the host, origin or
/// address that was refused; it never carries credentials or bodies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockedDestination(pub String);

impl fmt::Display for BlockedDestination {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for BlockedDestination {}

/// Whether an IP address must be refused without an explicit policy grant.
///
/// The block covers loopback, unspecified, RFC1918 private ranges, shared
/// CGNAT space (100.64.0.0/10, which contains the Alibaba metadata service),
/// link-local (169.254.0.0/16 and fe80::/10, which contain the cloud metadata
/// endpoints), ULA (fc00::/7), multicast, broadcast, reserved and
/// documentation ranges, plus IPv4-mapped IPv6 forms of blocked addresses.
/// Everything else is treated as publicly routable.
pub fn ip_is_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(addr) => ipv4_is_blocked(addr),
        IpAddr::V6(addr) => {
            if let Some(mapped) = addr.to_ipv4() {
                return ipv4_is_blocked(mapped);
            }
            ipv6_is_blocked(addr)
        }
    }
}

fn ipv4_is_blocked(addr: Ipv4Addr) -> bool {
    let o = addr.octets();
    match o {
        [0, _, _, _] => true,
        [10, _, _, _] => true,
        [100, 64..=127, _, _] => true,
        [127, _, _, _] => true,
        [169, 254, _, _] => true,
        [172, 16..=31, _, _] => true,
        [192, 168, _, _] => true,
        [192, 0, 0, _] => true,
        [192, 0, 2, _] | [198, 51, 100, _] | [203, 0, 113, _] => true,
        [192, 88, 99, _] => true,
        [198, 18..=19, _, _] => true,
        [224..=239, _, _, _] => true,
        [240..=255, _, _, _] => true,
        _ => {
            addr.is_unspecified()
                || addr.is_loopback()
                || addr.is_private()
                || addr.is_link_local()
                || addr.is_broadcast()
                || addr.is_multicast()
                || addr.is_documentation()
        }
    }
}

fn ipv6_is_blocked(addr: Ipv6Addr) -> bool {
    if addr.is_unspecified() || addr.is_loopback() || addr.is_multicast() {
        return true;
    }
    let s = addr.segments();
    if s[0] & 0xffc0 == 0xfe80 {
        return true;
    }
    if s[0] & 0xfe00 == 0xfc00 {
        return true;
    }
    if s[0] == 0x2001 && s[1] == 0x0db8 {
        return true;
    }
    if s[0] == 0x2001 && s[1] == 0x0002 {
        return true;
    }
    if s[0] == 0x2002 {
        return true;
    }
    if s[0] == 0x2001 && s[1] == 0x0000 {
        return true;
    }
    if s[0] == 0x0064 && s[1] == 0xff9b {
        return true;
    }
    if s[0] == 0x0100 && s[1] == 0 && s[2] == 0 && s[3] == 0 {
        return true;
    }
    false
}

/// Classification of an endpoint host string (without brackets or port).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostClass {
    /// A literal IP address that must be refused without an explicit grant.
    BlockedLiteral(IpAddr),
    /// A literal public IP address; still subject to resolver filtering.
    PublicLiteral(IpAddr),
    /// `localhost` (optionally with a trailing dot); loopback by name.
    LoopbackName,
    /// Any other DNS name; subject to resolver filtering at connect time.
    OtherName,
}

/// Classify an endpoint host without resolving it.
pub fn classify_host(host: &str) -> HostClass {
    let bare = host.strip_suffix('.').unwrap_or(host);
    if let Ok(ip) = bare.parse::<IpAddr>() {
        return if ip_is_blocked(ip) {
            HostClass::BlockedLiteral(ip)
        } else {
            HostClass::PublicLiteral(ip)
        };
    }
    if bare.eq_ignore_ascii_case("localhost") {
        return HostClass::LoopbackName;
    }
    HostClass::OtherName
}

/// Fail-closed check of one endpoint host before any resolution or socket
/// use. `origin` is the endpoint's canonical origin, `granted_origins` the
/// explicitly granted origins (empty when no policy grants this origin).
/// `allow_loopback` is the explicit loopback-fixture mode for transports
/// that support one; the model helper passes `true` because its HTTP
/// endpoints are loopback-only by validation.
pub fn check_endpoint_host(
    host: &str,
    origin: &str,
    allow_loopback: bool,
    granted_origins: &BTreeSet<String>,
) -> Result<(), BlockedDestination> {
    match classify_host(host) {
        HostClass::BlockedLiteral(ip) => {
            if granted_origins.contains(origin) {
                return Ok(());
            }
            if allow_loopback && ip.is_loopback() {
                return Ok(());
            }
            Err(BlockedDestination(format!(
                "refusing non-public endpoint host {host} for origin {origin}: \
                 grant the origin explicitly to allow it"
            )))
        }
        HostClass::LoopbackName => {
            if allow_loopback || granted_origins.contains(origin) {
                return Ok(());
            }
            Err(BlockedDestination(format!(
                "refusing loopback endpoint host {host} for origin {origin}: \
                 grant the origin explicitly to allow it"
            )))
        }
        HostClass::PublicLiteral(_) | HostClass::OtherName => Ok(()),
    }
}

/// Fail-closed check of a resolved address set at connect time. Every
/// address is checked: a set that mixes public and blocked addresses (the
/// DNS-rebinding shape) is refused as a whole, as is an empty set.
/// `granted_origin` is true only when the endpoint's own origin was
/// explicitly granted by policy.
pub fn check_resolved_addrs(
    addrs: &[SocketAddr],
    allow_loopback: bool,
    granted_origin: bool,
) -> Result<(), BlockedDestination> {
    if addrs.is_empty() {
        return Err(BlockedDestination(
            "refusing to connect: name resolution returned no addresses".to_owned(),
        ));
    }
    for addr in addrs {
        let ip = addr.ip();
        if ip_is_blocked(ip) {
            if granted_origin {
                continue;
            }
            if allow_loopback && ip.is_loopback() {
                continue;
            }
            return Err(BlockedDestination(format!(
                "refusing to connect: resolved address {addr} is not publicly routable"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddrV4};

    fn socket(ip: &str, port: u16) -> SocketAddr {
        SocketAddr::new(ip.parse().unwrap(), port)
    }

    fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    #[test]
    fn cloud_metadata_and_private_ranges_are_blocked() {
        for ip in [
            "169.254.169.254",
            "169.254.169.253",
            "100.100.100.200",
            "10.0.0.1",
            "10.255.255.255",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.0.1",
            "192.168.255.255",
            "127.0.0.1",
            "127.0.53.53",
            "0.0.0.0",
            "169.254.10.20",
            "100.64.0.1",
            "100.127.255.255",
            "192.0.2.1",
            "198.51.100.7",
            "203.0.113.9",
            "198.18.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "240.0.0.1",
        ] {
            assert!(ip_is_blocked(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn public_addresses_are_not_blocked() {
        for ip in [
            "8.8.8.8",
            "1.1.1.1",
            "93.184.216.34",
            "172.15.255.255",
            "172.32.0.1",
            "100.63.255.255",
            "100.128.0.1",
            "192.167.255.255",
            "192.169.0.1",
        ] {
            assert!(!ip_is_blocked(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn ipv6_special_ranges_are_blocked() {
        for ip in [
            "::1",
            "::",
            "fe80::1",
            "fe80::ffff:ffff:ffff:ffff",
            "fc00::1",
            "fd00::1",
            "ff02::1",
            "2001:db8::1",
            "2001:2::1",
            "2002:c000:200::1",
            "2001::1",
            "64:ff9b::0808:0808",
            "::ffff:10.0.0.1",
            "::ffff:169.254.169.254",
        ] {
            assert!(ip_is_blocked(ip.parse().unwrap()), "{ip}");
        }
        assert!(!ip_is_blocked("2606:4700:4700::1111".parse().unwrap()));
        assert!(!ip_is_blocked("2001:4860:4860::8888".parse().unwrap()));
        assert!(!ip_is_blocked("::ffff:8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn literal_endpoints_are_refused_without_a_grant() {
        let empty = BTreeSet::new();
        let origin = "https://169.254.169.254";
        assert!(check_endpoint_host("169.254.169.254", origin, false, &empty).is_err());
        assert!(check_endpoint_host("169.254.169.254", origin, true, &empty).is_err());
        let granted = BTreeSet::from([origin.to_owned()]);
        assert!(check_endpoint_host("169.254.169.254", origin, false, &granted).is_ok());

        let rfc1918 = "https://10.0.0.1";
        assert!(check_endpoint_host("10.0.0.1", rfc1918, false, &empty).is_err());
        assert!(check_endpoint_host("8.8.8.8", "https://8.8.8.8", false, &empty).is_ok());
        assert!(check_endpoint_host("example.test", "https://example.test", false, &empty).is_ok());

        assert!(check_endpoint_host("localhost", "http://localhost:8080", false, &empty).is_err());
        assert!(check_endpoint_host("localhost", "http://localhost:8080", true, &empty).is_ok());
        assert!(
            check_endpoint_host(
                "LOCALHOST.",
                "http://localhost:8080",
                false,
                &BTreeSet::from(["http://localhost:8080".to_owned()])
            )
            .is_ok()
        );
        assert!(check_endpoint_host("127.0.0.1", "https://127.0.0.1", false, &empty).is_err());
        assert!(check_endpoint_host("127.0.0.1", "https://127.0.0.1", true, &empty).is_ok());

        assert_eq!(v4(10, 0, 0, 1).is_loopback(), false);
    }

    #[test]
    fn resolved_sets_are_pinned_fail_closed() {
        let empty: &[SocketAddr] = &[];
        assert!(check_resolved_addrs(empty, false, false).is_err());
        assert!(check_resolved_addrs(&[socket("93.184.216.34", 443)], false, false).is_ok());
        // DNS-rebinding shape: one public and one private address.
        assert!(
            check_resolved_addrs(
                &[socket("93.184.216.34", 443), socket("10.0.0.1", 443)],
                false,
                false
            )
            .is_err()
        );
        // Order does not matter: a tainted tail is still refused.
        assert!(
            check_resolved_addrs(
                &[socket("10.0.0.1", 443), socket("93.184.216.34", 443)],
                false,
                false
            )
            .is_err()
        );
        assert!(check_resolved_addrs(&[socket("10.0.0.1", 443)], false, false).is_err());
        // An explicit grant preserves the destination.
        assert!(check_resolved_addrs(&[socket("10.0.0.1", 443)], false, true).is_ok());
        // Loopback fixtures still work when the transport allows them.
        assert!(check_resolved_addrs(&[socket("127.0.0.1", 80)], true, false).is_ok());
        assert!(check_resolved_addrs(&[socket("127.0.0.1", 80)], false, false).is_err());
        assert!(check_resolved_addrs(&[socket("::1", 443)], true, false).is_ok());
        // Metadata over a rebinding name is refused even with loopback allowed.
        assert!(
            check_resolved_addrs(
                &[socket("127.0.0.1", 80), socket("169.254.169.254", 80)],
                true,
                false
            )
            .is_err()
        );
    }

    #[test]
    fn rfc1918_boundaries_are_exact() {
        assert!(!ip_is_blocked(v4(172, 15, 255, 255)));
        assert!(ip_is_blocked(v4(172, 16, 0, 0)));
        assert!(ip_is_blocked(v4(172, 31, 255, 255)));
        assert!(!ip_is_blocked(v4(172, 32, 0, 0)));
        assert!(ip_is_blocked(v4(127, 0, 0, 1)));
        assert!(!ip_is_blocked(v4(128, 0, 0, 1)));
        let _ = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 80);
    }
}
