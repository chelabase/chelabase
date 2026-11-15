//! Request bodies: what a create, exec, connect or volume body may carry before the proxy
//! forwards it.
//!
//! The engine decodes bodies with Go's `encoding/json`, which matches struct keys
//! case-insensitively (folding `ſ` to `s` and `K`, the Kelvin sign, to `k`), keeps the last of
//! duplicate keys and merges repeated objects. A check over `serde_json::Value` (which also
//! keeps the last duplicate) or over exact keys alone would pass `"hostconfig"`, `"HoſtConfig"`
//! or a second `"HostConfig"` that the engine then honours. So:
//!
//! - [`parse_strict`] refuses a duplicate key at every level, in objects and maps alike;
//! - every struct the engine decodes (the body, HostConfig, each mount and its options,
//!   RestartPolicy, LogConfig, Healthcheck, NetworkingConfig, each endpoint, each ulimit and
//!   port binding) is checked against an allowlist byte for byte;
//! - **present means checked**: a key outside the allowlist is refused unless its value is
//!   `null` or `false`, and then it is dropped. `[]`, `{}`, `""` and `0` count as present (an
//!   empty `MaskedPaths` unmasks `/proc`);
//! - the proxy forwards the body these functions return, re-serialised, never the client's
//!   bytes.
//!
//! The allowlists are closed on purpose: Docker keeps adding HostConfig keys, and a denylist
//! would fail open on each new one.
//!
//! Refs a body names (volumes, `container:` network modes, networks) are resolved through
//! [`Scope::resolve`], which the server backs with inspects (m4). It answers only for objects
//! in the stack.

use std::fmt;

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};

use crate::Deny;
use crate::ops::OpKind;
use crate::profile::{MountType, Profile, is_exact_path};
use crate::scope::{Labels, Stack, force_labels};

/// What a ref names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    Container,
    Network,
    Volume,
}

/// The stack a body is checked against, and how its refs resolve.
pub struct Scope<'a> {
    pub stack: &'a Stack,
    /// A ref (name or ID, as the engine would look it up) to the object's canonical ID (a
    /// volume's name), or `None` when it doesn't exist or isn't in the stack.
    pub resolve: &'a dyn Fn(RefKind, &str) -> Option<String>,
}

impl Scope<'_> {
    fn resolve(&self, kind: RefKind, r: &str) -> Option<String> {
        if r.is_empty() {
            return None;
        }
        (self.resolve)(kind, r)
    }
}

/// A checked network connect or disconnect: the body to forward and the container it names,
/// for the server to resolve, scope-check and match against
/// [`Profile::allows_connect_container`]. The path's network is
/// [`Profile::allows`]'s check.
#[derive(Debug, Clone, PartialEq)]
pub struct NetworkConnect {
    pub body: Value,
    pub container: String,
}

/// Host paths no bind may expose, nor any parent or child of them: the engine's sockets and
/// state (the `docker-proxy` volume, holding the proxy's sockets, lives under
/// `/var/lib/docker/volumes`).
const NEVER_BIND: &[&str] = &[
    "/var/run/docker.sock",
    "/run/docker.sock",
    "/var/run/docker",
    "/run/docker",
    "/run/containerd",
    "/var/run/containerd",
    "/var/lib/docker",
    "/var/lib/containerd",
];

/// The volume holding the proxy's sockets: `<project>_docker-proxy`.
fn proxy_volume(stack: &Stack) -> String {
    format!("{}_docker-proxy", stack.project)
}

// ---------------------------------------------------------------------------------------------
// The strict parser.

/// Parses a body as JSON, refusing a duplicate key in any object (at any depth), trailing data,
/// and non-finite numbers. serde_json's recursion limit bounds the depth.
pub fn parse_strict(body: &[u8]) -> Result<Value, Deny> {
    let mut de = serde_json::Deserializer::from_slice(body);
    let value = StrictValue
        .deserialize(&mut de)
        .and_then(|v| de.end().map(|()| v))
        .map_err(|e| Deny::new(format!("the body doesn't parse strictly: {e}")))?;
    Ok(value)
}

struct StrictValue;

impl<'de> DeserializeSeed<'de> for StrictValue {
    type Value = Value;
    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for StrictValue {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_bool<E>(self, b: bool) -> Result<Value, E> {
        Ok(Value::Bool(b))
    }

    fn visit_i64<E>(self, n: i64) -> Result<Value, E> {
        Ok(Value::Number(n.into()))
    }

    fn visit_u64<E>(self, n: u64) -> Result<Value, E> {
        Ok(Value::Number(n.into()))
    }

    fn visit_f64<E: de::Error>(self, n: f64) -> Result<Value, E> {
        Number::from_f64(n)
            .map(Value::Number)
            .ok_or_else(|| E::custom("a number that isn't finite"))
    }

    fn visit_str<E>(self, s: &str) -> Result<Value, E> {
        Ok(Value::String(s.to_owned()))
    }

    fn visit_string<E>(self, s: String) -> Result<Value, E> {
        Ok(Value::String(s))
    }

    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_none<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut out = Vec::new();
        while let Some(v) = seq.next_element_seed(StrictValue)? {
            out.push(v);
        }
        Ok(Value::Array(out))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut out = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if out.contains_key(&key) {
                return Err(de::Error::custom(format!("duplicate key {key:?}")));
            }
            let value = map.next_value_seed(StrictValue)?;
            out.insert(key, value);
        }
        Ok(Value::Object(out))
    }
}

// ---------------------------------------------------------------------------------------------
// Allowlist helpers.

/// The type an allowlisted field must have. `Nested` fields are checked by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Bool,
    Int,
    Str,
    StrList,
    /// A string or a list of strings (the engine's `StrSlice`: Cmd, Entrypoint).
    StrOrList,
    /// An object of string values.
    StrMap,
    Nested,
}
use Kind::*;

fn deny(path: &str, why: impl fmt::Display) -> Deny {
    Deny::new(format!("{path}: {why}"))
}

/// Whether the engine could read `v` as set: anything but `null` or `false`.
fn present(v: &Value) -> bool {
    !matches!(v, Value::Null | Value::Bool(false))
}

fn is_int(v: &Value) -> bool {
    v.is_i64() || v.is_u64()
}

fn is_str_list(v: &Value) -> bool {
    v.as_array()
        .is_some_and(|items| items.iter().all(Value::is_string))
}

fn is_str_map(v: &Value) -> bool {
    v.as_object()
        .is_some_and(|m| m.values().all(Value::is_string))
}

/// One struct, strictly: allowlisted keys (byte for byte) of the right type are kept, `null`s
/// dropped; any other key is refused unless it is `null` or `false`, and then dropped.
fn fields(v: &Value, path: &str, allowed: &[(&str, Kind)]) -> Result<Map<String, Value>, Deny> {
    let Value::Object(obj) = v else {
        return Err(deny(path, "must be an object"));
    };
    let mut out = Map::new();
    for (key, value) in obj {
        let here = format!("{path}.{key}");
        match allowed.iter().find(|(k, _)| k == key) {
            Some(_) if value.is_null() => {}
            Some((_, kind)) => {
                let ok = match kind {
                    Bool => value.is_boolean(),
                    Int => is_int(value),
                    Str => value.is_string(),
                    StrList => is_str_list(value),
                    StrOrList => value.is_string() || is_str_list(value),
                    StrMap => is_str_map(value),
                    Nested => true,
                };
                if !ok {
                    return Err(deny(&here, format!("must be {kind:?}")));
                }
                out.insert(key.clone(), value.clone());
            }
            None if present(value) => return Err(deny(&here, "is not allowed")),
            None => {}
        }
    }
    Ok(out)
}

fn str_field<'a>(m: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    m.get(key).and_then(Value::as_str)
}

fn str_items(v: &Value) -> impl Iterator<Item = &str> {
    v.as_array().into_iter().flatten().filter_map(Value::as_str)
}

/// Forces the stack label onto `obj["Labels"]` (a string map, absent meaning none).
fn force_label_field(obj: &mut Map<String, Value>, path: &str, stack: &Stack) -> Result<(), Deny> {
    let mut labels = Labels::new();
    if let Some(v) = obj.get("Labels") {
        let Value::Object(raw) = v else {
            return Err(deny(&format!("{path}.Labels"), "must be StrMap"));
        };
        for (k, v) in raw {
            let Value::String(v) = v else {
                return Err(deny(&format!("{path}.Labels"), "must be StrMap"));
            };
            labels.insert(k.clone(), v.clone());
        }
    }
    force_labels(&mut labels, stack)?;
    let labels = labels
        .into_iter()
        .map(|(k, v)| (k, Value::String(v)))
        .collect();
    obj.insert("Labels".into(), Value::Object(labels));
    Ok(())
}

