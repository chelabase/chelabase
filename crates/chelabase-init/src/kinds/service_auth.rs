//! The NATS users file and Valkey ACL file a stack renders from its secrets (RT1 T1: S1, S6), and
//! the storage auth files derived from the compute-JWT key pair (user, 2026-10-16).
//!
//! Both auth files hold hashes only (bcrypt for NATS, SHA-256 for Valkey), never a password,
//! and are written 0600 into the stack's state dir. NATS and Valkey mount them read-only.

use anyhow::{Context, Result};
#[cfg(any(test, feature = "test-support"))]
use chelabase_neon::jwt::PUBLIC_PEM_FILE;
use chelabase_neon::jwt::StorageScope;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::fsx::{Dir, Kind, Perm};
use crate::kinds::stack_secrets::{NATS_USERS, create_private_dir};
#[cfg(any(test, feature = "test-support"))]
use crate::kinds::stack_secrets::{VALKEY_USERS, read_text};

/// bcrypt cost of the NATS password hashes (nats-server checks one on every connect).
const BCRYPT_COST: u32 = 10;

/// What one NATS user may publish and subscribe to (R11, W13, S2).
///
/// No user gets `allow_responses`: it lets a client publish once to the reply subject of any
/// message it receives, and the publisher picks that subject. A user that may publish to a
/// subject it also subscribes to (realtime's presence, the worker's resync events) would send
/// itself a message whose reply subject is `chelabase.provision`, `$JS.API.STREAM.DELETE.…` or
/// any other subject, and then publish there. Responders instead get publish rights on their
/// callers' reply inboxes (`_INBOX_<caller>.>`), which are the only subjects they answer on.
struct NatsPermissions {
    publish: Vec<&'static str>,
    publish_deny: Vec<&'static str>,
    subscribe: Vec<&'static str>,
}

