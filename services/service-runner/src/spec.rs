//! A container service's deployment spec (`function_deployments.spec`, `backend = "container"`)
//! and its environment, checked before anything reaches Docker. Unknown keys are refused, so a
//! spec can't ask for `privileged`, a mount, a network or anything else the runner doesn't set.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

/// Memory a container gets when its spec names none (MiB), unless the cap is lower.
pub const DEFAULT_MEMORY_MB: u32 = 512;
/// CPUs a container gets when its spec names none, unless the cap is lower.
pub const DEFAULT_CPUS: f64 = 1.0;
/// The least memory a spec may ask for (MiB).
pub const MIN_MEMORY_MB: u32 = 16;
/// The least CPU a spec may ask for (Docker's NanoCPUs floor).
pub const MIN_CPUS: f64 = 0.01;
/// Environment entries per container, and the bytes of one value and of all of them.
pub const MAX_ENV: usize = 128;
pub const MAX_ENV_VALUE: usize = 32 * 1024;
pub const MAX_ENV_TOTAL: usize = 256 * 1024;
/// The variable the runner sets to the spec's port; a caller can't set it.
pub const PORT_ENV: &str = "PORT";

/// The spec as stored. `deny_unknown_fields` at every level: anything else is refused.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerSpec {
    /// Always `"container"` here (a URL service never reaches the runner).
    pub backend: String,
    /// `name@sha256:<64 hex>` or `name:tag`.
    pub image: String,
    pub port: u16,
    #[serde(default = "default_health_path")]
    pub health_path: String,
    #[serde(default)]
    pub resources: Resources,
}

fn default_health_path() -> String {
    "/".to_string()
}

/// What a spec may lower (never raise past the runner's caps).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    pub memory_mb: Option<u32>,
    pub cpus: Option<f64>,
}

/// The runner's ceilings (`CHELABASE_SERVICE_RUNNER_MEMORY_CAP_MB`, `…_CPU_CAP`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Caps {
    pub memory_mb: u32,
    pub cpus: f64,
}

impl Default for Caps {
    fn default() -> Self {
        Caps {
            memory_mb: DEFAULT_MEMORY_MB,
            cpus: DEFAULT_CPUS,
        }
    }
}

/// Docker's units: bytes and billionths of a CPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub memory_bytes: i64,
    pub nano_cpus: i64,
}

impl Caps {
    pub fn limits(&self) -> Limits {
        to_limits(self.memory_mb, self.cpus)
    }
}

fn to_limits(memory_mb: u32, cpus: f64) -> Limits {
    Limits {
        memory_bytes: i64::from(memory_mb) * 1024 * 1024,
        nano_cpus: (cpus * 1e9).round() as i64,
    }
}

/// An image reference the runner accepts: a repository name and exactly one of a tag or a
/// sha256 digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    pub name: String,
    pub reference: Reference,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reference {
    Tag(String),
    /// `sha256:<64 hex>`.
    Digest(String),
}

impl fmt::Display for ImageRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.reference {
            Reference::Tag(t) => write!(f, "{}:{t}", self.name),
            Reference::Digest(d) => write!(f, "{}@{d}", self.name),
        }
    }
}

impl ImageRef {
    /// The pull's `tag` parameter: the tag or the digest (never empty, which would pull every
    /// tag).
    pub fn tag_or_digest(&self) -> &str {
        match &self.reference {
            Reference::Tag(t) | Reference::Digest(t) => t,
        }
    }
}

/// A spec that passed every check.
#[derive(Debug, Clone, PartialEq)]
pub struct Validated {
    pub image: ImageRef,
    pub port: u16,
    pub health_path: String,
    pub limits: Limits,
}

/// Why a spec or an environment was refused (the API's 422).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct SpecError(pub String);

fn bad(msg: impl Into<String>) -> SpecError {
    SpecError(msg.into())
}

/// Parses and checks a stored spec against `caps`.
pub fn parse(value: &serde_json::Value, caps: &Caps) -> Result<Validated, SpecError> {
    let spec: ContainerSpec =
        serde_json::from_value(value.clone()).map_err(|e| bad(format!("invalid spec: {e}")))?;
    validate(&spec, caps)
}

