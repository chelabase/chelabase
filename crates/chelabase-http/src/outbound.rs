//! Outbound requests to URLs a user chose (storage webhooks, m18): only to the public internet.
//!
//! A user-supplied URL could point the platform at its own network: loopback, a private or
//! CGNAT range, link-local (the cloud metadata service at 169.254.169.254), or their IPv6
//! equivalents. [`Outbound`] refuses all of those, whatever spelling the URL uses (decimal,
//! octal or hex IPv4, IPv4-mapped IPv6: the WHATWG parser normalizes them all to an address),
//! and it checks the addresses a host name resolves to **at connect time**, inside the client's
//! own resolver, so the address that is checked is the address that is dialled (no DNS
//! rebinding between a check and the connection). A host that resolves to any refused address
//! is refused as a whole.
//!
//! The client never follows a redirect ([`crate::client_builder`]'s policy) and never uses a
//! proxy (a proxy would resolve the target itself, past the check). Only `https` is accepted
//! unless `allow_http` is set (local development), and URLs carrying credentials are refused.
//! An operator's allowlist names hosts (exact host names or IP literals, case-insensitive) that
//! skip the address check, for self-hosted targets inside their own network.
//!
//! A **hard deny** (edge functions m9) overrides the allowlist: host names
//! ([`Outbound::deny_hosts`], compared without case or a trailing dot, container names
//! `<project>-<name>-<n>` included) and networks ([`Outbound::deny_nets`]) checked against
//! every resolved address at connect time. [`Outbound::deny_stack`] denies the stack's own
//! services ([`STACK_HOSTS`]), loopback, link-local and the given networks: an allowlisted name
//! that resolves to a stack container is still refused.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::BTreeSet;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

pub use reqwest::Url;

/// `CHELABASE_WEBHOOKS_ALLOW_HTTP`: `true` also accepts `http` URLs (local development).
pub const ENV_ALLOW_HTTP: &str = "CHELABASE_WEBHOOKS_ALLOW_HTTP";
/// `CHELABASE_WEBHOOKS_ALLOW_HOSTS`: comma-separated host names or IP literals that may resolve
/// to non-public addresses (self-hosted targets).
pub const ENV_ALLOW_HOSTS: &str = "CHELABASE_WEBHOOKS_ALLOW_HOSTS";

/// The settings' prefix for storage webhooks (`CHELABASE_WEBHOOKS_ALLOW_HTTP`, `_ALLOW_HOSTS`).
pub const WEBHOOKS_ENV: &str = "CHELABASE_WEBHOOKS";
/// The settings' prefix for external services (`CHELABASE_SERVICES_ALLOW_HTTP`, `_ALLOW_HOSTS`).
pub const SERVICES_ENV: &str = "CHELABASE_SERVICES";

/// The stack's own service names and aliases: never a target, allowlisted or not.
pub const STACK_HOSTS: &[&str] = &[
    "localhost", "metadata-db", "nats", "valkey", "seaweedfs", "create-bucket", "storage-broker",
    "safekeeper1", "safekeeper2", "safekeeper3", "storcon-db", "storage-controller", "pageserver",
    "register-safekeepers", "proxy-tls", "proxy", "identity", "orchestrator", "worker", "gateway",
    "gateway-functions", "auth", "rest", "realtime", "storage", "studio", "functions",
    "functions-public", "functions-internal", "functions-control", "functions-runtime",
    "service-runner", "init-secrets", "init-bootstrap", "mailpit", "control", "secrets",
];

/// Always denied by [`Outbound::deny_stack`]: "this network", loopback and link-local.
const STACK_NETS: &[&str] = &["0.0.0.0/8", "127.0.0.0/8", "169.254.0.0/16", "::/128", "::1/128", "fe80::/10"];

/// The longest URL accepted.
pub const MAX_URL_LEN: usize = 2048;

/// Why a URL or a connection was refused. Never carries the URL itself (its path or query may
/// hold a token), only the host or address at fault. Its `Display` is what a user may see: it
/// names the host as written, never an address the host resolved to (that would map the
/// operator's internal DNS); [`Refused::log_detail`] adds the address, for logs only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// Not a valid absolute URL, or longer than [`MAX_URL_LEN`].
    Invalid,
    /// Not `https` (or `http` when allowed).
    Scheme,
    /// The URL carries a user name or password.
    Credentials,
    /// The URL has no host.
    NoHost,
    /// The host is, or resolves to, an address that isn't public. `addr` is for logs only
    /// ([`Refused::log_detail`]).
    NotPublic { host: String, addr: IpAddr },
    /// The host didn't resolve.
    Unresolved { host: String },
    /// The host is one of the platform's own (by name), or resolves into its networks: the
    /// hard deny, which the allowlist doesn't lift.
    Internal { host: String },
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::Invalid => {
                write!(f, "not a valid absolute URL of at most {MAX_URL_LEN} bytes")
            }
            Refused::Scheme => f.write_str("the URL must use https"),
            Refused::Credentials => f.write_str("the URL must not carry a user name or password"),
            Refused::NoHost => f.write_str("the URL has no host"),
            Refused::NotPublic { host, .. } if literal(host).is_some() => {
                write!(f, "{host} is not a public address")
            }
            Refused::NotPublic { host, .. } => {
                write!(f, "{host} resolves to a non-public address")
            }
            Refused::Unresolved { host } => write!(f, "{host} does not resolve"),
            Refused::Internal { host } => write!(f, "{host} is part of the platform"),
        }
    }
}

impl Refused {
    /// The refusal with the address at fault, for the operator's logs only (never an API
    /// answer).
    pub fn log_detail(&self) -> String {
        match self {
            Refused::NotPublic { addr, .. } => format!("{self} ({addr})"),
            _ => self.to_string(),
        }
    }
}

impl std::error::Error for Refused {}

type Lookup<'a> = Pin<Box<dyn Future<Output = std::io::Result<Vec<IpAddr>>> + Send + 'a>>;

/// Resolves a host name to its addresses (the system resolver, or a fake in tests).
pub trait HostLookup: Send + Sync + 'static {
    fn lookup<'a>(&'a self, host: &'a str) -> Lookup<'a>;
}

/// The system resolver (`getaddrinfo` through tokio).
pub struct SystemLookup;

impl HostLookup for SystemLookup {
    fn lookup<'a>(&'a self, host: &'a str) -> Lookup<'a> {
        Box::pin(async move {
            Ok(tokio::net::lookup_host((host, 0))
                .await?
                .map(|a| a.ip())
                .collect())
        })
    }
}

