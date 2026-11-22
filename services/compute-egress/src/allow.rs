//! The service's settings and its allow-list, `CHELABASE_COMPUTE_EGRESS_ALLOW`.
//!
//! The list names destinations, comma-separated: `[<alias>=]<host>:<port>[@<listen-port>]`.
//! `<host>` is a DNS name or an IPv4 literal; a literal needs an `<alias>`, the name computes
//! dial for it. Computes dial the entry's name at its listen port: `<port>`, or `<listen-port>`
//! when given. There are no wildcards, networks, port ranges or flags, and nothing is ever
//! assigned automatically: two entries on one listen port are an error that names both.
//!
//! Every refusal names the setting and the entry, and an invalid entry refuses the whole list,
//! so the service never starts with part of it.

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use chelabase_http::outbound::{self, Net, Outbound};

/// The allow-list (empty or unset: nothing is reachable).
pub const ENV_ALLOW: &str = "CHELABASE_COMPUTE_EGRESS_ALLOW";
/// The one address every listener binds (never the wildcard address).
pub const ENV_LISTEN: &str = "CHELABASE_COMPUTE_EGRESS_LISTEN";
/// The address the resolver answers for a listed name (default: the listen address).
pub const ENV_ADVERTISE: &str = "CHELABASE_COMPUTE_EGRESS_ADVERTISE";
/// The resolver's port, UDP and TCP, on the listen address (default 53).
pub const ENV_DNS_PORT: &str = "CHELABASE_COMPUTE_EGRESS_DNS_PORT";
/// The health port, on loopback (default 8080).
pub const ENV_HEALTH_PORT: &str = "CHELABASE_COMPUTE_EGRESS_HEALTH_PORT";
/// Networks no destination may resolve into, whatever their class (the stack's own).
pub const ENV_INTERNAL_SUBNETS: &str = "CHELABASE_INTERNAL_SUBNETS";
/// The operator's deny list: IPv4 addresses and networks, comma-separated, never dialled even
/// when public (the host's own public addresses). Empty or unset: nothing more is refused. It
/// only adds refusals; nothing in it can allow an address.
pub const ENV_DENY: &str = "CHELABASE_COMPUTE_EGRESS_DENY";
/// Forwarded connections at once, in total (default 1024).
pub const ENV_MAX_CONNECTIONS: &str = "CHELABASE_COMPUTE_EGRESS_MAX_CONNECTIONS";
/// Forwarded connections at once from one source address (default 64).
pub const ENV_MAX_CONNECTIONS_PER_CLIENT: &str =
    "CHELABASE_COMPUTE_EGRESS_MAX_CONNECTIONS_PER_CLIENT";
/// Seconds to resolve and connect to a destination (default 10).
pub const ENV_DIAL_TIMEOUT_SECS: &str = "CHELABASE_COMPUTE_EGRESS_DIAL_TIMEOUT_SECS";

/// Where an entry's connections go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dest {
    /// A DNS name, resolved by the system resolver on every connection.
    Name(String),
    /// A public IPv4 address (the entry has an alias).
    Addr(Ipv4Addr),
}

impl std::fmt::Display for Dest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Dest::Name(n) => f.write_str(n),
            Dest::Addr(a) => write!(f, "{a}"),
        }
    }
}

/// One allow-listed destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The entry as written (trimmed), for messages and logs.
    pub raw: String,
    /// The name computes dial and the resolver answers: lower case, no trailing dot.
    pub name: String,
    pub dest: Dest,
    /// The destination's port.
    pub port: u16,
    /// The port this entry's listener binds on the listen address.
    pub listen_port: u16,
}

/// Everything the service reads from its environment.
#[derive(Debug, Clone)]
pub struct Settings {
    pub entries: Vec<Entry>,
    pub listen: IpAddr,
    pub advertise: Ipv4Addr,
    pub dns_port: u16,
    pub health_port: u16,
    pub internal: Vec<Net>,
    /// [`ENV_DENY`]: IPv4 networks (an address is its /32).
    pub deny: Vec<Net>,
    pub max_connections: usize,
    pub max_connections_per_client: usize,
    pub dial_timeout: Duration,
}

