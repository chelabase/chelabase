//! Request classification: method, path and query → one [`Op`], or [`Deny`].
//!
//! The grammar is a closed enum. Anything the match below doesn't name is denied: build,
//! commit, export, update, rename, prune, image delete, push, load, tag, system/df, plugins,
//! swarm, services, tasks, secrets, configs, nodes, session and distribution included.
//!
//! The engine's Go router decodes the path once and matches it against `/v{version}/…` or the
//! unversioned form. This module does the same, so both see the same segments:
//!
//! - the raw path is split on `/` and each segment is percent-decoded once;
//! - an empty segment (`//`, a trailing `/`), `.`, `..`, or a segment that holds `/` (`%2F`)
//!   or anything outside a conservative object-name charset after decoding is denied;
//! - a first segment of the form `v[0-9.]+` is an API version; only `v1.<minor>` with
//!   `minor >= 44` is accepted. Older versions apply unsafe defaults the body checks can't see.

use std::collections::HashSet;

use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};

use crate::Deny;

/// The oldest API version accepted (`/v1.44/…`). The host runs Docker 29 (API 1.56).
pub const MIN_API_MINOR: u32 = 44;

/// Local images a pull must never replace: the workbench builds them on the host, and a
/// same-named tag from a registry would overwrite the local build for every stack.
pub const RESERVED_REPOSITORIES: [&str; 3] = [
    "chelabase-workbench",
    "chelabase/functions-runtime",
    "chelabase-docker-proxy",
];

/// One allowed Docker API call. Object references are the path's (decoded) segment: a name,
/// short or full ID, resolved and scope-checked by the server before it forwards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// `GET|HEAD /_ping`
    Ping,
    /// `GET /version`
    Version,
    /// `GET /info`
    Info,
    /// `GET /events` (post-filtered by actor labels).
    Events,
    /// `GET /containers/json`
    ContainerList,
    /// `POST /containers/create`
    ContainerCreate,
    /// `GET /containers/{id}/json`
    ContainerInspect(String),
    /// `POST /containers/{id}/start`
    ContainerStart(String),
    /// `POST /containers/{id}/stop`
    ContainerStop(String),
    /// `POST /containers/{id}/kill`
    ContainerKill(String),
    /// `POST /containers/{id}/wait`
    ContainerWait(String),
    /// `DELETE /containers/{id}`
    ContainerDelete(String),
    /// `GET /containers/{id}/logs`
    ContainerLogs(String),
    /// `POST /containers/{id}/attach`
    ContainerAttach(String),
    /// `POST /containers/{id}/resize`
    ContainerResize(String),
    /// `GET /containers/{id}/archive`
    ArchiveGet(String),
    /// `PUT /containers/{id}/archive`
    ArchivePut(String),
    /// `HEAD /containers/{id}/archive`
    ArchiveHead(String),
    /// `POST /containers/{id}/exec` (the container).
    ExecCreate(String),
    /// `POST /exec/{id}/start` (the exec).
    ExecStart(String),
    /// `GET /exec/{id}/json`
    ExecInspect(String),
    /// `POST /exec/{id}/resize`
    ExecResize(String),
    /// `POST /images/create`, a registry pull only (never an import).
    ImageCreate(ImagePull),
    /// `GET /images/{ref}/json`; the reference may span segments (`docker.io/library/x:1`).
    ImageInspect(String),
    /// `GET /networks`
    NetworkList,
    /// `GET /networks/{id}`
    NetworkInspect(String),
    /// `POST /networks/create`
    NetworkCreate,
    /// `POST /networks/{id}/connect`
    NetworkConnect(String),
    /// `POST /networks/{id}/disconnect`
    NetworkDisconnect(String),
    /// `DELETE /networks/{id}`
    NetworkDelete(String),
    /// `GET /volumes`
    VolumeList,
    /// `GET /volumes/{name}`
    VolumeInspect(String),
    /// `POST /volumes/create`
    VolumeCreate,
}

/// [`Op`] without its target: what a [`Profile`](crate::Profile) lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpKind {
    Ping,
    Version,
    Info,
    Events,
    ContainerList,
    ContainerCreate,
    ContainerInspect,
    ContainerStart,
    ContainerStop,
    ContainerKill,
    ContainerWait,
    ContainerDelete,
    ContainerLogs,
    ContainerAttach,
    ContainerResize,
    ArchiveGet,
    ArchivePut,
    ArchiveHead,
    ExecCreate,
    ExecStart,
    ExecInspect,
    ExecResize,
    ImageCreate,
    ImageInspect,
    NetworkList,
    NetworkInspect,
    NetworkCreate,
    NetworkConnect,
    NetworkDisconnect,
    NetworkDelete,
    VolumeList,
    VolumeInspect,
    VolumeCreate,
}

impl OpKind {
    /// Every kind, in declaration order.
    pub const ALL: [OpKind; 33] = [
        OpKind::Ping,
        OpKind::Version,
        OpKind::Info,
        OpKind::Events,
        OpKind::ContainerList,
        OpKind::ContainerCreate,
        OpKind::ContainerInspect,
        OpKind::ContainerStart,
        OpKind::ContainerStop,
        OpKind::ContainerKill,
        OpKind::ContainerWait,
        OpKind::ContainerDelete,
        OpKind::ContainerLogs,
        OpKind::ContainerAttach,
        OpKind::ContainerResize,
        OpKind::ArchiveGet,
        OpKind::ArchivePut,
        OpKind::ArchiveHead,
        OpKind::ExecCreate,
        OpKind::ExecStart,
        OpKind::ExecInspect,
        OpKind::ExecResize,
        OpKind::ImageCreate,
        OpKind::ImageInspect,
        OpKind::NetworkList,
        OpKind::NetworkInspect,
        OpKind::NetworkCreate,
        OpKind::NetworkConnect,
        OpKind::NetworkDisconnect,
        OpKind::NetworkDelete,
        OpKind::VolumeList,
        OpKind::VolumeInspect,
        OpKind::VolumeCreate,
    ];

