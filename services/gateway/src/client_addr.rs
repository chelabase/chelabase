//! The client's address: the TCP peer, or, when the peer is a trusted reverse proxy
//! (`CHELABASE_GATEWAY_TRUSTED_PROXIES`), the address that proxy forwarded.

use http::HeaderMap;
use http::header::{self, HeaderName};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use std::sync::RwLock;

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

/// #426: the functions runtime's peer addresses (`CHELABASE_GATEWAY_FUNCTIONS_RUNTIME`): the
/// only peers whose [`ORIGINAL_CLIENT_IP_HEADER`] is believed. Each entry is a CIDR or address
/// (as [`Cidr::parse`]) or a host name, resolved now and again by [`FunctionsRuntime::refresh`]
/// (compose names the runtime's service, whose address changes when it is recreated). Empty
/// (the default) believes nobody.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FunctionsRuntimeSpec {
    pub addrs: TrustedProxies,
    pub hosts: Vec<String>,
}

impl FunctionsRuntimeSpec {
    /// Comma-separated entries; blank entries are skipped, so blank is none. An entry that
    /// isn't a CIDR or address must be a DNS host name (letters, digits, `-` and `.`, not only
    /// digits and dots), or the whole list is refused with the CIDR's reason.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let mut cidrs = Vec::new();
        let mut hosts = Vec::new();
        for entry in raw.split(',').map(str::trim).filter(|e| !e.is_empty()) {
            match Cidr::parse(entry) {
                Ok(c) => cidrs.push(c),
                Err(_) if is_host_name(entry) => hosts.push(entry.to_ascii_lowercase()),
                Err(e) => return Err(e),
            }
        }
        Ok(Self {
            addrs: TrustedProxies(cidrs),
            hosts,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.addrs.is_empty() && self.hosts.is_empty()
    }
}

/// A DNS host name: labels of letters, digits and inner `-`, joined by `.`; not only digits and
/// dots (that would be a mistyped address).
fn is_host_name(s: &str) -> bool {
    s.len() <= 253
        && !s.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        && s.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

/// The runtime's addresses as the gateway currently knows them: the spec's CIDRs, and the last
/// addresses its host names resolved to.
#[derive(Debug, Default)]
pub struct FunctionsRuntime {
    spec: FunctionsRuntimeSpec,
    resolved: RwLock<Vec<IpAddr>>,
}

impl FunctionsRuntime {
    pub fn new(spec: FunctionsRuntimeSpec) -> Self {
        Self {
            spec,
            resolved: RwLock::new(Vec::new()),
        }
    }

    /// Whether host names need [`Self::refresh`].
    pub fn has_hosts(&self) -> bool {
        !self.spec.hosts.is_empty()
    }

    /// Resolves the host names again (blocking) and keeps exactly what they resolve to now: a
    /// name that doesn't resolve trusts nothing until it does.
    pub fn refresh(&self) {
        let mut ips = Vec::new();
        for host in &self.spec.hosts {
            if let Ok(addrs) = (host.as_str(), 0).to_socket_addrs() {
                ips.extend(addrs.map(|a| a.ip().to_canonical()));
            }
        }
        self.set_resolved(ips);
    }

    /// Replaces the host names' addresses (tests, and [`Self::refresh`]).
    pub fn set_resolved(&self, ips: Vec<IpAddr>) {
        *self.resolved.write().unwrap_or_else(|e| e.into_inner()) = ips;
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = ip.to_canonical();
        self.spec.addrs.contains(ip)
            || self
                .resolved
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&ip)
    }
}

/// The original caller's address the functions runtime sends with a function's caller-bound
/// binding calls (#426). The runtime strips any copy function code sets; the gateway strips it
/// from every request before proxying.
pub const ORIGINAL_CLIENT_IP_HEADER: &str = "chelabase-original-client-ip";

/// #426: the original caller's address of a request from `peer`, the TCP peer itself (never the
/// address derived from a forwarding header): [`ORIGINAL_CLIENT_IP_HEADER`] when the peer is
/// the functions runtime and the header is exactly one line holding one IP literal. None
/// otherwise; the request is then limited under its client address as any other.
pub fn original_client(
    peer: Option<IpAddr>,
    runtime: &FunctionsRuntime,
    headers: &HeaderMap,
) -> Option<IpAddr> {
    if !runtime.contains(peer?) {
        return None;
    }
    let mut lines = headers.get_all(ORIGINAL_CLIENT_IP_HEADER).iter();
    let line = lines.next()?;
    if lines.next().is_some() {
        return None;
    }
    let ip: IpAddr = line.to_str().ok()?.parse().ok()?;
    Some(ip.to_canonical())
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

    // ---- #426: the functions runtime's original-address header ----

    fn runtime(raw: &str) -> FunctionsRuntime {
        FunctionsRuntime::new(FunctionsRuntimeSpec::parse(raw).unwrap())
    }

    fn with_original(lines: &[&str], extra: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for line in lines {
            map.append(ORIGINAL_CLIENT_IP_HEADER, line.parse().unwrap());
        }
        for (name, value) in extra {
            map.append(*name, value.parse().unwrap());
        }
        map
    }

    const RT: &str = "10.214.12.5";

    #[test]
    fn the_runtime_peer_is_believed_for_one_ip_literal() {
        let rt = runtime(RT);
        let one = |v: &str| original_client(Some(ip(RT)), &rt, &with_original(&[v], &[]));
        assert_eq!(one("203.0.113.9"), Some(ip("203.0.113.9")));
        assert_eq!(one("2001:db8::7"), Some(ip("2001:db8::7")));
        assert_eq!(one("::ffff:203.0.113.9"), Some(ip("203.0.113.9")), "mapped is IPv4");
        // The runtime connecting over a dual-stack socket is still the runtime.
        let mapped = original_client(Some(ip("::ffff:10.214.12.5")), &rt, &with_original(&["203.0.113.9"], &[]));
        assert_eq!(mapped, Some(ip("203.0.113.9")));
        // Not exactly one IP literal: not believed.
        for bad in ["", "203.0.113.9, 198.51.100.1", "203.0.113.9:443", "[2001:db8::7]", "unknown", " 203.0.113.9"] {
            assert_eq!(one(bad), None, "{bad:?}");
        }
        let two = with_original(&["203.0.113.9", "203.0.113.9"], &[]);
        assert_eq!(original_client(Some(ip(RT)), &rt, &two), None, "two lines");
        assert_eq!(original_client(Some(ip(RT)), &rt, &HeaderMap::new()), None, "no header");
        assert_eq!(original_client(None, &rt, &with_original(&["203.0.113.9"], &[])), None, "no peer");
    }

    /// Refusal 1: a request from any other peer carrying the header.
    #[test]
    fn the_header_from_any_other_peer_is_ignored() {
        let h = with_original(&["203.0.113.9"], &[]);
        assert_eq!(original_client(Some(ip("10.214.12.6")), &runtime(RT), &h), None);
        assert_eq!(original_client(Some(ip("203.0.113.9")), &runtime(RT), &h), None);
        // Nobody by default.
        assert_eq!(original_client(Some(ip(RT)), &FunctionsRuntime::default(), &h), None);
    }

    /// Refusal 2: a client behind a trusted proxy whose forwarding header names the runtime's
    /// address. Its client address is the runtime's, but its TCP peer is the proxy.
    #[test]
    fn a_forwarded_runtime_address_is_not_the_runtime() {
        let trusted = TrustedProxies::parse("192.0.2.1").unwrap();
        let h = with_original(&["203.0.113.9"], &[("x-forwarded-for", RT)]);
        let proxy = Some(ip("192.0.2.1"));
        assert_eq!(client_address(proxy, &trusted, &h), Some(ip(RT)), "the forwarded client is the runtime's address");
        assert_eq!(original_client(proxy, &runtime(RT), &h), None);
    }

    /// Refusal 3: the runtime's address also inside the trusted proxies. Another proxy in that
    /// range forwarding the runtime's address is refused; the runtime itself is believed, and
    /// its client address stays its own (it sends no forwarding header).
    #[test]
    fn a_runtime_inside_the_trusted_proxies_is_still_decided_on_the_peer() {
        let trusted = TrustedProxies::parse("10.214.12.0/24").unwrap();
        let rt = runtime(RT);
        let forged = with_original(&["203.0.113.9"], &[("x-forwarded-for", RT)]);
        let other_proxy = Some(ip("10.214.12.9"));
        assert_eq!(client_address(other_proxy, &trusted, &forged), Some(ip(RT)));
        assert_eq!(original_client(other_proxy, &rt, &forged), None);
        let direct = with_original(&["203.0.113.9"], &[]);
        assert_eq!(client_address(Some(ip(RT)), &trusted, &direct), Some(ip(RT)));
        assert_eq!(original_client(Some(ip(RT)), &rt, &direct), Some(ip("203.0.113.9")));
    }

    #[test]
    fn the_runtime_setting_takes_addresses_cidrs_and_host_names() {
        let spec = FunctionsRuntimeSpec::parse(" 10.214.12.0/24, functions-runtime ,,::1, Runtime.internal").unwrap();
        assert_eq!(spec.hosts, vec!["functions-runtime".to_string(), "runtime.internal".to_string()]);
        let rt = FunctionsRuntime::new(spec);
        assert!(rt.contains(ip("10.214.12.77")) && rt.contains(ip("::1")));
        assert!(!rt.contains(ip("10.214.13.1")));
        // A host name trusts exactly what it last resolved to.
        rt.set_resolved(vec![ip("172.30.0.4")]);
        assert!(rt.contains(ip("172.30.0.4")));
        rt.set_resolved(Vec::new());
        assert!(!rt.contains(ip("172.30.0.4")));
        assert!(FunctionsRuntimeSpec::parse(" , ").unwrap().is_empty());
        for bad in ["10.0.0.1/8", "0.0.0.0/0", "10.0.0", "1.2.3.4:80", "-runtime", "run_time", "a..b", "[::1]"] {
            assert!(FunctionsRuntimeSpec::parse(bad).is_err(), "{bad:?}");
        }
    }
}
