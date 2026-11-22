//! Which addresses may be dialled: public IPv4 unicast only, never the stack's own networks or
//! the service's own addresses. The check runs on every candidate address of every connection,
//! and the forwarder connects only to an address that passed it.
//!
//! No setting lifts any refusal; the operator's deny list only adds refusals. The only way to widen the public check is
//! [`Policy::with_public_check`], which exists in test builds only (`cfg(test)`), so neither
//! the binary nor any configuration can reach it.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use chelabase_http::outbound::{Net, is_public};

use crate::allow::Settings;

/// Why an address is not dialled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// One of the service's own addresses.
    OwnAddress,
    /// In `CHELABASE_INTERNAL_SUBNETS`.
    InternalSubnet,
    /// In the operator's deny list, `CHELABASE_COMPUTE_EGRESS_DENY`.
    Denied,
    /// Loopback, private, CGNAT, link-local, ULA, documentation, multicast, reserved or an IPv6
    /// form embedding one ([`is_public`]).
    NotPublic,
    /// A public IPv6 address: the egress network has no IPv6, so only IPv4 is dialled.
    NotIpv4,
}

impl Refusal {
    pub fn as_str(self) -> &'static str {
        match self {
            Refusal::OwnAddress => "own address",
            Refusal::InternalSubnet => "internal subnet",
            Refusal::Denied => "denied by CHELABASE_COMPUTE_EGRESS_DENY",
            Refusal::NotPublic => "not public",
            Refusal::NotIpv4 => "not ipv4",
        }
    }
}

/// The destination check.
#[derive(Debug, Clone)]
pub struct Policy {
    internal: Vec<Net>,
    deny: Vec<Net>,
    own: Vec<IpAddr>,
    public: fn(IpAddr) -> bool,
}

impl Policy {
    /// Refuses `internal` and `own` as well as every non-public address.
    pub fn new(internal: Vec<Net>, own: Vec<IpAddr>) -> Self {
        Policy {
            internal,
            deny: Vec::new(),
            own,
            public: is_public,
        }
    }

    /// The policy for these settings: their internal subnets, and as own addresses the listen
    /// and advertised addresses plus every local address of this network namespace.
    pub fn for_settings(settings: &Settings) -> Self {
        let mut own = vec![settings.listen, IpAddr::V4(settings.advertise)];
        own.extend(local_addresses());
        Policy::new(settings.internal.clone(), own).deny(settings.deny.clone())
    }

    /// Also refuses `deny` (the operator's deny list). It only adds refusals.
    pub fn deny(mut self, deny: Vec<Net>) -> Self {
        self.deny.extend(deny);
        self
    }

    /// Test builds only: count `public` addresses as public instead of [`is_public`] (so a
    /// loopback destination can stand in for a public one). Internal subnets, the deny list and
    /// own addresses stay refused.
    #[cfg(test)]
    pub(crate) fn with_public_check(mut self, public: fn(IpAddr) -> bool) -> Self {
        self.public = public;
        self
    }

    /// The IPv4 address to dial for `ip`, or why it is refused. An IPv4-mapped address is
    /// judged, and dialled, as its IPv4 address.
    pub fn check(&self, ip: IpAddr) -> Result<Ipv4Addr, Refusal> {
        let plain = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
            v4 => v4,
        };
        let same = |own: &IpAddr| {
            let own = match *own {
                IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(*own, IpAddr::V4),
                v4 => v4,
            };
            own == plain
        };
        if self.own.iter().any(same) {
            return Err(Refusal::OwnAddress);
        }
        if self.internal.iter().any(|n| n.contains(plain)) {
            return Err(Refusal::InternalSubnet);
        }
        if self.deny.iter().any(|n| n.contains(plain)) {
            return Err(Refusal::Denied);
        }
        if !(self.public)(ip) || !(self.public)(plain) {
            return Err(Refusal::NotPublic);
        }
        match plain {
            IpAddr::V4(v4) => Ok(v4),
            IpAddr::V6(_) => Err(Refusal::NotIpv4),
        }
    }
}