/// Who may be called: the scheme rule, the operator's allowlist, the hard deny and the
/// resolver.
#[derive(Clone)]
pub struct Outbound {
    allow_http: bool,
    allow_hosts: Arc<BTreeSet<String>>,
    /// Allowlist entries with a port (kept only to warn about).
    allow_hosts_ported: Arc<Vec<String>>,
    /// Denied names, normalized ([`deny_name`]).
    deny_hosts: Arc<BTreeSet<String>>,
    deny_nets: Arc<Vec<Net>>,
    lookup: Arc<dyn HostLookup>,
}

impl std::fmt::Debug for Outbound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Outbound")
            .field("allow_http", &self.allow_http)
            .field("allow_hosts", &self.allow_hosts)
            .field("deny_nets", &self.deny_nets)
            .finish()
    }
}

/// An IP network, `addr/prefix`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Net {
    addr: IpAddr,
    prefix: u8,
}

impl Net {
    pub fn new(addr: IpAddr, prefix: u8) -> Option<Self> {
        let max = if addr.is_ipv4() { 32 } else { 128 };
        (prefix <= max).then_some(Net { addr, prefix })
    }

    /// Whether `ip` is in the network (an IPv4-mapped IPv6 address as its IPv4 address).
    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
            v4 => v4,
        };
        let bits = |a: IpAddr| -> (u128, u32) {
            match a {
                IpAddr::V4(v4) => (u32::from(v4) as u128, 32),
                IpAddr::V6(v6) => (u128::from(v6), 128),
            }
        };
        let ((net, width), (addr, other)) = (bits(self.addr), bits(ip));
        if width != other {
            return false;
        }
        let shift = width - u32::from(self.prefix);
        shift >= width || (net >> shift) == (addr >> shift)
    }
}

impl std::str::FromStr for Net {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let bad = || format!("{s:?} is not a network (addr/prefix)");
        let (addr, prefix) = s.trim().split_once('/').ok_or_else(bad)?;
        let addr: IpAddr = addr.parse().map_err(|_| bad())?;
        Net::new(addr, prefix.parse().map_err(|_| bad())?).ok_or_else(bad)
    }
}

/// Comma-separated networks (`CHELABASE_INTERNAL_SUBNETS`); blank entries are skipped.
pub fn parse_nets(list: &str) -> Result<Vec<Net>, String> {
    list.split(',')
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(str::parse)
        .collect()
}

/// The networks this host is directly attached to: the IPv4 (`/proc/net/route`) and IPv6
/// (`/proc/net/ipv6_route`) routes without a gateway, a container's compose networks. A table
/// that can't be read adds none.
pub fn attached_nets() -> Vec<Net> {
    let read = |path: &str| std::fs::read_to_string(path).unwrap_or_default();
    let mut nets = parse_route_table(&read("/proc/net/route"));
    nets.extend(parse_ipv6_route_table(&read("/proc/net/ipv6_route")));
    nets
}

/// `ipv6_route` lines: destination, prefix length, source, source prefix, next hop (hex), …
fn parse_ipv6_route_table(table: &str) -> Vec<Net> {
    table
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            let dest = u128::from_str_radix(f.first()?, 16).ok()?;
            let prefix = u8::from_str_radix(f.get(1)?, 16).ok()?;
            let next_hop = u128::from_str_radix(f.get(4)?, 16).ok()?;
            (next_hop == 0 && prefix > 0).then(|| Net::new(IpAddr::V6(Ipv6Addr::from(dest)), prefix))?
        })
        .collect()
}

fn parse_route_table(table: &str) -> Vec<Net> {
    let hex = |s: &str| u32::from_str_radix(s, 16).ok().map(|v| Ipv4Addr::from(v.swap_bytes()));
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            let (dest, gateway, mask) = (hex(f.get(1)?)?, hex(f.get(2)?)?, hex(f.get(7)?)?);
            let prefix = u32::from(mask).leading_ones() as u8;
            (gateway.is_unspecified() && prefix > 0).then(|| Net::new(IpAddr::V4(dest), prefix))?
        })
        .collect()
}

impl Outbound {
    /// `https` only, no allowlist, no deny, the system resolver.
    pub fn new() -> Self {
        Self {
            allow_http: false,
            allow_hosts: Arc::new(BTreeSet::new()),
            allow_hosts_ported: Arc::new(Vec::new()),
            deny_hosts: Arc::new(BTreeSet::new()),
            deny_nets: Arc::new(Vec::new()),
            lookup: Arc::new(SystemLookup),
        }
    }

    /// Also accept `http` URLs.
    pub fn allow_http(mut self, allow: bool) -> Self {
        self.allow_http = allow;
        self
    }

    /// Hosts (names or IP literals) that skip the address check.
    pub fn allow_hosts<I: IntoIterator<Item = S>, S: AsRef<str>>(mut self, hosts: I) -> Self {
        let hosts: Vec<String> = hosts
            .into_iter()
            .map(|h| h.as_ref().trim().to_string())
            .filter(|h| !h.is_empty())
            .collect();
        self.allow_hosts_ported = Arc::new(hosts.iter().filter(|h| has_port(h)).cloned().collect());
        self.allow_hosts = Arc::new(hosts.iter().map(|h| normalize_host(h)).collect());
        self
    }

    /// Host names that are always refused, allowlisted or not (see [`Self::denied_name`]).
    pub fn deny_hosts<I: IntoIterator<Item = S>, S: AsRef<str>>(mut self, hosts: I) -> Self {
        self.deny_hosts = Arc::new(hosts.into_iter().map(|h| deny_name(h.as_ref())).collect());
        self
    }

    /// Networks no connection may reach, allowlisted or not.
    pub fn deny_nets(mut self, nets: impl IntoIterator<Item = Net>) -> Self {
        self.deny_nets = Arc::new(nets.into_iter().collect());
        self
    }

    /// The stack's hard deny: [`STACK_HOSTS`], loopback, link-local and `internal` (the stack's
    /// own networks).
    pub fn deny_stack(self, internal: impl IntoIterator<Item = Net>) -> Self {
        let mut nets: Vec<Net> = STACK_NETS.iter().map(|n| n.parse().expect("a network")).collect();
        nets.extend(internal);
        self.deny_hosts(STACK_HOSTS).deny_nets(nets)
    }