/// Every control-stream job subject is one token under `chelabase.` (`chelabase.provision`,
/// `chelabase.merge`, …: `chelabase-nats`'s `control_subjects`); `chelabase.*` matches exactly
/// those, never `chelabase.rpc.…`, `chelabase.rt.…` or another two-token subject.
const CONTROL_JOBS: &str = "chelabase.*";
const CORE_EVENTS: [&str; 2] = [
    "chelabase.resync.branch",
    "chelabase.member_roles.reconcile",
];
/// Creating or extending the control stream (`ensure_control_stream`).
const CONTROL_STREAM_API: [&str; 3] = [
    "$JS.API.STREAM.INFO.CHELABASE_CONTROL",
    "$JS.API.STREAM.CREATE.CHELABASE_CONTROL",
    "$JS.API.STREAM.UPDATE.CHELABASE_CONTROL",
];
/// The worker's durable pull consumers on the control stream.
const CONTROL_CONSUMER_API: [&str; 6] = [
    "$JS.API.CONSUMER.CREATE.CHELABASE_CONTROL",
    "$JS.API.CONSUMER.CREATE.CHELABASE_CONTROL.>",
    "$JS.API.CONSUMER.DURABLE.CREATE.CHELABASE_CONTROL.>",
    "$JS.API.CONSUMER.INFO.CHELABASE_CONTROL.>",
    "$JS.API.CONSUMER.MSG.NEXT.CHELABASE_CONTROL.>",
    "$JS.ACK.CHELABASE_CONTROL.>",
];
/// The tenant-attached events (carried gaps T4, P9): the subjects the orchestrator publishes.
const NEON_EVENTS: &str = "chelabase.neon.attached.*";
/// Creating or fixing the events stream (`ensure_neon_events_stream`): the orchestrator only
/// (R10).
const NEON_EVENTS_STREAM_API: [&str; 3] = [
    "$JS.API.STREAM.INFO.CHELABASE_NEON_EVENTS",
    "$JS.API.STREAM.CREATE.CHELABASE_NEON_EVENTS",
    "$JS.API.STREAM.UPDATE.CHELABASE_NEON_EVENTS",
];
/// The worker's attach waits (`wait_tenant_attached`): a consumer filtered to one tenant's
/// subject (`CREATE.<stream>.<name>.<filter>`, so an unfiltered, otherwise filtered or durable
/// create is refused), its pulls and its delete. Replies come on `_INBOX_worker.>`.
const NEON_EVENTS_WAIT_API: [&str; 3] = [
    "$JS.API.CONSUMER.CREATE.CHELABASE_NEON_EVENTS.*.chelabase.neon.attached.*",
    "$JS.API.CONSUMER.MSG.NEXT.CHELABASE_NEON_EVENTS.*",
    "$JS.API.CONSUMER.DELETE.CHELABASE_NEON_EVENTS.*",
];
/// What realtime publishes (S2, ruling (a)): its two KV buckets as async-nats drives them
/// (`$JS.FC.<stream>.…` carries the watches' flow-control replies), rows, watches and presence.
/// Never `chelabase.rpc.*`, a job subject, `rt.access`, `rt.project` or `rt.compute.stopping`.
const REALTIME_PUBLISH: [&str; 22] = [
    "$KV.CHELABASE_RT_LEASES.>",
    "$KV.CHELABASE_RT_SLOTS.>",
    "$JS.API.INFO",
    "$JS.API.STREAM.INFO.KV_CHELABASE_RT_LEASES",
    "$JS.API.STREAM.INFO.KV_CHELABASE_RT_SLOTS",
    "$JS.API.STREAM.CREATE.KV_CHELABASE_RT_LEASES",
    "$JS.API.STREAM.CREATE.KV_CHELABASE_RT_SLOTS",
    "$JS.API.STREAM.UPDATE.KV_CHELABASE_RT_LEASES",
    "$JS.API.STREAM.UPDATE.KV_CHELABASE_RT_SLOTS",
    "$JS.API.CONSUMER.CREATE.KV_CHELABASE_RT_LEASES",
    "$JS.API.CONSUMER.CREATE.KV_CHELABASE_RT_SLOTS",
    "$JS.API.CONSUMER.CREATE.KV_CHELABASE_RT_LEASES.>",
    "$JS.API.CONSUMER.CREATE.KV_CHELABASE_RT_SLOTS.>",
    "$JS.API.CONSUMER.DELETE.KV_CHELABASE_RT_LEASES.>",
    "$JS.API.CONSUMER.DELETE.KV_CHELABASE_RT_SLOTS.>",
    "$JS.API.DIRECT.GET.KV_CHELABASE_RT_LEASES.>",
    "$JS.API.DIRECT.GET.KV_CHELABASE_RT_SLOTS.>",
    "$JS.FC.KV_CHELABASE_RT_LEASES.>",
    "$JS.FC.KV_CHELABASE_RT_SLOTS.>",
    "chelabase.rt.rows.>",
    "chelabase.rt.watch.>",
    "chelabase.rt.presence.>",
];

fn orchestrator() -> NatsPermissions {
    let mut publish = vec![CONTROL_JOBS, "chelabase.rpc.>"];
    publish.extend(CORE_EVENTS);
    publish.extend(["chelabase.rt.project.>", "chelabase.rt.access.>"]);
    publish.extend(CONTROL_STREAM_API);
    publish.push(NEON_EVENTS);
    publish.extend(NEON_EVENTS_STREAM_API);
    NatsPermissions {
        publish,
        publish_deny: vec![],
        subscribe: vec!["_INBOX_orchestrator.>"],
    }
}

/// The worker answers `chelabase.rpc.*` requests from the orchestrator and from the test runner
/// (which runs the orchestrator in-process), on their reply inboxes.
fn worker() -> NatsPermissions {
    let mut publish = vec![CONTROL_JOBS];
    publish.extend(CORE_EVENTS);
    publish.extend(["chelabase.rt.project.>", "chelabase.rt.compute.stopping.>"]);
    publish.extend(CONTROL_STREAM_API);
    publish.extend(CONTROL_CONSUMER_API);
    publish.extend(["_INBOX_orchestrator.>", "_INBOX_runner.>"]);
    publish.extend(NEON_EVENTS_WAIT_API);
    let mut subscribe = vec!["chelabase.rpc.>"];
    subscribe.extend(CORE_EVENTS);
    subscribe.push("_INBOX_worker.>");
    NatsPermissions {
        publish,
        publish_deny: vec![],
        subscribe,
    }
}

