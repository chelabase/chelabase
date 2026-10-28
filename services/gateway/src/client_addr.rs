//! The client's address: the TCP peer, or, when the peer is a trusted reverse proxy
//! (`CHELABASE_GATEWAY_TRUSTED_PROXIES`), the address that proxy forwarded.

use http::HeaderMap;
use http::header::{self, HeaderName};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// The most bytes of forwarding header read from a trusted peer, all lines together; more
/// falls back to the peer.
pub const MAX_FORWARDED_BYTES: usize = 4096;
/// The most hops (list entries) read from a trusted peer; more falls back to the peer.
pub const MAX_FORWARDED_HOPS: usize = 32;

/// One `CHELABASE_GATEWAY_TRUSTED_PROXIES` entry: an IP network. An IPv4-mapped IPv6 entry
/// (`::ffff:10.0.0.0/104`) is stored as its IPv4 network.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cidr {
    net: IpAddr,
    prefix: u8,
}

impl Cidr {
    /// `10.0.0.0/8`, `fd00::/8`, or a single address (`192.168.1.5`, `::1`). Refuses bits set
    /// past the prefix (`10.0.0.1/8`), a prefix of 0 (that would trust every address), brackets,
    /// ports, zones and anything std's [`IpAddr`] doesn't parse.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let invalid = || format!("{raw:?} is not a CIDR (10.0.0.0/8, fd00::/8) or an IP address");
        let (addr, prefix) = match raw.split_once('/') {
            Some((addr, prefix)) => (addr, Some(prefix)),
            None => (raw, None),
        };
        let net: IpAddr = addr.parse().map_err(|_| invalid())?;
        let max = if net.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            None => max,
            Some(p) if !p.is_empty() && p.len() <= 3 && p.bytes().all(|b| b.is_ascii_digit()) => {
                p.parse::<u8>().map_err(|_| invalid())?
            }
            Some(_) => return Err(invalid()),
        };
        if prefix > max {
            return Err(format!("{raw:?}: the prefix is longer than /{max}"));
        }
        if masked(net, prefix) != net {
            return Err(format!(
                "{raw:?} has bits set past its /{prefix}; did you mean {}/{prefix}?",
                masked(net, prefix)
            ));
        }
        let (net, prefix) = match net {
            IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
                Some(v4) if prefix >= 96 => (IpAddr::V4(v4), prefix - 96),
                _ => (net, prefix),
            },
            v4 => (v4, prefix),
        };
        if prefix == 0 {
            return Err(format!(
                "{raw:?} would trust every address; list your proxies' own networks"
            ));
        }
        Ok(Self { net, prefix })
    }

    /// Whether `ip` (an IPv4-mapped address counts as its IPv4 address) is in the network.
    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = ip.to_canonical();
        ip.is_ipv4() == self.net.is_ipv4() && masked(ip, self.prefix) == self.net
    }
}

/// `ip` with every bit past the first `prefix` cleared.
fn masked(ip: IpAddr, prefix: u8) -> IpAddr {
    match ip {
        IpAddr::V4(v4) => {
            let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
            IpAddr::V4(Ipv4Addr::from(u32::from(v4) & mask))
        }
        IpAddr::V6(v6) => {
            let mask = u128::MAX.checked_shl(128 - u32::from(prefix)).unwrap_or(0);
            IpAddr::V6(Ipv6Addr::from(u128::from(v6) & mask))
        }
    }
}

/// The reverse proxies whose forwarded address the gateway believes. Empty (the default)
/// believes none: the client is always the TCP peer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrustedProxies(Vec<Cidr>);

impl TrustedProxies {
    /// A comma-separated list of [`Cidr`]s; blank entries are skipped, so blank is none.
    pub fn parse(raw: &str) -> Result<Self, String> {
        raw.split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(Cidr::parse)
            .collect::<Result<Vec<_>, _>>()
            .map(Self)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        self.0.iter().any(|c| c.contains(ip))
    }
}

const X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");

/// The client's address for a request from `peer`.
///
/// From a peer that is not a trusted proxy (or with none configured) it is the peer, whatever
/// the request's headers say. From a trusted proxy it is read from the one forwarding header
/// present, `X-Forwarded-For` or `Forwarded` (RFC 7239 `for=`): every line of it is one list,
/// walked right to left past trusted addresses; the first untrusted address is the client,
/// and when every hop is trusted, the left-most one is. It falls back to the peer, and never
/// fails, when the peer sent both headers or neither, when the header is longer than
/// [`MAX_FORWARDED_BYTES`] or has more than [`MAX_FORWARDED_HOPS`] hops, holds anything but
/// visible ASCII, or when a hop the walk reaches doesn't parse (`unknown`, an obfuscated
/// node, an empty entry). Hops left of the client are never read. In an address read from a
/// forwarding header, a port is dropped and an IPv4-mapped address is reported as IPv4; the
/// peer itself (untrusted, or the fallback) is returned exactly as it connected.
pub fn client_address(
    peer: Option<IpAddr>,
    trusted: &TrustedProxies,
    headers: &HeaderMap,
) -> Option<IpAddr> {
    let peer = peer?;
    if !trusted.contains(peer) {
        return Some(peer);
    }
    Some(forwarded_client(trusted, headers).unwrap_or(peer))
}