/// A name the engine accepts for a container or volume: `[A-Za-z0-9][A-Za-z0-9_.-]+`.
fn object_name_ok(name: &str) -> bool {
    name.len() >= 2
        && name.len() <= 255
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// `name` is well formed and starts with one of `prefixes` (with something after it).
fn name_with_prefix(name: &str, prefixes: &[String]) -> bool {
    object_name_ok(name)
        && prefixes
            .iter()
            .any(|p| name.len() > p.len() && name.starts_with(p.as_str()))
}

// ---------------------------------------------------------------------------------------------
// Container create.

const CONTAINER_FIELDS: &[(&str, Kind)] = &[
    ("Image", Str),
    ("Cmd", StrOrList),
    ("Entrypoint", StrOrList),
    ("Env", StrList),
    ("Labels", StrMap),
    ("User", Str),
    ("WorkingDir", Str),
    ("ExposedPorts", Nested),
    ("Tty", Bool),
    ("OpenStdin", Bool),
    ("StdinOnce", Bool),
    ("AttachStdin", Bool),
    ("AttachStdout", Bool),
    ("AttachStderr", Bool),
    ("Hostname", Str),
    ("Healthcheck", Nested),
    ("StopSignal", Str),
    ("StopTimeout", Int),
    ("NetworkingConfig", Nested),
    ("HostConfig", Nested),
];

const HOST_FIELDS: &[(&str, Kind)] = &[
    ("NetworkMode", Str),
    ("RestartPolicy", Nested),
    ("Memory", Int),
    ("MemorySwap", Int),
    ("NanoCpus", Int),
    ("CpuShares", Int),
    ("PidsLimit", Int),
    ("Mounts", Nested),
    ("Binds", StrList),
    ("Tmpfs", StrMap),
    ("LogConfig", Nested),
    ("CgroupnsMode", Str),
    ("ReadonlyRootfs", Bool),
    ("Init", Bool),
    ("AutoRemove", Bool),
    ("CapDrop", StrList),
    ("CapAdd", StrList),
    ("SecurityOpt", StrList),
    ("ExtraHosts", StrList),
    ("Ulimits", Nested),
    // `private` or `none` only (the service-runner pins `private`).
    ("IpcMode", Str),
    // Only when the profile publishes ports (m6 limits addresses and ports).
    ("PortBindings", Nested),
];

const MOUNT_FIELDS: &[(&str, Kind)] = &[
    ("Type", Str),
    ("Source", Str),
    ("Target", Str),
    ("ReadOnly", Bool),
    ("BindOptions", Nested),
    ("VolumeOptions", Nested),
    ("TmpfsOptions", Nested),
];

const BIND_OPTION_FIELDS: &[(&str, Kind)] = &[
    ("Propagation", Str),
    ("NonRecursive", Bool),
    ("ReadOnlyNonRecursive", Bool),
    ("ReadOnlyForceRecursive", Bool),
];
const VOLUME_OPTION_FIELDS: &[(&str, Kind)] = &[("NoCopy", Bool)];
const TMPFS_OPTION_FIELDS: &[(&str, Kind)] = &[("SizeBytes", Int), ("Mode", Int)];
const RESTART_FIELDS: &[(&str, Kind)] = &[("Name", Str), ("MaximumRetryCount", Int)];
const LOG_FIELDS: &[(&str, Kind)] = &[("Type", Str), ("Config", StrMap)];
const HEALTHCHECK_FIELDS: &[(&str, Kind)] = &[
    ("Test", StrList),
    ("Interval", Int),
    ("Timeout", Int),
    ("StartPeriod", Int),
    ("StartInterval", Int),
    ("Retries", Int),
];
const ULIMIT_FIELDS: &[(&str, Kind)] = &[("Name", Str), ("Soft", Int), ("Hard", Int)];
const PORT_BINDING_FIELDS: &[(&str, Kind)] = &[("HostIp", Str), ("HostPort", Str)];
const NETWORKING_FIELDS: &[(&str, Kind)] = &[("EndpointsConfig", Nested)];
/// An endpoint on create. `IPAMConfig` waits for the profile's subnets (m6).
const ENDPOINT_FIELDS: &[(&str, Kind)] = &[
    ("Aliases", StrList),
    ("GwPriority", Int),
    ("NetworkID", Str),
];
/// An endpoint on connect: the path names the network, so no `NetworkID`.
const CONNECT_ENDPOINT_FIELDS: &[(&str, Kind)] = &[("Aliases", StrList), ("GwPriority", Int)];

const LOG_DRIVERS: &[&str] = &["json-file", "local"];
const LOG_OPTIONS: &[&str] = &["max-size", "max-file"];
const RESTART_NAMES: &[&str] = &["", "no", "always", "unless-stopped", "on-failure"];
const SECURITY_OPTS: &[&str] = &["no-new-privileges", "no-new-privileges:true"];
const BIND_PROPAGATION: &[&str] = &["private", "rprivate"];
/// `Binds` options: no SELinux relabel (`z`, `Z` relabel the host path), no shared or slave
/// propagation.
const BIND_STRING_OPTIONS: &[&str] = &["ro", "rw", "nocopy", "private", "rprivate"];

/// Checks a container create body (and its `name` query parameter) and returns the body to
/// forward: the stack label forced, `HostConfig.CgroupnsMode` forced to `private`, keys outside
/// the allowlists dropped (they were `null` or `false`).
pub fn validate_container_create(
    body: &[u8],
    name: Option<&str>,
    profile: &Profile,
    scope: &Scope,
) -> Result<Value, Deny> {
    container_name(name, scope.stack)?;
    let v = parse_strict(body)?;
    let mut top = fields(&v, "body", CONTAINER_FIELDS)?;
    force_label_field(&mut top, "body", scope.stack)?;
    if let Some(h) = top.get("Healthcheck") {
        let h = fields(h, "Healthcheck", HEALTHCHECK_FIELDS)?;
        top.insert("Healthcheck".into(), Value::Object(h));
    }
    if let Some(p) = top.get("ExposedPorts") {
        exposed_ports(p)?;
    }
    if let Some(n) = top.get("NetworkingConfig") {
        let n = networking_config(n, profile, scope)?;
        top.insert("NetworkingConfig".into(), Value::Object(n));
    }
    // An absent HostConfig (or NetworkMode) is the default bridge, shared by every stack.
    let Some(h) = top.get("HostConfig") else {
        return Err(deny("body.HostConfig", "is required (NetworkMode)"));
    };
    let h = host_config(h, profile, scope)?;
    top.insert("HostConfig".into(), Value::Object(h));
    Ok(Value::Object(top))
}

/// The `name` query parameter: absent (or empty: the engine picks a random name, which can't
/// take a stack's prefix), or `chelabase-<slug>-…` / `<project>-…` (Compose's names). One
/// leading `/` is the engine's own spelling.
fn container_name(name: Option<&str>, stack: &Stack) -> Result<(), Deny> {
    let Some(raw) = name.filter(|n| !n.is_empty()) else {
        return Ok(());
    };
    let name = raw.strip_prefix('/').unwrap_or(raw);
    let prefixes = [
        format!("chelabase-{}-", stack.slug),
        format!("{}-", stack.project),
    ];
    if !name_with_prefix(name, &prefixes) {
        return Err(Deny::new(format!(
            "container name {raw:?} must start with chelabase-{}-",
            stack.slug
        )));
    }
    Ok(())
}

/// `{"80/tcp": {}}`: string keys, empty-object (or null) values.
fn exposed_ports(v: &Value) -> Result<(), Deny> {
    let Value::Object(m) = v else {
        return Err(deny("body.ExposedPorts", "must be an object"));
    };
    for (port, val) in m {
        if !(val.is_null() || val.as_object().is_some_and(Map::is_empty)) {
            return Err(deny(&format!("body.ExposedPorts.{port}"), "must be {}"));
        }
    }
    Ok(())
}

/// A network a container may join: one of the profile's (exact name) or one that resolves in
/// scope. The engine's built-in networks never are.
fn network_allowed(name: &str, profile: &Profile, scope: &Scope) -> bool {
    !matches!(name, "" | "host" | "bridge" | "default" | "none")
        && (profile.networks.iter().any(|n| n == name)
            || scope.resolve(RefKind::Network, name).is_some())
}

fn networking_config(
    v: &Value,
    profile: &Profile,
    scope: &Scope,
) -> Result<Map<String, Value>, Deny> {
    let mut n = fields(v, "body.NetworkingConfig", NETWORKING_FIELDS)?;
    if let Some(eps) = n.get("EndpointsConfig") {
        let Value::Object(eps) = eps else {
            return Err(deny(
                "body.NetworkingConfig.EndpointsConfig",
                "must be an object",
            ));
        };
        let mut out = Map::new();
        for (net, settings) in eps {
            let path = format!("body.NetworkingConfig.EndpointsConfig.{net}");
            if !network_allowed(net, profile, scope) {
                return Err(deny(
                    &path,
                    "is not one of the profile's or the stack's networks",
                ));
            }
            if settings.is_null() {
                out.insert(net.clone(), Value::Object(Map::new()));
                continue;
            }
            let s = fields(settings, &path, ENDPOINT_FIELDS)?;
            // The engine prefers NetworkID over the key for a user-defined network
            // (daemon/container_operations.go `getNetworkID`), so both must be one network.
            if let Some(id) = str_field(&s, "NetworkID").filter(|id| !id.is_empty())
                && id != net
            {
                let same = match (
                    scope.resolve(RefKind::Network, net),
                    scope.resolve(RefKind::Network, id),
                ) {
                    (Some(a), Some(b)) => a == b,
                    _ => false,
                };
                if !same {
                    return Err(deny(
                        &format!("{path}.NetworkID"),
                        format!("{id:?} is not the network {net}"),
                    ));
                }
            }
            out.insert(net.clone(), Value::Object(s));
        }
        n.insert("EndpointsConfig".into(), Value::Object(out));
    }
    Ok(n)
}

fn host_config(v: &Value, profile: &Profile, scope: &Scope) -> Result<Map<String, Value>, Deny> {
    const P: &str = "HostConfig";
    let mut h = fields(v, P, HOST_FIELDS)?;
    let at = |k: &str| format!("{P}.{k}");

    match str_field(&h, "NetworkMode") {
        None => return Err(deny(&at("NetworkMode"), "is required")),
        Some(mode) => network_mode(mode, profile, scope)?,
    }

    if let Some(r) = h.get("RestartPolicy") {
        let r = fields(r, &at("RestartPolicy"), RESTART_FIELDS)?;
        if let Some(name) = str_field(&r, "Name")
            && !RESTART_NAMES.contains(&name)
        {
            return Err(deny(
                &at("RestartPolicy.Name"),
                format!("{name:?} is not allowed"),
            ));
        }
        h.insert("RestartPolicy".into(), Value::Object(r));
    }

    if let Some(l) = h.get("LogConfig") {
        let l = fields(l, &at("LogConfig"), LOG_FIELDS)?;
        match str_field(&l, "Type") {
            Some(t) if LOG_DRIVERS.contains(&t) => {}
            t => {
                return Err(deny(
                    &at("LogConfig.Type"),
                    format!("{t:?} is not json-file or local"),
                ));
            }
        }
        if let Some(Value::Object(c)) = l.get("Config")
            && let Some(k) = c.keys().find(|k| !LOG_OPTIONS.contains(&k.as_str()))
        {
            return Err(deny(
                &at(&format!("LogConfig.Config.{k}")),
                "is not allowed",
            ));
        }
        h.insert("LogConfig".into(), Value::Object(l));
    }

    match str_field(&h, "CgroupnsMode") {
        None | Some("" | "private") => {}
        Some(m) => return Err(deny(&at("CgroupnsMode"), format!("{m:?} is not allowed"))),
    }
    h.insert("CgroupnsMode".into(), Value::String("private".into()));

    if let Some(m) = str_field(&h, "IpcMode")
        && !matches!(m, "private" | "none")
    {
        return Err(deny(&at("IpcMode"), format!("{m:?} is not allowed")));
    }

    if let Some(caps) = h.get("CapAdd") {
        for cap in str_items(caps) {
            if !cap_allowed(cap, profile) {
                return Err(deny(
                    &at("CapAdd"),
                    format!("{cap} is not one of {}'s capabilities", profile.name),
                ));
            }
        }
    }

    if let Some(opts) = h.get("SecurityOpt")
        && let Some(o) = str_items(opts).find(|o| !SECURITY_OPTS.contains(o))
    {
        return Err(deny(&at("SecurityOpt"), format!("{o:?} is not allowed")));
    }

    if let Some(ul) = h.get("Ulimits") {
        let Value::Array(items) = ul else {
            return Err(deny(&at("Ulimits"), "must be a list"));
        };
        let items = items
            .iter()
            .map(|u| fields(u, &at("Ulimits[]"), ULIMIT_FIELDS).map(Value::Object))
            .collect::<Result<Vec<_>, _>>()?;
        h.insert("Ulimits".into(), Value::Array(items));
    }

    if let Some(pb) = h.get("PortBindings") {
        if !profile.publish_ports {
            return Err(deny(&at("PortBindings"), "is not allowed"));
        }
        let pb = port_bindings(pb)?;
        h.insert("PortBindings".into(), Value::Object(pb));
    }

    if let Some(Value::Object(t)) = h.get("Tmpfs") {
        if !t.is_empty() && !profile.mount_types.contains(&MountType::Tmpfs) {
            return Err(deny(
                &at("Tmpfs"),
                format!("tmpfs mounts are not allowed for {}", profile.name),
            ));
        }
        if let Some(k) = t.keys().find(|k| !k.starts_with('/')) {
            return Err(deny(&at("Tmpfs"), format!("{k:?} is not an absolute path")));
        }
    }

    if let Some(ms) = h.get("Mounts") {
        let Value::Array(items) = ms else {
            return Err(deny(&at("Mounts"), "must be a list"));
        };
        let items = items
            .iter()
            .enumerate()
            .map(|(i, m)| mount(m, &at(&format!("Mounts[{i}]")), profile, scope).map(Value::Object))
            .collect::<Result<Vec<_>, _>>()?;
        h.insert("Mounts".into(), Value::Array(items));
    }

    if let Some(binds) = h.get("Binds") {
        for (i, b) in str_items(binds).enumerate() {
            bind_string(b, &at(&format!("Binds[{i}]")), profile, scope)?;
        }
    }
    Ok(h)
}

/// `NetworkMode`: a profile network, an in-scope network, `none`, or (with the profile flag)
/// `container:<ref>` for an in-scope container.
fn network_mode(mode: &str, profile: &Profile, scope: &Scope) -> Result<(), Deny> {
    let path = "HostConfig.NetworkMode";
    if mode == "none" {
        return Ok(());
    }
    if let Some(r) = mode.strip_prefix("container:") {
        if !profile.container_network_mode {
            return Err(deny(
                path,
                format!("container: is not allowed for {}", profile.name),
            ));
        }
        if scope.resolve(RefKind::Container, r).is_none() {
            return Err(deny(path, format!("container {r:?} is not in the stack")));
        }
        return Ok(());
    }
    if !network_allowed(mode, profile, scope) {
        return Err(deny(path, format!("{mode} is not allowed")));
    }
    Ok(())
}

/// A capability the profile lists, in any spelling the engine normalises to it (`CAP_` prefix,
/// any ASCII case). Non-ASCII is refused: Go's `ToUpper` folds `ſ` to `S`.
fn cap_allowed(cap: &str, profile: &Profile) -> bool {
    if cap.is_empty() || !cap.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return false;
    }
    let upper = cap.to_ascii_uppercase();
    let bare = upper.strip_prefix("CAP_").unwrap_or(&upper);
    bare != "ALL" && profile.cap_add.iter().any(|c| c == bare)
}