impl Settings {
    /// The settings from `get` (the environment, or a map in tests).
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let val = |k: &str| {
            get(k)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let raw = val(ENV_LISTEN).ok_or_else(|| {
            format!("{ENV_LISTEN} is required: the one address every listener binds")
        })?;
        let listen: IpAddr = raw
            .parse()
            .map_err(|_| format!("{ENV_LISTEN}={raw:?}: not an IP address"))?;
        if listen.is_unspecified() || listen.is_multicast() {
            return Err(format!(
                "{ENV_LISTEN}={raw:?}: must be one unicast address, never the wildcard address"
            ));
        }
        let advertise = match val(ENV_ADVERTISE) {
            Some(v) => match v.parse::<IpAddr>() {
                Ok(IpAddr::V4(a))
                    if !a.is_unspecified() && !a.is_multicast() && !a.is_broadcast() =>
                {
                    a
                }
                _ => return Err(format!("{ENV_ADVERTISE}={v:?}: must be an IPv4 address")),
            },
            None => match listen {
                IpAddr::V4(a) => a,
                IpAddr::V6(_) => {
                    return Err(format!(
                        "{ENV_ADVERTISE} is required when {ENV_LISTEN} is an IPv6 address: the resolver answers A records"
                    ));
                }
            },
        };
        let port = |k: &str, default: u16| match val(k) {
            None => Ok(default),
            Some(v) => port_number(&v).map_err(|e| format!("{k}={v:?}: {e}")),
        };
        let count = |k: &str, default: usize, max: usize| match val(k) {
            None => Ok(default),
            Some(v) => v
                .parse::<usize>()
                .ok()
                .filter(|n| (1..=max).contains(n) && v.bytes().all(|b| b.is_ascii_digit()))
                .ok_or_else(|| format!("{k}={v:?}: expected a whole number from 1 to {max}")),
        };
        let dns_port = port(ENV_DNS_PORT, 53)?;
        let health_port = health_port(val(ENV_HEALTH_PORT).as_deref())?;
        if dns_port == health_port {
            return Err(format!(
                "{ENV_DNS_PORT} and {ENV_HEALTH_PORT} are both {dns_port}; they must differ"
            ));
        }
        let internal = outbound::parse_nets(&val(ENV_INTERNAL_SUBNETS).unwrap_or_default())
            .map_err(|e| format!("{ENV_INTERNAL_SUBNETS}: {e}"))?;
        let deny = parse_deny(&val(ENV_DENY).unwrap_or_default())?;
        let max_connections = count(ENV_MAX_CONNECTIONS, 1024, 1_000_000)?;
        let max_connections_per_client = count(ENV_MAX_CONNECTIONS_PER_CLIENT, 64, 1_000_000)?;
        let dial_timeout = Duration::from_secs(count(ENV_DIAL_TIMEOUT_SECS, 10, 300)? as u64);
        let entries = parse_allow(
            &val(ENV_ALLOW).unwrap_or_default(),
            &[(dns_port, ENV_DNS_PORT), (health_port, ENV_HEALTH_PORT)],
            &internal,
        )?;
        // An address entry inside the deny list would be refused on every connection: say so now.
        for entry in &entries {
            if let Dest::Addr(addr) = entry.dest
                && deny.iter().any(|n| n.contains(IpAddr::V4(addr)))
            {
                return Err(format!(
                    "{ENV_ALLOW}: entry {:?}: {addr} is in {ENV_DENY}",
                    entry.raw
                ));
            }
        }
        Ok(Settings {
            entries,
            listen,
            advertise,
            dns_port,
            health_port,
            internal,
            deny,
            max_connections,
            max_connections_per_client,
            dial_timeout,
        })
    }
}

