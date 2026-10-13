//! The public entry: host settings and every URL derived from them.

use crate::mode::Mode;
use std::net::IpAddr;

/// The loopback upstreams the gateway reaches by default (`CHELABASE_*_UPSTREAM` override them).
/// This is the one place they live.
pub const DEFAULT_STUDIO_UPSTREAM: &str = "http://127.0.0.1:3000";
pub const DEFAULT_ZITADEL_UPSTREAM: &str = "http://127.0.0.1:8080";
pub const DEFAULT_DOCS_UPSTREAM: &str = "http://127.0.0.1:3001";
pub const DEFAULT_LANDING_UPSTREAM: &str = "http://127.0.0.1:3002";
pub const DEFAULT_MAIL_UPSTREAM: &str = "http://127.0.0.1:8025";

/// Zitadel's upstream: `CHELABASE_ZITADEL_UPSTREAM` (blank = unset), else
/// [`DEFAULT_ZITADEL_UPSTREAM`]. Reached directly (with the auth host as `Host`) by anything that
/// runs before the gateway, such as the CLI bootstrap.
pub fn zitadel_upstream(get: impl Fn(&str) -> Option<String>) -> String {
    value(&get, "CHELABASE_ZITADEL_UPSTREAM").unwrap_or_else(|| DEFAULT_ZITADEL_UPSTREAM.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    Http,
    Https,
}

impl Scheme {
    fn as_str(self) -> &'static str {
        match self {
            Scheme::Http => "http",
            Scheme::Https => "https",
        }
    }

    fn default_port(self) -> u16 {
        match self {
            Scheme::Http => 80,
            Scheme::Https => 443,
        }
    }
}

/// The public entry: host settings and every URL derived from them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    scheme: Scheme,
    studio: String,
    api: String,
    auth: String,
    docs: Option<String>,
    landing: Option<String>,
    mail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryError(pub String);

impl std::fmt::Display for EntryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for EntryError {}

fn fail<T>(msg: impl Into<String>) -> Result<T, EntryError> {
    Err(EntryError(msg.into()))
}

const REMOVED: &[(&str, &str)] = &[
    ("CHELABASE_API_HOSTS", "CHELABASE_API_HOST"),
    ("CHELABASE_STUDIO_ORIGINS", "CHELABASE_STUDIO_HOST"),
    ("CHELABASE_STUDIO_URL", "CHELABASE_STUDIO_HOST"),
];