fn port_bindings(v: &Value) -> Result<Map<String, Value>, Deny> {
    let Value::Object(m) = v else {
        return Err(deny("HostConfig.PortBindings", "must be an object"));
    };
    let mut out = Map::new();
    for (port, bindings) in m {
        let path = format!("HostConfig.PortBindings.{port}");
        let items = match bindings {
            Value::Null => Vec::new(),
            Value::Array(items) => items
                .iter()
                .map(|b| fields(b, &path, PORT_BINDING_FIELDS).map(Value::Object))
                .collect::<Result<_, _>>()?,
            _ => return Err(deny(&path, "must be a list")),
        };
        out.insert(port.clone(), Value::Array(items));
    }
    Ok(out)
}

/// One `Mounts[]` entry.
fn mount(
    v: &Value,
    path: &str,
    profile: &Profile,
    scope: &Scope,
) -> Result<Map<String, Value>, Deny> {
    let mut m = fields(v, path, MOUNT_FIELDS)?;
    let at = |k: &str| format!("{path}.{k}");
    match str_field(&m, "Target") {
        Some(t) if t.starts_with('/') && !t.chars().any(char::is_control) => {}
        _ => return Err(deny(&at("Target"), "must be an absolute path")),
    }
    let typ = str_field(&m, "Type").unwrap_or_default().to_string();
    let source = str_field(&m, "Source").map(str::to_string);
    let only = |m: &Map<String, Value>, keep: &str| -> Result<(), Deny> {
        for k in ["BindOptions", "VolumeOptions", "TmpfsOptions"] {
            if k != keep && m.contains_key(k) {
                return Err(deny(&at(k), format!("is not allowed on a {typ} mount")));
            }
        }
        Ok(())
    };
    let need = |t: MountType| -> Result<(), Deny> {
        if profile.mount_types.contains(&t) {
            Ok(())
        } else {
            Err(deny(
                path,
                format!("{typ} mounts are not allowed for {}", profile.name),
            ))
        }
    };
    match typ.as_str() {
        "bind" => {
            need(MountType::Bind)?;
            only(&m, "BindOptions")?;
            bind_source(
                source.as_deref().unwrap_or_default(),
                &at("Source"),
                profile,
            )?;
            if let Some(o) = m.get("BindOptions") {
                let o = fields(o, &at("BindOptions"), BIND_OPTION_FIELDS)?;
                if let Some(p) = str_field(&o, "Propagation")
                    && !BIND_PROPAGATION.contains(&p)
                {
                    return Err(deny(
                        &at("BindOptions.Propagation"),
                        format!("{p:?} is not allowed"),
                    ));
                }
                m.insert("BindOptions".into(), Value::Object(o));
            }
        }
        "volume" => {
            need(MountType::Volume)?;
            only(&m, "VolumeOptions")?;
            volume_source(source.as_deref().unwrap_or_default(), &at("Source"), scope)?;
            if let Some(o) = m.get("VolumeOptions") {
                let o = fields(o, &at("VolumeOptions"), VOLUME_OPTION_FIELDS)?;
                m.insert("VolumeOptions".into(), Value::Object(o));
            }
        }
        "tmpfs" => {
            need(MountType::Tmpfs)?;
            only(&m, "TmpfsOptions")?;
            if source.is_some() {
                return Err(deny(&at("Source"), "is not allowed on a tmpfs mount"));
            }
            if let Some(o) = m.get("TmpfsOptions") {
                let o = fields(o, &at("TmpfsOptions"), TMPFS_OPTION_FIELDS)?;
                m.insert("TmpfsOptions".into(), Value::Object(o));
            }
        }
        _ => return Err(deny(&at("Type"), format!("{typ:?} is not allowed"))),
    }
    Ok(m)
}