    /// Resolve with `lookup` instead of the system resolver.
    pub fn with_lookup(mut self, lookup: Arc<dyn HostLookup>) -> Self {
        self.lookup = lookup;
        self
    }

    /// From `{prefix}_ALLOW_HTTP` (`true` or `false`, blank is `false`) and
    /// `{prefix}_ALLOW_HOSTS` ([`WEBHOOKS_ENV`], [`SERVICES_ENV`]): each user of outbound
    /// requests has its own settings.
    pub fn from_lookup(
        prefix: &str,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let (http_var, hosts_var) = (format!("{prefix}_ALLOW_HTTP"), format!("{prefix}_ALLOW_HOSTS"));
        let allow_http = match lookup(&http_var).map(|v| v.trim().to_ascii_lowercase()) {
            None => false,
            Some(v) if v.is_empty() || v == "false" => false,
            Some(v) if v == "true" => true,
            Some(v) => return Err(format!("{http_var}={v:?}: expected true or false")),
        };
        let hosts = lookup(&hosts_var).unwrap_or_default();
        Ok(Self::new()
            .allow_http(allow_http)
            .allow_hosts(hosts.split(',').map(str::trim)))
    }

    /// Allowlist entries that carry a port, as given: hosts are compared without one, so these
    /// never match (the service warns at start).
    pub fn allow_hosts_with_port(&self) -> Vec<&str> {
        self.allow_hosts_ported.iter().map(String::as_str).collect()
    }

    fn allowlisted(&self, host: &str) -> bool {
        self.allow_hosts.contains(&normalize_host(host))
    }

    /// Whether `host` is a denied name: equal to one without case or a trailing dot, or a
    /// one-label container name for one (`<project>-<name>-<n>`, `<project>_<name>_<n>`).
    pub fn denied_name(&self, host: &str) -> bool {
        let host = deny_name(host);
        if self.deny_hosts.contains(&host) {
            return true;
        }
        if host.contains('.') {
            return false;
        }
        let base = host.trim_end_matches(|c: char| c.is_ascii_digit());
        let Some(base) = base.strip_suffix(['-', '_']).filter(|_| base.len() < host.len()) else {
            return false;
        };
        self.deny_hosts.iter().any(|d| {
            base == d
                || base
                    .strip_suffix(d.as_str())
                    .is_some_and(|p| p.ends_with(['-', '_']))
        })
    }

    fn denied_addr(&self, addr: IpAddr) -> bool {
        self.deny_nets.iter().any(|n| n.contains(addr))
    }

    /// The URL, parsed, if its form may be called: a valid absolute URL of an accepted scheme,
    /// without credentials, whose host isn't denied and, when an IP literal, is public (or
    /// allowlisted) and outside the denied networks. A host name is checked when it is
    /// resolved ([`Self::check_resolved`], and at connect time).
    pub fn check_url(&self, raw: &str) -> Result<Url, Refused> {
        if raw.len() > MAX_URL_LEN {
            return Err(Refused::Invalid);
        }
        let url = Url::parse(raw).map_err(|_| Refused::Invalid)?;
        // Again as stored: percent-encoding and IDNA can make the normalized URL longer.
        if url.as_str().len() > MAX_URL_LEN {
            return Err(Refused::Invalid);
        }
        match url.scheme() {
            "https" => {}
            "http" if self.allow_http => {}
            _ => return Err(Refused::Scheme),
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(Refused::Credentials);
        }
        let host = url
            .host_str()
            .filter(|h| !h.is_empty())
            .ok_or(Refused::NoHost)?;
        let internal = || Refused::Internal {
            host: host.to_string(),
        };
        if self.denied_name(host) {
            return Err(internal());
        }
        // The parser has already normalized every IPv4 spelling (decimal, octal, hex, short
        // forms) to dotted decimal, and an IPv6 host comes bracketed: either parses here.
        if let Some(addr) = literal(host) {
            if self.denied_addr(addr) {
                return Err(internal());
            }
            if !self.allowlisted(host) && !is_public(addr) {
                return Err(Refused::NotPublic {
                    host: host.to_string(),
                    addr,
                });
            }
        }
        Ok(url)
    }

    /// The URL's form ([`Self::check_url`]), then every address its host resolves to now: the
    /// check a registration makes. Delivery checks again at connect time.
    pub async fn check_resolved(&self, raw: &str) -> Result<Url, Refused> {
        let url = self.check_url(raw)?;
        let host = url.host_str().unwrap_or_default();
        if literal(host).is_none() {
            self.checked_lookup(host).await?;
        }
        Ok(url)
    }

    /// A client for this policy: no redirects, no proxy, `timeout` per request, and a resolver
    /// that refuses any host resolving to a non-public address (unless allowlisted) or into a
    /// denied network.
    pub fn client(&self, timeout: Duration) -> reqwest::Result<PublicClient> {
        self.build(self.builder().timeout(timeout))
    }

    /// [`Self::client`] for streamed bodies: no whole-request timeout, only `connect` to
    /// connect and `idle` between reads. The caller bounds the wait for the response's
    /// headers ([`PublicClient::send_within`]).
    pub fn streaming_client(&self, connect: Duration, idle: Duration) -> reqwest::Result<PublicClient> {
        self.build(self.builder().connect_timeout(connect).read_timeout(idle))
    }

    fn builder(&self) -> reqwest::ClientBuilder {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .dns_resolver(Guarded {
                policy: self.clone(),
            })
    }

    fn build(&self, builder: reqwest::ClientBuilder) -> reqwest::Result<PublicClient> {
        Ok(PublicClient {
            inner: builder.build()?,
            policy: self.clone(),
        })
    }

    /// `host`'s addresses, or why it is refused: a denied name, unresolvable, any address in a
    /// denied network, or any address not public (unless the host is allowlisted).
    async fn checked_lookup(&self, host: &str) -> Result<Vec<IpAddr>, Refused> {
        if self.denied_name(host) {
            return Err(Refused::Internal {
                host: host.to_string(),
            });
        }
        let unresolved = || Refused::Unresolved {
            host: host.to_string(),
        };
        let addrs = self.lookup.lookup(host).await.map_err(|_| unresolved())?;
        if addrs.is_empty() {
            return Err(unresolved());
        }
        if addrs.iter().any(|a| self.denied_addr(*a)) {
            return Err(Refused::Internal {
                host: host.to_string(),
            });
        }
        if !self.allowlisted(host)
            && let Some(addr) = addrs.iter().copied().find(|a| !is_public(*a))
        {
            return Err(Refused::NotPublic {
                host: host.to_string(),
                addr,
            });
        }
        Ok(addrs)
    }
}