/// Realtime answers presence sync requests from other realtime instances (and from the test
/// runner, which runs realtime in-process), on their reply inboxes.
fn realtime() -> NatsPermissions {
    let mut publish = REALTIME_PUBLISH.to_vec();
    publish.extend(["_INBOX_realtime.>", "_INBOX_runner.>"]);
    NatsPermissions {
        publish,
        publish_deny: vec![],
        subscribe: vec!["chelabase.rt.>", "_INBOX_realtime.>"],
    }
}

/// The test runner runs the worker, the orchestrator and realtime in-process (S24), so it has
/// the union of their rights, plus scratch streams for the crates' own live tests
/// (`CHELABASE_TEST_<tag>` on `chelabase.test.<tag>`). It still can't delete or purge the
/// control stream, the events stream or realtime's buckets, nor read the shared or another
/// service's inbox.
fn runner() -> NatsPermissions {
    let mut publish = Vec::new();
    let mut subscribe = Vec::new();
    for p in [orchestrator(), worker(), realtime()] {
        publish.extend(p.publish);
        subscribe.extend(
            p.subscribe
                .into_iter()
                .filter(|s| !s.starts_with("_INBOX_")),
        );
    }
    publish.extend([
        "chelabase.test.>",
        "$JS.API.STREAM.CREATE.*",
        "$JS.API.STREAM.DELETE.*",
        "$JS.API.STREAM.INFO.*",
        "$JS.API.CONSUMER.>",
        "$JS.ACK.>",
    ]);
    subscribe.extend(["chelabase.test.>", "_INBOX_runner.>"]);
    dedup(&mut publish);
    dedup(&mut subscribe);
    NatsPermissions {
        publish,
        publish_deny: vec![
            "$JS.API.STREAM.DELETE.CHELABASE_CONTROL",
            "$JS.API.STREAM.DELETE.CHELABASE_NEON_EVENTS",
            "$JS.API.STREAM.DELETE.KV_CHELABASE_RT_LEASES",
            "$JS.API.STREAM.DELETE.KV_CHELABASE_RT_SLOTS",
            "$JS.API.STREAM.PURGE.>",
            // No consumer of the control stream: it is a work queue, so creating one, pulling
            // or acknowledging would take the worker's jobs. Stream info stays (live tests).
            "$JS.API.CONSUMER.CREATE.CHELABASE_CONTROL",
            "$JS.API.CONSUMER.CREATE.CHELABASE_CONTROL.>",
            "$JS.API.CONSUMER.DURABLE.CREATE.CHELABASE_CONTROL.>",
            "$JS.API.CONSUMER.MSG.NEXT.CHELABASE_CONTROL.>",
            "$JS.API.CONSUMER.DELETE.CHELABASE_CONTROL.>",
            "$JS.ACK.CHELABASE_CONTROL.>",
        ],
        subscribe,
    }
}

fn dedup(v: &mut Vec<&'static str>) {
    let mut seen = std::collections::BTreeSet::new();
    v.retain(|s| seen.insert(*s));
}

fn permissions(user: &str) -> NatsPermissions {
    match user {
        "orchestrator" => orchestrator(),
        "worker" => worker(),
        "realtime" => realtime(),
        "runner" => runner(),
        other => unreachable!("no NATS user {other}"),
    }
}