/// A set, non-blank value (trimmed).
fn value(get: &impl Fn(&str) -> Option<String>, name: &str) -> Option<String> {
    get(name)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn scheme_from(get: &impl Fn(&str) -> Option<String>) -> Result<Scheme, EntryError> {
    let raw = value(get, "CHELABASE_PUBLIC_SCHEME");
    match raw.as_deref().map(str::to_ascii_lowercase).as_deref() {
        None | Some("https") => Ok(Scheme::Https),
        Some("http") => Ok(Scheme::Http),
        Some(_) => fail(format!(
            "CHELABASE_PUBLIC_SCHEME {:?} must be \"http\" or \"https\"",
            raw.unwrap_or_default()
        )),
    }
}

/// Validate one host setting and return it normalized (lowercase, no trailing dot, port kept).
/// The scheme's default port is refused: browsers drop it from `Origin` and `Host`, so a
/// setting carrying it could never match a request.
fn host_setting(scheme: Scheme, name: &str, raw: &str) -> Result<String, EntryError> {
    let raw = raw.trim();
    validate_host(raw).map_err(|e| EntryError(format!("{name}: {e}")))?;
    let mut host = normalize_host(raw);
    if let Some(port) = host_port(raw) {
        if port == scheme.default_port() {
            return fail(format!(
                "{name}: {raw:?} carries port {port}, the default for {}; browsers drop it \
                 from Origin and Host, so drop \":{port}\" from the setting",
                scheme.as_str()
            ));
        }
        host.push(':');
        host.push_str(&port.to_string());
    }
    Ok(host)
}

fn required_host(
    get: &impl Fn(&str) -> Option<String>,
    scheme: Scheme,
    name: &str,
) -> Result<String, EntryError> {
    match value(get, name) {
        Some(raw) => host_setting(scheme, name, &raw),
        None => fail(format!(
            "{name} is required (a host, optionally with :port)"
        )),
    }
}

fn optional_host(
    get: &impl Fn(&str) -> Option<String>,
    scheme: Scheme,
    name: &str,
) -> Result<Option<String>, EntryError> {
    value(get, name)
        .map(|raw| host_setting(scheme, name, &raw))
        .transpose()
}

fn issuer_of(scheme: Scheme, auth_host: &str) -> String {
    format!("{}://{auth_host}", scheme.as_str())
}

/// P3 for a lookup: the explicit issuer, when set, must equal the derived one.
fn check_issuer(
    get: &impl Fn(&str) -> Option<String>,
    scheme: Scheme,
    auth_host: Option<&str>,
) -> Result<(), EntryError> {
    let explicit = value(get, "CHELABASE_ZITADEL_ISSUER");
    match (auth_host, explicit) {
        (Some(host), Some(explicit)) => {
            let derived = issuer_of(scheme, host);
            if explicit
                .strip_suffix('/')
                .unwrap_or(&explicit)
                .eq_ignore_ascii_case(&derived)
            {
                Ok(())
            } else {
                fail(format!(
                    "CHELABASE_ZITADEL_ISSUER {explicit:?} differs from the issuer {derived:?} \
                     derived from CHELABASE_AUTH_HOST and CHELABASE_PUBLIC_SCHEME; \
                     remove CHELABASE_ZITADEL_ISSUER"
                ))
            }
        }
        (None, Some(_)) => fail(
            "CHELABASE_ZITADEL_ISSUER is derived from CHELABASE_AUTH_HOST; \
             set CHELABASE_AUTH_HOST (and remove CHELABASE_ZITADEL_ISSUER)",
        ),
        _ => Ok(()),
    }
}

fn ip_of(host: &str) -> Option<IpAddr> {
    let name = normalize_host(host);
    let name = name
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(&name);
    name.parse().ok()
}

/// P2: Studio and the API must be one site.
fn check_same_site(studio: &str, api: &str) -> Result<(), EntryError> {
    let both = "CHELABASE_STUDIO_HOST and CHELABASE_API_HOST";
    let (s, a) = (normalize_host(studio), normalize_host(api));
    match (ip_of(studio), ip_of(api)) {
        (None, None) => {
            let two_labels =
                |p: &str| p.split('.').count() >= 2 && p.split('.').all(|l| !l.is_empty());
            let parent = |h: &str| h.split_once('.').map(|(_, p)| p.to_string());
            match (parent(&s), parent(&a)) {
                (Some(ps), Some(pa)) if ps == pa && two_labels(&ps) => Ok(()),
                _ => fail(format!(
                    "{both} must be one site (sign-in cookies are SameSite=Strict): they must \
                     share a parent domain of at least two labels, such as studio.example.com \
                     and api.example.com; got {s:?} and {a:?}"
                )),
            }
        }
        (Some(si), Some(ai)) if si == ai => Ok(()),
        _ => fail(format!(
            "{both} must be one site (sign-in cookies are SameSite=Strict): if either is an IP \
             address, both must be the same address; got {s:?} and {a:?}"
        )),
    }
}

impl Entry {
    /// Reads CHELABASE_PUBLIC_SCHEME (default https), CHELABASE_{STUDIO,API,AUTH}_HOST (required),
    /// CHELABASE_{DOCS,LANDING,MAIL}_HOST (optional; blank = unset). Refuses: a missing or invalid
    /// host, a host carrying the scheme's default port, a bad scheme, two identical hosts (P1), Studio/API not one site (P2), the removed
    /// CHELABASE_API_HOSTS / CHELABASE_STUDIO_ORIGINS / CHELABASE_STUDIO_URL when non-blank (the
    /// message names the replacement), and CHELABASE_ZITADEL_ISSUER that differs from the derived
    /// issuer (P3). Messages name the variable.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, EntryError> {
        for (old, new) in REMOVED {
            if value(&get, old).is_some() {
                return fail(format!("{old} is no longer supported; use {new}"));
            }
        }
        let scheme = scheme_from(&get)?;
        let studio = required_host(&get, scheme, "CHELABASE_STUDIO_HOST")?;
        let api = required_host(&get, scheme, "CHELABASE_API_HOST")?;
        let auth = required_host(&get, scheme, "CHELABASE_AUTH_HOST")?;
        let docs = optional_host(&get, scheme, "CHELABASE_DOCS_HOST")?;
        let landing = optional_host(&get, scheme, "CHELABASE_LANDING_HOST")?;
        let mail = optional_host(&get, scheme, "CHELABASE_MAIL_HOST")?;

        let all: [(&str, Option<&String>); 6] = [
            ("CHELABASE_STUDIO_HOST", Some(&studio)),
            ("CHELABASE_API_HOST", Some(&api)),
            ("CHELABASE_AUTH_HOST", Some(&auth)),
            ("CHELABASE_DOCS_HOST", docs.as_ref()),
            ("CHELABASE_LANDING_HOST", landing.as_ref()),
            ("CHELABASE_MAIL_HOST", mail.as_ref()),
        ];
        for (i, (name_a, host_a)) in all.iter().enumerate() {
            for (name_b, host_b) in &all[i + 1..] {
                match (host_a, host_b) {
                    (Some(a), Some(b)) if a == b => {
                        return fail(format!(
                            "{name_a} and {name_b} are both {a:?}; each host must be different"
                        ));
                    }
                    _ => {}
                }
            }
        }

        check_same_site(&studio, &api)?;
        check_issuer(&get, scheme, Some(&auth))?;
        Ok(Self {
            scheme,
            studio,
            api,
            auth,
            docs,
            landing,
            mail,
        })
    }

    pub fn from_env() -> Result<Self, EntryError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    pub fn scheme(&self) -> Scheme {
        self.scheme
    }

    /// Platform mode refuses a plain-http entry (sign-in cookies and tokens would cross the
    /// network in clear); self-hosted mode accepts either scheme.
    pub fn check_mode(&self, mode: Mode) -> Result<(), EntryError> {
        match (mode, self.scheme) {
            (Mode::Platform, Scheme::Http) => fail(
                "platform mode requires an https entry: set CHELABASE_PUBLIC_SCHEME to https \
                 (or leave it unset)",
            ),
            _ => Ok(()),
        }
    }

    /// Normalized (lowercase, no trailing dot), port kept as configured.
    pub fn studio_host(&self) -> &str {
        &self.studio
    }

    pub fn api_host(&self) -> &str {
        &self.api
    }

    pub fn auth_host(&self) -> &str {
        &self.auth
    }

    pub fn docs_host(&self) -> Option<&str> {
        self.docs.as_deref()
    }

    pub fn landing_host(&self) -> Option<&str> {
        self.landing.as_deref()
    }

    pub fn mail_host(&self) -> Option<&str> {
        self.mail.as_deref()
    }

    /// `<scheme>://<studio host>` (no trailing slash); likewise the API base and the issuer.
    pub fn studio_origin(&self) -> String {
        format!("{}://{}", self.scheme.as_str(), self.studio)
    }

    pub fn api_base(&self) -> String {
        format!("{}://{}", self.scheme.as_str(), self.api)
    }

    pub fn issuer(&self) -> String {
        issuer_of(self.scheme, &self.auth)
    }
}