    /// Whether the call only reads (changes no object and runs nothing).
    pub fn is_read(self) -> bool {
        matches!(
            self,
            OpKind::Ping
                | OpKind::Version
                | OpKind::Info
                | OpKind::Events
                | OpKind::ContainerList
                | OpKind::ContainerInspect
                | OpKind::ContainerLogs
                | OpKind::ArchiveGet
                | OpKind::ArchiveHead
                | OpKind::ExecInspect
                | OpKind::ImageInspect
                | OpKind::NetworkList
                | OpKind::NetworkInspect
                | OpKind::VolumeList
                | OpKind::VolumeInspect
        )
    }
}

impl Op {
    pub fn kind(&self) -> OpKind {
        match self {
            Op::Ping => OpKind::Ping,
            Op::Version => OpKind::Version,
            Op::Info => OpKind::Info,
            Op::Events => OpKind::Events,
            Op::ContainerList => OpKind::ContainerList,
            Op::ContainerCreate => OpKind::ContainerCreate,
            Op::ContainerInspect(_) => OpKind::ContainerInspect,
            Op::ContainerStart(_) => OpKind::ContainerStart,
            Op::ContainerStop(_) => OpKind::ContainerStop,
            Op::ContainerKill(_) => OpKind::ContainerKill,
            Op::ContainerWait(_) => OpKind::ContainerWait,
            Op::ContainerDelete(_) => OpKind::ContainerDelete,
            Op::ContainerLogs(_) => OpKind::ContainerLogs,
            Op::ContainerAttach(_) => OpKind::ContainerAttach,
            Op::ContainerResize(_) => OpKind::ContainerResize,
            Op::ArchiveGet(_) => OpKind::ArchiveGet,
            Op::ArchivePut(_) => OpKind::ArchivePut,
            Op::ArchiveHead(_) => OpKind::ArchiveHead,
            Op::ExecCreate(_) => OpKind::ExecCreate,
            Op::ExecStart(_) => OpKind::ExecStart,
            Op::ExecInspect(_) => OpKind::ExecInspect,
            Op::ExecResize(_) => OpKind::ExecResize,
            Op::ImageCreate(_) => OpKind::ImageCreate,
            Op::ImageInspect(_) => OpKind::ImageInspect,
            Op::NetworkList => OpKind::NetworkList,
            Op::NetworkInspect(_) => OpKind::NetworkInspect,
            Op::NetworkCreate => OpKind::NetworkCreate,
            Op::NetworkConnect(_) => OpKind::NetworkConnect,
            Op::NetworkDisconnect(_) => OpKind::NetworkDisconnect,
            Op::NetworkDelete(_) => OpKind::NetworkDelete,
            Op::VolumeList => OpKind::VolumeList,
            Op::VolumeInspect(_) => OpKind::VolumeInspect,
            Op::VolumeCreate => OpKind::VolumeCreate,
        }
    }

    /// The object the path names (container, exec, image, network or volume), if any.
    pub fn target(&self) -> Option<&str> {
        match self {
            Op::ContainerInspect(t)
            | Op::ContainerStart(t)
            | Op::ContainerStop(t)
            | Op::ContainerKill(t)
            | Op::ContainerWait(t)
            | Op::ContainerDelete(t)
            | Op::ContainerLogs(t)
            | Op::ContainerAttach(t)
            | Op::ContainerResize(t)
            | Op::ArchiveGet(t)
            | Op::ArchivePut(t)
            | Op::ArchiveHead(t)
            | Op::ExecCreate(t)
            | Op::ExecStart(t)
            | Op::ExecInspect(t)
            | Op::ExecResize(t)
            | Op::ImageInspect(t)
            | Op::NetworkInspect(t)
            | Op::NetworkConnect(t)
            | Op::NetworkDisconnect(t)
            | Op::NetworkDelete(t)
            | Op::VolumeInspect(t) => Some(t),
            _ => None,
        }
    }
}

/// A registry pull (`POST /images/create?fromImage=…[&tag=…][&platform=…]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImagePull {
    /// `fromImage` with `tag` folded in, normalised.
    pub image: ImageRef,
    pub platform: Option<String>,
}

/// A normalised image reference, as the engine resolves it (`nginx` →
/// `docker.io/library/nginx`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    /// The registry host, lower-cased (`docker.io` for Docker Hub).
    pub domain: String,
    /// The repository path (`library/nginx`).
    pub path: String,
    pub tag: Option<String>,
    /// `sha256:<hex>`.
    pub digest: Option<String>,
}

impl ImageRef {
    /// Parses and normalises a reference the way the engine does. Anything the reference
    /// grammar doesn't allow, or that is ambiguous, is denied.
    pub fn parse(raw: &str) -> Result<Self, Deny> {
        let bad = || Deny::new(format!("image reference {raw:?} is not allowed"));
        if raw.is_empty() || raw.len() > 512 {
            return Err(bad());
        }
        let (name, digest) = match raw.split_once('@') {
            Some((name, digest)) => {
                if !is_digest(digest) {
                    return Err(bad());
                }
                (name, Some(digest.to_string()))
            }
            None => (raw, None),
        };
        // A tag is the part after the last `:` that follows the last `/`.
        let last_slash = name.rfind('/').map_or(0, |i| i + 1);
        let (name, tag) = match name[last_slash..].rfind(':') {
            Some(i) => {
                let (n, t) = name.split_at(last_slash + i);
                let t = &t[1..];
                if !is_tag(t) {
                    return Err(bad());
                }
                (n, Some(t.to_string()))
            }
            None => (name, None),
        };
        let mut parts: Vec<&str> = name.split('/').collect();
        // The engine's rule: the first component is a registry host when there is more than
        // one and it has a `.` or `:`, or is `localhost`. One with upper case but neither is
        // also a host to the engine; that spelling is ambiguous and refused.
        let first = parts[0];
        let domain = if parts.len() > 1 && (first.contains(['.', ':']) || first == "localhost") {
            if !is_domain(first) {
                return Err(bad());
            }
            parts.remove(0);
            match first.to_ascii_lowercase().as_str() {
                "index.docker.io" => "docker.io".to_string(),
                d => d.to_string(),
            }
        } else {
            "docker.io".to_string()
        };
        if parts.is_empty() || !parts.iter().all(|p| is_path_component(p)) {
            return Err(bad());
        }
        let mut path = parts.join("/");
        if domain == "docker.io" && parts.len() == 1 {
            path = format!("library/{path}");
        }
        Ok(Self {
            domain,
            path,
            tag,
            digest,
        })
    }