/// A name as the deny list compares it: lower case, no trailing dot, IPv6 brackets dropped.
fn deny_name(host: &str) -> String {
    normalize_host(host).trim_end_matches('.').to_string()
}

impl Default for Outbound {
    fn default() -> Self {
        Self::new()
    }
}

/// The address a URL's host names, when it is an IP literal.
fn literal(host: &str) -> Option<IpAddr> {
    normalize_host(host).parse().ok()
}

/// Whether an allowlist entry carries a port (`runner:8080`, `[::1]:80`); a bare IPv6 literal
/// does not.
fn has_port(entry: &str) -> bool {
    let host = normalize_host(entry);
    host.parse::<IpAddr>().is_err()
        && host
            .rsplit_once(':')
            .is_some_and(|(_, port)| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()))
}

/// A host as compared with the allowlist: lower case, IPv6 brackets dropped.
fn normalize_host(host: &str) -> String {
    host.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase()
}

/// Whether `ip` is a public unicast address: not loopback, private, CGNAT, link-local,
/// documentation, benchmarking, multicast, reserved or unspecified, nor an IPv6 form that
/// embeds or reaches one (mapped, compatible, NAT64, 6to4, Teredo, ULA, site- or link-local).
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(a == 0 // "this network" (0.0.0.0/8)
        || a == 10
        || a == 127
        || (a == 100 && (64..128).contains(&b)) // CGNAT 100.64.0.0/10
        || (a == 169 && b == 254) // link-local, cloud metadata
        || (a == 172 && (16..32).contains(&b))
        || (a == 192 && b == 0 && c == 0) // IETF protocol assignments
        || (a == 192 && b == 0 && c == 2) // TEST-NET-1
        || (a == 192 && b == 168)
        || (a == 198 && (b == 18 || b == 19)) // benchmarking
        || (a == 198 && b == 51 && c == 100) // TEST-NET-2
        || (a == 203 && b == 0 && c == 113) // TEST-NET-3
        || a >= 224) // multicast, reserved, broadcast
}

/// The IPv4 address in the last 32 bits of `ip`.
fn low_v4(ip: Ipv6Addr) -> Ipv4Addr {
    let o = ip.octets();
    Ipv4Addr::new(o[12], o[13], o[14], o[15])
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    // IPv4-mapped (::ffff:a.b.c.d): the IPv4 address decides.
    if s[..5] == [0; 5] && s[5] == 0xffff {
        return is_public_v4(low_v4(ip));
    }
    // NAT64 well-known prefix (64:ff9b::/96): the embedded IPv4 address decides.
    if s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        return is_public_v4(low_v4(ip));
    }
    // 6to4 (2002::/16): the embedded IPv4 address (bits 16..48) decides.
    if s[0] == 0x2002 {
        let o = ip.octets();
        return is_public_v4(Ipv4Addr::new(o[2], o[3], o[4], o[5]));
    }
    // Only global unicast (2000::/3) is public; that leaves out ::, ::1, IPv4-compatible
    // (::/96), IPv4-translated, the local-use NAT64 prefix (64:ff9b:1::/48), discard (100::/64),
    // ULA (fc00::/7, the AWS metadata fd00:ec2::254), link- and site-local (fe80::/10,
    // fec0::/10) and multicast (ff00::/8).
    if s[0] & 0xe000 != 0x2000 {
        return false;
    }
    !(
        // Teredo (2001::/32), benchmarking (2001:2::/48), ORCHID (2001:10::/28, 2001:20::/28)
        (s[0] == 0x2001 && (s[1] == 0 || s[1] == 2 || (0x10..0x30).contains(&s[1])))
        // documentation (2001:db8::/32, 3fff::/20)
        || (s[0] == 0x2001 && s[1] == 0xdb8)
        || (s[0] == 0x3fff && s[1] < 0x1000)
    )
}

/// The client's resolver: resolves, then refuses the whole host if any address is refused.
struct Guarded {
    policy: Outbound,
}

impl reqwest::dns::Resolve for Guarded {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let policy = self.policy.clone();
        Box::pin(async move {
            let addrs = policy.checked_lookup(name.as_str()).await?;
            let addrs: reqwest::dns::Addrs =
                Box::new(addrs.into_iter().map(|ip| SocketAddr::new(ip, 0)));
            Ok(addrs)
        })
    }
}

/// A client that only reaches public addresses (see the module docs). Built by
/// [`Outbound::client`]; every request starts with [`Self::request`], which checks the URL's form
/// and an IP-literal host before anything is sent (the resolver never sees a literal).
#[derive(Clone)]
pub struct PublicClient {
    inner: reqwest::Client,
    policy: Outbound,
}

impl std::fmt::Debug for PublicClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PublicClient")
            .field("policy", &self.policy)
            .finish()
    }
}

impl PublicClient {
    /// A `POST` to `url`, or why it may not be sent.
    pub fn post(&self, url: &str) -> Result<reqwest::RequestBuilder, Refused> {
        self.request(reqwest::Method::POST, url)
    }

    /// A request of any method to `url`, or why it may not be sent.
    pub fn request(&self, method: reqwest::Method, url: &str) -> Result<reqwest::RequestBuilder, Refused> {
        let url = self.policy.check_url(url)?;
        Ok(self.inner.request(method, url))
    }

    /// Sends `request`, waiting at most `headers` for the response's headers (its body then
    /// streams, bounded only by the client's idle timeout). `None` when the wait ran out.
    pub async fn send_within(
        request: reqwest::RequestBuilder,
        headers: Duration,
    ) -> Option<reqwest::Result<reqwest::Response>> {
        tokio::time::timeout(headers, request.send()).await.ok()
    }
}