/// Reads one hop of a forwarding header.
type HopParser = fn(&str) -> Option<IpAddr>;

/// The walk of [`client_address`]; None means fall back to the peer.
fn forwarded_client(trusted: &TrustedProxies, headers: &HeaderMap) -> Option<IpAddr> {
    let xff = headers.contains_key(&X_FORWARDED_FOR);
    let rfc = headers.contains_key(header::FORWARDED);
    let name = match (xff, rfc) {
        (true, false) => X_FORWARDED_FOR,
        (false, true) => header::FORWARDED,
        _ => return None,
    };
    let mut list = String::new();
    for (i, value) in headers.get_all(&name).iter().enumerate() {
        let bytes = value.as_bytes();
        if list.len() + usize::from(i > 0) + bytes.len() > MAX_FORWARDED_BYTES {
            return None;
        }
        if !bytes
            .iter()
            .all(|&b| b == b'\t' || (b' '..=b'~').contains(&b))
        {
            return None;
        }
        if i > 0 {
            list.push(',');
        }
        list.push_str(std::str::from_utf8(bytes).ok()?);
    }
    // `X-Forwarded-For` has no quoting; `Forwarded` values may be quoted strings.
    let (hops, parse): (Vec<&str>, HopParser) = if xff {
        (list.split(',').collect(), xff_hop)
    } else {
        (split_outside_quotes(&list, b',')?, forwarded_hop)
    };
    if hops.len() > MAX_FORWARDED_HOPS {
        return None;
    }
    let mut furthest = None;
    for hop in hops.iter().rev() {
        let ip = parse(hop)?;
        if !trusted.contains(ip) {
            return Some(ip);
        }
        furthest = Some(ip);
    }
    furthest
}

fn trim_ows(s: &str) -> &str {
    s.trim_matches([' ', '\t'])
}

/// One `X-Forwarded-For` entry: an IPv4 or IPv6 address, bracketed or not, an optional port.
fn xff_hop(hop: &str) -> Option<IpAddr> {
    node(trim_ows(hop), true)
}

/// One `Forwarded` element: `;`-separated `name=value` pairs, exactly one of them `for=`.
fn forwarded_hop(element: &str) -> Option<IpAddr> {
    let mut found = None;
    for pair in split_outside_quotes(element, b';')? {
        let (name, value) = trim_ows(pair).split_once('=')?;
        if !is_token(name) {
            return None;
        }
        let value = match value.strip_prefix('"') {
            Some(rest) => Value::Quoted(rest.strip_suffix('"')?),
            None if is_token(value) => Value::Token(value),
            None => return None,
        };
        if name.eq_ignore_ascii_case("for") {
            if found.is_some() {
                return None;
            }
            found = Some(value);
        }
    }
    match found? {
        // A token can't hold `:` or `[`: a bare IPv4 address only.
        Value::Token(v) => v.parse::<Ipv4Addr>().ok().map(IpAddr::V4),
        // No escapes: an address never needs one. IPv6 must be bracketed (RFC 7239 §6).
        Value::Quoted(v) if v.contains(['"', '\\']) => None,
        Value::Quoted(v) => node(v, false),
    }
}