/// Checks a spec against `caps`: the backend, the image reference, the port, the health path,
/// and limits that are at least the floors and never above the caps.
pub fn validate(spec: &ContainerSpec, caps: &Caps) -> Result<Validated, SpecError> {
    if spec.backend != "container" {
        return Err(bad("backend must be \"container\""));
    }
    let image = parse_image(&spec.image)?;
    if spec.port == 0 {
        return Err(bad("port must be 1-65535"));
    }
    if !valid_health_path(&spec.health_path) {
        return Err(bad(
            "health_path is an absolute path of at most 256 characters from A-Z a-z 0-9 / . _ ~ -",
        ));
    }
    let memory_mb = match spec.resources.memory_mb {
        None => DEFAULT_MEMORY_MB.min(caps.memory_mb),
        Some(m) if m < MIN_MEMORY_MB => {
            return Err(bad(format!(
                "resources.memory_mb must be at least {MIN_MEMORY_MB}"
            )));
        }
        Some(m) if m > caps.memory_mb => {
            return Err(bad(format!(
                "resources.memory_mb {m} is above the cap of {}",
                caps.memory_mb
            )));
        }
        Some(m) => m,
    };
    let cpus = match spec.resources.cpus {
        None => DEFAULT_CPUS.min(caps.cpus),
        Some(c) if !c.is_finite() || c < MIN_CPUS => {
            return Err(bad(format!("resources.cpus must be at least {MIN_CPUS}")));
        }
        Some(c) if c > caps.cpus => {
            return Err(bad(format!(
                "resources.cpus {c} is above the cap of {}",
                caps.cpus
            )));
        }
        Some(c) => c,
    };
    let limits = to_limits(memory_mb, cpus);
    let cap = caps.limits();
    // Rounding can't carry a value past its cap.
    let limits = Limits {
        memory_bytes: limits.memory_bytes.min(cap.memory_bytes),
        nano_cpus: limits.nano_cpus.min(cap.nano_cpus),
    };
    Ok(Validated {
        image,
        port: spec.port,
        health_path: spec.health_path.clone(),
        limits,
    })
}

fn valid_health_path(p: &str) -> bool {
    p.starts_with('/')
        && p.len() <= 256
        && p.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._~-".contains(&b))
}

/// `[registry[:port]/]path[/path…]` then `:tag` or `@sha256:<64 hex>` (exactly one). Path
/// components are Docker's (`[a-z0-9]+([._-][a-z0-9]+)*`), so nothing starts with `-` and
/// nothing can carry a URL or query; at most 255 characters in all.
pub fn parse_image(raw: &str) -> Result<ImageRef, SpecError> {
    let refuse = || {
        bad(format!(
            "image {raw:?} must be name@sha256:<64 hex> or name:tag"
        ))
    };
    if raw.is_empty() || raw.len() > 255 {
        return Err(refuse());
    }
    let (name, reference) = if let Some((name, digest)) = raw.split_once('@') {
        let hex = digest.strip_prefix("sha256:").ok_or_else(refuse)?;
        if hex.len() != 64
            || !hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(refuse());
        }
        (name, Reference::Digest(digest.to_string()))
    } else {
        // The tag follows the last `:` after the last `/` (a registry port comes before a `/`).
        let slash = raw.rfind('/').map_or(0, |i| i + 1);
        let colon = raw[slash..].rfind(':').ok_or_else(refuse)? + slash;
        let tag = &raw[colon + 1..];
        let valid_tag = !tag.is_empty()
            && tag.len() <= 128
            && tag.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
            && tag
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b));
        if !valid_tag {
            return Err(refuse());
        }
        (&raw[..colon], Reference::Tag(tag.to_string()))
    };
    if !valid_name(name) {
        return Err(refuse());
    }
    Ok(ImageRef {
        name: name.to_string(),
        reference,
    })
}

fn valid_name(name: &str) -> bool {
    let mut parts: Vec<&str> = name.split('/').collect();
    if parts.iter().any(|p| p.is_empty()) {
        return false;
    }
    // A first part with a `.` or `:`, or `localhost` (any case), is a registry host.
    if parts.len() > 1
        && (parts[0].contains(['.', ':']) || parts[0].eq_ignore_ascii_case("localhost"))
    {
        let host = parts.remove(0);
        let (h, port) = match host.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (host, None),
        };
        let host_ok = !h.is_empty()
            && h.starts_with(|c: char| c.is_ascii_alphanumeric())
            && h.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-.".contains(&b));
        let port_ok = port
            .is_none_or(|p| !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit()));
        // The engine pulls from the host's network namespace: no loopback or `localhost` name,
        // and no IP literal (a numeric last label is never a real top-level domain, so `127.1`
        // and `2130706433` count too).
        let lower = h.to_ascii_lowercase();
        let local = lower == "localhost"
            || lower.ends_with(".localhost")
            || h.parse::<std::net::IpAddr>().is_ok()
            || h.rsplit('.')
                .next()
                .is_some_and(|l| l.bytes().all(|b| b.is_ascii_digit()));
        if !host_ok || !port_ok || local {
            return false;
        }
    }
    !parts.is_empty() && parts.iter().all(|p| valid_component(p))
}