fn quoted_list(items: &[&str]) -> String {
    items
        .iter()
        .map(|s| format!("\"{s}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A password from `passwords`, refused when missing or empty.
fn password<'a>(passwords: &'a BTreeMap<String, String>, user: &str) -> Result<&'a str> {
    let pw = passwords
        .get(user)
        .with_context(|| format!("no password for {user}"))?;
    anyhow::ensure!(!pw.is_empty(), "the password for {user} is empty");
    Ok(pw)
}

#[cfg(any(test, feature = "test-support"))]
/// The NATS `authorization` block (included by `infra/compose/nats/nats.conf`): one user per
/// service with a bcrypt hash of its password and its permissions. No anonymous user.
/// (`up` renders through [`write_auth_files`], which keeps still-valid hashes.)
pub fn render_nats_auth(passwords: &BTreeMap<String, String>) -> Result<String> {
    render_nats_auth_reusing(passwords, &BTreeMap::new())
}

/// [`render_nats_auth`], keeping a user's earlier hash (`previous`) while it still matches the
/// password, so a re-render changes nothing.
pub fn render_nats_auth_reusing(
    passwords: &BTreeMap<String, String>,
    previous: &BTreeMap<String, String>,
) -> Result<String> {
    let mut out = String::from(
        "# Generated by chelabase workbench from the stack's NATS passwords (bcrypt hashes only);\n\
         # do not edit. One account, one user per service (RT1 T1, R11).\n\
         authorization {\n  users: [\n",
    );
    for name in NATS_USERS {
        let pw = password(passwords, name)?;
        let hash = match previous.get(name) {
            Some(h) if bcrypt::verify(pw, h).unwrap_or(false) => h.clone(),
            _ => bcrypt::hash(pw, BCRYPT_COST).context("hashing a NATS password")?,
        };
        let p = permissions(name);
        out.push_str(&format!(
            "    {{\n      user: \"{name}\"\n      password: \"{hash}\"\n"
        ));
        out.push_str("      permissions: {\n");
        out.push_str(&format!(
            "        publish: {{ allow: [{}]",
            quoted_list(&p.publish)
        ));
        if !p.publish_deny.is_empty() {
            out.push_str(&format!(", deny: [{}]", quoted_list(&p.publish_deny)));
        }
        out.push_str(" }\n");
        out.push_str(&format!(
            "        subscribe: {{ allow: [{}] }}\n",
            quoted_list(&p.subscribe)
        ));
        out.push_str("      }\n    }\n");
    }
    out.push_str("  ]\n}\n");
    Ok(out)
}

/// The bcrypt hash of each user in an earlier rendering of [`render_nats_auth`].
pub fn previous_nats_hashes(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut user: Option<String> = None;
    for line in text.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("user: \"") {
            user = rest.strip_suffix('"').map(str::to_string);
        } else if let Some(rest) = line.strip_prefix("password: \"")
            && let (Some(u), Some(hash)) = (user.take(), rest.strip_suffix('"'))
        {
            out.insert(u, hash.to_string());
        }
    }
    out
}

fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

/// Valkey's ACL file (S6, ruling (f)): `default` off; each user with `#<sha256>` of its
/// password, its key patterns, its channels and only the commands it issues.
pub fn render_valkey_acl(passwords: &BTreeMap<String, String>) -> Result<String> {
    // `+multi +exec`: a counter's INCR and EXPIRE NX go in one transaction.
    const REALTIME: &str =
        "+set +getdel +incr +decr +expire +zadd +zrem +zremrangebyscore +zcard +multi +exec";
    let h = |user: &str| -> Result<String> { Ok(sha256_hex(password(passwords, user)?)) };
    let lines = [
        "user default off".to_string(),
        format!(
            "user proxy on #{} resetchannels &neondb-proxy-ws-updates ~cancel:* -@all +subscribe +ping +set +get +hget +expire +client|setinfo",
            h("proxy")?
        ),
        format!(
            "user control on #{} resetchannels &neondb-proxy-ws-updates -@all +publish +ping +client|setinfo",
            h("control")?
        ),
        format!(
            "user realtime on #{} resetchannels ~rt:* -@all {REALTIME} +ping +client|setinfo",
            h("realtime")?
        ),
        // The runner runs realtime and the control plane in-process: both users' rules.
        format!(
            "user runner on #{} resetchannels &neondb-proxy-ws-updates ~rt:* -@all {REALTIME} +publish +ping +client|setinfo",
            h("runner")?
        ),
        format!("user health on #{} resetchannels -@all +ping", h("health")?),
    ];
    // Valkey's ACL file takes `user` lines only: no comment header (it refuses to start).
    let mut out = String::new();
    for line in lines {
        out.push_str(&line);
        out.push('\n');
    }
    Ok(out)
}