/// The allow-list's entries. `reserved` are ports no entry may listen on (the resolver's and
/// the health port), each with the setting that holds it; `internal` are networks an address
/// entry may not be in.
pub fn parse_allow(
    list: &str,
    reserved: &[(u16, &str)],
    internal: &[Net],
) -> Result<Vec<Entry>, String> {
    // The platform's own names (`outbound::STACK_HOSTS`, container names included).
    let platform = Outbound::new().deny_stack([]);
    let mut entries: Vec<Entry> = Vec::new();
    for raw in list.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        let bad = |why: String| format!("{ENV_ALLOW}: entry {raw:?}: {why}");
        let entry = parse_entry(raw, internal, &platform).map_err(bad)?;
        if let Some((_, var)) = reserved.iter().find(|(p, _)| *p == entry.listen_port) {
            return Err(bad(format!(
                "listen port {} is the port of {var}; give the entry another with @<listen-port>",
                entry.listen_port
            )));
        }
        for prev in &entries {
            if prev.name == entry.name && prev.dest == entry.dest && prev.port == entry.port {
                return Err(bad(format!("duplicates {:?}", prev.raw)));
            }
            if prev.name == entry.name && prev.dest != entry.dest {
                return Err(bad(format!(
                    "the name {} already stands for {} in {:?}",
                    entry.name, prev.dest, prev.raw
                )));
            }
            if prev.listen_port == entry.listen_port {
                return Err(format!(
                    "{ENV_ALLOW}: entries {:?} and {raw:?} both listen on port {}; give the second \
                     its own listen port with @<listen-port>, for example {raw}@<free port>",
                    prev.raw, entry.listen_port
                ));
            }
        }
        entries.push(entry);
    }
    Ok(entries)
}

/// One entry, or why it is refused.
fn parse_entry(raw: &str, internal: &[Net], platform: &Outbound) -> Result<Entry, String> {
    let (alias, rest) = match raw.split_once('=') {
        Some((alias, rest)) => (Some(alias), rest),
        None => (None, raw),
    };
    if rest.contains('=') {
        return Err("more than one '='; the form is [alias=]host:port[@listen-port]".into());
    }
    let (dest_port, listen) = match rest.split_once('@') {
        Some((d, l)) => (d, Some(l)),
        None => (rest, None),
    };
    let (host, port) = dest_port
        .rsplit_once(':')
        .ok_or("no port; the form is [alias=]host:port[@listen-port]")?;
    let port = port_number(port)?;
    let listen_port = match listen {
        Some(l) => port_number(l).map_err(|e| format!("listen port: {e}"))?,
        None => port,
    };
    for name in alias.into_iter().chain([host]) {
        if platform.denied_name(name) {
            return Err(format!("{name} is a platform service name"));
        }
    }
    let literal = match host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        Some(v6) => Some(IpAddr::V6(
            v6.parse()
                .map_err(|_| format!("{host} is not an IPv6 address"))?,
        )),
        None => host.parse::<Ipv4Addr>().ok().map(IpAddr::V4),
    };
    let dest = match literal {
        None => Dest::Name(dns_name(host)?),
        Some(addr) => {
            if alias.is_none() {
                return Err(format!(
                    "the address {addr} needs an alias, the name computes dial (alias={host}:{port})"
                ));
            }
            if internal.iter().any(|n| n.contains(addr)) {
                return Err(format!("{addr} is in {ENV_INTERNAL_SUBNETS}"));
            }
            if !outbound::is_public(addr) {
                return Err(format!("{addr} is not a public address"));
            }
            match addr {
                IpAddr::V4(v4) => Dest::Addr(v4),
                IpAddr::V6(_) => return Err(format!("{addr}: only IPv4 destinations are dialled")),
            }
        }
    };
    let name = match (alias, &dest) {
        (Some(alias), _) => dns_name(alias).map_err(|e| format!("alias: {e}"))?,
        (None, Dest::Name(n)) => n.clone(),
        (None, Dest::Addr(_)) => unreachable!("an address without an alias is refused above"),
    };
    Ok(Entry {
        raw: raw.to_string(),
        name,
        dest,
        port,
        listen_port,
    })
}

/// [`ENV_DENY`]'s networks, strictly: each entry is an IPv4 address (its /32) or an IPv4
/// network `a.b.c.d/n` with `n` plain digits from 0 to 32 and no bit set below the prefix.
/// Blank entries are skipped. Anything else, IPv6 included (the service dials IPv4 only), is an
/// error naming the setting and the entry.
pub fn parse_deny(list: &str) -> Result<Vec<Net>, String> {
    let mut nets = Vec::new();
    for raw in list.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        let bad = |why: &str| format!("{ENV_DENY}: entry {raw:?}: {why}");
        let (addr, prefix) = match raw.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (raw, None),
        };
        let addr: Ipv4Addr = match addr.parse::<IpAddr>() {
            Ok(IpAddr::V4(a)) => a,
            Ok(IpAddr::V6(_)) => {
                return Err(bad("IPv6 is not supported: only IPv4 is dialled"));
            }
            Err(_) if addr.contains(':') || addr.contains('[') => {
                return Err(bad("IPv6 is not supported: only IPv4 is dialled"));
            }
            Err(_) => return Err(bad("expected an IPv4 address or network (a.b.c.d/n)")),
        };
        let prefix = match prefix {
            None => 32,
            Some(p) => p
                .parse::<u8>()
                .ok()
                .filter(|n| *n <= 32 && !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
                .ok_or_else(|| bad("the prefix must be a whole number from 0 to 32"))?,
        };
        let host_bits = u32::MAX.checked_shr(u32::from(prefix)).unwrap_or(0);
        if u32::from(addr) & host_bits != 0 {
            let base = Ipv4Addr::from(u32::from(addr) & !host_bits);
            return Err(bad(&format!(
                "bits are set below the /{prefix} prefix (the network is {base}/{prefix})"
            )));
        }
        nets.push(Net::new(IpAddr::V4(addr), prefix).expect("a prefix of at most 32"));
    }
    Ok(nets)
}