    /// `domain/path`, the local repository a pull writes.
    pub fn repository(&self) -> String {
        format!("{}/{}", self.domain, self.path)
    }
}

/// Classifies one request. `path` is the raw request path (no query); `query` the raw query
/// string without `?`.
///
/// Only [`Op::ImageCreate`] reads the query here (it decides what the call does); the server
/// rebuilds every other op's query from its own allowlist.
pub fn classify(method: &str, path: &str, query: Option<&str>) -> Result<Op, Deny> {
    let deny = || Deny::new(format!("{method} {path} is not an allowed Docker API call"));
    let decoded = segments(path)?;
    let mut s: Vec<&str> = decoded.iter().map(String::as_str).collect();
    if let Some(first) = s.first()
        && is_version_like(first)
    {
        check_version(first)?;
        s.remove(0);
    }
    let id = |seg: &str| -> Result<String, Deny> {
        if is_object_name(seg) {
            Ok(seg.to_string())
        } else {
            Err(deny())
        }
    };
    let op = match (method, s.as_slice()) {
        ("GET" | "HEAD", ["_ping"]) => Op::Ping,
        ("GET", ["version"]) => Op::Version,
        ("GET", ["info"]) => Op::Info,
        ("GET", ["events"]) => Op::Events,
        ("GET", ["containers", "json"]) => Op::ContainerList,
        ("POST", ["containers", "create"]) => Op::ContainerCreate,
        ("GET", ["containers", c, "json"]) => Op::ContainerInspect(id(c)?),
        ("GET", ["containers", c, "logs"]) => Op::ContainerLogs(id(c)?),
        ("GET", ["containers", c, "archive"]) => Op::ArchiveGet(id(c)?),
        ("PUT", ["containers", c, "archive"]) => Op::ArchivePut(id(c)?),
        ("HEAD", ["containers", c, "archive"]) => Op::ArchiveHead(id(c)?),
        ("DELETE", ["containers", c]) => Op::ContainerDelete(id(c)?),
        ("POST", ["containers", c, action]) => {
            let c = id(c)?;
            match *action {
                "start" => Op::ContainerStart(c),
                "stop" => Op::ContainerStop(c),
                "kill" => Op::ContainerKill(c),
                "wait" => Op::ContainerWait(c),
                "attach" => Op::ContainerAttach(c),
                "resize" => Op::ContainerResize(c),
                "exec" => Op::ExecCreate(c),
                _ => return Err(deny()),
            }
        }
        ("POST", ["exec", e, "start"]) => Op::ExecStart(id(e)?),
        ("POST", ["exec", e, "resize"]) => Op::ExecResize(id(e)?),
        ("GET", ["exec", e, "json"]) => Op::ExecInspect(id(e)?),
        ("POST", ["images", "create"]) => Op::ImageCreate(image_pull(query)?),
        // The engine's route is `/images/{name:.*}/json`: the reference spans segments.
        ("GET", ["images", reference @ .., "json"]) if !reference.is_empty() => {
            if !reference.iter().all(|seg| is_image_segment(seg)) {
                return Err(deny());
            }
            Op::ImageInspect(reference.join("/"))
        }
        ("GET", ["networks"]) => Op::NetworkList,
        ("POST", ["networks", "create"]) => Op::NetworkCreate,
        ("GET", ["networks", n]) => Op::NetworkInspect(id(n)?),
        ("DELETE", ["networks", n]) => Op::NetworkDelete(id(n)?),
        ("POST", ["networks", n, "connect"]) => Op::NetworkConnect(id(n)?),
        ("POST", ["networks", n, "disconnect"]) => Op::NetworkDisconnect(id(n)?),
        ("GET", ["volumes"]) => Op::VolumeList,
        ("POST", ["volumes", "create"]) => Op::VolumeCreate,
        ("GET", ["volumes", v]) => Op::VolumeInspect(id(v)?),
        _ => return Err(deny()),
    };
    Ok(op)
}

/// The path's segments, each decoded once and checked. The leading `/` is required.
fn segments(path: &str) -> Result<Vec<String>, Deny> {
    let deny = |why: &str| Deny::new(format!("path {path:?}: {why}"));
    let rest = path.strip_prefix('/').ok_or_else(|| deny("not absolute"))?;
    if path.len() > 2048 {
        return Err(deny("too long"));
    }
    rest.split('/')
        .map(|raw| {
            if !valid_escapes(raw) {
                return Err(deny("malformed escape"));
            }
            let seg = percent_decode_str(raw)
                .decode_utf8()
                .map_err(|_| deny("not UTF-8"))?;
            if seg.is_empty() {
                return Err(deny("empty segment"));
            }
            if seg == "." || seg == ".." {
                return Err(deny("dot segment"));
            }
            if seg.contains('/') {
                return Err(deny("encoded slash"));
            }
            if seg.len() > 255 || !seg.chars().all(is_segment_char) {
                return Err(deny("character outside the allowed set"));
            }
            Ok(seg.into_owned())
        })
        .collect()
}

fn is_segment_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '@')
}

/// The engine's version route is `/v{version:[0-9.]+}`.
fn is_version_like(seg: &str) -> bool {
    seg.strip_prefix('v')
        .is_some_and(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit() || b == b'.'))
}

fn check_version(seg: &str) -> Result<(), Deny> {
    let deny = || {
        Deny::new(format!(
            "API version {seg} is not allowed (need v1.{MIN_API_MINOR} or newer)"
        ))
    };
    let v = &seg[1..];
    let (major, minor) = v.split_once('.').ok_or_else(deny)?;
    let digits = |s: &str| !s.is_empty() && s.len() <= 6 && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(major) || !digits(minor) {
        return Err(deny());
    }
    let major: u32 = major.parse().map_err(|_| deny())?;
    let minor: u32 = minor.parse().map_err(|_| deny())?;
    if major != 1 || minor < MIN_API_MINOR {
        return Err(deny());
    }
    Ok(())
}