/// Which rendered auth files changed on disk (see [`write_auth_files`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AuthFilesChanged {
    pub nats: bool,
    pub valkey: bool,
}

#[cfg(any(test, feature = "test-support"))]
/// Writes `<state>/nats/auth.conf` and `<state>/valkey/users.acl` (0600) from the secret files
/// under `keys`. A file whose contents don't change is left alone; one whose owner or mode isn't
/// the writer's 0600 is refused.
/// Reports which file's contents changed (created or rewritten): a running NATS or Valkey keeps
/// reading the old inode through its single-file bind mount, so `up` restarts it (R9).
pub fn write_auth_files(keys: &Path, state: &Path) -> Result<AuthFilesChanged> {
    let read = |dir: &str, users: &[&str]| -> Result<BTreeMap<String, String>> {
        users
            .iter()
            .map(|u| {
                Ok((
                    u.to_string(),
                    read_text(keys, &format!("{dir}/{u}.password"))?,
                ))
            })
            .collect()
    };
    let nats_path = state.join("nats/auth.conf");
    let previous = std::fs::read_to_string(&nats_path)
        .map(|t| previous_nats_hashes(&t))
        .unwrap_or_default();
    let nats = render_nats_auth_reusing(&read("nats", &NATS_USERS)?, &previous)?;
    let nats_changed = write_private(&nats_path, nats.as_bytes())?;
    let acl = render_valkey_acl(&read("valkey", &VALKEY_USERS)?)?;
    let valkey_changed = write_private(&state.join("valkey/users.acl"), acl.as_bytes())?;
    Ok(AuthFilesChanged {
        nats: nats_changed,
        valkey: valkey_changed,
    })
}

/// The storage tokens the workbench derives (scope, file under `<keys>/neon/storage-tokens/`).
pub const STORAGE_TOKENS: [(StorageScope, &str); 4] = [
    // storage controller -> pageserver
    (StorageScope::PageServerApi, "pageserver-api.jwt"),
    // storage controller, pageserver and safekeeper peers -> safekeeper
    (StorageScope::SafekeeperData, "safekeeper-data.jwt"),
    // pageserver -> storage controller upcalls
    (StorageScope::GenerationsApi, "generations-api.jwt"),
    // register-safekeepers and the runner's scripts -> storage controller
    (StorageScope::Admin, "storcon-admin.jwt"),
];

#[cfg(any(test, feature = "test-support"))]
/// From the existing compute-JWT key pair in `<keys>/neon` (never created or rewritten here):
/// its public key as SPKI PEM (`compute-jwt.pub.pem`, which the pageserver, safekeepers and
/// storage controller verify with) and the scoped storage tokens (0600, no expiry; Ed25519 is
/// deterministic, so a re-run writes the same bytes).
pub fn write_storage_auth(keys: &Path) -> Result<()> {
    let neon = keys.join("neon");
    let signer = chelabase_neon::jwt::load_signer(&neon)?;
    write_public(
        &neon.join(PUBLIC_PEM_FILE),
        signer.public_key_pem().as_bytes(),
    )?;
    let dir = neon.join("storage-tokens");
    create_private_dir(&dir)?;
    for (scope, file) in &STORAGE_TOKENS {
        let token = signer.storage_token(scope)?;
        write_private(&dir.join(file), token.as_bytes())?;
    }
    // The metrics token carries the clock (`iat`), so unlike the tokens above it is minted only
    // when missing or empty: rewriting it on every `up` would rotate it for nothing. A kept file is
    // left byte for byte; one that isn't the writer's 0600 is refused.
    let metrics = dir.join(METRICS_TOKEN_FILE);
    if std::fs::symlink_metadata(&metrics).is_ok_and(|m| m.len() > 0) {
        check_existing(&metrics, 0o600)?;
    } else {
        write_private(&metrics, signer.metrics_token(unix_now()?)?.as_bytes())?;
    }
    Ok(())
}

/// The runner's metrics token under `<keys>/neon/storage-tokens/` (scope `metrics`, S17).
pub const METRICS_TOKEN_FILE: &str = "metrics.token";

#[cfg(any(test, feature = "test-support"))]
fn unix_now() -> Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("the clock is before the epoch")?
        .as_secs())
}