/// The health port from the setting's value: unset or blank is the default (8080), anything
/// else must be a port. The service and `compute-egress health` both read it this way.
pub fn health_port(value: Option<&str>) -> Result<u16, String> {
    match value.map(str::trim).filter(|v| !v.is_empty()) {
        None => Ok(8080),
        Some(v) => port_number(v).map_err(|e| format!("{ENV_HEALTH_PORT}={v:?}: {e}")),
    }
}

/// A port: plain digits, 1 to 65535.
fn port_number(s: &str) -> Result<u16, String> {
    if s.is_empty() || s.len() > 5 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("{s:?} is not a port (1-65535)"));
    }
    match s.parse::<u32>() {
        Ok(p @ 1..=65535) => Ok(p as u16),
        _ => Err(format!("{s:?} is not a port (1-65535)")),
    }
}

/// A DNS host name, lower case without a trailing dot: at least two labels of letters, digits
/// and inner hyphens, at most 63 bytes each and 253 in all, the last not all digits (the
/// system resolver would read such a name as an address).
fn dns_name(s: &str) -> Result<String, String> {
    let name = s.strip_suffix('.').unwrap_or(s).to_ascii_lowercase();
    let bad = |why: &str| Err(format!("{s:?} {why}"));
    if name.is_empty() || name.len() > 253 {
        return bad("is not a host name of 1 to 253 bytes");
    }
    let labels: Vec<&str> = name.split('.').collect();
    if labels.len() < 2 {
        return bad("needs at least two labels (a.b)");
    }
    for label in &labels {
        let ok = (1..=63).contains(&label.len())
            && label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-');
        if !ok {
            return bad("is not a host name (letters, digits and inner hyphens; no wildcards)");
        }
    }
    if labels
        .last()
        .is_some_and(|l| l.bytes().all(|b| b.is_ascii_digit()))
    {
        return bad("reads as an address, not a host name");
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The `health` subcommand reads the same setting as the service: blank (compose passes the
    // variable through, empty when `.env` doesn't set it) is the default, never an error.
    #[test]
    fn the_health_port_reads_a_blank_setting_as_the_default() {
        assert_eq!(health_port(None), Ok(8080));
        assert_eq!(health_port(Some("")), Ok(8080));
        assert_eq!(health_port(Some("  ")), Ok(8080));
        assert_eq!(health_port(Some(" 9090 ")), Ok(9090));
        assert!(health_port(Some("0")).is_err());
        assert!(health_port(Some("http")).is_err());
        // The service agrees on every one of them.
        for v in ["", "  ", "9090"] {
            let get = |k: &str| match k {
                ENV_LISTEN => Some("127.0.0.1".to_string()),
                ENV_HEALTH_PORT => Some(v.to_string()),
                _ => None,
            };
            let s = Settings::from_lookup(get).unwrap();
            assert_eq!(Ok(s.health_port), health_port(Some(v)), "{v:?}");
        }
    }
    use std::collections::HashMap;

    const RESERVED: &[(u16, &str)] = &[(53, ENV_DNS_PORT), (8080, ENV_HEALTH_PORT)];

    fn parse(list: &str) -> Result<Vec<Entry>, String> {
        parse_allow(list, RESERVED, &[])
    }

    fn one(list: &str) -> Entry {
        let mut entries = parse(list).unwrap_or_else(|e| panic!("{list:?}: {e}"));
        assert_eq!(entries.len(), 1, "{list:?}");
        entries.remove(0)
    }

    /// Refused, with a message that names the setting and the entry.
    #[track_caller]
    fn refused(list: &str, entry: &str) -> String {
        let err = match parse(list) {
            Ok(e) => panic!("{list:?} was accepted: {e:?}"),
            Err(e) => e,
        };
        assert!(err.contains(ENV_ALLOW), "{list:?}: {err}");
        assert!(err.contains(entry), "{list:?} should name {entry:?}: {err}");
        err
    }

    #[test]
    fn parses_host_port() {
        let e = one("db.example.com:5432");
        assert_eq!(e.name, "db.example.com");
        assert_eq!(e.dest, Dest::Name("db.example.com".into()));
        assert_eq!((e.port, e.listen_port), (5432, 5432));
        assert_eq!(e.raw, "db.example.com:5432");
        // Case and a trailing dot are normalized; whitespace around entries is ignored.
        let e = one("  Db.Example.COM.:443 , ");
        assert_eq!(e.name, "db.example.com");
        assert_eq!(e.dest, Dest::Name("db.example.com".into()));
        assert_eq!(e.raw, "Db.Example.COM.:443");
    }

    #[test]
    fn parses_alias_for_an_address() {
        let e = one("warehouse.ext=1.1.1.1:5432");
        assert_eq!(e.name, "warehouse.ext");
        assert_eq!(e.dest, Dest::Addr(Ipv4Addr::new(1, 1, 1, 1)));
        assert_eq!((e.port, e.listen_port), (5432, 5432));
        // An alias for a name: computes dial the alias, the forwarder resolves the host.
        let e = one("pg.ext=db.example.com:6432");
        assert_eq!(e.name, "pg.ext");
        assert_eq!(e.dest, Dest::Name("db.example.com".into()));
    }

    #[test]
    fn parses_a_listen_port() {
        let e = one("b.example.com:5432@15432");
        assert_eq!(e.name, "b.example.com");
        assert_eq!((e.port, e.listen_port), (5432, 15432));
    }

    #[test]
    fn an_empty_list_has_no_entries() {
        for list in ["", "  ", " , ,"] {
            assert_eq!(parse(list).unwrap(), Vec::new(), "{list:?}");
        }
    }

    #[test]
    fn parses_several_entries_in_order() {
        let entries =
            parse("a.example.com:5432, b.example.com:5432@15432,c.ext=8.8.8.8:53@5353").unwrap();
        let names: Vec<_> = entries
            .iter()
            .map(|e| (e.name.as_str(), e.listen_port))
            .collect();
        assert_eq!(
            names,
            [
                ("a.example.com", 5432),
                ("b.example.com", 15432),
                ("c.ext", 5353)
            ]
        );
    }

    #[test]
    fn the_workbench_host_name_is_not_a_platform_name() {
        // Listed in the workbench: it must start, then be refused per connection.
        let e = one("host.docker.internal:5432");
        assert_eq!(e.dest, Dest::Name("host.docker.internal".into()));
    }

    #[test]
    fn refuses_an_entry_without_a_port() {
        refused("db.example.com", "db.example.com");
        refused("db.example.com:", "db.example.com:");
        refused("a.ext=1.1.1.1", "a.ext=1.1.1.1");
    }

    #[test]
    fn refuses_port_zero() {
        refused("db.example.com:0", "db.example.com:0");
        refused("db.example.com:5432@0", "db.example.com:5432@0");
    }

    #[test]
    fn refuses_a_port_above_65535() {
        refused("db.example.com:65536", "db.example.com:65536");
        refused("db.example.com:5432@70000", "db.example.com:5432@70000");
    }

    #[test]
    fn refuses_a_port_that_is_not_plain_digits() {
        for e in [
            "db.example.com:+5432",
            "db.example.com:5432@+1",
            "db.example.com:0x10",
            "db.example.com: 5432",
        ] {
            refused(e, e);
        }
    }

    #[test]
    fn refuses_an_address_without_an_alias() {
        let err = refused("1.1.1.1:5432", "1.1.1.1:5432");
        assert!(err.contains("alias"), "{err}");
    }

    #[test]
    fn refuses_a_non_public_address() {
        for e in [
            "a.ext=10.0.0.1:5432",
            "a.ext=127.0.0.1:5432",
            "a.ext=172.16.0.1:5432",
            "a.ext=192.168.1.1:5432",
            "a.ext=100.64.0.1:5432",
            "a.ext=169.254.169.254:80",
            "a.ext=0.0.0.0:5432",
            "a.ext=203.0.113.7:5432",
            "a.ext=[fd00::1]:5432",
            "a.ext=[::ffff:10.0.0.1]:5432",
            "a.ext=[::1]:5432",
        ] {
            refused(e, e);
        }
    }

    #[test]
    fn refuses_an_address_in_the_internal_subnets() {
        let internal: Vec<Net> = vec!["192.88.99.0/24".parse().unwrap()];
        let err = parse_allow("a.ext=192.88.99.5:5432", RESERVED, &internal).unwrap_err();
        assert!(
            err.contains(ENV_ALLOW) && err.contains("a.ext=192.88.99.5:5432"),
            "{err}"
        );
    }

    #[test]
    fn refuses_an_ipv6_destination() {
        refused("a.ext=[2606:4700::1111]:443", "a.ext=[2606:4700::1111]:443");
    }

    #[test]
    fn refuses_a_platform_service_name() {
        for e in [
            "metadata-db:5432",
            "gateway:8000",
            "Gateway.:8000",
            "gateway=1.1.1.1:8000",
            "pageserver=db.example.com:6400",
            "chelabase-proxy-1:4432",
            "localhost:5432",
        ] {
            let err = refused(e, e);
            assert!(err.contains("platform"), "{e}: {err}");
        }
        // A public name that merely contains one is fine.
        one("gateway.example.com:443");
    }

    #[test]
    fn refuses_a_duplicate_entry() {
        refused(
            "a.example.com:5432,a.example.com:5432",
            "a.example.com:5432",
        );
        refused(
            "a.example.com:5432,A.example.com.:5432@15432",
            "A.example.com.:5432@15432",
        );
    }

    #[test]
    fn refuses_one_name_for_two_destinations() {
        refused(
            "a.example.com=b.example.com:5432,a.example.com:6432",
            "a.example.com:6432",
        );
    }

    #[test]
    fn refuses_a_listen_port_on_the_resolver_or_health_port() {
        let err = refused("dns.example.com:53", "dns.example.com:53");
        assert!(err.contains(ENV_DNS_PORT), "{err}");
        let err = refused("x.example.com:5432@8080", "x.example.com:5432@8080");
        assert!(err.contains(ENV_HEALTH_PORT), "{err}");
    }

    #[test]
    fn refuses_an_unknown_suffix() {
        for e in [
            "db.example.com:5432/tcp",
            "db.example.com:5432@15432x",
            "db.example.com:5432@1@2",
            "db.example.com:5432 tls",
            "db.example.com:5432:1",
            "a=b=db.example.com:5432",
        ] {
            refused(e, e);
        }
    }

    #[test]
    fn refuses_wildcards_networks_and_ranges() {
        for e in [
            "*.example.com:443",
            "a.ext=10.0.0.0/8:5432",
            "db.example.com:5432-5440",
            "a.ext=1.1.1.0/24:5432",
        ] {
            refused(e, e);
        }
    }

    #[test]
    fn refuses_a_name_that_reads_as_an_address() {
        // The system resolver would read these as addresses.
        for e in [
            "127.1:5432",
            "0x7f.1:5432",
            "a.2130706433:5432",
            "a.ext=127.0.0.1.:5432",
        ] {
            refused(e, e);
        }
    }

    #[test]
    fn refuses_a_malformed_name() {
        for e in [
            "db:5432",
            "-db.example.com:5432",
            "db-.example.com:5432",
            "db..example.com:5432",
            "db_x.example.com:5432",
            "dé.example.com:5432",
            ".example.com:5432",
            "=db.example.com:5432",
        ] {
            refused(e, e);
        }
        let long = format!("{}.example.com:5432", "a".repeat(64));
        refused(&long, &long);
        let long = format!("{}com:5432", "abcdefghi.".repeat(26));
        refused(&long, &long);
    }

    #[test]
    fn two_entries_on_one_port_need_an_assignment() {
        let err = refused(
            "a.example.com:5432,b.example.com:5432",
            "a.example.com:5432",
        );
        assert!(err.contains("b.example.com:5432"), "{err}");
        assert!(err.contains("@<listen-port>"), "{err}");
        // An assignment onto a port another entry already uses is the same clash.
        let err = refused(
            "a.example.com:5432,b.example.com:6432@5432",
            "b.example.com:6432@5432",
        );
        assert!(err.contains("a.example.com:5432"), "{err}");
        // With an assignment, both start.
        assert_eq!(
            parse("a.example.com:5432,b.example.com:5432@15432")
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn no_entry_form_allows_a_private_address() {
        let internal: Vec<Net> = vec!["192.88.99.0/24".parse().unwrap()];
        for e in [
            "10.0.0.1:5432",
            "a.ext=10.0.0.1:5432",
            "a.ext=10.0.0.1:5432@15432",
            "a.ext=[::ffff:10.0.0.1]:5432",
            "a.ext=[::ffff:a00:1]:5432",
            "a.ext=[64:ff9b::a00:1]:5432",
            "a.ext=[2002:a00:1::]:5432",
            "a.ext=[fd00:ec2::254]:80",
            "a.ext=[fe80::1]:5432",
            "a.ext=127.0.0.1:5432",
            "a.ext=192.88.99.5:5432",
            "a.ext=0177.0.0.1:5432",
            "a.ext=0x7f000001:5432",
            "a.ext=2130706433:5432",
            "a.ext=127.1:5432",
            "a.ext=127.0.0.1.:5432",
            "10.0.0.1=db.example.com:5432",
        ] {
            assert!(
                parse_allow(e, RESERVED, &internal).is_err(),
                "{e} was accepted"
            );
        }
    }

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn settings_have_defaults() {
        let s = Settings::from_lookup(env(&[(ENV_LISTEN, "10.1.2.3")])).unwrap();
        assert_eq!(s.listen, IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3)));
        assert_eq!(s.advertise, Ipv4Addr::new(10, 1, 2, 3));
        assert_eq!((s.dns_port, s.health_port), (53, 8080));
        assert_eq!(
            (s.max_connections, s.max_connections_per_client),
            (1024, 64)
        );
        assert_eq!(s.dial_timeout, Duration::from_secs(10));
        assert!(s.entries.is_empty() && s.internal.is_empty());
    }

    #[test]
    fn settings_read_every_value() {
        let s = Settings::from_lookup(env(&[
            (ENV_LISTEN, "10.1.2.3"),
            (ENV_ADVERTISE, "10.9.8.7"),
            (ENV_DNS_PORT, "5353"),
            (ENV_HEALTH_PORT, "9000"),
            (ENV_INTERNAL_SUBNETS, "172.30.0.0/24, 172.31.0.0/24"),
            (ENV_MAX_CONNECTIONS, "10"),
            (ENV_MAX_CONNECTIONS_PER_CLIENT, "2"),
            (ENV_DIAL_TIMEOUT_SECS, "3"),
            (ENV_ALLOW, "db.example.com:53"),
        ]))
        .unwrap();
        assert_eq!(s.advertise, Ipv4Addr::new(10, 9, 8, 7));
        assert_eq!((s.dns_port, s.health_port), (5353, 9000));
        assert_eq!(s.internal.len(), 2);
        assert_eq!((s.max_connections, s.max_connections_per_client), (10, 2));
        assert_eq!(s.dial_timeout, Duration::from_secs(3));
        // Port 53 is free for an entry once the resolver is elsewhere.
        assert_eq!(s.entries[0].listen_port, 53);
    }

    #[test]
    fn settings_need_a_listen_address() {
        let err = Settings::from_lookup(env(&[])).unwrap_err();
        assert!(err.contains(ENV_LISTEN), "{err}");
    }

    #[test]
    fn settings_refuse_the_wildcard_address() {
        for listen in ["0.0.0.0", "::", "egress", "224.0.0.1"] {
            let err = Settings::from_lookup(env(&[(ENV_LISTEN, listen)])).unwrap_err();
            assert!(err.contains(ENV_LISTEN), "{listen}: {err}");
        }
    }

    #[test]
    fn settings_refuse_an_advertised_value_that_is_not_an_ipv4_address() {
        for adv in ["egress", "0.0.0.0", "fd00::1", "10.0.0.1:53"] {
            let err = Settings::from_lookup(env(&[(ENV_LISTEN, "10.1.2.3"), (ENV_ADVERTISE, adv)]))
                .unwrap_err();
            assert!(err.contains(ENV_ADVERTISE), "{adv}: {err}");
        }
        // An IPv6 listen address needs an advertised IPv4 address.
        assert!(Settings::from_lookup(env(&[(ENV_LISTEN, "fd00::5")])).is_err());
    }

    #[test]
    fn settings_refuse_bad_numbers() {
        for (k, v) in [
            (ENV_DNS_PORT, "0"),
            (ENV_DNS_PORT, "65536"),
            (ENV_HEALTH_PORT, "x"),
            (ENV_MAX_CONNECTIONS, "0"),
            (ENV_MAX_CONNECTIONS_PER_CLIENT, "0"),
            (ENV_DIAL_TIMEOUT_SECS, "0"),
            (ENV_DIAL_TIMEOUT_SECS, "-1"),
            (ENV_INTERNAL_SUBNETS, "nope"),
        ] {
            let err = Settings::from_lookup(env(&[(ENV_LISTEN, "10.1.2.3"), (k, v)])).unwrap_err();
            assert!(err.contains(k), "{k}={v}: {err}");
        }
        // The resolver and the health port must differ.
        let err = Settings::from_lookup(env(&[(ENV_LISTEN, "10.1.2.3"), (ENV_DNS_PORT, "8080")]))
            .unwrap_err();
        assert!(err.contains(ENV_HEALTH_PORT), "{err}");
    }

    /// Every invalid deny entry stops the start, naming the setting and the entry: a value that
    /// isn't an IPv4 address or network, a prefix out of range or not plain digits, host bits
    /// below the prefix, and any IPv6 form (the service dials IPv4 only).
    #[test]
    fn settings_refuse_an_invalid_deny_entry() {
        for bad in [
            "nope",
            "203.0.113.0/33",
            "203.0.113.0/",
            "203.0.113.0/+8",
            "203.0.113.0/0x8",
            "/24",
            "203.0.113.7/24",
            "203.0.113",
            "203.0.113.1:5432",
            "fd00::/8",
            "2001:db8::1",
            "::ffff:203.0.113.5",
            "[2001:db8::1]",
        ] {
            let list = format!("198.51.100.0/24, {bad}");
            let err = Settings::from_lookup(env(&[
                (ENV_LISTEN, "10.1.2.3"),
                ("CHELABASE_COMPUTE_EGRESS_DENY", &list),
            ]))
            .map(|_| ())
            .expect_err(bad);
            assert!(
                err.contains("CHELABASE_COMPUTE_EGRESS_DENY") && err.contains(bad.trim()),
                "{bad}: {err}"
            );
        }
    }

    #[test]
    fn settings_read_the_deny_list() {
        let s = Settings::from_lookup(env(&[
            (ENV_LISTEN, "10.1.2.3"),
            (ENV_DENY, " 203.0.113.7 ,, 198.51.100.0/24,0.0.0.0/0 "),
        ]))
        .unwrap();
        let want: Vec<Net> = ["203.0.113.7/32", "198.51.100.0/24", "0.0.0.0/0"]
            .iter()
            .map(|n| n.parse().unwrap())
            .collect();
        assert_eq!(s.deny, want);
        for blank in ["", " ", ","] {
            let s =
                Settings::from_lookup(env(&[(ENV_LISTEN, "10.1.2.3"), (ENV_DENY, blank)])).unwrap();
            assert!(s.deny.is_empty(), "{blank:?}");
        }
    }

    #[test]
    fn settings_refuse_an_address_entry_in_the_deny_list() {
        let err = Settings::from_lookup(env(&[
            (ENV_LISTEN, "10.1.2.3"),
            (
                ENV_ALLOW,
                "db.example.com:5432,pub.example.com=192.88.99.7:5432@15432",
            ),
            (ENV_DENY, "192.88.99.0/24"),
        ]))
        .unwrap_err();
        assert!(
            err.contains(ENV_ALLOW)
                && err.contains(ENV_DENY)
                && err.contains("pub.example.com=192.88.99.7"),
            "{err}"
        );
    }

    #[test]
    fn settings_refuse_an_invalid_list() {
        let err = Settings::from_lookup(env(&[
            (ENV_LISTEN, "10.1.2.3"),
            (ENV_ALLOW, "db.example.com:5432,bad"),
        ]))
        .unwrap_err();
        assert!(err.contains(ENV_ALLOW) && err.contains("bad"), "{err}");
    }
}