/// A container, exec, network or volume name or ID.
fn is_object_name(seg: &str) -> bool {
    seg.starts_with(|c: char| c.is_ascii_alphanumeric())
        && seg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// One `/`-separated piece of an image reference or ID (`sha256:…`, `nginx:1`).
fn is_image_segment(seg: &str) -> bool {
    seg.starts_with(|c: char| c.is_ascii_alphanumeric())
}

/// Every `%` starts a two-hex-digit escape (Go refuses anything else).
fn valid_escapes(raw: &str) -> bool {
    let b = raw.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            if i + 2 >= b.len() || !(b[i + 1].is_ascii_hexdigit() && b[i + 2].is_ascii_hexdigit()) {
                return false;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    true
}

fn is_digest(s: &str) -> bool {
    s.strip_prefix("sha256:").is_some_and(|h| {
        h.len() == 64
            && h.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

fn is_tag(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

fn is_domain(s: &str) -> bool {
    let (host, port) = match s.split_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (s, None),
    };
    !host.is_empty()
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
        && port
            .is_none_or(|p| !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit()))
}

/// A repository path component: lower-case alphanumerics joined by `.`, `_` or `-`.
fn is_path_component(s: &str) -> bool {
    let alnum = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    s.starts_with(alnum)
        && s.ends_with(alnum)
        && s.chars().all(|c| alnum(c) || matches!(c, '.' | '_' | '-'))
        && !s.contains("..")
}

/// `POST /images/create`: a registry pull of one tag or digest, never an import and never a
/// reserved local repository.
///
/// The engine reads these parameters with Go's `ParseForm`, which also merges an
/// `application/x-www-form-urlencoded` request body into them. The server (m4) must forward
/// this op with an empty body and no such content type, or the query checked here isn't the
/// whole request.
fn image_pull(query: Option<&str>) -> Result<ImagePull, Deny> {
    let pairs = parse_query(query)?;
    let mut from = None;
    let mut tag = None;
    let mut platform = None;
    for (k, v) in pairs {
        match k.as_str() {
            "fromImage" => from = Some(v),
            "tag" => tag = Some(v),
            "platform" => platform = Some(v),
            // fromSrc, repo, changes, message: an import, which can overwrite any local tag.
            other => {
                return Err(Deny::new(format!(
                    "image create parameter {other:?} is not allowed"
                )));
            }
        }
    }
    let from = from
        .filter(|f| !f.is_empty())
        .ok_or_else(|| Deny::new("image create without fromImage is an import"))?;
    let mut image = ImageRef::parse(&from)?;
    if let Some(tag) = tag {
        if image.tag.is_some() || image.digest.is_some() {
            return Err(Deny::new(format!(
                "{from:?} already names a tag or digest; tag {tag:?} is ambiguous"
            )));
        }
        if is_digest(&tag) {
            image.digest = Some(tag);
        } else if is_tag(&tag) {
            image.tag = Some(tag);
        } else {
            return Err(Deny::new(format!("tag {tag:?} is not allowed")));
        }
    }
    if image.tag.is_none() && image.digest.is_none() {
        return Err(Deny::new(format!(
            "pulling {from:?} without a tag pulls every tag"
        )));
    }
    if is_reserved_repository(&image) {
        return Err(Deny::new(format!(
            "{} is a locally built image and is never pulled",
            image.repository()
        )));
    }
    if let Some(p) = &platform
        && (p.is_empty()
            || p.len() > 64
            || !p.chars().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '/' | '.' | '_' | '-')
            }))
    {
        return Err(Deny::new(format!("platform {p:?} is not allowed")));
    }
    Ok(ImagePull { image, platform })
}

/// Whether `image` is in one of [`RESERVED_REPOSITORIES`].
pub fn is_reserved_repository(image: &ImageRef) -> bool {
    let repo = image.repository();
    RESERVED_REPOSITORIES
        .iter()
        .any(|r| ImageRef::parse(r).is_ok_and(|reserved| reserved.repository() == repo))
}

/// Splits a raw query into decoded `(key, value)` pairs, form-style (`+` is a space). A
/// malformed escape, invalid UTF-8, a `;` separator, an empty key or a repeated key is denied:
/// the engine reads the first of a repeated key, and a rebuilt query must never mean
/// something else.
pub fn parse_query(query: Option<&str>) -> Result<Vec<(String, String)>, Deny> {
    let Some(query) = query else {
        return Ok(Vec::new());
    };
    let deny = |why: &str| Deny::new(format!("query {query:?}: {why}"));
    if query.contains(';') {
        return Err(deny("`;` separator"));
    }
    let decode = |raw: &str| -> Result<String, Deny> {
        if !valid_escapes(raw) {
            return Err(deny("malformed escape"));
        }
        let plus = raw.replace('+', " ");
        percent_decode_str(&plus)
            .decode_utf8()
            .map(|s| s.into_owned())
            .map_err(|_| deny("not UTF-8"))
    };
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for part in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = part.split_once('=').unwrap_or((part, ""));
        let k = decode(k)?;
        let v = decode(v)?;
        if k.is_empty() {
            return Err(deny("empty key"));
        }
        if !seen.insert(k.clone()) {
            return Err(deny("repeated key"));
        }
        out.push((k, v));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{Preset, PresetConfig, Profile};

    const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn config() -> PresetConfig {
        PresetConfig {
            compute_network: "chelabase-feat-x_compute".into(),
            control_network: "chelabase-feat-x_control".into(),
            stack_networks: vec!["chelabase-feat-x_default".into()],
            runner_binds: vec!["/home/dev/chelabase/.chelabase/stacks/feat-x/grpc-tls".into()],
            reserved_images: vec![
                format!("ghcr.io/chelabase/neon-compute-v17:v1@{DIGEST}"),
                "chelabase/neon-storage:dev".into(),
            ],
        }
    }

    fn ok(method: &str, path: &str, query: Option<&str>) -> Op {
        classify(method, path, query)
            .unwrap_or_else(|e| panic!("{method} {path}?{query:?} denied: {e}"))
    }

    fn denied(method: &str, path: &str, query: Option<&str>) -> bool {
        classify(method, path, query).is_err()
    }

    /// Each client's calls, from the code (the epic plan's inventory table).
    fn inventory() -> Vec<(Preset, &'static str, String, Option<String>)> {
        let ep = "chelabase-feat-x-compute-ep1";
        let svc = "0123456789ab";
        let mut calls: Vec<(Preset, &str, String, Option<String>)> = Vec::new();
        let mut add = |p: Preset, m: &'static str, path: &str, q: Option<&str>| {
            calls.push((p, m, path.to_string(), q.map(str::to_string)));
        };
        // orchestrator: chelabase-neon compute.rs DockerLocator (inspect only).
        let o = Preset::Orchestrator;
        add(o, "GET", &format!("/v1.53/containers/{ep}/json"), None);
        // worker: chelabase-neon backend.rs DockerBackend.
        let w = Preset::Worker;
        let compute =
            format!("ghcr.io%2Fchelabase%2Fneon-compute-v17%3Av1%40{DIGEST}").replace(':', "%3A");
        add(
            w,
            "POST",
            "/v1.53/images/create",
            Some(&format!("fromImage={compute}")),
        );
        add(
            w,
            "POST",
            "/v1.53/containers/create",
            Some(&format!("name={ep}")),
        );
        add(w, "POST", &format!("/v1.53/containers/{ep}/start"), None);
        add(
            w,
            "DELETE",
            &format!("/v1.53/containers/{ep}"),
            Some("force=true&v=false&link=false"),
        );
        add(
            w,
            "PUT",
            &format!("/v1.53/containers/{ep}/archive"),
            Some("path=%2Fvar%2Fdb%2Ftls"),
        );
        add(
            w,
            "GET",
            &format!("/v1.53/containers/{ep}/archive"),
            Some("path=%2Fpgdata%2Fx"),
        );
        add(w, "POST", &format!("/v1.53/containers/{ep}/exec"), None);
        add(w, "POST", "/v1.53/exec/4f2a9c/start", None);
        add(w, "GET", "/v1.53/exec/4f2a9c/json", None);
        add(
            w,
            "POST",
            "/v1.53/networks/chelabase-feat-x_control/connect",
            None,
        );
        add(w, "GET", "/v1.53/containers/json", Some("all=true"));
        add(w, "GET", &format!("/v1.53/containers/{ep}/json"), None);
        // service-runner: services/service-runner/src/docker.rs DockerBackend.
        let s = Preset::ServiceRunner;
        add(
            s,
            "GET",
            "/v1.53/images/docker.io/library/nginx:1/json",
            None,
        );
        add(
            s,
            "POST",
            "/v1.53/images/create",
            Some("fromImage=docker.io%2Flibrary%2Fnginx&tag=1"),
        );
        add(s, "GET", "/v1.53/containers/json", Some("all=true"));
        add(s, "GET", &format!("/v1.53/containers/{svc}/json"), None);
        add(
            s,
            "POST",
            "/v1.53/containers/create",
            Some("name=chelabase-feat-x-svc"),
        );
        add(s, "POST", &format!("/v1.53/containers/{svc}/start"), None);
        add(
            s,
            "POST",
            &format!("/v1.53/containers/{svc}/stop"),
            Some("t=10"),
        );
        add(
            s,
            "DELETE",
            &format!("/v1.53/containers/{svc}"),
            Some("force=true&v=true"),
        );
        add(
            s,
            "GET",
            &format!("/v1.53/containers/{svc}/logs"),
            Some("stdout=true&stderr=true&tail=200"),
        );
        add(s, "GET", "/v1.53/networks", None);
        add(s, "GET", "/v1.53/networks/chelabase-feat-x-svc", None);
        add(s, "POST", "/v1.53/networks/create", None);
        add(s, "POST", "/v1.53/networks/abcdef/connect", None);
        add(s, "POST", "/v1.53/networks/abcdef/disconnect", None);
        add(s, "DELETE", "/v1.53/networks/abcdef", None);
        // runner: the docker CLI and compose (unversioned and versioned paths).
        let r = Preset::Runner;
        add(r, "HEAD", "/_ping", None);
        add(r, "GET", "/_ping", None);
        add(r, "GET", "/v1.56/version", None);
        add(r, "GET", "/v1.56/info", None);
        add(r, "GET", "/v1.56/events", None);
        add(r, "POST", "/v1.56/containers/create", None);
        add(
            r,
            "POST",
            "/v1.56/containers/x/attach",
            Some("stream=1&stdout=1"),
        );
        add(
            r,
            "POST",
            "/v1.56/containers/x/wait",
            Some("condition=next-exit"),
        );
        add(r, "POST", "/v1.56/containers/x/kill", None);
        add(r, "POST", "/v1.56/containers/x/resize", Some("h=40&w=120"));
        add(r, "DELETE", "/v1.56/containers/x", None);
        add(r, "POST", "/v1.56/containers/x/exec", None);
        add(r, "POST", "/v1.56/exec/e/start", None);
        add(r, "POST", "/v1.56/exec/e/resize", None);
        add(
            r,
            "HEAD",
            "/v1.56/containers/x/archive",
            Some("path=%2Ftls"),
        );
        add(r, "GET", "/v1.56/volumes", None);
        add(r, "GET", "/v1.56/volumes/chelabase-feat-x_pgdata", None);
        add(r, "POST", "/v1.56/volumes/create", None);
        add(r, "GET", "/v1.56/networks", None);
        add(r, "GET", "/v1.56/networks/chelabase-feat-x_default", None);
        calls
    }

    #[test]
    fn inventory_allowed_per_preset() {
        let cfg = config();
        let orchestrator = Profile::preset(Preset::Orchestrator, &cfg).unwrap();
        for (preset, method, path, query) in inventory() {
            let op = ok(method, &path, query.as_deref());
            let profile = Profile::preset(preset, &cfg).unwrap();
            profile
                .allows(&op)
                .unwrap_or_else(|e| panic!("{preset:?} {method} {path}: {e}"));
            // The runner's harness uses everything the others do.
            Profile::preset(Preset::Runner, &cfg)
                .unwrap()
                .allows(&op)
                .unwrap_or_else(|e| panic!("runner {method} {path}: {e}"));
            // Under the orchestrator only its own reads pass.
            if orchestrator.allows(&op).is_ok() {
                assert!(op.kind().is_read(), "orchestrator allowed {op:?}");
                assert_eq!(
                    op.kind(),
                    OpKind::ContainerInspect,
                    "orchestrator allowed {op:?}, outside its inventory"
                );
            } else {
                assert!(
                    op.kind() != OpKind::ContainerInspect,
                    "orchestrator denied its own {op:?}"
                );
            }
        }
    }

    #[test]
    fn classifies_the_grammar() {
        assert_eq!(ok("GET", "/containers/json", None), Op::ContainerList);
        assert_eq!(
            ok("GET", "/v1.44/containers/abc/json", None),
            Op::ContainerInspect("abc".into())
        );
        assert_eq!(
            ok("DELETE", "/containers/abc", None),
            Op::ContainerDelete("abc".into())
        );
        assert_eq!(
            ok("GET", "/images/ghcr.io/chelabase/x:1/json", None),
            Op::ImageInspect("ghcr.io/chelabase/x:1".into())
        );
        assert_eq!(
            ok("GET", "/images/sha256:ab12/json", None),
            Op::ImageInspect("sha256:ab12".into())
        );
        // Decoded once: `%61bc` is `abc`.
        assert_eq!(
            ok("POST", "/containers/%61bc/start", None),
            Op::ContainerStart("abc".into())
        );
        match ok(
            "POST",
            "/images/create",
            Some("fromImage=nginx&tag=1.27&platform=linux%2Famd64"),
        ) {
            Op::ImageCreate(p) => {
                assert_eq!(p.image.repository(), "docker.io/library/nginx");
                assert_eq!(p.image.tag.as_deref(), Some("1.27"));
                assert_eq!(p.platform.as_deref(), Some("linux/amd64"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unlisted_and_traversal_denied() {
        let unlisted = [
            ("POST", "/build"),
            ("POST", "/v1.56/build"),
            ("POST", "/build/prune"),
            ("POST", "/commit"),
            ("GET", "/containers/x/export"),
            ("POST", "/containers/x/update"),
            ("POST", "/containers/x/rename"),
            ("POST", "/containers/x/pause"),
            ("POST", "/containers/x/restart"),
            ("GET", "/containers/x/top"),
            ("GET", "/containers/x/changes"),
            ("GET", "/containers/x/stats"),
            ("POST", "/containers/prune"),
            ("GET", "/images/json"),
            ("DELETE", "/images/nginx"),
            ("POST", "/images/nginx/push"),
            ("POST", "/images/nginx/tag"),
            ("POST", "/images/load"),
            ("GET", "/images/get"),
            ("GET", "/images/nginx/get"),
            ("GET", "/images/nginx/history"),
            ("POST", "/images/prune"),
            ("GET", "/images/search"),
            ("GET", "/system/df"),
            ("GET", "/plugins"),
            ("POST", "/plugins/pull"),
            ("GET", "/swarm"),
            ("POST", "/swarm/init"),
            ("GET", "/services"),
            ("GET", "/tasks"),
            ("GET", "/secrets"),
            ("POST", "/secrets/create"),
            ("GET", "/configs"),
            ("GET", "/nodes"),
            ("POST", "/session"),
            ("GET", "/distribution/nginx/json"),
            ("POST", "/networks/prune"),
            ("POST", "/volumes/prune"),
            ("DELETE", "/volumes/x"),
            ("PUT", "/volumes/x"),
            ("POST", "/auth"),
            ("POST", "/grpc"),
            // Right path, wrong method.
            ("POST", "/containers/json"),
            ("GET", "/containers/create"),
            ("GET", "/containers/x/start"),
            ("PATCH", "/containers/x/archive"),
            ("HEAD", "/containers/json"),
            ("get", "/containers/json"),
            ("CONNECT", "/_ping"),
            // Extra or missing segments.
            ("GET", "/containers/a/b/json"),
            ("GET", "/containers//json"),
            ("GET", "/containers/json/"),
            ("GET", "/containers/x/json/"),
            ("DELETE", "/containers/"),
            ("GET", "/networks/"),
            ("GET", "/networks/a/b"),
            ("GET", "/images//json"),
            ("GET", "/images/json/"),
            ("GET", "/"),
            ("GET", ""),
            ("GET", "containers/json"),
            ("GET", "//containers/json"),
        ];
        for (m, p) in unlisted {
            assert!(
                denied(m, p, None),
                "{m} {p} classified as {:?}",
                classify(m, p, None)
            );
        }
        let traversal = [
            "/containers/../build",
            "/containers/x/../../build",
            "/v1.44/../build",
            "/containers/%2e%2e/json",
            "/containers/%2E%2E/json",
            "/containers/./json",
            "/containers/a%2Fb/json",
            "/containers/a%2fb/start",
            "/images/a%2Fb/json",
            "/images/../json",
            "/images/a/../json",
            "/containers/x%00/json",
            "/containers/x%20y/json",
            "/containers/%25/json",
            "/containers/x%zz/json",
            "/containers/%ff/json",
            "/containers/x;y/json",
            "/containers/x?/json",
            "/containers/x#/json",
            "/containers/x\\y/json",
        ];
        for p in traversal {
            let m = if p.ends_with("start") { "POST" } else { "GET" };
            assert!(
                denied(m, p, None),
                "{p} classified as {:?}",
                classify(m, p, None)
            );
        }
    }

    #[test]
    fn old_api_versions_denied() {
        for p in [
            "/v1.43/containers/json",
            "/v1.40/containers/create",
            "/v1.12/containers/json",
            "/v1.4/containers/json",
            "/v0.99/containers/json",
            "/v2.0/containers/json",
            "/v1/containers/json",
            "/v1./containers/json",
            "/v.44/containers/json",
            "/v1.44.0/containers/json",
            "/v1..44/containers/json",
            "/v1.99999999999/containers/json",
            "/v/containers/json",
            "/V1.44/containers/json",
            // Decoded before the check, as the engine does.
            "/%761.43/containers/json",
            "/v1%2E43/containers/json",
            // A version alone, or twice.
            "/v1.44",
            "/v1.44/",
            "/v1.44/v1.44/containers/json",
        ] {
            let m = if p.ends_with("create") { "POST" } else { "GET" };
            assert!(
                denied(m, p, None),
                "{p} classified as {:?}",
                classify(m, p, None)
            );
        }
        for p in [
            "/v1.44/containers/json",
            "/v1.56/containers/json",
            "/v1.044/containers/json",
            "/containers/json",
        ] {
            assert_eq!(ok("GET", p, None), Op::ContainerList, "{p}");
        }
        assert_eq!(
            ok("POST", "/v1.56/containers/create", None),
            Op::ContainerCreate
        );
        assert_eq!(ok("POST", "/containers/create", None), Op::ContainerCreate);
        // `volumes` starts with a `v` but isn't a version.
        assert_eq!(ok("GET", "/volumes", None), Op::VolumeList);
    }

    #[test]
    fn image_import_and_reserved_refs_refused() {
        let pull = |q: &str| classify("POST", "/images/create", Some(q));
        for q in [
            "fromSrc=-&repo=chelabase-workbench&tag=dev",
            "fromImage=nginx&tag=1&fromSrc=http%3A%2F%2Fx",
            "fromImage=nginx&tag=1&repo=chelabase-workbench",
            "fromImage=nginx&tag=1&changes=CMD%20x",
            "fromImage=nginx&tag=1&message=x",
            "fromImage=nginx&tag=1&unknown=1",
            "fromImage=nginx&tag=1&FromImage=chelabase-workbench",
            // No fromImage: an import.
            "tag=1",
            "",
            // Repeated: the engine reads the first.
            "fromImage=chelabase-workbench&fromImage=nginx&tag=1",
            "fromImage=nginx&tag=1&tag=2",
            // Reserved local references, in every spelling that normalises to them.
            "fromImage=chelabase-workbench",
            "fromImage=chelabase-workbench&tag=dev",
            "fromImage=chelabase-workbench%3Adev",
            "fromImage=library%2Fchelabase-workbench&tag=x",
            "fromImage=docker.io%2Flibrary%2Fchelabase-workbench&tag=x",
            "fromImage=index.docker.io%2Fchelabase-workbench&tag=x",
            "fromImage=DOCKER.IO%2Fchelabase-workbench&tag=x",
            "fromImage=chelabase%2Ffunctions-runtime&tag=latest",
            "fromImage=docker.io%2Fchelabase%2Ffunctions-runtime&tag=latest",
            "fromImage=chelabase-docker-proxy&tag=1",
            &format!("fromImage=chelabase-workbench%40{DIGEST}"),
            &format!("fromImage=chelabase-workbench&tag={DIGEST}"),
            // Pulling every tag, or a malformed reference.
            "fromImage=nginx",
            "fromImage=nginx&tag=",
            "fromImage=Nginx&tag=1",
            "fromImage=nginx%3A1&tag=2",
            "fromImage=nginx%20x&tag=1",
            "fromImage=%2Fnginx&tag=1",
            "fromImage=nginx%2F&tag=1",
            "fromImage=a%2F..%2Fb&tag=1",
            "fromImage=nginx&tag=1%2F2",
            "fromImage=nginx%40sha256%3Axyz",
            "fromImage=nginx&tag=1&platform=linux%2Famd64%3Bx",
        ] {
            assert!(pull(q).is_err(), "{q} classified as {:?}", pull(q));
        }
        let profile = Profile::preset(Preset::ServiceRunner, &config()).unwrap();
        let op = ok(
            "POST",
            "/images/create",
            Some("fromImage=docker.io/library/nginx&tag=1"),
        );
        profile.allows(&op).unwrap();
        // Configured reserved images: a local build's repository is refused entirely; a
        // digest-pinned one only in its exact pinned form.
        let worker = Profile::preset(Preset::Worker, &config()).unwrap();
        for q in [
            "fromImage=chelabase%2Fneon-storage&tag=dev",
            "fromImage=chelabase%2Fneon-storage&tag=other",
            "fromImage=ghcr.io%2Fchelabase%2Fneon-compute-v17&tag=v1",
            "fromImage=ghcr.io%2Fchelabase%2Fneon-compute-v17&tag=v2",
        ] {
            let op = ok("POST", "/images/create", Some(q));
            assert!(worker.allows(&op).is_err(), "{q} allowed for the worker");
            assert!(
                profile.allows(&op).is_err(),
                "{q} allowed for the service-runner"
            );
        }
        let pinned = format!("fromImage=ghcr.io/chelabase/neon-compute-v17:v1@{DIGEST}");
        worker
            .allows(&ok("POST", "/images/create", Some(&pinned)))
            .unwrap();
    }

    #[test]
    fn image_refs_normalise_like_the_engine() {
        let r = ImageRef::parse("nginx").unwrap();
        assert_eq!(
            (r.domain.as_str(), r.path.as_str()),
            ("docker.io", "library/nginx")
        );
        let r = ImageRef::parse("localhost:5000/a/b:1").unwrap();
        assert_eq!(r.repository(), "localhost:5000/a/b");
        assert_eq!(r.tag.as_deref(), Some("1"));
        let r = ImageRef::parse("localhost/a").unwrap();
        assert_eq!(r.repository(), "localhost/a");
        let r = ImageRef::parse(&format!("Ghcr.IO/x/y:t@{DIGEST}")).unwrap();
        assert_eq!(r.repository(), "ghcr.io/x/y");
        assert_eq!(r.digest.as_deref(), Some(DIGEST));
        let r = ImageRef::parse("user/repo").unwrap();
        assert_eq!(r.repository(), "docker.io/user/repo");
        for bad in [
            "", "a//b", "-a", "a:", "a@", "a:b:c", "UPPER", "a/B", "a b", "a/../b",
        ] {
            assert!(ImageRef::parse(bad).is_err(), "{bad:?} parsed");
        }
    }

    #[test]
    fn queries_parse_form_style_and_refuse_repeats() {
        assert_eq!(
            parse_query(Some("a=1&b=x+y&c=%2F&d")).unwrap(),
            vec![
                ("a".into(), "1".into()),
                ("b".into(), "x y".into()),
                ("c".into(), "/".into()),
                ("d".into(), String::new())
            ]
        );
        assert!(parse_query(None).unwrap().is_empty());
        assert!(parse_query(Some("")).unwrap().is_empty());
        assert!(parse_query(Some("a=1&a=2")).is_err());
        assert!(parse_query(Some("a=%zz")).is_err());
        assert!(parse_query(Some("a=%ff")).is_err());
        assert!(parse_query(Some("a=1;b=2")).is_err());
    }

    /// The canonical (unversioned, decoded) path and methods of an accepted op: anything
    /// [`classify`] accepts must be exactly one of these.
    fn canonical(op: &Op) -> (&'static [&'static str], String) {
        let one = |m: &'static [&'static str], p: String| (m, p);
        const G: &[&str] = &["GET"];
        const P: &[&str] = &["POST"];
        const D: &[&str] = &["DELETE"];
        match op {
            Op::Ping => one(&["GET", "HEAD"], "/_ping".into()),
            Op::Version => one(G, "/version".into()),
            Op::Info => one(G, "/info".into()),
            Op::Events => one(G, "/events".into()),
            Op::ContainerList => one(G, "/containers/json".into()),
            Op::ContainerCreate => one(P, "/containers/create".into()),
            Op::ContainerInspect(t) => one(G, format!("/containers/{t}/json")),
            Op::ContainerStart(t) => one(P, format!("/containers/{t}/start")),
            Op::ContainerStop(t) => one(P, format!("/containers/{t}/stop")),
            Op::ContainerKill(t) => one(P, format!("/containers/{t}/kill")),
            Op::ContainerWait(t) => one(P, format!("/containers/{t}/wait")),
            Op::ContainerDelete(t) => one(D, format!("/containers/{t}")),
            Op::ContainerLogs(t) => one(G, format!("/containers/{t}/logs")),
            Op::ContainerAttach(t) => one(P, format!("/containers/{t}/attach")),
            Op::ContainerResize(t) => one(P, format!("/containers/{t}/resize")),
            Op::ArchiveGet(t) => one(G, format!("/containers/{t}/archive")),
            Op::ArchivePut(t) => one(&["PUT"], format!("/containers/{t}/archive")),
            Op::ArchiveHead(t) => one(&["HEAD"], format!("/containers/{t}/archive")),
            Op::ExecCreate(t) => one(P, format!("/containers/{t}/exec")),
            Op::ExecStart(t) => one(P, format!("/exec/{t}/start")),
            Op::ExecInspect(t) => one(G, format!("/exec/{t}/json")),
            Op::ExecResize(t) => one(P, format!("/exec/{t}/resize")),
            Op::ImageCreate(_) => one(P, "/images/create".into()),
            Op::ImageInspect(t) => one(G, format!("/images/{t}/json")),
            Op::NetworkList => one(G, "/networks".into()),
            Op::NetworkInspect(t) => one(G, format!("/networks/{t}")),
            Op::NetworkCreate => one(P, "/networks/create".into()),
            Op::NetworkConnect(t) => one(P, format!("/networks/{t}/connect")),
            Op::NetworkDisconnect(t) => one(P, format!("/networks/{t}/disconnect")),
            Op::NetworkDelete(t) => one(D, format!("/networks/{t}")),
            Op::VolumeList => one(G, "/volumes".into()),
            Op::VolumeInspect(t) => one(G, format!("/volumes/{t}")),
            Op::VolumeCreate => one(P, "/volumes/create".into()),
        }
    }

    /// What the engine would route: the decoded path with an accepted version prefix removed.
    fn engine_view(path: &str) -> Option<String> {
        let decoded: Vec<String> = path
            .split('/')
            .map(|s| percent_decode_str(s).decode_utf8().map(|c| c.into_owned()))
            .collect::<Result<_, _>>()
            .ok()?;
        let mut segs: Vec<&str> = decoded.iter().map(String::as_str).collect();
        if segs.first() != Some(&"") {
            return None;
        }
        segs.remove(0);
        if let Some(v) = segs.first().and_then(|s| s.strip_prefix("v1."))
            && v.parse::<u32>().is_ok_and(|n| n >= MIN_API_MINOR)
        {
            segs.remove(0);
        }
        Some(format!("/{}", segs.join("/")))
    }

    fn check_round_trip(
        method: &str,
        path: &str,
    ) -> Result<(), proptest::test_runner::TestCaseError> {
        let query = Some("fromImage=nginx&tag=1");
        if let Ok(op) = classify(method, path, query) {
            let (methods, canon) = canonical(&op);
            proptest::prop_assert!(methods.contains(&method), "{method} {path} → {op:?}");
            proptest::prop_assert_eq!(
                Some(canon),
                engine_view(path),
                "{} {} → {:?}",
                method,
                path,
                op
            );
            for seg in path.split('/') {
                let d = percent_decode_str(seg).decode_utf8_lossy();
                proptest::prop_assert!(d != ".." && d != "." && !d.contains('/'), "{path}");
            }
        }
        Ok(())
    }

    fn segment() -> impl proptest::strategy::Strategy<Value = String> {
        use proptest::prelude::*;
        prop_oneof![
            Just("containers".to_string()),
            Just("images".to_string()),
            Just("exec".to_string()),
            Just("networks".to_string()),
            Just("volumes".to_string()),
            Just("json".to_string()),
            Just("create".to_string()),
            Just("start".to_string()),
            Just("archive".to_string()),
            Just("update".to_string()),
            Just("build".to_string()),
            Just("_ping".to_string()),
            Just("v1.44".to_string()),
            Just("v1.43".to_string()),
            Just("..".to_string()),
            Just("%2e%2e".to_string()),
            Just("a%2Fb".to_string()),
            Just(String::new()),
            "[a-zA-Z0-9._:%@-]{1,8}",
        ]
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config::with_cases(4000))]

        /// Random paths from the grammar's own vocabulary: whatever is accepted is exactly a
        /// grammar form, with a method that form allows.
        #[test]
        fn accepted_paths_are_grammar_forms(
            method in proptest::sample::select(vec!["GET", "HEAD", "POST", "PUT", "DELETE", "PATCH", "OPTIONS", "get"]),
            segs in proptest::collection::vec(segment(), 0..6),
        ) {
            let path = format!("/{}", segs.join("/"));
            check_round_trip(method, &path)?;
        }

        /// Arbitrary strings, too.
        #[test]
        fn arbitrary_paths_are_denied_or_grammar_forms(
            method in "[A-Za-z]{0,7}",
            path in "\\PC{0,40}",
        ) {
            check_round_trip(&method, &path)?;
        }
    }
}