/// Mints a metrics token at `now` into `<keys>/neon/storage-tokens/metrics.token`, replacing the
/// kept one (rotation; `chelabase neon mint-metrics-token`). It needs the existing compute-JWT
/// key pair and never creates one. Returns the file it wrote (0600).
// The CLI goes through `mint_metrics_token_in` (CHELABASE_NEON_DIR); this keys-dir form is the pinned API.
#[cfg(any(test, feature = "test-support"))]
pub fn mint_metrics_token(keys: &Path, now: u64) -> Result<PathBuf> {
    mint_metrics_token_in(&keys.join("neon"), now)
}

/// [`mint_metrics_token`] for a compute-JWT directory given directly (`CHELABASE_NEON_DIR`).
pub fn mint_metrics_token_in(neon: &Path, now: u64) -> Result<PathBuf> {
    let signer = chelabase_neon::jwt::load_signer(neon)?;
    let token = signer.metrics_token(now)?;
    let dir = neon.join("storage-tokens");
    create_private_dir(&dir)?;
    let path = dir.join(METRICS_TOKEN_FILE);
    write_with_mode(&path, token.as_bytes(), 0o600, Existing::Replace)?;
    Ok(path)
}

#[cfg(any(test, feature = "test-support"))]
/// Writes `contents` to `path` (0600) through a temporary file and a rename, so a reader never
/// sees half a file; unchanged contents are not rewritten. An existing file whose owner or mode
/// isn't the writer's 0600 is refused, never silently tightened (`chelabase-init`'s rule). True
/// when the contents changed (the file was written).
fn write_private(path: &Path, contents: &[u8]) -> Result<bool> {
    write_with_mode(path, contents, 0o600, Existing::Check)
}

#[cfg(any(test, feature = "test-support"))]
fn write_public(path: &Path, contents: &[u8]) -> Result<bool> {
    write_with_mode(path, contents, 0o644, Existing::Check)
}

/// What an existing file's owner and mode must be before it is rewritten.
#[derive(Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(any(test, feature = "test-support")), allow(dead_code))]
enum Existing {
    /// Exactly the writer's and `mode`: anything else is refused.
    Check,
    /// Replaced whatever they are (a deliberate rotation).
    Replace,
}

fn want(mode: u32) -> Perm {
    let (uid, gid) = crate::fsx::writer();
    Perm { uid, gid, mode }
}

/// Fails unless `path`'s owner and mode are the writer's and `mode` (a missing file passes).
fn check_existing(path: &Path, mode: u32) -> Result<()> {
    let (dir, name) = open_parent(path)?;
    if let Some(st) = dir.stat(name)? {
        anyhow::ensure!(
            st.kind == Kind::File,
            "{} is not a regular file",
            path.display()
        );
        let want = want(mode);
        anyhow::ensure!(
            st.perm == want,
            "{}: wrong owner or mode; expected {want}, found {}",
            path.display(),
            st.perm
        );
    }
    Ok(())
}

fn open_parent(path: &Path) -> Result<(Dir, &str)> {
    let parent = path.parent().context("a file path")?;
    create_private_dir(parent)?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .with_context(|| format!("{} has no file name", path.display()))?;
    // These dirs may predate the check (made under the user's umask): group write is accepted.
    Ok((Dir::open_root(parent, true)?, name))
}

fn write_with_mode(path: &Path, contents: &[u8], mode: u32, existing: Existing) -> Result<bool> {
    let (dir, name) = open_parent(path)?;
    match dir.stat(name)? {
        None => {
            anyhow::ensure!(
                dir.create_new(name, contents, want(mode))?,
                "{} appeared while it was written",
                path.display()
            );
            Ok(true)
        }
        Some(st) if st.kind != Kind::File => {
            anyhow::bail!("refusing {}: it is not a regular file", path.display())
        }
        Some(_) => {
            if existing == Existing::Check {
                check_existing(path, mode)?;
                if dir.read(name)? == contents {
                    return Ok(false);
                }
            }
            dir.replace(name, contents, want(mode))?;
            Ok(true)
        }
    }
}