/// P3 for services that need only the issuer: None when neither CHELABASE_AUTH_HOST nor
/// CHELABASE_ZITADEL_ISSUER is set; the derived issuer when the auth host is set (a different
/// explicit issuer refused); an error naming CHELABASE_AUTH_HOST when only the issuer is set.
pub fn issuer_from_lookup(
    get: impl Fn(&str) -> Option<String>,
) -> Result<Option<String>, EntryError> {
    let scheme = scheme_from(&get)?;
    let auth = optional_host(&get, scheme, "CHELABASE_AUTH_HOST")?;
    check_issuer(&get, scheme, auth.as_deref())?;
    Ok(auth.map(|host| issuer_of(scheme, &host)))
}

/// Validation of one host setting (moved from the gateway's registry): a hostname or IPv4
/// address, or a bracketed IPv6 address, optionally with `:port`.
pub fn validate_host(entry: &str) -> Result<(), String> {
    let port_ok = |port: &str| {
        !port.is_empty()
            && port.bytes().all(|b| b.is_ascii_digit())
            && !port.starts_with('0')
            && port.parse::<u16>().is_ok()
    };
    if let Some(rest) = entry.strip_prefix('[') {
        let (addr, after) = rest
            .split_once(']')
            .ok_or_else(|| format!("host {entry:?}: missing closing ]"))?;
        let after_ok = after.is_empty() || after.strip_prefix(':').is_some_and(port_ok);
        return if addr.parse::<std::net::Ipv6Addr>().is_ok() && after_ok {
            Ok(())
        } else {
            Err(format!("host {entry:?} is not a valid [IPv6]:port"))
        };
    }
    if entry.matches(':').count() > 1 {
        return Err(format!(
            "host {entry:?} looks like an IPv6 address; bracket it, e.g. [{entry}]"
        ));
    }
    let host = match entry.split_once(':') {
        Some((host, port)) if port_ok(port) => host,
        Some(_) => return Err(format!("host {entry:?} has an invalid port")),
        None => entry,
    };
    if host.parse::<std::net::Ipv4Addr>().is_ok() {
        return Ok(());
    }
    let name = host.strip_suffix('.').unwrap_or(host);
    let label_ok = |l: &str| {
        !l.is_empty()
            && l.len() <= 63
            && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !l.starts_with('-')
            && !l.ends_with('-')
    };
    let last = name.rsplit('.').next().unwrap_or("");
    if name.split('.').all(label_ok) && !last.bytes().all(|b| b.is_ascii_digit()) {
        Ok(())
    } else {
        Err(format!("host {entry:?} is not a valid hostname"))
    }
}

/// Lowercase, drop any port and a single trailing dot (`[::1]:8000` → `[::1]`).
pub fn normalize_host(raw: &str) -> String {
    let raw = raw.trim();
    let without_port = if let Some(rest) = raw.strip_prefix('[') {
        match rest.find(']') {
            Some(end) => &raw[..end + 2],
            None => raw,
        }
    } else {
        raw.rsplit_once(':').map_or(raw, |(host, _)| host)
    };
    let without_dot = without_port.strip_suffix('.').unwrap_or(without_port);
    without_dot.to_ascii_lowercase()
}

/// The port of a host (`a.b:8000` → Some(8000), `[::1]` → None).
pub fn host_port(raw: &str) -> Option<u16> {
    let raw = raw.trim();
    let tail = if let Some(rest) = raw.strip_prefix('[') {
        rest.split_once(']')?.1.strip_prefix(':')?
    } else {
        raw.rsplit_once(':')?.1
    };
    tail.parse().ok()
}

#[cfg(test)]
mod tests;