/// A bind source: exactly one of the profile's binds (absolute, normalised: no `..`, `.`, `//`
/// or trailing `/`), never the engine's sockets or state, nor a parent or child of them.
fn bind_source(src: &str, path: &str, profile: &Profile) -> Result<(), Deny> {
    if !profile.mount_types.contains(&MountType::Bind) {
        return Err(deny(
            path,
            format!("bind mounts are not allowed for {}", profile.name),
        ));
    }
    if !is_exact_path(src) {
        return Err(deny(
            path,
            format!("{src:?} is not an absolute, normalised path"),
        ));
    }
    let related = |a: &str, b: &str| {
        a == b
            || a.strip_prefix(b).is_some_and(|r| r.starts_with('/'))
            || b.strip_prefix(a).is_some_and(|r| r.starts_with('/'))
    };
    if let Some(never) = NEVER_BIND.iter().find(|n| related(src, n)) {
        return Err(deny(path, format!("{src:?} exposes {never}")));
    }
    if !profile.binds.iter().any(|b| b == src) {
        return Err(deny(
            path,
            format!("{src:?} is not one of {}'s binds", profile.name),
        ));
    }
    Ok(())
}

/// A volume source: a named volume that exists in the stack (a missing one would be created
/// unlabelled) and isn't the proxy's.
fn volume_source(src: &str, path: &str, scope: &Scope) -> Result<(), Deny> {
    if !object_name_ok(src) {
        return Err(deny(
            path,
            format!("{src:?} is not a volume name (anonymous volumes are not allowed)"),
        ));
    }
    let Some(id) = scope.resolve(RefKind::Volume, src) else {
        return Err(deny(path, format!("volume {src:?} is not in the stack")));
    };
    let proxy = proxy_volume(scope.stack);
    if src == proxy
        || scope
            .resolve(RefKind::Volume, &proxy)
            .is_some_and(|p| p == id)
    {
        return Err(deny(path, "the proxy's volume is never mounted"));
    }
    Ok(())
}

/// A `Binds` string, `src:dst[:opts]`, as the engine's Linux parser splits it: a source with
/// a leading `/` is a bind (checked like a `bind` mount), anything else a named volume. One
/// part (an anonymous volume), more than three (a Windows path), a relative destination or an
/// option outside [`BIND_STRING_OPTIONS`] is refused.
fn bind_string(b: &str, path: &str, profile: &Profile, scope: &Scope) -> Result<(), Deny> {
    let parts: Vec<&str> = b.split(':').collect();
    let (src, dst, opts) = match parts.as_slice() {
        [src, dst] => (*src, *dst, None),
        [src, dst, opts] => (*src, *dst, Some(*opts)),
        _ => return Err(deny(path, format!("{b:?} is not src:dst[:opts]"))),
    };
    if !dst.starts_with('/') || dst.chars().any(char::is_control) {
        return Err(deny(
            path,
            format!("{b:?}: the destination must be an absolute path"),
        ));
    }
    if let Some(opts) = opts
        && let Some(o) = opts.split(',').find(|o| !BIND_STRING_OPTIONS.contains(o))
    {
        return Err(deny(path, format!("{b:?}: option {o:?} is not allowed")));
    }
    if src.starts_with('/') {
        bind_source(src, path, profile)
    } else {
        if !profile.mount_types.contains(&MountType::Volume) {
            return Err(deny(
                path,
                format!("volume mounts are not allowed for {}", profile.name),
            ));
        }
        volume_source(src, path, scope)
    }
}

// ---------------------------------------------------------------------------------------------
// Exec, network, connect and volume bodies.

const EXEC_FIELDS: &[(&str, Kind)] = &[
    ("Cmd", StrList),
    ("Env", StrList),
    ("User", Str),
    ("WorkingDir", Str),
    ("AttachStdin", Bool),
    ("AttachStdout", Bool),
    ("AttachStderr", Bool),
    ("Tty", Bool),
    ("DetachKeys", Str),
];

/// Checks an exec create body. `Privileged` may only be `false` or absent.
pub fn validate_exec_create(body: &[u8]) -> Result<Value, Deny> {
    let v = parse_strict(body)?;
    Ok(Value::Object(fields(&v, "exec", EXEC_FIELDS)?))
}

const NETWORK_FIELDS: &[(&str, Kind)] = &[
    ("Name", Str),
    ("Driver", Str),
    ("Internal", Bool),
    ("Attachable", Bool),
    ("Labels", StrMap),
    ("EnableIPv4", Bool),
    ("EnableIPv6", Bool),
    ("IPAM", Nested),
];
/// IPAM: the default driver only. `Config` (subnets) waits for the profile's subnets (m6).
const IPAM_FIELDS: &[(&str, Kind)] = &[("Driver", Str)];

/// Checks a network create body: a `bridge` network named `<project>_…` or
/// `chelabase-<slug>-…` (the service-runner's slot networks), not attachable, no options,
/// labels forced.
pub fn validate_network_create(body: &[u8], scope: &Scope) -> Result<Value, Deny> {
    let v = parse_strict(body)?;
    let mut n = fields(&v, "network", NETWORK_FIELDS)?;
    let stack = scope.stack;
    let prefixes = [
        format!("{}_", stack.project),
        format!("chelabase-{}_", stack.slug),
        format!("chelabase-{}-", stack.slug),
    ];
    let name = str_field(&n, "Name").unwrap_or_default();
    if !name_with_prefix(name, &prefixes) {
        return Err(deny(
            "network.Name",
            format!("{name:?} must start with {}_", stack.project),
        ));
    }
    if let Some(d) = str_field(&n, "Driver")
        && d != "bridge"
    {
        return Err(deny("network.Driver", format!("{d:?} is not bridge")));
    }
    if n.get("Attachable") == Some(&Value::Bool(true)) {
        return Err(deny("network.Attachable", "must be false"));
    }
    if let Some(i) = n.get("IPAM") {
        let i = fields(i, "network.IPAM", IPAM_FIELDS)?;
        if let Some(d) = str_field(&i, "Driver")
            && d != "default"
        {
            return Err(deny("network.IPAM.Driver", format!("{d:?} is not default")));
        }
        n.insert("IPAM".into(), Value::Object(i));
    }
    force_label_field(&mut n, "network", stack)?;
    Ok(Value::Object(n))
}

const CONNECT_FIELDS: &[(&str, Kind)] = &[("Container", Str), ("EndpointConfig", Nested)];
const DISCONNECT_FIELDS: &[(&str, Kind)] = &[("Container", Str), ("Force", Bool)];

/// Checks a network connect or disconnect body and returns it with the container it names
/// (non-empty). A connect's `EndpointConfig` may set aliases and the gateway priority only.
pub fn validate_network_connect(kind: OpKind, body: &[u8]) -> Result<NetworkConnect, Deny> {
    let allowed = match kind {
        OpKind::NetworkConnect => CONNECT_FIELDS,
        OpKind::NetworkDisconnect => DISCONNECT_FIELDS,
        _ => return Err(Deny::new(format!("{kind:?} has no connect body"))),
    };
    let v = parse_strict(body)?;
    let mut c = fields(&v, "connect", allowed)?;
    let container = str_field(&c, "Container").unwrap_or_default().to_string();
    if container.is_empty() {
        return Err(deny("connect.Container", "is required"));
    }
    if let Some(e) = c.get("EndpointConfig") {
        let e = fields(e, "connect.EndpointConfig", CONNECT_ENDPOINT_FIELDS)?;
        c.insert("EndpointConfig".into(), Value::Object(e));
    }
    Ok(NetworkConnect {
        body: Value::Object(c),
        container,
    })
}

const VOLUME_FIELDS: &[(&str, Kind)] = &[("Name", Str), ("Driver", Str), ("Labels", StrMap)];