enum Value<'a> {
    Token(&'a str),
    Quoted(&'a str),
}

/// `1.2.3.4`, `1.2.3.4:80`, `[2001:db8::7]`, `[2001:db8::7]:443`, and with `bare_v6`
/// `2001:db8::7`. IPv4-mapped IPv6 comes back as IPv4.
fn node(s: &str, bare_v6: bool) -> Option<IpAddr> {
    let ip = if let Some(rest) = s.strip_prefix('[') {
        let (v6, after) = rest.split_once(']')?;
        if !after.is_empty() {
            port(after.strip_prefix(':')?)?;
        }
        IpAddr::V6(v6.parse::<Ipv6Addr>().ok()?)
    } else if let Ok(v4) = s.parse::<Ipv4Addr>() {
        IpAddr::V4(v4)
    } else if let Some((v4, p)) = s.split_once(':').filter(|(_, p)| !p.contains(':')) {
        port(p)?;
        IpAddr::V4(v4.parse::<Ipv4Addr>().ok()?)
    } else if bare_v6 {
        IpAddr::V6(s.parse::<Ipv6Addr>().ok()?)
    } else {
        return None;
    };
    Some(ip.to_canonical())
}

fn port(p: &str) -> Option<u16> {
    if p.is_empty() || p.len() > 5 || !p.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    p.parse().ok()
}

/// RFC 9110 `token`: one or more tchars.
fn is_token(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// `s` split on `sep` outside quoted strings (a backslash inside one escapes the next byte);
/// None when a quoted string is left open.
fn split_outside_quotes(s: &str, sep: u8) -> Option<Vec<&str>> {
    let bytes = s.as_bytes();
    let (mut parts, mut start, mut quoted, mut i) = (Vec::new(), 0, false, 0);
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if quoted => i += 1,
            b'"' => quoted = !quoted,
            b if b == sep && !quoted => {
                parts.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if quoted || i > bytes.len() {
        return None;
    }
    parts.push(&s[start..]);
    Some(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cidr(raw: &str) -> Cidr {
        Cidr::parse(raw).unwrap()
    }

    fn ip(raw: &str) -> IpAddr {
        raw.parse().unwrap()
    }

    #[test]
    fn a_cidr_contains_its_network_only() {
        let c = cidr("10.0.0.0/8");
        assert!(c.contains(ip("10.0.0.0")) && c.contains(ip("10.255.255.255")));
        assert!(!c.contains(ip("11.0.0.0")) && !c.contains(ip("9.255.255.255")));
        assert!(!c.contains(ip("::ffff:11.0.0.1")));
        assert!(c.contains(ip("::ffff:10.0.0.1")));
        assert!(!c.contains(ip("fd00::1")));
        let c = cidr("172.16.0.0/12");
        assert!(c.contains(ip("172.31.255.255")) && !c.contains(ip("172.32.0.0")));
        let c = cidr("192.168.1.5/32");
        assert!(c.contains(ip("192.168.1.5")) && !c.contains(ip("192.168.1.4")));
        let c = cidr("fd00::/8");
        assert!(
            c.contains(ip("fdff::1")) && !c.contains(ip("fe00::")) && !c.contains(ip("10.0.0.1"))
        );
        let c = cidr("2001:db8::7/128");
        assert!(c.contains(ip("2001:db8::7")) && !c.contains(ip("2001:db8::8")));
        let c = cidr("2001:db8:aa::/47");
        assert!(c.contains(ip("2001:db8:ab::1")) && !c.contains(ip("2001:db8:ac::1")));
    }

    #[test]
    fn a_bare_address_is_a_single_host() {
        assert_eq!(cidr("192.168.1.5"), cidr("192.168.1.5/32"));
        assert_eq!(cidr("::1"), cidr("::1/128"));
    }

    #[test]
    fn an_ipv4_mapped_entry_is_its_ipv4_network() {
        assert_eq!(cidr("::ffff:10.0.0.0/104"), cidr("10.0.0.0/8"));
        assert_eq!(cidr("::ffff:10.0.0.1"), cidr("10.0.0.1/32"));
        assert!(cidr("::ffff:10.0.0.0/104").contains(ip("10.1.1.1")));
    }

    #[test]
    fn bad_cidrs_are_refused() {
        for bad in [
            "",
            " ",
            "nope",
            "10.0.0.0/33",
            "fd00::/129",
            "10.0.0.0/",
            "/8",
            "10.0.0.0/+8",
            "10.0.0.0/ 8",
            "10.0.0.0 /8",
            "10.0.0.0/-1",
            "10.0.0.0/8/8",
            "10.0.0.1/8",
            "fd00::1/8",
            "0.0.0.0/0",
            "::/0",
            "::ffff:0.0.0.0/96",
            "[::1]",
            "[::1]/128",
            "10.0.0.1:80",
            "localhost",
            "01.0.0.0/8",
            "fe80::1%eth0",
            "10.0.0.0/0008",
        ] {
            assert!(Cidr::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_list_parses_every_entry() {
        assert!(TrustedProxies::parse("").unwrap().is_empty());
        assert!(TrustedProxies::parse(" , ,").unwrap().is_empty());
        let t = TrustedProxies::parse("10.0.0.0/8, fd00::/8").unwrap();
        assert!(
            t.contains(ip("10.1.1.1")) && t.contains(ip("fd00::1")) && !t.contains(ip("8.8.8.8"))
        );
        let msg = TrustedProxies::parse("10.0.0.0/8, 10.0.0.1/8").unwrap_err();
        assert!(msg.contains("10.0.0.1/8"), "{msg}");
    }
}