/// Why the guard refused the host, when that is why a send failed (as opposed to a network
/// error).
pub fn refused_by_guard(err: &reqwest::Error) -> Option<Refused> {
    let mut source = std::error::Error::source(err);
    while let Some(e) = source {
        if let Some(refused) = e.downcast_ref::<Refused>() {
            return Some(refused.clone());
        }
        source = e.source();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    /// Every refused range, at its edges, and public addresses next to them.
    #[test]
    fn only_public_unicast_addresses_are_public() {
        let refused = [
            // IPv4
            "0.0.0.0",
            "0.1.2.3",
            "10.0.0.0",
            "10.255.255.255",
            "100.64.0.0",
            "100.127.255.255",
            "127.0.0.1",
            "127.255.255.254",
            "169.254.0.1",
            "169.254.169.254",
            "172.16.0.1",
            "172.31.255.255",
            "192.0.0.1",
            "192.0.2.1",
            "192.168.0.1",
            "192.168.255.255",
            "198.18.0.1",
            "198.19.255.255",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "239.255.255.255",
            "240.0.0.1",
            "255.255.255.255",
            // IPv6
            "::",
            "::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::ffff:169.254.169.254",
            "::127.0.0.1",
            "::ffff:0:7f00:1",
            "64:ff9b::7f00:1",
            "64:ff9b::a00:1",
            "64:ff9b:1::1",
            "100::1",
            "2001::1",
            "2001:db8::1",
            "2001:2::1",
            "2001:10::1",
            "2002:7f00:1::1",
            "2002:a00:1::1",
            "fc00::1",
            "fd00:ec2::254",
            "fe80::1",
            "febf::1",
            "fec0::1",
            "ff02::1",
            "3fff::1",
            "4000::1",
            "::2",
        ];
        for a in refused {
            assert!(!is_public(ip(a)), "{a} must be refused");
        }
        let public = [
            "1.1.1.1",
            "8.8.8.8",
            "9.255.255.255",
            "11.0.0.0",
            "100.63.255.255",
            "100.128.0.0",
            "126.255.255.255",
            "128.0.0.0",
            "169.253.255.255",
            "169.255.0.0",
            "172.15.255.255",
            "172.32.0.0",
            "192.167.255.255",
            "192.169.0.0",
            "198.17.255.255",
            "198.20.0.0",
            "223.255.255.255",
            "93.184.216.34",
            "::ffff:93.184.216.34",
            "64:ff9b::5db8:d822",
            "2002:5db8:d822::1",
            "2606:4700:4700::1111",
            "2a00:1450:4001:80b::200e",
        ];
        for a in public {
            assert!(is_public(ip(a)), "{a} must be public");
        }
    }

    /// Every spelling of a refused address in a URL is refused before anything resolves, and
    /// the scheme, credential and form rules hold.
    #[test]
    fn urls_are_checked_in_every_spelling() {
        let http = Outbound::new().allow_http(true);
        for url in [
            "http://127.0.0.1/hook",
            "http://127.1/",
            "http://2130706433/",
            "http://0x7f000001/",
            "http://0177.0.0.1/",
            "http://0x7f.0.0.1/",
            "http://017700000001/",
            "http://10.0.0.1:8080/",
            "http://10.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://[::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://[::ffff:a9fe:a9fe]/",
            "http://[0:0:0:0:0:0:0:1]/",
            "http://[fd00:ec2::254]/",
            "http://[fe80::1]/",
            "http://100.64.0.1/",
            "http://0.0.0.0/",
            "http://0/",
            "http://192.168.1.1/",
        ] {
            assert!(
                matches!(http.check_url(url), Err(Refused::NotPublic { .. })),
                "{url}: {:?}",
                http.check_url(url)
            );
        }
        let strict = Outbound::new();
        assert_eq!(
            strict.check_url("http://example.com/"),
            Err(Refused::Scheme)
        );
        for (url, want) in [
            ("ftp://example.com/", Refused::Scheme),
            ("file:///etc/passwd", Refused::Scheme),
            ("gopher://example.com/", Refused::Scheme),
            ("javascript:alert(1)", Refused::Scheme),
            ("https://user:pw@example.com/", Refused::Credentials),
            ("https://user@example.com/", Refused::Credentials),
            ("not a url", Refused::Invalid),
            ("/relative", Refused::Invalid),
            ("", Refused::Invalid),
        ] {
            assert_eq!(http.check_url(url), Err(want), "{url}");
        }
        let long = format!("https://example.com/{}", "a".repeat(MAX_URL_LEN));
        assert_eq!(strict.check_url(&long), Err(Refused::Invalid));
        for ok in [
            "https://example.com/hook?x=1",
            "https://93.184.216.34/",
            "https://[2606:4700:4700::1111]/",
            "https://EXAMPLE.com:8443/",
        ] {
            assert!(strict.check_url(ok).is_ok(), "{ok}");
        }
        assert!(http.check_url("http://example.com/").is_ok());
        // An allowlisted literal passes; another doesn't.
        let allowed = Outbound::new()
            .allow_http(true)
            .allow_hosts(["10.0.0.5", "[::1]"]);
        assert!(allowed.check_url("http://10.0.0.5:9000/").is_ok());
        assert!(allowed.check_url("http://[::1]/").is_ok());
        assert!(allowed.check_url("http://10.0.0.6/").is_err());
    }

    /// A fake resolver: host → addresses, each call answered by the next entry (the last one
    /// repeats), counting calls.
    #[derive(Default)]
    struct Fake {
        answers: Mutex<HashMap<String, Vec<Vec<IpAddr>>>>,
        calls: AtomicUsize,
    }

    impl Fake {
        fn with(answers: &[(&str, &[&[&str]])]) -> Arc<Self> {
            let map = answers
                .iter()
                .map(|(h, seq)| {
                    (
                        h.to_string(),
                        seq.iter()
                            .map(|a| a.iter().map(|s| ip(s)).collect())
                            .collect(),
                    )
                })
                .collect();
            Arc::new(Self {
                answers: Mutex::new(map),
                calls: AtomicUsize::new(0),
            })
        }
    }

    impl HostLookup for Fake {
        fn lookup<'a>(&'a self, host: &'a str) -> Lookup<'a> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut answers = self.answers.lock().unwrap();
            let out = match answers.get_mut(host) {
                Some(seq) if seq.len() > 1 => Ok(seq.remove(0)),
                Some(seq) => Ok(seq[0].clone()),
                None => Err(std::io::Error::other("no such host")),
            };
            Box::pin(std::future::ready(out))
        }
    }

    #[tokio::test]
    async fn registration_resolves_and_refuses_any_private_answer() {
        let fake = Fake::with(&[
            ("public.test", &[&["93.184.216.34"]]),
            ("private.test", &[&["10.0.0.5"]]),
            ("mixed.test", &[&["93.184.216.34", "127.0.0.1"]]),
            ("v6.test", &[&["::1"]]),
            ("metadata.test", &[&["169.254.169.254"]]),
            ("inside.test", &[&["172.18.0.5"]]),
        ]);
        let o = Outbound::new()
            .allow_hosts(["inside.test"])
            .with_lookup(fake);
        assert!(o.check_resolved("https://public.test/x").await.is_ok());
        assert!(o.check_resolved("https://inside.test/x").await.is_ok());
        for host in ["private.test", "mixed.test", "v6.test", "metadata.test"] {
            assert!(
                matches!(
                    o.check_resolved(&format!("https://{host}/x")).await,
                    Err(Refused::NotPublic { .. })
                ),
                "{host}"
            );
        }
        assert!(matches!(
            o.check_resolved("https://nowhere.test/").await,
            Err(Refused::Unresolved { .. })
        ));
        assert!(matches!(
            o.check_resolved("https://127.0.0.1/").await,
            Err(Refused::NotPublic { .. })
        ));
    }

    /// The allowlist is exact: `runner` (any case) passes, by name or resolved; a name that only
    /// contains, ends or starts with it, its address as a literal, or a trailing-dot spelling
    /// stay public-only.
    #[tokio::test]
    async fn the_allowlist_matches_hosts_exactly() {
        let inside = &["172.18.0.5"][..];
        let fake = Fake::with(&[
            ("runner", &[inside]),
            ("RUNNER", &[inside]),
            ("runner.evil.com", &[inside]),
            ("xrunner", &[inside]),
            ("runnerx", &[inside]),
            ("evil.runner", &[inside]),
            ("runner.", &[inside]),
        ]);
        let o = Outbound::new().allow_http(true).allow_hosts(["runner"]).with_lookup(fake);
        assert!(o.check_resolved("http://runner:8080/hook").await.is_ok());
        assert!(o.check_resolved("http://RUNNER/hook").await.is_ok());
        for url in [
            "http://runner.evil.com/",
            "http://xrunner/",
            "http://runnerx/",
            "http://evil.runner/",
            "http://runner./",
            "http://172.18.0.5/",
        ] {
            assert!(
                matches!(o.check_resolved(url).await, Err(Refused::NotPublic { .. })),
                "{url}: {:?}",
                o.check_resolved(url).await
            );
        }
        // Listed, the address passes too.
        let listed = Outbound::new().allow_http(true).allow_hosts(["runner", "172.18.0.5"]);
        assert!(listed.check_url("http://172.18.0.5/").is_ok());
    }

    /// A local server counting the connections it accepts; it answers `reply` to each.
    async fn server(reply: String) -> (u16, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let _ = sock.write_all(reply.as_bytes()).await;
            }
        });
        (port, hits)
    }

    fn ok_reply() -> String {
        "HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_string()
    }

    /// At connect time: a name resolving to loopback is refused with no connection made, even
    /// when it resolved publicly a moment before (DNS rebinding); an IP literal is refused
    /// before the client sees it; an allowlisted name connects (the positive control).
    #[tokio::test]
    async fn connections_go_only_where_the_checked_address_says() {
        let (port, hits) = server(ok_reply()).await;
        let fake = Fake::with(&[
            ("evil.test", &[&["127.0.0.1"]]),
            ("rebind.test", &[&["93.184.216.34"], &["127.0.0.1"]]),
            ("hook.test", &[&["127.0.0.1"]]),
        ]);
        let o = Outbound::new()
            .allow_http(true)
            .allow_hosts(["hook.test"])
            .with_lookup(fake.clone());
        let client = o.client(Duration::from_secs(5)).unwrap();

        let err = client
            .post(&format!("http://evil.test:{port}/"))
            .unwrap()
            .send()
            .await
            .unwrap_err();
        assert!(refused_by_guard(&err).is_some(), "{err:?}");

        // Registration sees a public address; the connection's own lookup sees loopback.
        assert!(
            o.check_resolved(&format!("http://rebind.test:{port}/"))
                .await
                .is_ok()
        );
        let err = client
            .post(&format!("http://rebind.test:{port}/"))
            .unwrap()
            .send()
            .await
            .unwrap_err();
        assert!(refused_by_guard(&err).is_some(), "{err:?}");

        assert!(matches!(
            client.post(&format!("http://127.0.0.1:{port}/")),
            Err(Refused::NotPublic { .. })
        ));
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "no connection to the private address"
        );

        let resp = client
            .post(&format!("http://hook.test:{port}/"))
            .unwrap()
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    /// A redirect to a private address comes back as the response; nothing follows it.
    #[tokio::test]
    async fn a_redirect_is_never_followed() {
        let (inner_port, inner_hits) = server(ok_reply()).await;
        let (port, hits) = server(format!(
            "HTTP/1.1 302 Found\r\nlocation: http://127.0.0.1:{inner_port}/\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
        ))
        .await;
        let o = Outbound::new()
            .allow_http(true)
            .allow_hosts(["hook.test"])
            .with_lookup(Fake::with(&[("hook.test", &[&["127.0.0.1"]])]));
        let resp = o
            .client(Duration::from_secs(5))
            .unwrap()
            .post(&format!("http://hook.test:{port}/"))
            .unwrap()
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 302);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert_eq!(inner_hits.load(Ordering::SeqCst), 0);
    }

    /// A refusal the API shows names the host, never the address it resolved to (that would
    /// map the operator's internal DNS); the address is only in the detail for logs. No other
    /// refusal carries one either.
    #[tokio::test]
    async fn a_refusal_names_the_host_never_the_resolved_address() {
        let fake = Fake::with(&[
            ("db.internal", &[&["10.20.30.40"]]),
            ("mixed.test", &[&["93.184.216.34", "fd00::7"]]),
        ]);
        let o = Outbound::new().with_lookup(fake);
        let refused = o.check_resolved("https://db.internal/").await.unwrap_err();
        let msg = refused.to_string();
        assert!(!msg.contains("10.20.30.40"), "leaks: {msg}");
        assert!(msg.contains("db.internal"), "{msg}");
        assert!(
            refused.log_detail().contains("10.20.30.40"),
            "{}",
            refused.log_detail()
        );
        let refused = o.check_resolved("https://mixed.test/x").await.unwrap_err();
        assert!(!refused.to_string().contains("fd00"), "{refused}");
        assert!(!refused.to_string().contains("93.184"), "{refused}");
        // A literal names only what the user wrote (normalized), never anything resolved.
        let literal = o.check_url("https://0x7f000001/").unwrap_err();
        assert_eq!(literal.to_string(), "127.0.0.1 is not a public address");
        for raw in [
            "http://db.internal/",
            "https://u:p@db.internal/",
            "nope",
            "https://nowhere.test/",
        ] {
            let r = match o.check_url(raw) {
                Err(r) => r,
                Ok(_) => o.check_resolved(raw).await.unwrap_err(),
            };
            assert!(!r.to_string().contains("10.20"), "{raw}: {r}");
        }
    }

    /// The cap applies to the URL as it is stored (normalized), so an accepted URL always
    /// fits: percent-encoding can triple its length.
    #[test]
    fn the_length_cap_applies_after_normalizing() {
        let raw = format!("https://example.com/{}", "\u{e9}".repeat(1000));
        assert!(raw.len() <= MAX_URL_LEN);
        assert_eq!(Outbound::new().check_url(&raw), Err(Refused::Invalid));
        let fits = format!("https://example.com/{}", "a".repeat(MAX_URL_LEN - 20));
        assert_eq!(fits.len(), MAX_URL_LEN);
        assert_eq!(
            Outbound::new().check_url(&fits).unwrap().as_str().len(),
            MAX_URL_LEN
        );
    }

    /// An allowlist entry with a port can never match (hosts are compared without one): it is
    /// reported so the service can warn at start. Bare IPv6 literals are not mistaken for one.
    #[test]
    fn allowlist_entries_with_a_port_are_reported() {
        let o = Outbound::new().allow_hosts([
            "runner:8080",
            "10.0.0.5:9000",
            "[::1]:80",
            "::1",
            "[fd00::1]",
            "2001:db8::8080",
            "runner",
        ]);
        assert_eq!(
            o.allow_hosts_with_port(),
            ["runner:8080", "10.0.0.5:9000", "[::1]:80"]
        );
        assert!(
            Outbound::new()
                .allow_hosts(["runner", "::1"])
                .allow_hosts_with_port()
                .is_empty()
        );
    }

    #[test]
    fn the_settings_come_from_the_environment() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        let o = Outbound::from_lookup(WEBHOOKS_ENV, env(&[])).unwrap();
        assert!(!o.allow_http && o.allow_hosts.is_empty());
        let o = Outbound::from_lookup(WEBHOOKS_ENV, env(&[
            (ENV_ALLOW_HTTP, "true"),
            (ENV_ALLOW_HOSTS, " Runner , [::1],,10.0.0.5 "),
        ]))
        .unwrap();
        assert!(o.allow_http);
        assert_eq!(
            o.allow_hosts.iter().cloned().collect::<Vec<_>>(),
            ["10.0.0.5", "::1", "runner"]
        );
        assert!(Outbound::from_lookup(WEBHOOKS_ENV, env(&[(ENV_ALLOW_HTTP, "yes")])).is_err());
        assert!(
            !Outbound::from_lookup(WEBHOOKS_ENV, env(&[(ENV_ALLOW_HTTP, "")]))
                .unwrap()
                .allow_http
        );
    }

    /// m9: services and webhooks read their own settings; one never applies to the other.
    #[test]
    fn each_prefix_reads_its_own_settings() {
        let pairs = [
            ("CHELABASE_WEBHOOKS_ALLOW_HOSTS", "hooks.internal"),
            ("CHELABASE_WEBHOOKS_ALLOW_HTTP", "true"),
            ("CHELABASE_SERVICES_ALLOW_HOSTS", "svc.internal"),
        ];
        let env = |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string());
        let services = Outbound::from_lookup(SERVICES_ENV, env).unwrap();
        assert!(!services.allow_http);
        assert_eq!(services.allow_hosts.iter().cloned().collect::<Vec<_>>(), ["svc.internal"]);
        let webhooks = Outbound::from_lookup(WEBHOOKS_ENV, env).unwrap();
        assert!(webhooks.allow_http);
        assert_eq!(webhooks.allow_hosts.iter().cloned().collect::<Vec<_>>(), ["hooks.internal"]);
        let err = Outbound::from_lookup(SERVICES_ENV, |k| (k == "CHELABASE_SERVICES_ALLOW_HTTP").then(|| "yes".into()))
            .unwrap_err();
        assert!(err.contains("CHELABASE_SERVICES_ALLOW_HTTP"), "{err}");
    }

    #[test]
    fn networks_parse_and_contain() {
        let n: Net = "172.18.0.0/16".parse().unwrap();
        assert!(n.contains(ip("172.18.3.4")) && n.contains(ip("::ffff:172.18.0.9")));
        assert!(!n.contains(ip("172.19.0.1")) && !n.contains(ip("fd00::1")));
        let v6: Net = "fd00:1::/32".parse().unwrap();
        assert!(v6.contains(ip("fd00:1::5")) && !v6.contains(ip("fd00:2::5")));
        assert!("0.0.0.0/0".parse::<Net>().unwrap().contains(ip("8.8.8.8")));
        for bad in ["10.0.0.0", "10.0.0.0/33", "nope/8", "::/129"] {
            assert!(bad.parse::<Net>().is_err(), "{bad}");
        }
        assert_eq!(parse_nets(" 10.1.0.0/16, ,192.168.5.0/24").unwrap().len(), 2);
        assert!(parse_nets("10.1.0.0/16,x").is_err());
    }

    /// The attached networks are the routes without a gateway (the default route is not one).
    #[test]
    fn attached_networks_come_from_the_route_table() {
        let table = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
            eth0\t00000000\t010012AC\t0003\t0\t0\t0\t00000000\t0\t0\t0\n\
            eth0\t000012AC\t00000000\t0001\t0\t0\t0\t0000FFFF\t0\t0\t0\n\
            eth1\t0010A8C0\t00000000\t0001\t0\t0\t0\t00F0FFFF\t0\t0\t0\n";
        assert_eq!(
            parse_route_table(table),
            vec![
                Net::new(ip("172.18.0.0"), 16).unwrap(),
                Net::new(ip("192.168.16.0"), 20).unwrap()
            ]
        );
    }

    /// Audit S1: the IPv6 attached networks are the routes without a next hop (the default
    /// route is not one).
    #[test]
    fn attached_ipv6_networks_come_from_the_route_table() {
        let table = "\
fd00000000000000000000000000ab00 78 00000000000000000000000000000000 00 00000000000000000000000000000000 00000100 00000001 00000000 00000001     eth0
fe800000000000000000000000000000 40 00000000000000000000000000000000 00 00000000000000000000000000000000 00000100 00000001 00000000 00000001     eth0
00000000000000000000000000000000 00 00000000000000000000000000000000 00 fd0000000000000000000000000000ab 00000400 00000001 00000000 00000003     eth0
20010db8000000000000000000000000 40 00000000000000000000000000000000 00 fd0000000000000000000000000000ab 00000400 00000001 00000000 00000003     eth0
";
        assert_eq!(
            parse_ipv6_route_table(table),
            vec![
                Net::new(ip("fd00::ab00"), 120).unwrap(),
                Net::new(ip("fe80::"), 64).unwrap()
            ]
        );
        let nets = parse_ipv6_route_table(table);
        assert!(nets[0].contains(ip("fd00::abff")) && !nets[0].contains(ip("fd00::ac00")));
    }

    /// m9's hard deny: the stack's names in any case, with a trailing dot or as container
    /// names, refused even when allowlisted; names that only look alike pass.
    #[tokio::test]
    async fn the_stack_hosts_are_denied_even_when_allowlisted() {
        let o = Outbound::new()
            .allow_http(true)
            .allow_hosts(["metadata-db", "nats", "chelabase-metadata-db-1", "ok.example"])
            .deny_stack([])
            .with_lookup(Fake::with(&[("ok.example", &[&["93.184.216.34"]])]));
        for url in [
            "http://metadata-db:5432",
            "http://nats:4222",
            "http://METADATA-DB./",
            "http://metadata-db./",
            "http://chelabase-metadata-db-1/",
            "http://feat-x-orchestrator-12:8080/",
            "http://stack_worker_1/",
            "http://localhost/",
        ] {
            assert!(
                matches!(o.check_url(url), Err(Refused::Internal { .. })),
                "{url}: {:?}",
                o.check_url(url)
            );
            assert!(matches!(o.check_resolved(url).await, Err(Refused::Internal { .. })), "{url}");
        }
        for name in ["metadata-db.example.com", "mymetadata-db", "metadata-dbx", "ok.example"] {
            assert!(!o.denied_name(name), "{name}");
        }
        assert!(o.check_resolved("http://ok.example/").await.is_ok());
        // Loopback and link-local literals are denied even when allowlisted.
        let listed = Outbound::new()
            .allow_http(true)
            .allow_hosts(["127.0.0.1", "[::1]", "169.254.169.254"])
            .deny_stack([]);
        for url in ["http://127.0.0.1/", "http://[::1]/", "http://169.254.169.254/", "http://[::ffff:127.0.0.1]/"] {
            assert!(matches!(listed.check_url(url), Err(Refused::Internal { .. })), "{url}");
        }
    }

    /// An allowlisted name resolving into a denied network is refused at registration and at
    /// connect time (no connection made), and so is a literal in it.
    #[tokio::test]
    async fn an_allowlisted_name_resolving_into_the_stack_is_refused() {
        let (port, hits) = server(ok_reply()).await;
        let stack: Net = "172.18.0.0/16".parse().unwrap();
        let fake = Fake::with(&[
            ("host.docker.internal", &[&["172.18.0.3"]]),
            ("rebind.test", &[&["93.184.216.34"], &["172.18.0.3"]]),
            ("loop.test", &[&["127.0.0.1"]]),
        ]);
        let o = Outbound::new()
            .allow_http(true)
            .allow_hosts(["host.docker.internal", "rebind.test", "loop.test", "172.18.0.3"])
            .deny_stack([stack])
            .with_lookup(fake);
        assert!(matches!(
            o.check_resolved("http://host.docker.internal/").await,
            Err(Refused::Internal { .. })
        ));
        assert!(matches!(o.check_url("http://172.18.0.3/"), Err(Refused::Internal { .. })));
        assert!(o.check_resolved("http://rebind.test/").await.is_ok(), "public at save time");
        let client = o.streaming_client(Duration::from_secs(2), Duration::from_secs(2)).unwrap();
        for host in ["rebind.test", "loop.test"] {
            let err = client
                .request(reqwest::Method::GET, &format!("http://{host}:{port}/"))
                .unwrap()
                .send()
                .await
                .unwrap_err();
            assert!(matches!(refused_by_guard(&err), Some(Refused::Internal { .. })), "{host}: {err:?}");
        }
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    /// `request` sends any method; the streaming client outlives a body slower than its idle
    /// timeout in total, and `send_within` bounds only the wait for headers.
    #[tokio::test]
    async fn the_streaming_client_has_no_total_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let methods = Arc::new(Mutex::new(Vec::new()));
        let seen = methods.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let seen = seen.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let head = String::from_utf8_lossy(&buf[..n]).to_string();
                    let method = head.split(' ').next().unwrap_or_default().to_string();
                    seen.lock().unwrap().push(method.clone());
                    if method == "PUT" {
                        // Never answers.
                        tokio::time::sleep(Duration::from_secs(10)).await;
                        return;
                    }
                    let _ = sock
                        .write_all(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n")
                        .await;
                    for _ in 0..4 {
                        tokio::time::sleep(Duration::from_millis(700)).await;
                        let _ = sock.write_all(b"1\r\nx\r\n").await;
                    }
                    let _ = sock.write_all(b"0\r\n\r\n").await;
                });
            }
        });
        let o = Outbound::new()
            .allow_http(true)
            .allow_hosts(["svc.test"])
            .with_lookup(Fake::with(&[("svc.test", &[&["127.0.0.1"]])]));
        let client = o.streaming_client(Duration::from_secs(1), Duration::from_secs(1)).unwrap();
        let url = format!("http://svc.test:{port}/");
        let req = client.request(reqwest::Method::DELETE, &url).unwrap();
        let resp = PublicClient::send_within(req, Duration::from_secs(1)).await.unwrap().unwrap();
        assert_eq!(resp.text().await.unwrap(), "xxxx", "2.8 s of body, past every timeout");
        let req = client.request(reqwest::Method::PUT, &url).unwrap();
        assert!(PublicClient::send_within(req, Duration::from_millis(300)).await.is_none());
        assert_eq!(*methods.lock().unwrap(), ["DELETE", "PUT"]);
        assert!(matches!(
            client.request(reqwest::Method::GET, "http://10.0.0.1/"),
            Err(Refused::NotPublic { .. })
        ));
    }
}