/// This namespace's local addresses (`/proc/net/fib_trie`, `/proc/net/if_inet6`); a table that
/// can't be read adds none.
pub fn local_addresses() -> Vec<IpAddr> {
    let read = |path: &str| std::fs::read_to_string(path).unwrap_or_default();
    let mut addrs = parse_fib_trie(&read("/proc/net/fib_trie"));
    addrs.extend(parse_if_inet6(&read("/proc/net/if_inet6")));
    addrs.sort();
    addrs.dedup();
    addrs
}

/// `fib_trie`: each `|-- <addr>` line followed by a `/32 host LOCAL` line is a local address.
fn parse_fib_trie(table: &str) -> Vec<IpAddr> {
    let mut last: Option<Ipv4Addr> = None;
    let mut addrs = Vec::new();
    for line in table.lines() {
        let line = line.trim();
        if let Some(addr) = line.strip_prefix("|-- ") {
            last = addr.trim().parse().ok();
        } else if line.starts_with("/32 host LOCAL")
            && let Some(addr) = last.take()
        {
            addrs.push(IpAddr::V4(addr));
        }
    }
    addrs
}

/// `if_inet6`: the first field of each line is an address in 32 hex digits.
fn parse_if_inet6(table: &str) -> Vec<IpAddr> {
    table
        .lines()
        .filter_map(|line| {
            let hex = line.split_whitespace().next()?;
            (hex.len() == 32)
                .then(|| u128::from_str_radix(hex, 16).ok())
                .flatten()
                .map(|v| IpAddr::V6(Ipv6Addr::from(v)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn policy() -> Policy {
        Policy::new(vec!["192.88.99.0/24".parse().unwrap()], vec![ip("9.9.9.9")])
    }

    #[track_caller]
    fn refused_as(addrs: &[&str], why: Refusal) {
        for a in addrs {
            assert_eq!(policy().check(ip(a)), Err(why), "{a}");
        }
    }

    #[test]
    fn refuses_loopback() {
        refused_as(
            &["127.0.0.1", "127.255.0.9", "::1", "::ffff:127.0.0.1"],
            Refusal::NotPublic,
        );
    }

    #[test]
    fn refuses_rfc1918() {
        refused_as(
            &["10.0.0.1", "172.16.5.4", "172.31.255.255", "192.168.1.1"],
            Refusal::NotPublic,
        );
    }

    #[test]
    fn refuses_cgnat() {
        refused_as(&["100.64.0.1", "100.127.255.254"], Refusal::NotPublic);
    }

    #[test]
    fn refuses_link_local() {
        refused_as(
            &["169.254.169.254", "169.254.0.1", "fe80::1"],
            Refusal::NotPublic,
        );
    }

    #[test]
    fn refuses_ula() {
        refused_as(&["fd00:ec2::254", "fc00::1"], Refusal::NotPublic);
    }

    #[test]
    fn refuses_an_ipv4_mapped_private_address() {
        refused_as(
            &[
                "::ffff:10.0.0.1",
                "::ffff:192.168.0.1",
                "::ffff:169.254.169.254",
            ],
            Refusal::NotPublic,
        );
    }

    #[test]
    fn refuses_other_non_public_classes() {
        refused_as(
            &[
                "0.0.0.0",
                "192.0.2.1",
                "198.18.0.1",
                "224.0.0.1",
                "255.255.255.255",
                "2001:db8::1",
                "64:ff9b::a00:1",
            ],
            Refusal::NotPublic,
        );
    }

    #[test]
    fn refuses_the_internal_subnets() {
        refused_as(
            &["192.88.99.5", "::ffff:192.88.99.5"],
            Refusal::InternalSubnet,
        );
    }

    #[test]
    fn refuses_the_deny_list() {
        let p = policy().deny(vec![
            "203.0.113.0/24".parse().unwrap(),
            "1.1.1.1/32".parse().unwrap(),
        ]);
        for a in ["1.1.1.1", "::ffff:1.1.1.1", "203.0.113.9"] {
            assert_eq!(p.check(ip(a)), Err(Refusal::Denied), "{a}");
        }
        assert_eq!(p.check(ip("1.1.1.2")), Ok(Ipv4Addr::new(1, 1, 1, 2)));
        // It only adds refusals: a non-public address stays refused whatever the list holds.
        let wide = policy().deny(vec!["0.0.0.0/0".parse().unwrap()]);
        assert_eq!(wide.check(ip("8.8.8.8")), Err(Refusal::Denied));
        assert_eq!(wide.check(ip("10.0.0.1")), Err(Refusal::Denied));
        assert_eq!(wide.check(ip("::1")), Err(Refusal::NotPublic));
    }

    #[test]
    fn refuses_its_own_addresses() {
        refused_as(&["9.9.9.9", "::ffff:9.9.9.9"], Refusal::OwnAddress);
    }

    #[test]
    fn refuses_a_public_ipv6_address() {
        refused_as(&["2606:4700::1111"], Refusal::NotIpv4);
    }

    #[test]
    fn allows_a_public_address() {
        assert_eq!(policy().check(ip("1.1.1.1")), Ok(Ipv4Addr::new(1, 1, 1, 1)));
        assert_eq!(
            policy().check(ip("::ffff:8.8.8.8")),
            Ok(Ipv4Addr::new(8, 8, 8, 8))
        );
        // m5's hermetic destinations live here; it stays public unless listed as internal.
        assert_eq!(
            Policy::new(vec![], vec![]).check(ip("192.88.99.7")),
            Ok(Ipv4Addr::new(192, 88, 99, 7))
        );
    }

    #[test]
    fn the_injected_check_keeps_internal_and_own_refused() {
        let p = policy()
            .deny(vec!["127.0.0.8/29".parse().unwrap()])
            .with_public_check(|_| true);
        assert_eq!(p.check(ip("192.88.99.5")), Err(Refusal::InternalSubnet));
        assert_eq!(p.check(ip("127.0.0.9")), Err(Refusal::Denied));
        assert_eq!(p.check(ip("9.9.9.9")), Err(Refusal::OwnAddress));
        assert_eq!(p.check(ip("2606:4700::1111")), Err(Refusal::NotIpv4));
        assert_eq!(p.check(ip("127.0.0.2")), Ok(Ipv4Addr::new(127, 0, 0, 2)));
    }

    #[test]
    fn reads_local_addresses() {
        let fib = "Main:\n  +-- 0.0.0.0/0 3 0 5\n     |-- 0.0.0.0\n        /0 universe UNICAST\n     +-- 127.0.0.0/8 2 0 2\n        +-- 127.0.0.0/31 1 0 0\n           |-- 127.0.0.0\n              /8 host LOCAL\n           |-- 127.0.0.1\n              /32 host LOCAL\n     |-- 172.20.0.0\n        /16 link UNICAST\n     |-- 172.20.0.5\n        /32 host LOCAL\n     |-- 172.20.255.255\n        /32 link BROADCAST\nLocal:\n     |-- 172.20.0.5\n        /32 host LOCAL\n";
        let mut got = parse_fib_trie(fib);
        got.sort();
        got.dedup();
        assert_eq!(got, [ip("127.0.0.1"), ip("172.20.0.5")]);
        let inet6 = "00000000000000000000000000000001 01 80 10 80       lo\nfe80000000000000004200fffeac1405 2c 40 20 80     eth0\nbad line\n";
        assert_eq!(
            parse_if_inet6(inet6),
            [ip("::1"), ip("fe80::42:ff:feac:1405")]
        );
    }
}
