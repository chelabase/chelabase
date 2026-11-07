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

/// Who may be called: the scheme rule, the operator's allowlist and the resolver.
#[derive(Clone)]
pub struct Outbound {
    allow_http: bool,
    allow_hosts: Arc<BTreeSet<String>>,
    /// Allowlist entries with a port (kept only to warn about).
    allow_hosts_ported: Arc<Vec<String>>,
    lookup: Arc<dyn HostLookup>,
}

impl std::fmt::Debug for Outbound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Outbound")
            .field("allow_http", &self.allow_http)
            .field("allow_hosts", &self.allow_hosts)
            .finish()
    }
}

impl Outbound {
    /// `https` only, no allowlist, the system resolver.
    pub fn new() -> Self {
        Self {
            allow_http: false,
            allow_hosts: Arc::new(BTreeSet::new()),
            allow_hosts_ported: Arc::new(Vec::new()),
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

    /// Resolve with `lookup` instead of the system resolver.
    pub fn with_lookup(mut self, lookup: Arc<dyn HostLookup>) -> Self {
        self.lookup = lookup;
        self
    }

    /// From [`ENV_ALLOW_HTTP`] (`true` or `false`, blank is `false`) and [`ENV_ALLOW_HOSTS`].
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let allow_http = match lookup(ENV_ALLOW_HTTP).map(|v| v.trim().to_ascii_lowercase()) {
            None => false,
            Some(v) if v.is_empty() || v == "false" => false,
            Some(v) if v == "true" => true,
            Some(v) => return Err(format!("{ENV_ALLOW_HTTP}={v:?}: expected true or false")),
        };
        let hosts = lookup(ENV_ALLOW_HOSTS).unwrap_or_default();
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

    /// The URL, parsed, if its form may be called: a valid absolute URL of an accepted scheme,
    /// without credentials, whose host, when an IP literal, is public (or allowlisted). A host
    /// name is checked when it is resolved ([`Self::check_resolved`], and at connect time).
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
        if self.allowlisted(host) {
            return Ok(url);
        }
        // The parser has already normalized every IPv4 spelling (decimal, octal, hex, short
        // forms) to dotted decimal, and an IPv6 host comes bracketed: either parses here.
        if let Some(addr) = literal(host)
            && !is_public(addr)
        {
            return Err(Refused::NotPublic {
                host: host.to_string(),
                addr,
            });
        }
        Ok(url)
    }

    /// The URL's form ([`Self::check_url`]), then every address its host resolves to now: the
    /// check a registration makes. Delivery checks again at connect time.
    pub async fn check_resolved(&self, raw: &str) -> Result<Url, Refused> {
        let url = self.check_url(raw)?;
        let host = url.host_str().unwrap_or_default();
        if literal(host).is_none() {
            checked_lookup(&self.allow_hosts, self.lookup.as_ref(), host).await?;
        }
        Ok(url)
    }

    /// A client for this policy: no redirects, no proxy, `timeout` per request, and a resolver
    /// that refuses any host resolving to a non-public address (unless allowlisted).
    pub fn client(&self, timeout: Duration) -> reqwest::Result<PublicClient> {
        let resolver = Guarded {
            allow_hosts: self.allow_hosts.clone(),
            lookup: self.lookup.clone(),
        };
        let inner = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(timeout)
            .dns_resolver(resolver)
            .build()?;
        Ok(PublicClient {
            inner,
            policy: self.clone(),
        })
    }
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

/// `host`'s addresses, or why it is refused: unresolvable, or any address not public (unless
/// the host is allowlisted).
async fn checked_lookup(
    allow_hosts: &BTreeSet<String>,
    lookup: &dyn HostLookup,
    host: &str,
) -> Result<Vec<IpAddr>, Refused> {
    let unresolved = || Refused::Unresolved {
        host: host.to_string(),
    };
    let addrs = lookup.lookup(host).await.map_err(|_| unresolved())?;
    if addrs.is_empty() {
        return Err(unresolved());
    }
    if !allow_hosts.contains(&normalize_host(host))
        && let Some(addr) = addrs.iter().copied().find(|a| !is_public(*a))
    {
        return Err(Refused::NotPublic {
            host: host.to_string(),
            addr,
        });
    }
    Ok(addrs)
}

/// The client's resolver: resolves, then refuses the whole host if any address isn't public.
struct Guarded {
    allow_hosts: Arc<BTreeSet<String>>,
    lookup: Arc<dyn HostLookup>,
}

impl reqwest::dns::Resolve for Guarded {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let (allow_hosts, lookup) = (self.allow_hosts.clone(), self.lookup.clone());
        Box::pin(async move {
            let addrs = checked_lookup(&allow_hosts, lookup.as_ref(), name.as_str()).await?;
            let addrs: reqwest::dns::Addrs =
                Box::new(addrs.into_iter().map(|ip| SocketAddr::new(ip, 0)));
            Ok(addrs)
        })
    }
}

/// A client that only reaches public addresses (see the module docs). Built by
/// [`Outbound::client`]; every request starts with [`Self::post`], which checks the URL's form
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
        let url = self.policy.check_url(url)?;
        Ok(self.inner.post(url))
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
        let o = Outbound::from_lookup(env(&[])).unwrap();
        assert!(!o.allow_http && o.allow_hosts.is_empty());
        let o = Outbound::from_lookup(env(&[
            (ENV_ALLOW_HTTP, "true"),
            (ENV_ALLOW_HOSTS, " Runner , [::1],,10.0.0.5 "),
        ]))
        .unwrap();
        assert!(o.allow_http);
        assert_eq!(
            o.allow_hosts.iter().cloned().collect::<Vec<_>>(),
            ["10.0.0.5", "::1", "runner"]
        );
        assert!(Outbound::from_lookup(env(&[(ENV_ALLOW_HTTP, "yes")])).is_err());
        assert!(
            !Outbound::from_lookup(env(&[(ENV_ALLOW_HTTP, "")]))
                .unwrap()
                .allow_http
        );
    }
}