/// Checks a volume create body: named `<project>_…`, driver `local`, no driver options,
/// labels forced. An anonymous (unnamed) volume is refused.
pub fn validate_volume_create(body: &[u8], scope: &Scope) -> Result<Value, Deny> {
    let v = parse_strict(body)?;
    let mut vol = fields(&v, "volume", VOLUME_FIELDS)?;
    let stack = scope.stack;
    let prefixes = [
        format!("{}_", stack.project),
        format!("chelabase-{}_", stack.slug),
    ];
    let name = str_field(&vol, "Name").unwrap_or_default();
    if !name_with_prefix(name, &prefixes) {
        return Err(deny(
            "volume.Name",
            format!("{name:?} must start with {}_", stack.project),
        ));
    }
    if let Some(d) = str_field(&vol, "Driver")
        && d != "local"
    {
        return Err(deny("volume.Driver", format!("{d:?} is not local")));
    }
    force_label_field(&mut vol, "volume", stack)?;
    Ok(Value::Object(vol))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{Preset, PresetConfig};
    use crate::scope::STACK_LABEL;
    use proptest::prelude::*;
    use serde_json::json;

    const LISTED: &str = "/home/dev/chelabase/.chelabase/stacks/feat-x/grpc-tls";
    const NET: &str = "chelabase-feat-x_default";

    fn config() -> PresetConfig {
        PresetConfig {
            compute_network: "chelabase-feat-x_compute".into(),
            control_network: "chelabase-feat-x_control".into(),
            stack_networks: vec![NET.into()],
            runner_binds: vec![LISTED.into()],
            reserved_images: vec!["chelabase/neon-compute-v17:dev".into()],
        }
    }

    fn preset(p: Preset) -> Profile {
        Profile::preset(p, &config()).unwrap()
    }

    fn stack() -> Stack {
        Stack::new("feat-x", "chelabase-feat-x").unwrap()
    }

    /// The in-scope objects: anything else (another stack's, the default bridge) is `None`.
    fn resolve(kind: RefKind, r: &str) -> Option<String> {
        let id = match (kind, r) {
            (RefKind::Network, NET | "net-id-1") => "n1",
            (RefKind::Network, "chelabase-feat-x_compute") => "n2",
            (RefKind::Network, "chelabase-feat-x_control") => "n3",
            (RefKind::Network, "chelabase-feat-x-svc-abc") => "n4",
            (RefKind::Volume, "chelabase-feat-x_data") => "chelabase-feat-x_data",
            (RefKind::Volume, "chelabase-feat-x_docker-proxy") => "chelabase-feat-x_docker-proxy",
            (RefKind::Container, "chelabase-feat-x-worker-1" | "c1") => "c1",
            _ => return None,
        };
        Some(id.to_string())
    }

    fn with_scope<T>(stack: &Stack, f: impl FnOnce(&Scope) -> T) -> T {
        f(&Scope {
            stack,
            resolve: &resolve,
        })
    }

    fn create_named(profile: &Profile, name: Option<&str>, body: &Value) -> Result<Value, Deny> {
        let raw = serde_json::to_vec(body).unwrap();
        with_scope(&stack(), |s| {
            validate_container_create(&raw, name, profile, s)
        })
    }

    fn create(profile: &Profile, body: &Value) -> Result<Value, Deny> {
        create_named(profile, None, body)
    }

    fn create_raw(profile: &Profile, raw: &str) -> Result<Value, Deny> {
        with_scope(&stack(), |s| {
            validate_container_create(raw.as_bytes(), None, profile, s)
        })
    }

    /// A minimal body the runner preset accepts.
    fn base() -> Value {
        json!({"Image": "busybox:1", "HostConfig": {"NetworkMode": NET}})
    }

    fn with_host(key: &str, value: Value) -> Value {
        let mut b = base();
        b["HostConfig"][key] = value;
        b
    }

    fn assert_stack_label(v: &Value) {
        assert_eq!(v["Labels"][STACK_LABEL], "feat-x", "{v}");
    }

    /// crates/chelabase-neon/src/backend.rs `create_body` (worker), as bollard serialises it.
    fn worker_create() -> Value {
        json!({
            "Image": "chelabase/neon-compute-v17:dev",
            "Entrypoint": ["/usr/local/bin/compute_ctl"],
            "Cmd": [
                "--pgbin=/usr/local/bin/postgres",
                "--pgdata=/var/db/postgres/compute",
                "--connstr=postgresql://cloud_admin@localhost:55433/postgres",
                "--compute-id=ep-1",
                "--control-plane-uri=http://worker:3030",
                "--external-http-port=3080"
            ],
            "Env": ["NEON_CONTROL_PLANE_TOKEN=tok"],
            "Labels": {"chelabase.dev/endpoint-id": "ep-1", "chelabase.dev/stack": "feat-x"},
            "HostConfig": {
                "NetworkMode": "chelabase-feat-x_compute",
                "RestartPolicy": {"Name": "unless-stopped"},
                "Memory": 2147483648_i64,
                "NanoCpus": 1000000000
            }
        })
    }

    /// services/service-runner/src/docker.rs `create_body`, as bollard serialises it.
    fn service_runner_create() -> Value {
        json!({
            "Image": format!("sha256:{}", "b".repeat(64)),
            "User": "65532:65532",
            "Env": ["PORT=8080"],
            "Labels": {
                "chelabase.dev/stack": "feat-x",
                "chelabase.dev/service-id": "abc",
                "chelabase.dev/env-digest": "digest"
            },
            "AttachStdin": false,
            "AttachStdout": false,
            "AttachStderr": false,
            "Tty": false,
            "OpenStdin": false,
            "HostConfig": {
                "Privileged": false,
                "ReadonlyRootfs": true,
                "CapDrop": ["ALL"],
                "CapAdd": [],
                "SecurityOpt": ["no-new-privileges:true"],
                "Memory": 268435456,
                "MemorySwap": 268435456,
                "NanoCpus": 500000000,
                "PidsLimit": 256,
                "NetworkMode": "chelabase-feat-x-svc-abc",
                "Mounts": [
                    {"Target": "/tmp", "Type": "tmpfs", "ReadOnly": false,
                     "TmpfsOptions": {"SizeBytes": 67108864, "Mode": 1023}},
                    {"Target": "/data", "Type": "tmpfs", "ReadOnly": false,
                     "TmpfsOptions": {"SizeBytes": 67108864, "Mode": 1023}}
                ],
                "Binds": [],
                "IpcMode": "private",
                "CgroupnsMode": "private",
                "PublishAllPorts": false,
                "OomKillDisable": false,
                "RestartPolicy": {"Name": "unless-stopped"},
                "LogConfig": {"Type": "json-file", "Config": {"max-size": "10m", "max-file": "3"}}
            }
        })
    }

    #[test]
    fn inventory_bodies_pass() {
        let s = stack();
        // Worker: the compute create, its exec, its connect to the control network.
        let out = create_named(
            &preset(Preset::Worker),
            Some("chelabase-feat-x-compute-ep-1"),
            &worker_create(),
        )
        .unwrap();
        assert_stack_label(&out);
        assert_eq!(out["HostConfig"]["CgroupnsMode"], "private");
        assert_eq!(out["Cmd"], worker_create()["Cmd"]);
        let exec = json!({
            "AttachStdin": true, "AttachStdout": true, "AttachStderr": true,
            "Env": ["PGPASSWORD=x"], "Cmd": ["psql", "-X"], "User": "postgres"
        });
        assert_eq!(
            validate_exec_create(exec.to_string().as_bytes()).unwrap(),
            exec
        );
        let c = validate_network_connect(OpKind::NetworkConnect, br#"{"Container":"0123abcd"}"#)
            .unwrap();
        assert_eq!(c.container, "0123abcd");

        // Service-runner: its create, slot network, connect and disconnect.
        let out = create_named(
            &preset(Preset::ServiceRunner),
            Some("chelabase-feat-x-svc-abc-def-123"),
            &service_runner_create(),
        )
        .unwrap();
        assert_stack_label(&out);
        assert_eq!(out["HostConfig"]["Mounts"][0]["Type"], "tmpfs");
        let net = json!({
            "Name": "chelabase-feat-x-svc-abc-def", "Driver": "bridge", "Internal": false,
            "Attachable": false,
            "Labels": {"chelabase.dev/stack": "feat-x", "chelabase.dev/service-id": "abc"}
        });
        let out = with_scope(&s, |sc| {
            validate_network_create(net.to_string().as_bytes(), sc)
        })
        .unwrap();
        assert_stack_label(&out);
        let c = validate_network_connect(
            OpKind::NetworkConnect,
            br#"{"Container":"svc","EndpointConfig":{"GwPriority":-1}}"#,
        )
        .unwrap();
        assert_eq!(c.container, "svc");
        assert_eq!(c.body["EndpointConfig"]["GwPriority"], -1);
        let c = validate_network_connect(
            OpKind::NetworkDisconnect,
            br#"{"Container":"svc","Force":true}"#,
        )
        .unwrap();
        assert_eq!(c.container, "svc");
    }

    #[test]
    fn each_escape_field_refused() {
        let runner = preset(Preset::Runner);
        assert_stack_label(&create(&runner, &base()).unwrap());
        let cases: &[(&str, Value)] = &[
            ("Privileged", json!(true)),
            ("CapAdd", json!(["SYS_ADMIN"])),
            ("CapAdd", json!(["ALL"])),
            ("CapAdd", json!(["ſys_admin"])),
            (
                "Devices",
                json!([{"PathOnHost": "/dev/sda", "PathInContainer": "/dev/sda", "CgroupPermissions": "rwm"}]),
            ),
            ("DeviceRequests", json!([{"Driver": "nvidia", "Count": -1}])),
            ("DeviceCgroupRules", json!(["a *:* rwm"])),
            ("PidMode", json!("host")),
            ("IpcMode", json!("host")),
            ("IpcMode", json!("shareable")),
            ("UTSMode", json!("host")),
            ("UsernsMode", json!("host")),
            ("NetworkMode", json!("host")),
            ("NetworkMode", json!("bridge")),
            ("NetworkMode", json!("default")),
            ("NetworkMode", json!("")),
            ("SecurityOpt", json!(["seccomp=unconfined"])),
            ("SecurityOpt", json!(["apparmor=unconfined"])),
            ("SecurityOpt", json!(["label=disable"])),
            ("MaskedPaths", json!([])),
            ("ReadonlyPaths", json!([])),
            ("Runtime", json!("runc")),
            ("CgroupParent", json!("/")),
            ("Cgroup", json!("host")),
            ("VolumesFrom", json!(["chelabase-b-db-1"])),
            ("VolumeDriver", json!("local")),
            ("Links", json!(["x:y"])),
            ("OomKillDisable", json!(true)),
            ("Isolation", json!("hyperv")),
            ("Sysctls", json!({"kernel.shm_rmid_forced": "1"})),
            ("PublishAllPorts", json!(true)),
            (
                "PortBindings",
                json!({"80/tcp": [{"HostIp": "", "HostPort": "80"}]}),
            ),
            ("GroupAdd", json!(["docker"])),
            ("FrobnicateHost", json!(1)),
        ];
        for (key, value) in cases {
            let err = create(&runner, &with_host(key, value.clone()))
                .expect_err(&format!("{key}: {value}"));
            assert!(err.0.contains(key), "{key}: {err}");
        }
        // Top-level fields outside the allowlist.
        for (key, value) in [
            ("Volumes", json!({"/data": {}})),
            ("NetworkDisabled", json!(true)),
            ("Shell", json!(["/bin/sh"])),
            ("Domainname", json!("x")),
        ] {
            let mut b = base();
            b[key] = value;
            let err = create(&runner, &b).expect_err(key);
            assert!(err.0.contains(key), "{key}: {err}");
        }
        // The profile's capability, spelled any way Docker normalises it, passes.
        for cap in ["NET_ADMIN", "CAP_NET_ADMIN", "net_admin"] {
            create(&runner, &with_host("CapAdd", json!([cap]))).unwrap();
        }
        // The worker may add none.
        let mut b = worker_create();
        b["HostConfig"]["CapAdd"] = json!(["NET_ADMIN"]);
        assert!(create(&preset(Preset::Worker), &b).is_err());
        // HostConfig itself is required (an absent NetworkMode is the default bridge).
        assert!(create(&runner, &json!({"Image": "busybox:1"})).is_err());
        assert!(create(&runner, &json!({"Image": "busybox:1", "HostConfig": {}})).is_err());
    }

    fn mount(m: Value) -> Value {
        with_host("Mounts", json!([m]))
    }

    #[test]
    fn bind_sources() {
        let runner = preset(Preset::Runner);
        let bind = |src: &str| mount(json!({"Type": "bind", "Source": src, "Target": "/tls"}));
        create(&runner, &bind(LISTED)).unwrap();
        create(
            &runner,
            &mount(
                json!({"Type": "bind", "Source": LISTED, "Target": "/tls", "ReadOnly": true,
                          "BindOptions": {"Propagation": "rprivate"}}),
            ),
        )
        .unwrap();
        for src in [
            "/",
            "/var/run/docker.sock",
            "/run/docker.sock",
            "/home/dev/chelabase/.chelabase/stacks/other",
            "/home/dev/chelabase/.chelabase/stacks/feat-x",
            "/home/dev/chelabase/.chelabase/stacks/feat-x/../feat-x/grpc-tls",
            "/home/dev/chelabase/.chelabase/stacks/feat-x/grpc-tls/",
            "/home/dev/chelabase/.chelabase/stacks/feat-x//grpc-tls",
            "//home/dev/chelabase/.chelabase/stacks/feat-x/grpc-tls",
            "home/dev/chelabase/.chelabase/stacks/feat-x/grpc-tls",
            "",
        ] {
            assert!(create(&runner, &bind(src)).is_err(), "{src:?}");
        }
        for opts in [
            json!({"Propagation": "rshared"}),
            json!({"CreateMountpoint": true}),
            json!({"propagation": "rshared"}),
        ] {
            let m =
                json!({"Type": "bind", "Source": LISTED, "Target": "/tls", "BindOptions": opts});
            assert!(create(&runner, &mount(m)).is_err(), "{opts}");
        }
        // A bind's Target is required and absolute.
        assert!(create(&runner, &mount(json!({"Type": "bind", "Source": LISTED}))).is_err());
        assert!(
            create(
                &runner,
                &mount(json!({"Type": "bind", "Source": LISTED, "Target": "tls"}))
            )
            .is_err()
        );

        let binds = |b: &str| with_host("Binds", json!([b]));
        for ok in [
            format!("{LISTED}:/tls"),
            format!("{LISTED}:/tls:ro"),
            format!("{LISTED}:/tls:ro,rprivate"),
        ] {
            create(&runner, &binds(&ok)).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        for bad in [
            "/:/h".to_string(),
            "/var/run/docker.sock:/var/run/docker.sock".to_string(),
            "/var/run:/r".to_string(),
            "/var/lib/docker/volumes/chelabase-feat-x_docker-proxy/_data:/p".to_string(),
            format!("{LISTED}/:/tls"),
            format!("{LISTED}/../grpc-tls:/tls"),
            format!("{LISTED}:/tls:z"),
            format!("{LISTED}:/tls:Z"),
            format!("{LISTED}:/tls:rshared"),
            format!("{LISTED}:/tls:ro:extra"),
            format!("{LISTED}:tls"),
            LISTED.to_string(),
            "/data:ro".to_string(),
            "C:\\Users\\x:/y".to_string(),
            "c:/x:/y".to_string(),
            ":/x".to_string(),
        ] {
            assert!(create(&runner, &binds(&bad)).is_err(), "{bad}");
        }

        // Even a profile that lists the socket can't bind it.
        let mut wide = runner.clone();
        wide.binds = vec!["/var/run/docker.sock".into(), "/var/run".into()];
        assert!(create(&wide, &bind("/var/run/docker.sock")).is_err());
        assert!(create(&wide, &bind("/var/run")).is_err());
        // A profile without bind mounts can't bind a listed path either.
        let mut sr = preset(Preset::ServiceRunner);
        sr.binds = vec![LISTED.into()];
        let mut b = service_runner_create();
        b["HostConfig"]["Mounts"] = json!([{"Type": "bind", "Source": LISTED, "Target": "/tls"}]);
        assert!(create(&sr, &b).is_err());
        let mut b = service_runner_create();
        b["HostConfig"]["Binds"] = json!([format!("{LISTED}:/tls")]);
        assert!(create(&sr, &b).is_err());
    }

    #[test]
    fn foreign_volume_and_container_netns() {
        let runner = preset(Preset::Runner);
        let vol = |src: &str| mount(json!({"Type": "volume", "Source": src, "Target": "/d"}));
        create(&runner, &vol("chelabase-feat-x_data")).unwrap();
        create(
            &runner,
            &with_host("Binds", json!(["chelabase-feat-x_data:/d:ro"])),
        )
        .unwrap();
        create(
            &runner,
            &mount(
                json!({"Type": "volume", "Source": "chelabase-feat-x_data", "Target": "/d",
                          "VolumeOptions": {"NoCopy": true}}),
            ),
        )
        .unwrap();
        for src in [
            "chelabase-b_data",
            "chelabase-feat-x_docker-proxy",
            "",
            "missing",
        ] {
            assert!(create(&runner, &vol(src)).is_err(), "{src:?}");
            let b = with_host("Binds", json!([format!("{src}:/d")]));
            assert!(create(&runner, &b).is_err(), "{src:?}");
        }
        // Anonymous volumes and driver configs.
        assert!(create(&runner, &mount(json!({"Type": "volume", "Target": "/d"}))).is_err());
        assert!(create(&runner, &with_host("Binds", json!(["/d"]))).is_err());
        for opts in [
            json!({"DriverConfig": {"Name": "local", "Options": {"device": "/", "o": "bind"}}}),
            json!({"Labels": {"x": "y"}}),
            json!({"Subpath": "../.."}),
        ] {
            let m = json!({"Type": "volume", "Source": "chelabase-feat-x_data", "Target": "/d",
                           "VolumeOptions": opts});
            assert!(create(&runner, &mount(m)).is_err(), "{opts}");
        }
        // Other mount types.
        for t in ["npipe", "cluster", "image", "Bind", ""] {
            let m = json!({"Type": t, "Source": LISTED, "Target": "/d"});
            assert!(create(&runner, &mount(m)).is_err(), "{t}");
        }
        // The worker may mount nothing.
        let mut b = worker_create();
        b["HostConfig"]["Mounts"] =
            json!([{"Type": "volume", "Source": "chelabase-feat-x_data", "Target": "/d"}]);
        assert!(create(&preset(Preset::Worker), &b).is_err());

        // `container:` network mode.
        let netns = |r: &str| with_host("NetworkMode", json!(format!("container:{r}")));
        create(&runner, &netns("chelabase-feat-x-worker-1")).unwrap();
        create(&runner, &netns("c1")).unwrap();
        for r in ["chelabase-b-worker-1", "", "c9"] {
            assert!(create(&runner, &netns(r)).is_err(), "{r:?}");
        }
        let mut no_flag = runner.clone();
        no_flag.container_network_mode = false;
        assert!(create(&no_flag, &netns("chelabase-feat-x-worker-1")).is_err());
        // A network the profile doesn't list but that resolves in scope passes; a foreign one
        // doesn't.
        create(
            &runner,
            &with_host("NetworkMode", json!("chelabase-feat-x-svc-abc")),
        )
        .unwrap();
        create(&runner, &with_host("NetworkMode", json!("none"))).unwrap();
        assert!(
            create(
                &runner,
                &with_host("NetworkMode", json!("chelabase-b_compute"))
            )
            .is_err()
        );
        assert!(create(&runner, &with_host("NetworkMode", json!("Container:c1"))).is_err());
    }

    #[test]
    fn exec_privileged() {
        let ok = json!({"Cmd": ["true"], "Privileged": false});
        validate_exec_create(ok.to_string().as_bytes()).unwrap();
        validate_exec_create(br#"{"Cmd":["true"],"Tty":true,"DetachKeys":"ctrl-p"}"#).unwrap();
        for bad in [
            r#"{"Cmd":["true"],"Privileged":true}"#,
            r#"{"Cmd":["true"],"privileged":true}"#,
            r#"{"Cmd":["true"],"Privileged":false,"Privileged":true}"#,
            r#"{"Cmd":["true"],"Privileged":"true"}"#,
            r#"{"Cmd":["true"],"Privileged":1}"#,
            r#"{"Cmd":["true"],"ConsoleSize":[0,0]}"#,
            r#"{"Cmd":"true"}"#,
            r#"[]"#,
        ] {
            assert!(validate_exec_create(bad.as_bytes()).is_err(), "{bad}");
        }
    }

    fn network_create(stack: &Stack, body: &Value) -> Result<Value, Deny> {
        with_scope(stack, |s| {
            validate_network_create(body.to_string().as_bytes(), s)
        })
    }

    #[test]
    fn network_driver_and_config_from() {
        let s = stack();
        let net = |extra: Value| {
            let mut b = json!({"Name": "chelabase-feat-x_n"});
            for (k, v) in extra.as_object().unwrap() {
                b[k] = v.clone();
            }
            b
        };
        for ok in [
            net(json!({})),
            net(json!({"Driver": "bridge"})),
            net(json!({"Internal": true, "Attachable": false, "EnableIPv6": false})),
            net(json!({"IPAM": {"Driver": "default"}})),
            net(json!({"Ingress": false, "ConfigOnly": false, "Options": null})),
        ] {
            let out = network_create(&s, &ok).unwrap_or_else(|e| panic!("{ok}: {e}"));
            assert_stack_label(&out);
        }
        for bad in [
            net(json!({"Driver": "host"})),
            net(json!({"Driver": "macvlan"})),
            net(json!({"Driver": "ipvlan"})),
            net(json!({"Driver": "overlay"})),
            net(json!({"Driver": ""})),
            net(json!({"ConfigFrom": {"Network": "chelabase-b_compute"}})),
            net(json!({"ConfigOnly": true})),
            net(json!({"Ingress": true})),
            net(json!({"Scope": "swarm"})),
            net(json!({"Attachable": true})),
            net(json!({"IPAM": {"Driver": "custom"}})),
            net(json!({"IPAM": {"Config": [{"Subnet": "192.168.1.0/24"}]}})),
            net(json!({"IPAM": {"Options": {"x": "y"}}})),
            net(json!({"Options": {"com.docker.network.bridge.name": "docker0"}})),
            net(json!({"Options": {}})),
            net(json!({"Labels": {"chelabase.dev/stack": "b"}})),
            net(json!({"driver": "macvlan"})),
        ] {
            assert!(network_create(&s, &bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn go_decoder_tricks() {
        let runner = preset(Preset::Runner);
        let net = format!(r#""HostConfig":{{"NetworkMode":"{NET}"}}"#);
        let host = |extra: &str| {
            format!(r#"{{"Image":"x","HostConfig":{{"NetworkMode":"{NET}",{extra}}}}}"#)
        };
        let cases = [
            // Case-folded and Unicode-folded keys.
            format!(r#"{{"Image":"x","hostconfig":{{"Privileged":true}},{net}}}"#),
            format!(r#"{{"Image":"x","HoſtConfig":{{"Privileged":true}},{net}}}"#),
            format!("{{\"Image\":\"x\",\"Ho\\u017ftConfig\":{{\"Privileged\":true}},{net}}}"),
            host("\"Lin\u{212a}s\":[\"x:y\"]"),
            host(r#""privileged":true"#),
            // Escapes decode before the check: this is plain Privileged.
            host(r#""Privil\u0065ged":true"#),
            // Duplicates at the top level, in HostConfig and in a map.
            format!(
                r#"{{"Image":"x","Labels":{{"chelabase.dev/stack":"feat-x"}},"Labels":{{"chelabase.dev/stack":"b"}},{net}}}"#
            ),
            format!(
                r#"{{"Image":"x",{net},"HostConfig":{{"NetworkMode":"{NET}","Privileged":true}}}}"#
            ),
            format!(r#"{{"Image":"x","Labels":{{"a":"1","a":"2"}},{net}}}"#),
            host(r#""Memory":1,"Memory":2"#),
            // A duplicate spelled with an escape is still a duplicate once decoded.
            host(r#""Memory":1,"Memor\u0079":2"#),
            // Case variants inside nested structs.
            host(
                r#""Mounts":[{"Type":"volume","Source":"chelabase-feat-x_data","Target":"/d","volumeOptions":{"DriverConfig":{"Name":"x"}}}]"#,
            ),
            host(&format!(
                r#""Mounts":[{{"Type":"bind","Source":"{LISTED}","Target":"/t","BindOptions":{{"propagation":"rshared"}}}}]"#
            )),
            host(r#""Mounts":[{"Type":"tmpfs","Target":"/t","target":"/etc"}]"#),
            host(r#""RestartPolicy":{"Name":"no","name":"always"}"#),
            host(r#""LogConfig":{"Type":"json-file","type":"gelf"}"#),
            host(r#""LogConfig":{"Type":"json-file","Config":{"max-size":"1m","max-size":"2m"}}"#),
            format!(
                r#"{{"Image":"x",{net},"NetworkingConfig":{{"EndpointsConfig":{{"{NET}":{{"networkid":"n9"}}}}}}}}"#
            ),
            format!(
                r#"{{"Image":"x",{net},"NetworkingConfig":{{"endpointsconfig":{{"chelabase-b_compute":{{}}}}}}}}"#
            ),
            // Trailing data, a second document.
            format!(r#"{{"Image":"x",{net}}} {{"HostConfig":{{"Privileged":true}}}}"#),
        ];
        // The base form passes, so each refusal is the trick's.
        create_raw(&runner, &format!(r#"{{"Image":"x",{net}}}"#)).unwrap();
        for raw in &cases {
            assert!(create_raw(&runner, raw).is_err(), "{raw}");
        }
        assert!(parse_strict(br#"{"a":{"b":1,"b":1}}"#).is_err());
        assert!(parse_strict(br#"[{"a":1,"a":1}]"#).is_err());
        parse_strict(br#"{"a":{"b":1},"A":{"b":1}}"#).unwrap();
    }

    #[test]
    fn empty_present_refused() {
        let runner = preset(Preset::Runner);
        for (key, empty) in [
            ("MaskedPaths", json!([])),
            ("ReadonlyPaths", json!([])),
            ("Sysctls", json!({})),
            ("Devices", json!([])),
            ("Runtime", json!("")),
            ("ShmSize", json!(0)),
        ] {
            assert!(
                create(&runner, &with_host(key, empty.clone())).is_err(),
                "{key}"
            );
            let out = create(&runner, &with_host(key, Value::Null)).unwrap();
            assert!(out["HostConfig"].get(key).is_none(), "{key} is dropped");
        }
        // Keys outside the allowlist set to false are dropped, not forwarded.
        let out = create(&runner, &with_host("Privileged", json!(false))).unwrap();
        assert!(out["HostConfig"].get("Privileged").is_none());
        let mut b = base();
        b["hostconfig"] = Value::Null;
        let out = create(&runner, &b).unwrap();
        assert!(out.get("hostconfig").is_none());
    }

    #[test]
    fn cgroupns_forced_private() {
        let runner = preset(Preset::Runner);
        for b in [
            base(),
            with_host("CgroupnsMode", json!("private")),
            with_host("CgroupnsMode", json!("")),
            with_host("CgroupnsMode", Value::Null),
        ] {
            let out = create(&runner, &b).unwrap();
            assert_eq!(out["HostConfig"]["CgroupnsMode"], "private", "{b}");
        }
        for v in ["host", "Host", "PRIVATE"] {
            let err = create(&runner, &with_host("CgroupnsMode", json!(v))).unwrap_err();
            assert!(err.0.contains("CgroupnsMode"), "{err}");
        }
    }

    #[test]
    fn log_driver_allowlist() {
        let runner = preset(Preset::Runner);
        let log = |v: Value| with_host("LogConfig", v);
        for t in ["json-file", "local"] {
            create(&runner, &log(json!({"Type": t}))).unwrap();
            create(
                &runner,
                &log(json!({"Type": t, "Config": {"max-size": "1m", "max-file": "2"}})),
            )
            .unwrap();
        }
        for t in [
            "gelf",
            "syslog",
            "fluentd",
            "awslogs",
            "splunk",
            "journald",
            "none",
            "",
            "JSON-FILE",
        ] {
            let err = create(&runner, &log(json!({"Type": t}))).unwrap_err();
            assert!(err.0.contains("LogConfig"), "{t}: {err}");
        }
        for bad in [
            json!({}),
            json!({"Type": "json-file", "Config": {"gelf-address": "udp://10.0.0.1:12201"}}),
            json!({"Type": "json-file", "Config": {"max-size": 1}}),
            json!({"Type": "json-file", "Config": {"Max-Size": "1m"}}),
        ] {
            assert!(create(&runner, &log(bad.clone())).is_err(), "{bad}");
        }
    }

    #[test]
    fn create_name_prefixes() {
        let a = Stack::new("a", "chelabase-a").unwrap();
        let volume = |stack: &Stack, body: Value| {
            with_scope(stack, |s| {
                validate_volume_create(body.to_string().as_bytes(), s)
            })
        };
        for bad in [
            json!({"Name": "chelabase-b_x"}),
            json!({"Name": "chelabase-b-docker-proxy"}),
            json!({"Name": "chelabase-a-b_x"}),
            json!({"Name": "chelabase-a"}),
            json!({}),
            json!({"Name": ""}),
            json!({"Name": "chelabase-a_x", "Driver": "nfs"}),
            json!({"Name": "chelabase-a_x", "DriverOpts": {"device": "/", "o": "bind"}}),
            json!({"Name": "chelabase-a_x", "DriverOpts": {}}),
            json!({"Name": "chelabase-a_x/../y"}),
        ] {
            assert!(volume(&a, bad.clone()).is_err(), "{bad}");
        }
        for ok in [
            json!({"Name": "chelabase-a_x"}),
            json!({"Name": "chelabase-a_docker-proxy", "Driver": "local", "DriverOpts": null}),
        ] {
            let out = volume(&a, ok.clone()).unwrap_or_else(|e| panic!("{ok}: {e}"));
            assert_eq!(out["Labels"][STACK_LABEL], "a");
        }

        for bad in ["chelabase-b_x", "chelabase-b-x", "chelabase-a", "x", ""] {
            assert!(network_create(&a, &json!({"Name": bad})).is_err(), "{bad}");
        }
        assert!(network_create(&a, &json!({})).is_err());
        for ok in ["chelabase-a_x", "chelabase-a-svc-1-2"] {
            network_create(&a, &json!({"Name": ok})).unwrap();
        }

        let runner = preset(Preset::Runner);
        let body = json!({"Image": "busybox:1", "HostConfig": {"NetworkMode": "none"}}).to_string();
        let named = |stack: &Stack, name: Option<&str>| {
            with_scope(stack, |s| {
                validate_container_create(body.as_bytes(), name, &runner, s)
            })
        };
        for bad in [
            "chelabase-b-worker",
            "/chelabase-b-worker",
            "chelabase-a",
            "chelabase-ab-x",
            "chelabase-a-w/x",
            "//chelabase-a-w",
            "worker",
        ] {
            assert!(named(&a, Some(bad)).is_err(), "{bad}");
        }
        for ok in [
            None,
            Some(""),
            Some("chelabase-a-worker"),
            Some("/chelabase-a-worker"),
        ] {
            named(&a, ok).unwrap_or_else(|e| panic!("{ok:?}: {e}"));
        }
        // The main stack's project is `chelabase`: Compose names its objects `chelabase_x` and
        // `chelabase-x-1`.
        let main = Stack::new("main", "chelabase").unwrap();
        volume(&main, json!({"Name": "chelabase_x"})).unwrap();
        assert!(volume(&main, json!({"Name": "chelabase-b_x"})).is_err());
        network_create(&main, &json!({"Name": "chelabase_default"})).unwrap();
        assert!(network_create(&main, &json!({"Name": "chelabase-b_default"})).is_err());
        named(&main, Some("chelabase-main-compute-ep")).unwrap();
        named(&main, Some("chelabase-orchestrator-1")).unwrap();
    }

    #[test]
    fn endpoint_networkid_mismatch() {
        let runner = preset(Preset::Runner);
        let ep = |key: &str, settings: Value| {
            let mut b = base();
            b["NetworkingConfig"] = json!({"EndpointsConfig": {key: settings}});
            b
        };
        for ok in [
            ep(NET, json!({})),
            ep(NET, json!({"NetworkID": "net-id-1"})),
            ep(NET, json!({"NetworkID": NET})),
            ep(NET, json!({"Aliases": ["db"], "GwPriority": 1})),
            ep("chelabase-feat-x-svc-abc", json!({})),
            ep(NET, Value::Null),
        ] {
            create(&runner, &ok).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        for bad in [
            // Docker prefers NetworkID over the key for a user-defined network.
            ep(NET, json!({"NetworkID": "n9"})),
            ep(NET, json!({"NetworkID": "chelabase-feat-x_compute"})),
            ep("chelabase-b_compute", json!({})),
            ep("bridge", json!({})),
            ep("host", json!({})),
            ep(NET, json!({"IPAMConfig": {"IPv4Address": "10.0.0.5"}})),
            ep(NET, json!({"MacAddress": "02:42:ac:11:00:02"})),
            ep(NET, json!({"DriverOpts": {"x": "y"}})),
            ep(NET, json!({"Links": ["a:b"]})),
            ep(NET, json!({"aliases": ["db"]})),
        ] {
            assert!(create(&runner, &bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn network_connect_bodies() {
        use OpKind::{NetworkConnect as C, NetworkDisconnect as D};
        let c = validate_network_connect(
            C,
            br#"{"Container":"x","EndpointConfig":{"Aliases":["a"],"GwPriority":-1}}"#,
        )
        .unwrap();
        assert_eq!(c.container, "x");
        validate_network_connect(D, br#"{"Container":"x"}"#).unwrap();
        for (kind, bad) in [
            (C, r#"{}"#),
            (C, r#"{"Container":""}"#),
            (C, r#"{"Container":1}"#),
            (C, r#"{"Container":"x","Container":"y"}"#),
            (C, r#"{"Container":"x","container":"y"}"#),
            (C, r#"{"Container":"x","Force":true}"#),
            (
                C,
                r#"{"Container":"x","EndpointConfig":{"IPAMConfig":{"IPv4Address":"10.0.0.5"}}}"#,
            ),
            (
                C,
                r#"{"Container":"x","EndpointConfig":{"NetworkID":"n9"}}"#,
            ),
            (C, r#"{"Container":"x","EndpointConfig":{"Links":["a:b"]}}"#),
            (C, r#"{"Container":"x","EndpointConfig":{"DriverOpts":{}}}"#),
            (C, r#"{"Container":"x","EndpointConfig":{"aliases":["a"]}}"#),
            (C, r#"{"Container":"x","EndpointConfig":[]}"#),
            (D, r#"{"Container":"x","EndpointConfig":{}}"#),
            (D, r#"{"Container":"x","Force":"yes"}"#),
            (C, r#"{"Container":"x"} trailing"#),
            (OpKind::ContainerCreate, r#"{"Container":"x"}"#),
        ] {
            assert!(
                validate_network_connect(kind, bad.as_bytes()).is_err(),
                "{kind:?} {bad}"
            );
        }
    }

    const ESCAPES: &[&str] = &[
        "Privileged",
        "CapAdd",
        "Devices",
        "DeviceRequests",
        "PidMode",
        "IpcMode",
        "UTSMode",
        "UsernsMode",
        "CgroupParent",
        "Runtime",
        "Sysctls",
        "MaskedPaths",
        "ReadonlyPaths",
        "VolumesFrom",
        "VolumeDriver",
        "Links",
        "OomKillDisable",
        "Isolation",
        "PublishAllPorts",
        "Cgroup",
        "DeviceCgroupRules",
        "GroupAdd",
        "ShmSize",
        "StorageOpt",
        "Dns",
        "Volumes",
    ];

    /// An allowlisted key spelled another way: ASCII case flips, `ſ` for `s`, `K` for `k`.
    fn variant(key: &str, seed: u64) -> String {
        let mut out: String = key
            .chars()
            .enumerate()
            .map(|(i, c)| {
                if seed >> (i % 64) & 1 == 1 {
                    c.to_ascii_lowercase()
                } else {
                    c.to_ascii_uppercase()
                }
            })
            .collect();
        if seed.is_multiple_of(3) {
            out = out.replacen(['s', 'S'], "ſ", 1);
        } else if seed % 3 == 1 {
            out = out.replacen(['k', 'K'], "\u{212a}", 1);
        }
        out
    }

    fn any_key(allowed: &'static [&'static str]) -> impl Strategy<Value = String> {
        prop_oneof![
            "[A-Za-z]{1,20}",
            proptest::sample::select(ESCAPES).prop_map(String::from),
            (proptest::sample::select(allowed), any::<u64>()).prop_map(|(k, s)| variant(k, s)),
            proptest::sample::select(allowed).prop_map(|k| format!("{k} ")),
        ]
    }

    /// Anything Docker could read as set: never `null` or `false`.
    fn non_default() -> impl Strategy<Value = Value> {
        prop_oneof![
            Just(json!(true)),
            Just(json!(0)),
            any::<i64>().prop_map(|n| json!(n)),
            Just(json!("")),
            "[a-z/:=]{1,12}".prop_map(|s| json!(s)),
            Just(json!([])),
            Just(json!({})),
            Just(json!(["x"])),
            Just(json!({"a": "b"})),
            Just(json!([{"a": 1}])),
        ]
    }

    fn names<K: Sync>(fields: &'static [(&'static str, K)]) -> &'static [&'static str] {
        let v: Vec<&'static str> = fields.iter().map(|(k, _)| *k).collect();
        Box::leak(v.into_boxed_slice())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2048))]

        /// The fail-closed invariant: a key that isn't on the allowlist, with any value but
        /// `null` or `false`, is refused, at the top level, in HostConfig and in a mount.
        #[test]
        fn unlisted_keys_refused(
            level in 0..3usize,
            (key, value) in (0..3usize).prop_flat_map(|l| {
                let allowed = [names(CONTAINER_FIELDS), names(HOST_FIELDS), names(MOUNT_FIELDS)][l];
                (any_key(allowed), non_default())
            }),
        ) {
            let runner = preset(Preset::Runner);
            let (allowed, mut body) = match level {
                0 => (names(CONTAINER_FIELDS), base()),
                1 => (names(HOST_FIELDS), base()),
                _ => (names(MOUNT_FIELDS), mount(json!({"Type": "tmpfs", "Target": "/t"}))),
            };
            let obj = match level {
                0 => body.as_object_mut().unwrap(),
                1 => body["HostConfig"].as_object_mut().unwrap(),
                _ => body["HostConfig"]["Mounts"][0].as_object_mut().unwrap(),
            };
            obj.insert(key.clone(), value.clone());
            if !allowed.contains(&key.as_str()) {
                prop_assert!(create(&runner, &body).is_err(), "{key:?}: {value} passed");
            }
        }
    }
}