fn valid_component(p: &str) -> bool {
    let b = p.as_bytes();
    if b.is_empty() || !is_alnum(b[0]) || !is_alnum(b[b.len() - 1]) {
        return false;
    }
    let mut prev_sep = false;
    for &c in b {
        if is_alnum(c) {
            prev_sep = false;
        } else if b"._-".contains(&c) {
            // `__` is allowed; any other run of separators isn't.
            if prev_sep && c != b'_' {
                return false;
            }
            prev_sep = true;
        } else {
            return false;
        }
    }
    true
}

fn is_alnum(c: u8) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit()
}

/// The container's environment: the caller's entries (names `[A-Za-z_][A-Za-z0-9_]*`, no NUL,
/// bounded) and `PORT`, which only the runner sets.
pub fn environment(env: &BTreeMap<String, String>, port: u16) -> Result<Vec<String>, SpecError> {
    if env.len() > MAX_ENV {
        return Err(bad(format!("at most {MAX_ENV} environment entries")));
    }
    let mut total = 0;
    let mut out = Vec::with_capacity(env.len() + 1);
    for (name, value) in env {
        let valid_name = !name.is_empty()
            && name.len() <= 128
            && name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
        if !valid_name {
            return Err(bad(format!(
                "environment name {name:?} is not [A-Za-z_][A-Za-z0-9_]*"
            )));
        }
        if name == PORT_ENV {
            return Err(bad("PORT is set by the runner"));
        }
        if value.len() > MAX_ENV_VALUE || value.contains('\0') {
            return Err(bad(format!(
                "environment value of {name} must be at most {MAX_ENV_VALUE} bytes, with no NUL"
            )));
        }
        total += name.len() + value.len();
        if total > MAX_ENV_TOTAL {
            return Err(bad(format!(
                "environment must be at most {MAX_ENV_TOTAL} bytes"
            )));
        }
        out.push(format!("{name}={value}"));
    }
    out.push(format!("{PORT_ENV}={port}"));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn caps() -> Caps {
        Caps::default()
    }

    fn base() -> serde_json::Value {
        json!({"backend": "container", "image": "ghcr.io/acme/echo:1.2", "port": 8080, "health_path": "/health"})
    }

    #[test]
    fn a_plain_spec_gets_the_default_limits() {
        let v = parse(&base(), &caps()).unwrap();
        assert_eq!(v.port, 8080);
        assert_eq!(v.health_path, "/health");
        assert_eq!(v.image.to_string(), "ghcr.io/acme/echo:1.2");
        assert_eq!(
            v.limits,
            Limits {
                memory_bytes: 512 * 1024 * 1024,
                nano_cpus: 1_000_000_000
            }
        );
    }

    #[test]
    fn a_spec_may_lower_the_limits_but_never_raise_them() {
        let mut s = base();
        s["resources"] = json!({"memory_mb": 128, "cpus": 0.5});
        let v = parse(&s, &caps()).unwrap();
        assert_eq!(v.limits.memory_bytes, 128 * 1024 * 1024);
        assert_eq!(v.limits.nano_cpus, 500_000_000);
        for over in [
            json!({"memory_mb": 513}),
            json!({"cpus": 1.5}),
            json!({"memory_mb": 8}),
            json!({"cpus": 0.0}),
        ] {
            let mut s = base();
            s["resources"] = over.clone();
            assert!(parse(&s, &caps()).is_err(), "{over}");
        }
        // A lower cap lowers the default too.
        let low = Caps {
            memory_mb: 256,
            cpus: 0.25,
        };
        let v = parse(&base(), &low).unwrap();
        assert_eq!(v.limits, low.limits());
    }

    /// The acceptance list: privileged, a bind mount, host network, and anything else not in
    /// the spec's shape is refused, at the top level and inside `resources`.
    #[test]
    fn unknown_keys_are_refused() {
        for (key, value) in [
            ("privileged", json!(true)),
            ("binds", json!(["/:/host"])),
            (
                "mounts",
                json!([{"type": "bind", "source": "/", "target": "/host"}]),
            ),
            ("network_mode", json!("host")),
            ("pid_mode", json!("host")),
            ("devices", json!(["/dev/sda"])),
            ("cap_add", json!(["SYS_ADMIN"])),
            ("user", json!("0")),
            ("env", json!({"A": "b"})),
            ("command", json!(["sh"])),
        ] {
            let mut s = base();
            s[key] = value;
            assert!(parse(&s, &caps()).is_err(), "{key}");
        }
        let mut s = base();
        s["resources"] = json!({"memory_mb": 64, "privileged": true});
        assert!(parse(&s, &caps()).is_err());
        let mut s = base();
        s["backend"] = json!("url");
        assert!(parse(&s, &caps()).is_err());
    }

    #[test]
    fn image_references() {
        let digest = format!("sha256:{}", "a".repeat(64));
        for ok in [
            "busybox:1.37".to_string(),
            "ghcr.io/acme/echo:v1".to_string(),
            "registry.example.com:443/a/b_c__d/e-f.g:tag_1.2-x".to_string(),
            format!("ghcr.io/acme/echo@{digest}"),
        ] {
            assert!(parse_image(&ok).is_ok(), "{ok}");
        }
        let r = parse_image(&format!("ghcr.io/acme/echo@{digest}")).unwrap();
        assert_eq!(r.reference, Reference::Digest(digest.clone()));
        assert_eq!(r.tag_or_digest(), digest);
        for bad in [
            "",
            "busybox",   // no tag (a pull would take every tag)
            "Busybox:1", // upper case
            "-busybox:1",
            "busybox:-1",
            "busybox:1@sha256:abc",
            "busybox@sha256:ABCDEF",
            "busybox:1 --privileged",
            "busybox:1?x=y",
            "../etc:1",
            "a//b:1",
            "a/b/:1",
            "http://evil/x:1",
            "busybox:",
            "a..b:1",
        ] {
            assert!(parse_image(bad).is_err(), "{bad}");
        }
        assert!(parse_image(&format!("{}:1", "a".repeat(260))).is_err());
        let mut s = base();
        s["image"] = json!(format!("busybox@{digest}"));
        assert!(parse(&s, &caps()).is_ok());
    }

    /// Audit N1: the engine pulls from the host's network namespace, so a loopback, IP-literal
    /// or `localhost` registry would let a spec reach the host's own listeners.
    #[test]
    fn local_and_ip_registries_are_refused() {
        for bad in [
            "localhost/echo:1",
            "localhost:5000/echo:1",
            "LOCALHOST:5000/echo:1",
            "registry.localhost/echo:1",
            "127.0.0.1:2375/echo:1",
            "127.0.0.1/echo:1",
            "10.0.0.5:5000/echo:1",
            "169.254.169.254/echo:1",
            "127.1:5000/echo:1",
            "2130706433:5000/echo:1",
            "[::1]:5000/echo:1",
        ] {
            assert!(parse_image(bad).is_err(), "{bad}");
        }
        assert!(parse_image("ghcr.io/acme/echo:1").is_ok());
        assert!(parse_image("registry.example.com:5000/echo:1").is_ok());
        assert!(parse_image("my-registry.internal.example:443/a/b:1").is_ok());
    }

    #[test]
    fn ports_and_health_paths() {
        let mut s = base();
        s["port"] = json!(0);
        assert!(parse(&s, &caps()).is_err());
        s["port"] = json!(70000);
        assert!(parse(&s, &caps()).is_err());
        for bad in ["health", "/a b", "/a?b", "/a\nb"] {
            let mut s = base();
            s["health_path"] = json!(bad);
            assert!(parse(&s, &caps()).is_err(), "{bad}");
        }
        let mut s = base();
        s.as_object_mut().unwrap().remove("health_path");
        assert_eq!(parse(&s, &caps()).unwrap().health_path, "/");
    }

    #[test]
    fn environment_entries_are_checked_and_port_is_the_runners() {
        let env: BTreeMap<String, String> =
            [("A".into(), "1".into()), ("_B2".into(), "x=y".into())].into();
        assert_eq!(
            environment(&env, 8080).unwrap(),
            ["A=1", "_B2=x=y", "PORT=8080"]
        );
        for (k, v) in [
            ("PORT", "1"),
            ("1A", "x"),
            ("A-B", "x"),
            ("", "x"),
            ("A", "a\0b"),
        ] {
            let env: BTreeMap<String, String> = [(k.to_string(), v.to_string())].into();
            assert!(environment(&env, 1).is_err(), "{k:?}");
        }
        let many: BTreeMap<String, String> = (0..=MAX_ENV)
            .map(|i| (format!("V{i}"), String::new()))
            .collect();
        assert!(environment(&many, 1).is_err());
        let big: BTreeMap<String, String> = [("A".into(), "x".repeat(MAX_ENV_VALUE + 1))].into();
        assert!(environment(&big, 1).is_err());
    }
}
