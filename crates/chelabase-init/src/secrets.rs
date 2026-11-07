//! `chelabase-init secrets`: every key, secret and auth file a stack's services read, made in
//! one directory tree, never overwritten.
//!
//! # Layout
//!
//! Paths are relative to the root (`--dir`). The machine part is the workbench's
//! `CHELABASE_KEYS_DIR` layout (compose's top-level `secrets:` block names these files):
//!
//! | Path | What |
//! |---|---|
//! | `secrets/secrets.key` | the control plane's secrets key (AES-256, base64) |
//! | `gateway/internal-signing.pem`, `gateway/internal-verify.pem` | the gateway's internal-identity pair |
//! | `auth/cookie.key` | auth's cookie key |
//! | `auth/app-jwt.key`, `auth/app-jwt.pub` | the app-user JWT pair |
//! | `auth/member-jwt.key`, `auth/member-jwt.pub` | the member JWT pair (auth signs; never the app-user pair) |
//! | `identity/{secret,db.password}` | the identity service's Better Auth secret and its database password |
//! | `neon/compute-jwt.pem`, `neon/compute-jwt.pub` | the compute-JWT pair |
//! | `neon/compute-jwt.pub.pem` | derived: its public key as SPKI PEM |
//! | `neon/storage-tokens/*.jwt` | derived: the scoped storage tokens |
//! | `neon/storage-tokens/metrics.token` | derived once: the runner's metrics token |
//! | `compute-ca/ca.key`, `compute-ca/ca.crt` | the compute CA |
//! | `orchestrator-tls/`, `realtime-tls/` | client pairs (`current` -> `v<secs>/`), renewed when due |
//! | `nats/*.password`, `valkey/*.password` | NATS and Valkey user passwords |
//! | `realtime/{pg.password,ticket.key,internal.token}` | realtime's secrets |
//! | `tokens/*.token` | the internal bearers (`*-identity.token`: each identity caller's own) |
//!
//! The install part holds what each install has on its own (in the workbench it goes under
//! each stack's state dir; self-hosted, it is the same root):
//!
//! | Path | What |
//! |---|---|
//! | `nats/auth.conf`, `valkey/users.acl` | derived: the NATS users (bcrypt) and Valkey ACL (SHA-256) |
//! | `db/metadata.password` | the metadata database's `chelabase` user |
//! | `db/storcon.password` | the storage controller database's `storcon` user |
//! | `s3/access-key`, `s3/secret-key` | the object store's credentials |
//! | `setup/setup.code` | the one-time setup code (`XXXX-XXXX-XXXX-XXXX-XXXX`) |
//!
//! # Rules
//!
//! - A missing file is created; an existing one is never rewritten. Derived files are rendered
//!   from the existing source files and rewritten only when that rendering differs.
//! - Every existing entry is checked before anything is written: an empty or unreadable file,
//!   a blank value, a key, certificate or setup code that doesn't parse, a symlink anywhere in
//!   the layout, half a key pair, or an owner or mode other than the ownership map's fails the
//!   run, naming the file. A run the check refuses writes nothing. A run that passes it can still
//!   fail while writing (an I/O error); the files written so far stay, and a rerun completes
//!   them. The setup code is the last file written, and a code that was written is printed even
//!   if the run then fails.
//! - Each root must belong to the writer (or root) and not be writable by group or others (the
//!   workbench accepts group write on its umask-made dirs, never world write).
//! - Owners and modes come from [`LAYOUT`]'s `spec` column ([`OwnershipMap`] for overrides). A
//!   directory is 0750 with a consumer group when the files below it in the same physical root
//!   (both parts, self-hosted) are readable by one group, else 0700.
//! - Runs on the same root are serialised by a lock file (`.chelabase-init.lock`).
//! - Seeds ([`InitSecrets::seeds`], [`crate::seed`]): a missing plain value with a seed is
//!   written from it, through the same write path (mode, owner, atomic publish) as a generated
//!   one, and derived files are rendered from it. An existing file whose value differs from its
//!   seed fails the check, naming the key and file, so a run never silently ignores a seed.
//! - Nothing here prints or logs a secret: the report carries file paths and the setup code only.

use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::fsx::{Dir, Kind, Perm};

use crate::kinds::service_auth::AuthFilesChanged;
pub use crate::owners::{Id, OwnershipMap, Spec};
pub use crate::seed::Seeds;

/// Which root an entry lives under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    /// Shared by every stack of a machine in the workbench.
    Machine,
    /// One install's own (a workbench stack's state dir).
    Install,
}

/// A key pair whose two files are made together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pair {
    Internal,
    AppJwt,
    MemberJwt,
    ComputeJwt,
    ComputeCa,
}

/// How an entry's contents are made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gen {
    /// A `chelabase_secrets::SecretKey`, base64.
    SecretKey,
    /// 32 random bytes as 43 base64url characters.
    Text32,
    /// [`Gen::Text32`], never seeded: its reader needs at least 32 bytes (a seed may hold 16) or
    /// puts it in a URL (a seed may hold URL metacharacters).
    Token32,
    /// 32 random bytes, raw.
    Raw32,
    /// One half of a pair (`private` true for the private key).
    Pair { pair: Pair, private: bool },
    /// A random [`Gen::Text32`], or `dev` for a workbench stack (`dev_fixed`).
    Password { dev: &'static str },
    /// `len` random letters and digits, or `dev` for a workbench stack.
    Alnum { len: usize, dev: &'static str },
    /// The setup code.
    SetupCode,
    /// Rendered from other entries.
    Derived(Derived),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Derived {
    StoragePem,
    /// An index into [`crate::kinds::service_auth::STORAGE_TOKENS`].
    StorageToken(usize),
    MetricsToken,
    NatsAuth,
    ValkeyAcl,
}

/// One file of the layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub path: &'static str,
    pub part: Part,
    pub make: Gen,
    /// The default owner, group and mode (self-hosted).
    pub spec: Spec,
}

/// `postgres:*-alpine` runs as 70:70.
pub const POSTGRES_GID: u32 = 70;
/// `valkey/valkey:*-alpine` runs as 999:1000.
pub const VALKEY_GID: u32 = 1000;
/// The Neon storage image's own user is 1000:1000.
pub const NEON_GID: u32 = 1000;

const fn e(path: &'static str, part: Part, make: Gen, spec: Spec) -> Entry {
    Entry {
        path,
        part,
        make,
        spec,
    }
}
const M: Part = Part::Machine;
const I: Part = Part::Install;
const P: Spec = Spec::PRIVATE;
const PUB: Spec = Spec::PUBLIC;
const fn half(pair: Pair, private: bool) -> Gen {
    Gen::Pair { pair, private }
}

/// The ownership map's defaults and every file this command makes. NATS runs as root, so its
/// files need no group; the files of the postgres, Valkey and Neon images are 0640 with
/// the image's group (the readers' containers join it, or `--owners` changes it).
pub const LAYOUT: &[Entry] = &[
    e("secrets/secrets.key", M, Gen::SecretKey, P),
    e(
        "gateway/internal-signing.pem",
        M,
        half(Pair::Internal, true),
        P,
    ),
    e(
        "gateway/internal-verify.pem",
        M,
        half(Pair::Internal, false),
        PUB,
    ),
    e("auth/cookie.key", M, Gen::SecretKey, P),
    e("auth/app-jwt.key", M, half(Pair::AppJwt, true), P),
    e("auth/app-jwt.pub", M, half(Pair::AppJwt, false), PUB),
    e("auth/member-jwt.key", M, half(Pair::MemberJwt, true), P),
    e("auth/member-jwt.pub", M, half(Pair::MemberJwt, false), PUB),
    e("identity/secret", M, Gen::Token32, P),
    e("identity/db.password", M, Gen::Token32, P),
    e("neon/compute-jwt.pem", M, half(Pair::ComputeJwt, true), P),
    e(
        "neon/compute-jwt.pub",
        M,
        half(Pair::ComputeJwt, false),
        PUB,
    ),
    e("compute-ca/ca.key", M, half(Pair::ComputeCa, true), P),
    e("compute-ca/ca.crt", M, half(Pair::ComputeCa, false), PUB),
    e("nats/orchestrator.password", M, Gen::Text32, P),
    e("nats/worker.password", M, Gen::Text32, P),
    e("nats/realtime.password", M, Gen::Text32, P),
    e("nats/runner.password", M, Gen::Text32, P),
    e("nats/storage.password", M, Gen::Text32, P),
    e("valkey/proxy.password", M, Gen::Text32, P),
    e("valkey/control.password", M, Gen::Text32, P),
    e("valkey/realtime.password", M, Gen::Text32, P),
    e("valkey/runner.password", M, Gen::Text32, P),
    // Valkey's healthcheck reads it inside the Valkey container.
    e(
        "valkey/health.password",
        M,
        Gen::Text32,
        Spec::group(VALKEY_GID),
    ),
    e("realtime/pg.password", M, Gen::Text32, P),
    e("realtime/ticket.key", M, Gen::Raw32, P),
    e("realtime/internal.token", M, Gen::Text32, P),
    e("tokens/rest-internal.token", M, Gen::Text32, P),
    e("tokens/data-keys.token", M, Gen::Text32, P),
    e("tokens/app-auth.token", M, Gen::Text32, P),
    e("tokens/app-sessions.token", M, Gen::Text32, P),
    e("tokens/storage-internal.token", M, Gen::Text32, P),
    e("tokens/proxy-api.token", M, Gen::Text32, P),
    e("tokens/storcon-hook.token", M, Gen::Text32, P),
    e("tokens/auth-identity.token", M, Gen::Token32, P),
    e("tokens/orchestrator-identity.token", M, Gen::Token32, P),
    e("tokens/worker-identity.token", M, Gen::Token32, P),
    e("tokens/bootstrap-identity.token", M, Gen::Token32, P),
    e(
        "neon/compute-jwt.pub.pem",
        M,
        Gen::Derived(Derived::StoragePem),
        PUB,
    ),
    e(
        "neon/storage-tokens/pageserver-api.jwt",
        M,
        Gen::Derived(Derived::StorageToken(0)),
        Spec::group(NEON_GID),
    ),
    e(
        "neon/storage-tokens/safekeeper-data.jwt",
        M,
        Gen::Derived(Derived::StorageToken(1)),
        Spec::group(NEON_GID),
    ),
    e(
        "neon/storage-tokens/generations-api.jwt",
        M,
        Gen::Derived(Derived::StorageToken(2)),
        Spec::group(NEON_GID),
    ),
    e(
        "neon/storage-tokens/storcon-admin.jwt",
        M,
        Gen::Derived(Derived::StorageToken(3)),
        Spec::group(NEON_GID),
    ),
    e(
        "neon/storage-tokens/metrics.token",
        M,
        Gen::Derived(Derived::MetricsToken),
        P,
    ),
    e("nats/auth.conf", I, Gen::Derived(Derived::NatsAuth), P),
    e(
        "valkey/users.acl",
        I,
        Gen::Derived(Derived::ValkeyAcl),
        Spec::group(VALKEY_GID),
    ),
    e(
        "db/metadata.password",
        I,
        Gen::Password { dev: "chelabase" },
        Spec::group(POSTGRES_GID),
    ),
    e(
        "db/storcon.password",
        I,
        Gen::Password { dev: "storcon" },
        Spec::group(POSTGRES_GID),
    ),
    e(
        "s3/access-key",
        I,
        Gen::Alnum {
            len: 20,
            dev: "chelabase-dev",
        },
        Spec::group(NEON_GID),
    ),
    e(
        "s3/secret-key",
        I,
        Gen::Alnum {
            len: 40,
            dev: "chelabase-dev-secret",
        },
        Spec::group(NEON_GID),
    ),
    e("setup/setup.code", I, Gen::SetupCode, P),
];

/// The client-pair directories (machine part), each with its certificate's CN. Their insides
/// (`current` is a symlink by design) belong to `chelabase_neon::compute_tls`.
pub const CLIENT_PAIRS: [(&str, &str); 2] = [
    (
        "orchestrator-tls",
        chelabase_neon::compute_tls::ORCHESTRATOR_CN,
    ),
    ("realtime-tls", chelabase_neon::compute_tls::REALTIME_CN),
];

/// The setup code's alphabet: digits and capitals without 0/O, 1/I/L.
pub const SETUP_ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";

/// The lock file in each root.
pub const LOCK_FILE: &str = ".chelabase-init.lock";

/// How [`ensure_secrets`] runs.
#[derive(Debug, Clone)]
pub struct InitSecrets {
    /// The workbench's fixed dev values for the database passwords and the S3 credentials (its
    /// stacks' volumes were made with them).
    pub dev_fixed: bool,
    /// Where the install part goes; `None` is the root itself.
    pub install_dir: Option<PathBuf>,
    pub owners: OwnershipMap,
    /// Accept a directory or public file that already exists with group bits other than the
    /// map's (never world-writable, executable or set-id), as the workbench's shared keys dir
    /// has them from before this check (made under the user's umask). Secrets stay exact.
    pub legacy_modes: bool,
    /// Chosen values for plain generated files ([`crate::seed`]), written instead of a generated
    /// value when the file is missing. Empty: everything is generated.
    pub seeds: Seeds,
}

impl InitSecrets {
    /// A self-hosted install: one root, random values, the given ownership map.
    pub fn self_hosted(owners: OwnershipMap) -> Self {
        InitSecrets {
            dev_fixed: false,
            install_dir: None,
            owners,
            legacy_modes: false,
            seeds: Seeds::default(),
        }
    }

    /// A workbench stack: the machine part in the shared keys dir, the install part in the
    /// stack's state dir, everything the stack user's, the fixed dev values.
    pub fn workbench(stack_state: &Path) -> Self {
        InitSecrets {
            dev_fixed: true,
            install_dir: Some(stack_state.to_path_buf()),
            owners: OwnershipMap::writer_only(),
            legacy_modes: true,
            seeds: Seeds::default(),
        }
    }
}

/// A setup code. Its `Debug` hides it; `Display` shows it.
#[derive(Clone, PartialEq, Eq)]
pub struct SetupCode(String);

impl SetupCode {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SetupCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::fmt::Debug for SetupCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SetupCode(..)")
    }
}

/// What a run did. File paths only, and the setup code when this run created it.
#[derive(Debug, Default)]
pub struct Report {
    /// Files this run generated.
    pub created: Vec<PathBuf>,
    /// Files this run wrote from a seed ([`InitSecrets::seeds`]); not in `created`.
    pub seeded: Vec<PathBuf>,
    /// Layout files that existed and were left as they were.
    pub kept: Vec<PathBuf>,
    pub rewritten: Vec<PathBuf>,
    pub setup_code: Option<SetupCode>,
    /// Whether `nats/auth.conf` or `valkey/users.acl` changed (a running server must restart).
    pub auth_files: AuthFilesChanged,
}

/// Creates what is missing under `dir` (see the module doc).
pub fn ensure_secrets(dir: &Path, opts: &InitSecrets) -> Result<Report> {
    let mut report = Report::default();
    ensure_secrets_into(dir, opts, &mut report)?;
    Ok(report)
}

/// [`ensure_secrets`], filling `report` as it goes, so a caller still sees what a failed run
/// wrote (a setup code above all).
pub fn ensure_secrets_into(dir: &Path, opts: &InitSecrets, report: &mut Report) -> Result<()> {
    let machine = Dir::open_root(dir, opts.legacy_modes)?;
    let machine_path = dir
        .canonicalize()
        .with_context(|| format!("resolving {}", dir.display()))?;
    let install_given = opts.install_dir.as_deref().unwrap_or(dir);
    let install = Dir::open_root(install_given, opts.legacy_modes)?;
    let install_path = install_given
        .canonicalize()
        .with_context(|| format!("resolving {}", install_given.display()))?;
    // One run at a time per root, always machine first.
    let _machine_lock = machine.lock(LOCK_FILE)?;
    let _install_lock = if install_path != machine_path {
        Some(install.lock(LOCK_FILE)?)
    } else {
        None
    };
    let mut run = Run {
        opts,
        roots: [(machine, machine_path), (install, install_path)],
        have: BTreeMap::new(),
        report,
    };
    // Every check, contents included, comes before the first write; the setup code is the
    // last write.
    run.check()?;
    run.create_sources()?;
    run.client_pairs()?;
    run.derive()?;
    run.setup_code()
}

/// The setup code stored under `dir` (the install root).
pub fn read_setup_code(dir: &Path) -> Result<SetupCode> {
    // Reading only: a group-writable (umask-made) root is accepted, a world-writable one isn't.
    let root = Dir::open_root(dir, true)?;
    let setup = root.open_dir("setup")?;
    let bytes = setup.read("setup.code")?;
    let code = String::from_utf8(bytes).ok().map(|s| s.trim().to_string());
    match code {
        Some(c) if is_setup_code(&c) => Ok(SetupCode(c)),
        _ => bail!(
            "{} does not hold a setup code",
            setup.child("setup.code").display()
        ),
    }
}

/// Whether an existing file's contents can be used: a blank value, a key that doesn't parse or
/// a corrupt setup code is refused (by `check`, before anything is written).
fn check_content(entry: &Entry, bytes: &[u8]) -> std::result::Result<(), String> {
    let text = || std::str::from_utf8(bytes).map_err(|_| "not UTF-8 text".to_string());
    let blank = |t: &str| {
        if t.trim().is_empty() {
            Err("blank (only whitespace)".to_string())
        } else {
            Ok(())
        }
    };
    match entry.make {
        Gen::Raw32 if bytes.len() == 32 => Ok(()),
        Gen::Raw32 => Err(format!("holds {} bytes, not 32", bytes.len())),
        Gen::SecretKey => chelabase_secrets::SecretKey::from_base64(text()?)
            .map(|_| ())
            .map_err(|_| "not a base64 32-byte key".to_string()),
        Gen::SetupCode if is_setup_code(text()?.trim()) => Ok(()),
        Gen::SetupCode => Err("not a setup code (XXXX-XXXX-XXXX-XXXX-XXXX)".to_string()),
        Gen::Text32 | Gen::Password { .. } | Gen::Alnum { .. } => blank(text()?),
        Gen::Token32 => {
            let t = text()?;
            blank(t)?;
            if t.trim().len() < 32 {
                Err("shorter than 32 characters".to_string())
            } else {
                Ok(())
            }
        }
        Gen::Pair {
            pair: Pair::Internal | Pair::AppJwt | Pair::MemberJwt,
            private,
        } => {
            let ok = if private {
                chelabase_auth::internal::InternalSigner::from_pem(bytes).is_ok()
            } else {
                chelabase_auth::internal::InternalVerifier::from_pem(bytes).is_ok()
            };
            if ok {
                Ok(())
            } else {
                Err("not a usable Ed25519 PEM key".to_string())
            }
        }
        // Parsed together, in `check_pair`.
        Gen::Pair { .. } => blank(text()?),
        // Kept as it is: it carries the clock.
        Gen::Derived(Derived::MetricsToken) => blank(text()?),
        // Re-rendered from their sources whatever they hold.
        Gen::Derived(_) => Ok(()),
    }
}

/// Whether a pair whose halves parse each on its own also loads as one.
fn check_pair(pair: Pair, private: &[u8], public: &[u8]) -> std::result::Result<(), String> {
    let (k, p) = (
        String::from_utf8_lossy(private),
        String::from_utf8_lossy(public),
    );
    let ok = match pair {
        Pair::Internal | Pair::AppJwt | Pair::MemberJwt => true,
        Pair::ComputeJwt => chelabase_neon::ComputeSigner::new(&k, &p).is_ok(),
        Pair::ComputeCa => chelabase_neon::compute_tls::ComputeCa::from_pem(&p, &k).is_ok(),
    };
    if ok {
        Ok(())
    } else {
        Err("not a usable key pair".to_string())
    }
}

fn is_setup_code(s: &str) -> bool {
    s.len() == 24
        && s.bytes().enumerate().all(|(i, b)| {
            if i % 5 == 4 {
                b == b'-'
            } else {
                SETUP_ALPHABET.contains(&b)
            }
        })
}

fn new_setup_code() -> SetupCode {
    let raw = crate::random::from_alphabet(SETUP_ALPHABET, 20);
    let groups: Vec<&str> = (0..5).map(|i| &raw[i * 4..i * 4 + 4]).collect();
    SetupCode(groups.join("-"))
}

/// One `ensure_secrets` run.
struct Run<'a> {
    opts: &'a InitSecrets,
    /// The machine root and the install root (maybe the same directory), with their paths.
    roots: [(Dir, PathBuf); 2],
    /// The bytes of every layout file that exists (read in the check, or written by this run).
    have: BTreeMap<&'static str, Vec<u8>>,
    report: &'a mut Report,
}

fn part_index(part: Part) -> usize {
    match part {
        Part::Machine => 0,
        Part::Install => 1,
    }
}

/// Every directory of the layout in `part`, parents first.
fn layout_dirs(part: Part) -> Vec<String> {
    let mut dirs = std::collections::BTreeSet::new();
    for entry in LAYOUT.iter().filter(|e| e.part == part) {
        let mut prefix = String::new();
        let parts: Vec<&str> = entry.path.split('/').collect();
        for c in &parts[..parts.len() - 1] {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(c);
            dirs.insert(prefix.clone());
        }
    }
    let mut dirs: Vec<String> = dirs.into_iter().collect();
    dirs.sort_by_key(|d| (d.matches('/').count(), d.clone()));
    dirs
}

fn split(rel: &str) -> (Vec<&str>, &str) {
    let mut parts: Vec<&str> = rel.split('/').collect();
    let name = parts.pop().expect("a non-empty path");
    (parts, name)
}

impl Run<'_> {
    fn root(&self, part: Part) -> &(Dir, PathBuf) {
        &self.roots[part_index(part)]
    }

    fn abs(&self, part: Part, rel: &str) -> PathBuf {
        self.root(part).1.join(rel)
    }

    fn perm(&self, rel: &str) -> Perm {
        self.opts.owners.spec(rel).resolve()
    }

    /// Whether both parts live in one directory (self-hosted).
    fn one_root(&self) -> bool {
        self.roots[0].1 == self.roots[1].1
    }

    /// A directory's owner and mode, from every layout file below it in the same physical root
    /// (both parts when they share one): 0750 with their group when the group-readable ones
    /// share one, else 0700.
    fn dir_perm(&self, part: Part, rel: &str) -> Perm {
        let (wuid, wgid) = crate::fsx::writer();
        let prefix = format!("{rel}/");
        let one_root = self.one_root();
        let files: Vec<Perm> = LAYOUT
            .iter()
            .filter(|e| (e.part == part || one_root) && e.path.starts_with(&prefix))
            .map(|e| self.perm(e.path))
            .collect();
        let shared = |f: fn(&Perm) -> u32, own: u32| {
            let mut ids = files.iter().map(f);
            match ids.next() {
                Some(first) if ids.all(|i| i == first) => first,
                _ => own,
            }
        };
        let uid = shared(|p| p.uid, wuid);
        let groups: std::collections::BTreeSet<u32> = files
            .iter()
            .filter(|p| p.mode & 0o040 != 0 && p.mode & 0o004 == 0)
            .map(|p| p.gid)
            .collect();
        match groups.iter().next() {
            Some(&gid) if groups.len() == 1 => Perm {
                uid,
                gid,
                mode: 0o750,
            },
            _ => Perm {
                uid,
                gid: shared(|p| p.gid, wgid),
                mode: 0o700,
            },
        }
    }

    /// Whether `actual` is acceptable for `want` (see [`InitSecrets::legacy_modes`]).
    fn acceptable(&self, actual: Perm, want: Perm, is_dir: bool) -> bool {
        if actual == want {
            return true;
        }
        let public = is_dir || want.mode & 0o004 != 0;
        let forbidden = if is_dir { 0o7002 } else { 0o7113 };
        self.opts.legacy_modes
            && public
            && actual.uid == want.uid
            && actual.gid == want.gid
            && actual.mode & forbidden == 0
    }

    /// The directory holding `rel`, or `None` when one of its directories doesn't exist yet.
    fn parent(&self, part: Part, rel: &str) -> Result<Option<Dir>> {
        let (dirs, _) = split(rel);
        let root = &self.root(part).0;
        let mut cur: Option<Dir> = None;
        for d in dirs {
            let at = cur.as_ref().unwrap_or(root);
            if at.stat(d)?.is_none() {
                return Ok(None);
            }
            cur = Some(at.open_dir(d)?);
        }
        Ok(Some(match cur {
            Some(d) => d,
            None => root.open_dir_self()?,
        }))
    }

    /// The directory holding `rel`, its missing directories created.
    fn parent_created(&mut self, part: Part, rel: &str) -> Result<Dir> {
        let (dirs, _) = split(rel);
        let mut prefix = String::new();
        let mut cur = self.root(part).0.open_dir_self()?;
        for d in dirs {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(d);
            cur = if cur.stat(d)?.is_none() {
                let perm = if CLIENT_PAIRS.iter().any(|(p, _)| *p == prefix) {
                    let (uid, gid) = crate::fsx::writer();
                    Perm {
                        uid,
                        gid,
                        mode: 0o700,
                    }
                } else {
                    self.dir_perm(part, &prefix)
                };
                cur.create_dir(d, perm)?
            } else {
                cur.open_dir(d)?
            };
        }
        Ok(cur)
    }

    /// Checks every existing entry; fails naming each problem before anything is written.
    fn check(&mut self) -> Result<()> {
        let mut problems: Vec<String> = Vec::new();
        let describe = |path: &Path, what: &str, want: Perm, found: Perm| {
            format!("{}: {what}; expected {want}, found {found}", path.display())
        };
        // Each physical directory once: with one root, the install part's dirs are the
        // machine part's too.
        let one_root = self.one_root();
        for part in [Part::Machine, Part::Install] {
            if part == Part::Install && one_root {
                continue;
            }
            let mut dirs = layout_dirs(part);
            if part == Part::Machine {
                if one_root {
                    dirs.extend(layout_dirs(Part::Install));
                }
                dirs.extend(CLIENT_PAIRS.iter().map(|(d, _)| d.to_string()));
                dirs.sort_by_key(|d| (d.matches('/').count(), d.clone()));
                dirs.dedup();
            }
            for rel in dirs {
                let path = self.abs(part, &rel);
                let (_, name) = split(&rel);
                let parent = match self.parent(part, &rel) {
                    Ok(Some(p)) => p,
                    Ok(None) => continue,
                    Err(e) => {
                        problems.push(format!("{e:#}"));
                        continue;
                    }
                };
                let Some(st) = parent.stat(name)? else {
                    continue;
                };
                match st.kind {
                    Kind::Dir => {}
                    Kind::Symlink => {
                        problems.push(format!("refusing {}: it is a symlink", path.display()));
                        continue;
                    }
                    _ => {
                        problems.push(format!("{} is not a directory", path.display()));
                        continue;
                    }
                }
                if CLIENT_PAIRS.iter().any(|(p, _)| *p == rel) {
                    // Its insides are compute_tls's; only who owns it is checked.
                    // (The workbench's were made under the user's umask: group-writable.)
                    let (uid, _) = crate::fsx::writer();
                    let forbidden = if self.opts.legacy_modes {
                        0o7002
                    } else {
                        0o7022
                    };
                    if st.perm.uid != uid || st.perm.mode & forbidden != 0 {
                        problems.push(format!(
                            "{}: expected owner {uid}, not writable by others, found {}",
                            path.display(),
                            st.perm
                        ));
                    }
                    continue;
                }
                let want = self.dir_perm(part, &rel);
                if !self.acceptable(st.perm, want, true) {
                    problems.push(describe(&path, "wrong owner or mode", want, st.perm));
                }
            }
        }
        for entry in LAYOUT {
            let path = self.abs(entry.part, entry.path);
            let (_, name) = split(entry.path);
            let parent = match self.parent(entry.part, entry.path) {
                Ok(Some(p)) => p,
                Ok(None) => continue,
                Err(e) => {
                    problems.push(format!("{e:#}"));
                    continue;
                }
            };
            let Some(st) = parent.stat(name)? else {
                continue;
            };
            let want = self.perm(entry.path);
            match st.kind {
                Kind::File => {}
                Kind::Symlink => {
                    problems.push(format!("refusing {}: it is a symlink", path.display()));
                    continue;
                }
                _ => {
                    problems.push(format!("{} is not a regular file", path.display()));
                    continue;
                }
            }
            if !self.acceptable(st.perm, want, false) {
                problems.push(describe(&path, "wrong owner or mode", want, st.perm));
                continue;
            }
            if st.size == 0 {
                problems.push(describe(&path, "empty", want, st.perm));
                continue;
            }
            match parent.read(name) {
                Ok(bytes) if !bytes.is_empty() => {
                    if let Err(what) = check_content(entry, &bytes) {
                        problems.push(format!("{}: {what}", path.display()));
                        continue;
                    }
                    self.have.insert(entry.path, bytes);
                }
                Ok(_) => problems.push(describe(&path, "empty", want, st.perm)),
                Err(e) => problems.push(describe(
                    &path,
                    &format!("unreadable ({e:#})"),
                    want,
                    st.perm,
                )),
            }
        }
        // Every file this run would make must be one the writer can give its owner and group.
        for entry in LAYOUT.iter().filter(|e| !self.have.contains_key(e.path)) {
            let want = self.perm(entry.path);
            if !crate::fsx::can_own(want.uid, want.gid) {
                let (uid, gid) = crate::fsx::writer();
                problems.push(format!(
                    "{}: can't be given owner {}:{} by uid {uid} (gid {gid}): run as root, \
                     join that group, or pass --owners",
                    self.abs(entry.part, entry.path).display(),
                    want.uid,
                    want.gid
                ));
            }
        }
        for pair in [
            Pair::Internal,
            Pair::AppJwt,
            Pair::MemberJwt,
            Pair::ComputeJwt,
            Pair::ComputeCa,
        ] {
            let (private, public) = pair_paths(pair);
            if let (Some(k), Some(p)) = (self.have.get(private), self.have.get(public))
                && let Err(what) = check_pair(pair, k, p)
            {
                problems.push(format!(
                    "{} / {}: {what}",
                    self.abs(Part::Machine, private).display(),
                    self.abs(Part::Machine, public).display()
                ));
            }
            if self.have.contains_key(private) != self.have.contains_key(public) {
                let path = self.abs(Part::Machine, private);
                problems.push(format!(
                    "only one of {private} / {public} exists in {}; delete both to regenerate",
                    path.parent().unwrap_or(&path).display()
                ));
            }
        }
        // A seed applies on first creation only: an existing file with another value fails the
        // run (naming the key and file, never either value) rather than doing nothing silently.
        for entry in LAYOUT {
            let (Some((seed, value)), Some(bytes)) =
                (self.opts.seeds.get(entry.path), self.have.get(entry.path))
            else {
                continue;
            };
            if !crate::seed::same_value(bytes, value) {
                problems.push(format!(
                    "{}: {} differs from what the file holds. Seeds apply only when a file is \
                     first created; to keep the file, remove the {} line from the seeds file \
                     (.env); changing a value is a key rotation (chelabase/chelabase#6)",
                    self.abs(entry.part, entry.path).display(),
                    seed.key,
                    seed.key
                ));
            }
        }
        problems.dedup();
        if !problems.is_empty() {
            bail!(
                "nothing was changed; fix these first:\n  {}",
                problems.join("\n  ")
            );
        }
        for entry in LAYOUT.iter().filter(|e| self.have.contains_key(e.path)) {
            let path = self.abs(entry.part, entry.path);
            self.report.kept.push(path);
        }
        Ok(())
    }

    /// Writes `rel` as a new file at its map entry; one that appears meanwhile is an error.
    fn put_new(&mut self, entry: &Entry, bytes: Vec<u8>) -> Result<()> {
        let path = self.put_new_file(entry, bytes)?;
        self.report.created.push(path);
        Ok(())
    }

    /// [`Run::put_new`] for a seed: the same write path, reported under `seeded`.
    fn put_seeded(&mut self, entry: &Entry, bytes: Vec<u8>) -> Result<()> {
        let path = self.put_new_file(entry, bytes)?;
        self.report.seeded.push(path);
        Ok(())
    }

    fn put_new_file(&mut self, entry: &Entry, bytes: Vec<u8>) -> Result<PathBuf> {
        let dir = self.parent_created(entry.part, entry.path)?;
        let (_, name) = split(entry.path);
        let path = self.abs(entry.part, entry.path);
        if !dir.create_new(name, &bytes, self.perm(entry.path))? {
            bail!("{} appeared during the run; run it again", path.display());
        }
        self.have.insert(entry.path, bytes);
        Ok(path)
    }

    fn entry(path: &str) -> &'static Entry {
        LAYOUT
            .iter()
            .find(|e| e.path == path)
            .expect("a layout path")
    }

    fn create_sources(&mut self) -> Result<()> {
        let dev = self.opts.dev_fixed;
        for entry in LAYOUT {
            if self.have.contains_key(entry.path) {
                continue;
            }
            // `crate::seed::SEED_KEYS` names plain generated values only (its tests pin that).
            if let Some((_, value)) = self.opts.seeds.get(entry.path) {
                let bytes = value.as_bytes().to_vec();
                self.put_seeded(entry, bytes)?;
                continue;
            }
            let bytes = match entry.make {
                Gen::SecretKey => chelabase_secrets::SecretKey::generate()
                    .to_base64()
                    .into_bytes(),
                Gen::Text32 | Gen::Token32 => crate::random::text32().into_bytes(),
                Gen::Raw32 => crate::random::bytes::<32>().to_vec(),
                Gen::Password { dev: fixed } if dev => fixed.as_bytes().to_vec(),
                Gen::Password { .. } => crate::random::text32().into_bytes(),
                Gen::Alnum { dev: fixed, .. } if dev => fixed.as_bytes().to_vec(),
                Gen::Alnum { len, .. } => {
                    crate::random::from_alphabet(crate::random::ALPHANUMERIC, len).into_bytes()
                }
                // Written last, by `setup_code`.
                Gen::SetupCode => continue,
                Gen::Pair {
                    pair,
                    private: true,
                } => {
                    self.create_pair(pair)?;
                    continue;
                }
                // Made with its private half; `check` refused half a pair.
                Gen::Pair { private: false, .. } | Gen::Derived(_) => continue,
            };
            self.put_new(entry, bytes)?;
        }
        Ok(())
    }

    /// The setup code, when missing: the run's last write, so no later step can fail and leave a
    /// code that was never shown.
    fn setup_code(&mut self) -> Result<()> {
        let entry = Self::entry("setup/setup.code");
        if self.have.contains_key(entry.path) {
            return Ok(());
        }
        let code = new_setup_code();
        self.put_new(entry, code.as_str().as_bytes().to_vec())?;
        self.report.setup_code = Some(code);
        Ok(())
    }

    /// Both halves of a pair; if the public half can't be written, the private one goes again.
    fn create_pair(&mut self, pair: Pair) -> Result<()> {
        let (private, public) = match pair {
            Pair::Internal | Pair::AppJwt | Pair::MemberJwt => {
                chelabase_auth::internal::generate_keypair_pem()
            }
            Pair::ComputeJwt => {
                let (pem, x) = chelabase_neon::jwt::generate_keypair();
                (pem, format!("{x}\n"))
            }
            Pair::ComputeCa => {
                let ca = chelabase_neon::compute_tls::ComputeCa::generate(SystemTime::now())?;
                (ca.key_pem().to_string(), ca.cert_pem().to_string())
            }
        };
        let (private_path, public_path) = pair_paths(pair);
        self.put_new(Self::entry(private_path), private.into_bytes())?;
        if let Err(e) = self.put_new(Self::entry(public_path), public.into_bytes()) {
            let dir = self.parent_created(Part::Machine, private_path)?;
            dir.remove(split(private_path).1);
            self.have.remove(private_path);
            self.report.created.pop();
            return Err(e);
        }
        Ok(())
    }

    /// The orchestrator's and realtime's client pairs, issued or renewed when due.
    fn client_pairs(&mut self) -> Result<()> {
        let ca = chelabase_neon::compute_tls::ComputeCa::from_pem(
            &text(&self.have, "compute-ca/ca.crt")?,
            &text(&self.have, "compute-ca/ca.key")?,
        )?;
        let now = SystemTime::now();
        for (dir, cn) in CLIENT_PAIRS {
            self.parent_created(Part::Machine, &format!("{dir}/current"))?;
            let path = self.abs(Part::Machine, dir);
            if chelabase_neon::compute_tls::renew_client_pair(&path, &ca, cn, now)? {
                self.report.created.push(path);
            }
        }
        Ok(())
    }

    /// The derived files, from the sources as they are now.
    fn derive(&mut self) -> Result<()> {
        use crate::kinds::service_auth::{
            STORAGE_TOKENS, render_nats_auth_reusing, render_valkey_acl,
        };
        use crate::kinds::stack_secrets::{NATS_USERS, VALKEY_USERS};
        let signer = chelabase_neon::ComputeSigner::new(
            &text(&self.have, "neon/compute-jwt.pem")?,
            &text(&self.have, "neon/compute-jwt.pub")?,
        )?;
        for entry in LAYOUT {
            let Gen::Derived(kind) = entry.make else {
                continue;
            };
            let rendered = match kind {
                Derived::StoragePem => signer.public_key_pem().to_string(),
                Derived::StorageToken(i) => {
                    let (scope, file) = &STORAGE_TOKENS[i];
                    anyhow::ensure!(entry.path.ends_with(file), "{} is not {file}", entry.path);
                    signer.storage_token(scope)?
                }
                // Minted once: it carries the clock.
                Derived::MetricsToken if self.have.contains_key(entry.path) => continue,
                Derived::MetricsToken => signer.metrics_token(
                    SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .context("the clock is before 1970")?
                        .as_secs(),
                )?,
                Derived::NatsAuth => {
                    let previous = self
                        .have
                        .get(entry.path)
                        .map(|b| {
                            crate::kinds::service_auth::previous_nats_hashes(
                                &String::from_utf8_lossy(b),
                            )
                        })
                        .unwrap_or_default();
                    render_nats_auth_reusing(
                        &passwords(&self.have, "nats", &NATS_USERS)?,
                        &previous,
                    )?
                }
                Derived::ValkeyAcl => {
                    render_valkey_acl(&passwords(&self.have, "valkey", &VALKEY_USERS)?)?
                }
            };
            let changed = match self.have.get(entry.path) {
                Some(old) if *old == rendered.as_bytes() => false,
                Some(_) => {
                    let dir = self.parent_created(entry.part, entry.path)?;
                    dir.replace(
                        split(entry.path).1,
                        rendered.as_bytes(),
                        self.perm(entry.path),
                    )?;
                    let path = self.abs(entry.part, entry.path);
                    self.report.kept.retain(|p| *p != path);
                    self.report.rewritten.push(path);
                    self.have.insert(entry.path, rendered.into_bytes());
                    true
                }
                None => {
                    self.put_new(entry, rendered.into_bytes())?;
                    true
                }
            };
            match kind {
                Derived::NatsAuth => self.report.auth_files.nats = changed,
                Derived::ValkeyAcl => self.report.auth_files.valkey = changed,
                _ => {}
            }
        }
        Ok(())
    }
}

/// The users' passwords from `<dir>/<user>.password`, without a trailing line break.
fn passwords(
    have: &BTreeMap<&'static str, Vec<u8>>,
    dir: &str,
    users: &[&str],
) -> Result<BTreeMap<String, String>> {
    users
        .iter()
        .map(|u| {
            let v = text(have, &format!("{dir}/{u}.password"))?;
            Ok((u.to_string(), v.trim_end_matches(['\n', '\r']).to_string()))
        })
        .collect()
}

fn pair_paths(pair: Pair) -> (&'static str, &'static str) {
    let find = |private: bool| {
        LAYOUT
            .iter()
            .find(|e| e.make == Gen::Pair { pair, private })
            .expect("both halves are in the layout")
            .path
    };
    (find(true), find(false))
}

fn text(have: &BTreeMap<&'static str, Vec<u8>>, path: &str) -> Result<String> {
    let bytes = have
        .get(path)
        .with_context(|| format!("{path} is missing"))?;
    String::from_utf8(bytes.clone()).with_context(|| format!("{path} is not text"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    /// The default map with the test user's own ids (a non-root writer can't chgrp to 70).
    fn opts() -> InitSecrets {
        InitSecrets::self_hosted(OwnershipMap::default().with_all(Id::Writer, Id::Writer))
    }

    /// A temporary dir that is the writer's alone (tempfile follows the umask, which may give
    /// the group write; such a root is refused).
    fn tmp() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt as _;
        let t = tempfile::tempdir().unwrap();
        std::fs::set_permissions(t.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        t
    }

    fn me() -> (u32, u32) {
        crate::fsx::writer()
    }

    /// Every regular file below `root` (lock file excluded): bytes, mtime (s, ns) and mode.
    type Snap = BTreeMap<PathBuf, (Vec<u8>, i64, i64, u32)>;

    fn snapshot(root: &Path) -> Snap {
        let mut out = BTreeMap::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(d) = stack.pop() {
            for entry in std::fs::read_dir(&d).unwrap() {
                let p = entry.unwrap().path();
                let meta = std::fs::symlink_metadata(&p).unwrap();
                if meta.is_dir() {
                    stack.push(p);
                } else if meta.is_file() && !p.ends_with(LOCK_FILE) {
                    out.insert(
                        p.strip_prefix(root).unwrap().to_path_buf(),
                        (
                            std::fs::read(&p).unwrap_or_default(),
                            meta.mtime(),
                            meta.mtime_nsec(),
                            meta.mode(),
                        ),
                    );
                }
            }
        }
        out
    }

    fn compose_file(name: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../infra/compose")
            .join(name);
        std::fs::read_to_string(path).unwrap()
    }

    /// The workbench: the files the overlay's top-level `secrets:` block reads from
    /// `${CHELABASE_KEYS_DIR}`, all of which chelabase-init makes; the others are the object
    /// store's dev keys and gRPC TLS set in the stack dir, which `chelabase workbench up` writes
    /// (`ensure_object_store_dev_files`). The one other file it may read is the stack's setup
    /// code, which chelabase-init writes into the stack dir (#126).
    fn workbench_secret_files() -> Vec<String> {
        const STACK_SETUP_CODE: &str = "setup/setup.code";
        assert!(
            LAYOUT.iter().any(|e| e.path == STACK_SETUP_CODE),
            "{STACK_SETUP_CODE} is in chelabase-init's layout"
        );
        let text = compose_file("docker-compose.workbench.yml");
        let block = text
            .split("\nsecrets:\n")
            .nth(1)
            .expect("a top-level secrets block in the workbench overlay");
        let mut keys = Vec::new();
        let mut others = Vec::new();
        for line in block
            .lines()
            .take_while(|l| l.is_empty() || l.starts_with(' '))
        {
            let Some(rest) = line.split("file: \"").nth(1) else {
                continue;
            };
            let file = rest.split('"').next().unwrap();
            if let Some(p) = file.strip_prefix("${CHELABASE_KEYS_DIR}/") {
                keys.push(p.to_string());
            } else if file.strip_prefix("${CHELABASE_STACK_DIR}/") == Some(STACK_SETUP_CODE) {
                // Not in the keys dir, so not checked against a keys-dir run below.
            } else {
                others.push(file.to_string());
            }
        }
        assert_eq!(
            others,
            [
                "${CHELABASE_STACK_DIR}/s3/access-key",
                "${CHELABASE_STACK_DIR}/s3/secret-key",
                "${CHELABASE_STACK_DIR}/storage-s3/access-key",
                "${CHELABASE_STACK_DIR}/storage-s3/secret-key",
                "${CHELABASE_STACK_DIR}/grpc-tls/ca.crt",
                "${CHELABASE_STACK_DIR}/grpc-tls/server.crt",
                "${CHELABASE_STACK_DIR}/grpc-tls/server.key",
                "${CHELABASE_STACK_DIR}/grpc-tls/client.crt",
                "${CHELABASE_STACK_DIR}/grpc-tls/client.key",
            ],
            "a workbench secret outside the keys dir that neither chelabase-init nor the workbench makes"
        );
        assert!(keys.len() > 30, "{keys:?}");
        keys
    }

    /// Self-hosted: every `subpath` the base compose file mounts from the `secrets` volume,
    /// which `chelabase-init secrets` fills. The others are in the dirs init-secrets' own script
    /// makes after it (the bootstrap's outputs, and the object store's `storage` keys and gRPC
    /// TLS set).
    fn selfhost_secret_paths() -> Vec<String> {
        const SCRIPT_DIRS: [&str; 1] = ["bootstrap"];
        const SCRIPT_FILES: [&str; 2] = ["storage-s3", "grpc-tls"];
        let text = compose_file("docker-compose.yml");
        let mut paths = Vec::new();
        let mut others = Vec::new();
        for line in text.lines().filter(|l| l.contains("source: secrets,")) {
            let Some(rest) = line.split("subpath: ").nth(1) else {
                continue;
            };
            let sub = rest.split(['}', ',', ' ']).next().unwrap().to_string();
            let top = sub.split('/').next().unwrap();
            let list = if SCRIPT_DIRS.contains(&top) || SCRIPT_FILES.contains(&top) {
                &mut others
            } else {
                &mut paths
            };
            if !list.contains(&sub) {
                list.push(sub);
            }
        }
        others.sort();
        assert_eq!(
            others,
            [
                "bootstrap",
                "grpc-tls/ca.crt",
                "grpc-tls/client.crt",
                "grpc-tls/client.key",
                "grpc-tls/server.crt",
                "grpc-tls/server.key",
                "storage-s3/access-key",
                "storage-s3/secret-key",
            ],
            "a self-hosted mount outside chelabase-init's layout that init-secrets doesn't make"
        );
        let script = text.split("\n  init-secrets:\n").nth(1).unwrap();
        assert!(
            script.contains(&format!("for d in {}; do", SCRIPT_DIRS.join(" "))),
            "init-secrets makes {SCRIPT_DIRS:?}"
        );
        for made in ["new_key storage-s3/access-key 20", "new_key storage-s3/secret-key 40", "T=\"$$S/grpc-tls\""] {
            assert!(script.contains(made), "init-secrets runs {made}");
        }
        assert!(paths.len() > 30, "{paths:?}");
        paths
    }

    fn mode(p: &Path) -> u32 {
        std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn creates_full_layout() {
        let t = tmp();
        let o = opts();
        let report = ensure_secrets(t.path(), &o).unwrap();
        for file in workbench_secret_files() {
            let p = t.path().join(&file);
            assert!(
                p.is_file(),
                "{file} (named by the workbench) was not created"
            );
            assert!(std::fs::metadata(&p).unwrap().len() > 0, "{file} is empty");
        }
        // A subpath can be a file or a dir; a file must have a value.
        for sub in selfhost_secret_paths() {
            let p = t.path().join(&sub);
            let meta = std::fs::symlink_metadata(&p).unwrap_or_else(|_| {
                panic!("{sub} (mounted by self-hosted compose) was not created")
            });
            assert!(meta.is_dir() || meta.len() > 0, "{sub} is empty");
        }
        let (uid, gid) = me();
        for entry in LAYOUT {
            let p = t.path().join(entry.path);
            let perm = o.owners.spec(entry.path).resolve();
            let meta = std::fs::symlink_metadata(&p).unwrap();
            assert!(meta.is_file(), "{}", entry.path);
            assert_eq!(
                (meta.uid(), meta.gid(), mode(&p)),
                (perm.uid, perm.gid, perm.mode),
                "{}",
                entry.path
            );
        }
        for dir in [
            "db",
            "setup",
            "s3",
            "secrets",
            "compute-ca",
            "neon/storage-tokens",
        ] {
            let m = mode(&t.path().join(dir));
            assert!(m == 0o700 || m == 0o750, "{dir}: {m:o}");
            assert_eq!(std::fs::metadata(t.path().join(dir)).unwrap().uid(), uid);
        }
        // A directory whose files are group-readable is 0750, else 0700.
        assert_eq!(mode(&t.path().join("db")), 0o750);
        assert_eq!(mode(&t.path().join("setup")), 0o700);
        assert_eq!(std::fs::metadata(t.path().join("db")).unwrap().gid(), gid);
        for (dir, _) in CLIENT_PAIRS {
            for f in ["client.crt", "client.key", "ca.crt"] {
                assert!(
                    t.path().join(dir).join("current").join(f).is_file(),
                    "{dir}/{f}"
                );
            }
        }
        // The values have their formats.
        let read = |p: &str| std::fs::read_to_string(t.path().join(p)).unwrap();
        assert!(
            chelabase_secrets::SecretKey::from_file(&t.path().join("secrets/secrets.key")).is_ok()
        );
        let code = report.setup_code.expect("created on this run");
        assert_eq!(read("setup/setup.code"), code.as_str());
        assert!(report.auth_files.nats && report.auth_files.valkey);
        assert!(t.path().join("nats/auth.conf").read_link().is_err());
    }

    #[test]
    fn rerun_changes_nothing() {
        let t = tmp();
        ensure_secrets(t.path(), &opts()).unwrap();
        let before = snapshot(t.path());
        let again = ensure_secrets(t.path(), &opts()).unwrap();
        assert_eq!(snapshot(t.path()), before);
        assert!(again.setup_code.is_none(), "no setup code on a rerun");
        assert!(
            again.created.is_empty() && again.rewritten.is_empty(),
            "{again:?}"
        );
        assert!(!again.auth_files.nats && !again.auth_files.valkey);
    }

    #[test]
    fn fills_only_missing() {
        let t = tmp();
        ensure_secrets(t.path(), &opts()).unwrap();
        let before = snapshot(t.path());

        // A plain secret: only it is made again.
        std::fs::remove_file(t.path().join("db/storcon.password")).unwrap();
        let r = ensure_secrets(t.path(), &opts()).unwrap();
        assert_eq!(r.created, [t.path().join("db/storcon.password")]);
        let after = snapshot(t.path());
        for (p, v) in &before {
            if p != Path::new("db/storcon.password") {
                assert_eq!(after.get(p), Some(v), "{}", p.display());
            }
        }

        // Derived files come back from the sources that are there, byte for byte.
        let before = snapshot(t.path());
        for f in [
            "nats/auth.conf",
            "valkey/users.acl",
            "neon/storage-tokens/pageserver-api.jwt",
            "neon/compute-jwt.pub.pem",
        ] {
            std::fs::remove_file(t.path().join(f)).unwrap();
        }
        let r = ensure_secrets(t.path(), &opts()).unwrap();
        assert_eq!(r.created.len(), 4, "{r:?}");
        let after = snapshot(t.path());
        for p in [
            "valkey/users.acl",
            "neon/storage-tokens/pageserver-api.jwt",
            "neon/compute-jwt.pub.pem",
            "nats/worker.password",
            "neon/compute-jwt.pem",
        ] {
            assert_eq!(after[Path::new(p)].0, before[Path::new(p)].0, "{p}");
        }
        // auth.conf's bcrypt salts differ, but every hash is of the kept password.
        let auth = String::from_utf8(after[Path::new("nats/auth.conf")].0.clone()).unwrap();
        let hashes = crate::kinds::service_auth::previous_nats_hashes(&auth);
        for u in crate::kinds::stack_secrets::NATS_USERS {
            let pw = String::from_utf8(before[Path::new(&format!("nats/{u}.password"))].0.clone())
                .unwrap();
            assert!(bcrypt::verify(&pw, &hashes[u]).unwrap(), "{u}");
        }

        // A new source password re-renders its derived file from it.
        std::fs::remove_file(t.path().join("valkey/proxy.password")).unwrap();
        let r = ensure_secrets(t.path(), &opts()).unwrap();
        assert_eq!(r.rewritten, [t.path().join("valkey/users.acl")]);
        assert!(r.auth_files.valkey && !r.auth_files.nats);
    }

    #[test]
    fn refuses_empty_or_loose_file() {
        let t = tmp();
        ensure_secrets(t.path(), &opts()).unwrap();
        let (uid, gid) = me();
        type Spoil = Box<dyn Fn(&Path)>;
        let cases: [(&str, Spoil, &str); 5] = [
            (
                "tokens/app-auth.token",
                Box::new(|p| std::fs::write(p, "").unwrap()),
                "empty",
            ),
            (
                "db/metadata.password",
                Box::new(|p| {
                    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o644)).unwrap()
                }),
                "mode 0644",
            ),
            (
                "valkey/users.acl",
                Box::new(|p| {
                    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600)).unwrap()
                }),
                "mode 0600",
            ),
            (
                "setup/setup.code",
                Box::new(|p| {
                    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o000)).unwrap()
                }),
                "mode 0000",
            ),
            (
                "db",
                Box::new(|p| {
                    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap()
                }),
                "mode 0755",
            ),
        ];
        for (file, spoil, found) in cases {
            let p = t.path().join(file);
            let original = mode(&p);
            spoil(&p);
            let before = snapshot(t.path());
            let err = format!("{:#}", ensure_secrets(t.path(), &opts()).unwrap_err());
            assert!(err.contains(&p.display().to_string()), "{file}: {err}");
            assert!(err.contains(found), "{file}: {err}");
            assert!(err.contains(&format!("owner {uid}:{gid}")), "{file}: {err}");
            assert!(err.contains("expected"), "{file}: {err}");
            assert_eq!(snapshot(t.path()), before, "{file}: nothing changes");
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(original)).unwrap();
        }
        // Half a key pair is refused too, and not completed.
        std::fs::remove_file(t.path().join("gateway/internal-verify.pem")).unwrap();
        let err = format!("{:#}", ensure_secrets(t.path(), &opts()).unwrap_err());
        assert!(
            err.contains("internal-verify.pem") && err.contains("delete both"),
            "{err}"
        );
        assert!(!t.path().join("gateway/internal-verify.pem").exists());
    }

    #[test]
    fn ownership_map_applied() {
        let t = tmp();
        let (uid, gid) = me();
        let owners = t.path().join("owners.json");
        std::fs::write(
            &owners,
            format!(r#"{{"all": {{"uid": {uid}, "gid": {gid}}}, "files": {{"secrets/secrets.key": {{"gid": {gid}, "mode": "0640"}}}}}}"#),
        )
        .unwrap();
        let map =
            OwnershipMap::from_override_json(&std::fs::read_to_string(&owners).unwrap()).unwrap();
        let root = t.path().join("root");
        ensure_secrets(&root, &InitSecrets::self_hosted(map)).unwrap();
        let check = |p: &str, m: u32| {
            let meta = std::fs::metadata(root.join(p)).unwrap();
            assert_eq!(
                (meta.uid(), meta.gid(), meta.mode() & 0o7777),
                (uid, gid, m),
                "{p}"
            );
        };
        // A consumer-group file of the defaults, and one the override made group-readable.
        check("db/metadata.password", 0o640);
        check("secrets/secrets.key", 0o640);
        check("secrets", 0o750);
        check("tokens/app-auth.token", 0o600);
        check("tokens", 0o700);
        check("auth/app-jwt.pub", 0o644);
        // The workbench map: everything owner-only but public files.
        let wb = OwnershipMap::writer_only();
        assert_eq!(wb.spec("db/metadata.password").mode, 0o600);
        assert_eq!(wb.spec("auth/app-jwt.pub").mode, 0o644);
        // The self-hosted defaults name the images' groups.
        let d = OwnershipMap::default();
        assert_eq!(d.spec("db/metadata.password"), Spec::group(POSTGRES_GID));
        assert_eq!(d.spec("valkey/users.acl"), Spec::group(VALKEY_GID));
        assert_eq!(d.spec("secrets/secrets.key"), Spec::PRIVATE);
    }

    #[test]
    fn a_map_the_writer_cant_apply_writes_nothing() {
        if crate::fsx::writer().0 == 0 {
            return; // root can give any owner
        }
        let t = tmp();
        let root = t.path().join("root");
        let map = OwnershipMap::default().with_all(Id::Is(crate::fsx::writer().0 + 1), Id::Writer);
        let err = format!(
            "{:#}",
            ensure_secrets(&root, &InitSecrets::self_hosted(map)).unwrap_err()
        );
        assert!(err.contains("run as root"), "{err}");
        let left: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            left,
            [std::ffi::OsString::from(LOCK_FILE)],
            "nothing but the lock file"
        );
    }

    #[test]
    fn refuses_symlinks() {
        let elsewhere = tmp();
        // A symlink in place of a file.
        let t = tmp();
        ensure_secrets(t.path(), &opts()).unwrap();
        let file = t.path().join("tokens/app-auth.token");
        std::fs::remove_file(&file).unwrap();
        std::os::unix::fs::symlink(elsewhere.path().join("stolen"), &file).unwrap();
        let err = format!("{:#}", ensure_secrets(t.path(), &opts()).unwrap_err());
        assert!(
            err.contains("symlink") && err.contains("app-auth.token"),
            "{err}"
        );
        assert!(
            !elsewhere.path().join("stolen").exists(),
            "nothing written through it"
        );

        // A symlink in place of a directory, on a fresh root.
        for dir in ["db", "neon", "orchestrator-tls", "setup"] {
            let t = tmp();
            let target = tmp();
            std::os::unix::fs::symlink(target.path(), t.path().join(dir)).unwrap();
            let err = format!("{:#}", ensure_secrets(t.path(), &opts()).unwrap_err());
            assert!(err.contains("symlink"), "{dir}: {err}");
            assert_eq!(
                std::fs::read_dir(target.path()).unwrap().count(),
                0,
                "{dir}: nothing written through it"
            );
            assert!(
                !t.path().join("secrets").exists(),
                "{dir}: nothing written at all"
            );
        }

        // The root itself.
        let t = tmp();
        let link = t.path().join("link");
        std::os::unix::fs::symlink(elsewhere.path(), &link).unwrap();
        assert!(ensure_secrets(&link, &opts()).is_err());
        assert_eq!(std::fs::read_dir(elsewhere.path()).unwrap().count(), 0);
    }

    #[test]
    fn setup_code_reprint() {
        let t = tmp();
        let code = ensure_secrets(t.path(), &opts())
            .unwrap()
            .setup_code
            .unwrap();
        assert_eq!(read_setup_code(t.path()).unwrap(), code);
        let s = code.as_str();
        assert_eq!(s.len(), 24);
        for (i, c) in s.chars().enumerate() {
            if i % 5 == 4 {
                assert_eq!(c, '-', "{s}");
            } else {
                assert!(SETUP_ALPHABET.contains(&(c as u8)), "{s}");
            }
        }
        for lookalike in ['0', 'O', '1', 'I', 'L'] {
            assert!(!SETUP_ALPHABET.contains(&(lookalike as u8)));
        }
        assert_eq!(format!("{code:?}"), "SetupCode(..)");
        assert!(read_setup_code(tmp().path()).is_err());
    }

    #[test]
    fn workbench_stacks_get_own_setup_code() {
        let t = tmp();
        let keys = t.path().join("keys");
        let (a, b) = (t.path().join("stacks/a"), t.path().join("stacks/b"));
        let ra = ensure_secrets(&keys, &InitSecrets::workbench(&a)).unwrap();
        let machine = snapshot(&keys);
        let rb = ensure_secrets(&keys, &InitSecrets::workbench(&b)).unwrap();
        assert_eq!(
            snapshot(&keys),
            machine,
            "the machine part is shared, not remade"
        );
        let (ca, cb) = (ra.setup_code.unwrap(), rb.setup_code.unwrap());
        assert_ne!(ca, cb);
        assert_eq!(read_setup_code(&a).unwrap(), ca);
        assert_eq!(read_setup_code(&b).unwrap(), cb);
        assert!(!keys.join("setup").exists() && !keys.join("db").exists());
        // Fixed dev values, owner-only files.
        let read = |p: PathBuf| std::fs::read_to_string(p).unwrap();
        assert_eq!(read(a.join("db/metadata.password")), "chelabase");
        assert_eq!(mode(&a.join("db/metadata.password")), 0o600);
        assert!(a.join("nats/auth.conf").is_file() && b.join("valkey/users.acl").is_file());
    }

    #[test]
    fn workbench_accepts_its_umask_made_keys_dir() {
        // The shared keys dir from before: public files and dirs made under umask 002.
        let t = tmp();
        let (keys, state) = (t.path().join("keys"), t.path().join("state"));
        ensure_secrets(&keys, &InitSecrets::workbench(&state)).unwrap();
        for p in ["auth", "gateway", "neon", "secrets"] {
            std::fs::set_permissions(keys.join(p), std::fs::Permissions::from_mode(0o775)).unwrap();
        }
        for p in [
            "compute-ca/ca.crt",
            "neon/compute-jwt.pub",
            "gateway/internal-verify.pem",
        ] {
            std::fs::set_permissions(keys.join(p), std::fs::Permissions::from_mode(0o664)).unwrap();
        }
        let before = snapshot(&keys);
        ensure_secrets(&keys, &InitSecrets::workbench(&state)).unwrap();
        assert_eq!(snapshot(&keys), before);
        // Never a loose secret, nor a world-writable public file.
        std::fs::set_permissions(
            keys.join("compute-ca/ca.crt"),
            std::fs::Permissions::from_mode(0o666),
        )
        .unwrap();
        assert!(ensure_secrets(&keys, &InitSecrets::workbench(&state)).is_err());
        std::fs::set_permissions(
            keys.join("compute-ca/ca.crt"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        std::fs::set_permissions(
            keys.join("secrets/secrets.key"),
            std::fs::Permissions::from_mode(0o640),
        )
        .unwrap();
        assert!(ensure_secrets(&keys, &InitSecrets::workbench(&state)).is_err());
        // Self-hosted runs never accept them.
        std::fs::set_permissions(
            keys.join("secrets/secrets.key"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let strict = InitSecrets {
            install_dir: Some(state.clone()),
            ..InitSecrets::self_hosted(OwnershipMap::writer_only())
        };
        assert!(ensure_secrets(&keys, &strict).is_err());
    }

    #[test]
    fn concurrent_runs_leave_one_set() {
        for _ in 0..3 {
            let t = tmp();
            let root = t.path().to_path_buf();
            let runs: Vec<_> = (0..2)
                .map(|_| {
                    let root = root.clone();
                    std::thread::spawn(move || ensure_secrets(&root, &opts()).unwrap())
                })
                .collect();
            let reports: Vec<Report> = runs.into_iter().map(|h| h.join().unwrap()).collect();
            assert_eq!(reports.iter().filter(|r| r.setup_code.is_some()).count(), 1);
            // One consistent set: the pairs match, and a third run changes nothing.
            let before = snapshot(&root);
            let r = ensure_secrets(&root, &opts()).unwrap();
            assert!(r.created.is_empty() && r.rewritten.is_empty(), "{r:?}");
            assert_eq!(snapshot(&root), before);
            let signer = chelabase_auth::internal::InternalSigner::from_pem_file(
                &root.join("gateway/internal-signing.pem"),
            )
            .unwrap();
            let verifier = chelabase_auth::internal::InternalVerifier::from_pem_file(
                &root.join("gateway/internal-verify.pem"),
            )
            .unwrap();
            let id = chelabase_auth::internal::Identity {
                sub: "u".into(),
                auth: chelabase_auth::internal::AuthMethod::ManagementJwt,
                request_id: "r".into(),
                data: None,
                email: None,
                name: None,
            };
            verifier
                .verify(&signer.sign("a", &id).unwrap(), "a")
                .unwrap();
            let left: Vec<_> = snapshot(&root)
                .into_keys()
                .filter(|p| p.to_string_lossy().ends_with(".tmp"))
                .collect();
            assert!(left.is_empty(), "{left:?}");
        }
    }

    #[test]
    fn setup_code_printed_once() {
        let t = tmp();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        crate::cli::secrets(t.path(), &opts(), None, &mut out, &mut err).unwrap();
        let code = read_setup_code(t.path()).unwrap();
        let out1 = String::from_utf8(out).unwrap();
        assert_eq!(out1, format!("Setup code: {code}\n"));
        let (mut out, mut err2) = (Vec::new(), Vec::new());
        crate::cli::secrets(t.path(), &opts(), None, &mut out, &mut err2).unwrap();
        assert!(out.is_empty(), "{}", String::from_utf8_lossy(&out));
        assert!(!String::from_utf8_lossy(&err2).contains(code.as_str()));
        let mut out = Vec::new();
        crate::cli::setup_code(t.path(), &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!("{code}\n"),
            "the code and nothing else"
        );
        let _ = err;
    }

    #[test]
    fn output_has_no_secrets() {
        let t = tmp();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        crate::cli::secrets(t.path(), &opts(), None, &mut out, &mut err).unwrap();
        std::fs::remove_file(t.path().join("valkey/proxy.password")).unwrap();
        crate::cli::secrets(t.path(), &opts(), None, &mut out, &mut err).unwrap();
        // A failing run's message too.
        std::fs::write(t.path().join("tokens/app-auth.token"), "").unwrap();
        let failure = format!(
            "{:#}",
            crate::cli::secrets(t.path(), &opts(), None, &mut out, &mut err).unwrap_err()
        );
        let code = read_setup_code(t.path()).unwrap();
        let mut text =
            String::from_utf8(out)
                .unwrap()
                .replacen(&format!("Setup code: {code}\n"), "", 1);
        text.push_str(&String::from_utf8(err).unwrap());
        text.push_str(&failure);
        assert!(
            !text.contains(code.as_str()),
            "the setup code only once, on stdout"
        );
        let mut values = Vec::new();
        for (path, (bytes, ..)) in snapshot(t.path()) {
            let s = String::from_utf8_lossy(&bytes).into_owned();
            // Every line of every file (PEM bodies, hashes, the steps file's password).
            for line in s
                .lines()
                .map(str::trim)
                .filter(|l| l.len() >= 8 && !l.starts_with("-----") && !l.starts_with('#'))
            {
                values.push((path.clone(), line.to_string()));
            }
            if let Some(pw) = s.split("Password: \"").nth(1) {
                values.push((path.clone(), pw.split('"').next().unwrap().to_string()));
            }
        }
        assert!(values.len() > 40);
        for (path, v) in values {
            assert!(!text.contains(&v), "{} leaked: {text}", path.display());
        }
    }

    /// A supplementary group of the test user other than its own, if it has one.
    fn other_group() -> Option<u32> {
        let (_, egid) = me();
        let n = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
        let mut groups = vec![0 as libc::gid_t; n.max(0) as usize];
        let n = unsafe { libc::getgroups(n, groups.as_mut_ptr()) };
        groups.truncate(n.max(0) as usize);
        groups.into_iter().find(|g| *g != egid)
    }

    // Audit B1: self-hosted, `nats/` and `valkey/` hold both machine and install files in one
    // root; an override that gives one of them a group must not break the next run.
    #[test]
    fn an_owners_override_survives_reruns() {
        let (uid, gid) = me();
        let mut overrides = vec![format!(
            r#""nats/auth.conf": {{"gid": {gid}, "mode": "0640"}}"#
        )];
        if let Some(other) = other_group() {
            overrides.push(format!(
                r#""valkey/users.acl": {{"gid": {other}, "mode": "0640"}}"#
            ));
        }
        for files in overrides {
            let t = tmp();
            let json =
                format!(r#"{{"all": {{"uid": {uid}, "gid": {gid}}}, "files": {{{files}}}}}"#);
            let map = OwnershipMap::from_override_json(&json).unwrap();
            let root = t.path().join("root");
            ensure_secrets(&root, &InitSecrets::self_hosted(map.clone())).unwrap();
            let before = snapshot(&root);
            let again = ensure_secrets(&root, &InitSecrets::self_hosted(map));
            assert!(again.is_ok(), "{files}: {:#}", again.unwrap_err());
            assert_eq!(snapshot(&root), before, "{files}");
        }
    }

    /// A populated root with its setup code and one plain secret removed, so a run that wrote
    /// anything would show it.
    fn populated_with_two_missing() -> tempfile::TempDir {
        let t = tmp();
        ensure_secrets(t.path(), &opts()).unwrap();
        std::fs::remove_file(t.path().join("setup/setup.code")).unwrap();
        std::fs::remove_file(t.path().join("db/storcon.password")).unwrap();
        t
    }

    // Audit B2: a source whose content is unusable is refused in the check, naming the file,
    // before anything (the setup code included) is written.
    #[test]
    fn an_unusable_source_refuses_and_writes_nothing() {
        let cases: &[(&str, &[u8])] = &[
            ("valkey/proxy.password", b"\n"),
            ("nats/worker.password", b"  \n"),
            ("db/metadata.password", b"\n"),
            ("neon/compute-jwt.pem", b"garbage"),
            ("neon/compute-jwt.pub", b"garbage\n"),
            ("compute-ca/ca.crt", b"garbage"),
            ("compute-ca/ca.key", b"garbage"),
            ("gateway/internal-signing.pem", b"garbage"),
            ("gateway/internal-verify.pem", b"garbage"),
            ("auth/app-jwt.key", b"garbage"),
            ("auth/app-jwt.pub", b"garbage"),
            ("secrets/secrets.key", b"garbage"),
            ("auth/cookie.key", b"garbage"),
            ("realtime/ticket.key", b"short"),
            ("neon/storage-tokens/metrics.token", b"\n"),
        ];
        for (file, bad) in cases {
            let t = populated_with_two_missing();
            std::fs::write(t.path().join(file), bad).unwrap();
            let before = snapshot(t.path());
            let err = match ensure_secrets(t.path(), &opts()) {
                Ok(_) => panic!("{file}: accepted"),
                Err(e) => format!("{e:#}"),
            };
            assert!(
                err.contains(&t.path().join(file).display().to_string()),
                "{file}: {err}"
            );
            assert_eq!(
                snapshot(t.path()),
                before,
                "{file}: the refused run wrote something"
            );
            assert!(!t.path().join("setup/setup.code").exists(), "{file}");
            assert!(!t.path().join("db/storcon.password").exists(), "{file}");
        }
    }

    // Audit B2: the setup code is the last file a run writes, so no later step can fail after it
    // and leave a code nobody saw.
    #[test]
    fn the_setup_code_is_written_last() {
        let t = tmp();
        let r = ensure_secrets(t.path(), &opts()).unwrap();
        assert_eq!(
            r.created.last(),
            Some(&t.path().join("setup/setup.code")),
            "{r:?}"
        );
        let t = populated_with_two_missing();
        std::fs::remove_file(t.path().join("nats/auth.conf")).unwrap();
        let r = ensure_secrets(t.path(), &opts()).unwrap();
        assert_eq!(
            r.created.last(),
            Some(&t.path().join("setup/setup.code")),
            "{r:?}"
        );
    }

    // Audit N1: a corrupt setup code is refused by the check, naming the file.
    #[test]
    fn a_corrupt_setup_code_is_refused() {
        for bad in [
            "hello",
            "ABCD-EFGH-JKMN-PQRS-TUV0",
            "ABCDEFGHJKMNPQRSTUVWXYZ2",
        ] {
            let t = tmp();
            ensure_secrets(t.path(), &opts()).unwrap();
            let path = t.path().join("setup/setup.code");
            std::fs::write(&path, bad).unwrap();
            let before = snapshot(t.path());
            let err = format!("{:#}", ensure_secrets(t.path(), &opts()).unwrap_err());
            assert!(err.contains(&path.display().to_string()), "{bad}: {err}");
            assert_eq!(snapshot(t.path()), before);
        }
    }

    // Audit S1: the root itself must be the writer's and not writable by others (group write
    // only under the workbench's legacy modes).
    #[test]
    fn refuses_a_root_others_can_write() {
        let set = |p: &Path, m: u32| {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap()
        };
        let t = tmp();
        let root = t.path().join("root");
        std::fs::create_dir(&root).unwrap();
        for m in [0o777, 0o770, 0o702] {
            set(&root, m);
            let err = format!("{:#}", ensure_secrets(&root, &opts()).unwrap_err());
            assert!(
                err.contains(&root.display().to_string()) && err.contains("writable"),
                "{m:o}: {err}"
            );
            assert_eq!(
                std::fs::read_dir(&root).unwrap().count(),
                0,
                "{m:o}: nothing written"
            );
        }
        // The workbench's umask-made roots are group-writable; never world-writable.
        let state = t.path().join("state");
        std::fs::create_dir(&state).unwrap();
        set(&root, 0o775);
        set(&state, 0o775);
        ensure_secrets(&root, &InitSecrets::workbench(&state)).unwrap();
        set(&state, 0o777);
        assert!(ensure_secrets(&root, &InitSecrets::workbench(&state)).is_err());
        set(&state, 0o775);
        set(&root, 0o777);
        assert!(ensure_secrets(&root, &InitSecrets::workbench(&state)).is_err());
    }

    // Audit S2: the per-kind generators `dev bootstrap` still runs (and the compatibility ones
    // the CLI's tests use) make exactly the layout's files, with the same kinds.
    #[test]
    fn the_legacy_generators_match_the_layout() {
        use crate::kinds::stack_secrets::{Kind as Legacy, SECRET_FILES};
        for (file, kind) in SECRET_FILES {
            let entry = LAYOUT
                .iter()
                .find(|e| e.path == *file)
                .unwrap_or_else(|| panic!("{file}"));
            let want = match kind {
                Legacy::Text => Gen::Text32,
                Legacy::RawKey => Gen::Raw32,
            };
            assert_eq!(entry.make, want, "{file}");
        }
        let t = tmp();
        let k = t.path();
        crate::kinds::internal_keys::ensure(&k.join("gateway")).unwrap();
        crate::kinds::secrets_key::ensure(&k.join("secrets")).unwrap();
        crate::kinds::secrets_key::ensure_file(
            &k.join("auth"),
            crate::kinds::secrets_key::COOKIE_KEY_FILE,
        )
        .unwrap();
        crate::kinds::app_keys::ensure(&k.join("auth")).unwrap();
        crate::kinds::compute_jwt::ensure(&k.join("neon")).unwrap();
        crate::kinds::compute_ca::ensure(&k.join("compute-ca")).unwrap();
        crate::kinds::stack_secrets::ensure(k).unwrap();
        crate::kinds::service_auth::write_storage_auth(k).unwrap();
        crate::kinds::service_auth::write_auth_files(k, k).unwrap();
        let made: std::collections::BTreeSet<String> = snapshot(k)
            .into_keys()
            .map(|p| p.display().to_string())
            .collect();
        // The identity files (m5, #118) came after the legacy generators were frozen as this
        // baseline: only `ensure_secrets` makes them.
        let layout: std::collections::BTreeSet<String> = LAYOUT
            .iter()
            .filter(|e| e.part == Part::Machine || matches!(e.make, Gen::Derived(_)))
            .filter(|e| {
                e.make != Gen::Token32
                    && !matches!(
                        e.make,
                        Gen::Pair {
                            pair: Pair::MemberJwt,
                            ..
                        }
                    )
            })
            .map(|e| e.path.to_string())
            .collect();
        assert_eq!(made, layout);
    }

    // auth-replacement m5 (#118): the identity service's files.

    /// The identity service's own files, its callers' bearers and the member-JWT pair.
    const IDENTITY_FILES: [(&str, u32); 8] = [
        ("identity/secret", 0o600),
        ("identity/db.password", 0o600),
        ("tokens/auth-identity.token", 0o600),
        ("tokens/orchestrator-identity.token", 0o600),
        ("tokens/worker-identity.token", 0o600),
        // S1 (audit): the bootstrap's own, so the service tells it from the worker.
        ("tokens/bootstrap-identity.token", 0o600),
        ("auth/member-jwt.key", 0o600),
        ("auth/member-jwt.pub", 0o644),
    ];

    #[test]
    fn identity_entries_modes() {
        let t = tmp();
        let o = opts();
        let report = ensure_secrets(t.path(), &o).unwrap();
        let (uid, gid) = me();
        for (file, want) in IDENTITY_FILES {
            let entry = LAYOUT
                .iter()
                .find(|e| e.path == file)
                .unwrap_or_else(|| panic!("{file} is not in the layout"));
            assert_eq!(entry.part, Part::Machine, "{file}");
            let p = t.path().join(file);
            let meta = std::fs::symlink_metadata(&p).unwrap();
            assert!(meta.is_file(), "{file}");
            assert_eq!((meta.uid(), meta.gid(), mode(&p)), (uid, gid, want), "{file}");
            assert!(report.created.contains(&p), "{file}");
        }
        // Owner-only directory: nothing below it is group-readable.
        assert_eq!(mode(&t.path().join("identity")), 0o700);
        // The service refuses a secret or a caller token under 32 bytes, and puts the database
        // password into a URL: base64url only.
        for file in &IDENTITY_FILES[..6] {
            let v = std::fs::read_to_string(t.path().join(file.0)).unwrap();
            assert!(v.len() >= 32, "{}", file.0);
            assert!(
                v.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
                "{}",
                file.0
            );
        }
        // Each caller has its own token.
        let tokens: std::collections::BTreeSet<String> = IDENTITY_FILES[2..6]
            .iter()
            .map(|(f, _)| std::fs::read_to_string(t.path().join(f)).unwrap())
            .collect();
        assert_eq!(tokens.len(), 4);
        // The member pair is a usable Ed25519 pair, and not the app-user pair.
        let signer = chelabase_auth::internal::InternalSigner::from_pem_file(
            &t.path().join("auth/member-jwt.key"),
        )
        .unwrap();
        let verifier = chelabase_auth::internal::InternalVerifier::from_pem_file(
            &t.path().join("auth/member-jwt.pub"),
        )
        .unwrap();
        let id = chelabase_auth::internal::Identity {
            sub: "u".into(),
            auth: chelabase_auth::internal::AuthMethod::ManagementJwt,
            request_id: "r".into(),
            data: None,
            email: None,
            name: None,
        };
        verifier.verify(&signer.sign("a", &id).unwrap(), "a").unwrap();
        assert_ne!(
            std::fs::read(t.path().join("auth/member-jwt.key")).unwrap(),
            std::fs::read(t.path().join("auth/app-jwt.key")).unwrap()
        );
        // None of them is seedable (a seed's rules allow 16 characters and URL metacharacters).
        for (file, _) in IDENTITY_FILES {
            assert!(
                crate::seed::SEED_KEYS.iter().all(|k| k.path != file),
                "{file}"
            );
        }
        // A loose mode on one of them is refused, naming it, and nothing is written.
        let p = t.path().join("identity/db.password");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o640)).unwrap();
        let before = snapshot(t.path());
        let err = format!("{:#}", ensure_secrets(t.path(), &o).unwrap_err());
        assert!(err.contains(&p.display().to_string()), "{err}");
        assert_eq!(snapshot(t.path()), before);
    }

    #[test]
    fn never_overwrite_member_jwt() {
        let t = tmp();
        ensure_secrets(t.path(), &opts()).unwrap();
        let before = snapshot(t.path());
        // A rerun keeps every identity file byte for byte (and its mtime and mode).
        let again = ensure_secrets(t.path(), &opts()).unwrap();
        assert_eq!(snapshot(t.path()), before);
        for (file, _) in IDENTITY_FILES {
            assert!(again.kept.contains(&t.path().join(file)), "{file}");
        }
        // One missing secret is made alone; the pair and the others stay.
        std::fs::remove_file(t.path().join("identity/secret")).unwrap();
        let r = ensure_secrets(t.path(), &opts()).unwrap();
        assert_eq!(r.created, [t.path().join("identity/secret")]);
        let after = snapshot(t.path());
        for (file, _) in &IDENTITY_FILES[1..] {
            assert_eq!(after[Path::new(file)], before[Path::new(file)], "{file}");
        }
        // Half the member pair is refused, not completed: a new private key would sign tokens
        // the kept public key can't verify.
        std::fs::remove_file(t.path().join("auth/member-jwt.pub")).unwrap();
        let err = format!("{:#}", ensure_secrets(t.path(), &opts()).unwrap_err());
        assert!(
            err.contains("member-jwt.pub") && err.contains("delete both"),
            "{err}"
        );
        assert!(!t.path().join("auth/member-jwt.pub").exists());
        assert_eq!(
            std::fs::read(t.path().join("auth/member-jwt.key")).unwrap(),
            before[Path::new("auth/member-jwt.key")].0
        );
        // A key that doesn't parse is refused, and stays as it is.
        let t = populated_with_two_missing();
        for (file, bad) in [
            ("auth/member-jwt.key", &b"garbage"[..]),
            ("auth/member-jwt.pub", b"garbage"),
            ("identity/secret", b"\n"),
            ("tokens/worker-identity.token", b"  \n"),
        ] {
            let keep = std::fs::read(t.path().join(file)).unwrap();
            std::fs::write(t.path().join(file), bad).unwrap();
            let before = snapshot(t.path());
            let err = format!("{:#}", ensure_secrets(t.path(), &opts()).unwrap_err());
            assert!(
                err.contains(&t.path().join(file).display().to_string()),
                "{file}: {err}"
            );
            assert_eq!(snapshot(t.path()), before, "{file}");
            std::fs::write(t.path().join(file), keep).unwrap();
        }
    }

    /// object-storage m19 (#152): the files the self-hosted `storage` service mounts from the
    /// `secrets` volume (its block in docker-compose.yml), sorted.
    fn selfhost_storage_mounts() -> Vec<String> {
        let text = compose_file("docker-compose.yml");
        let block = text
            .split("\n  storage:\n")
            .nth(1)
            .expect("a storage service in the self-hosted compose file");
        let mut subs: Vec<String> = block
            .lines()
            .take_while(|l| l.is_empty() || l.starts_with("    "))
            .filter(|l| l.contains("source: secrets,"))
            .filter_map(|l| l.split("subpath: ").nth(1))
            .map(|rest| rest.split(['}', ',', ' ']).next().unwrap().to_string())
            .collect();
        subs.sort();
        subs
    }

    /// The storage service mounts exactly its own files, each made by a fresh run (the layout's,
    /// at the app user's owner-only or public mode; the S3 pair by init-secrets' script), and
    /// NATS's auth file carries its user.
    #[test]
    fn storage_service_mounts_come_from_init() {
        let mounts = selfhost_storage_mounts();
        assert_eq!(
            mounts,
            [
                "compute-ca/ca.crt",
                "gateway/internal-verify.pem",
                "nats/storage.password",
                "storage-s3/access-key",
                "storage-s3/secret-key",
                "tokens/storage-internal.token",
            ],
            "storage mounts only its own secrets, the verify key and the CA certificate"
        );
        let t = tmp();
        let o = opts();
        let report = ensure_secrets(t.path(), &o).unwrap();
        let (uid, gid) = me();
        for sub in mounts.iter().filter(|s| !s.starts_with("storage-s3/")) {
            let entry = LAYOUT
                .iter()
                .find(|e| e.path == sub)
                .unwrap_or_else(|| panic!("{sub} is not in chelabase-init's layout"));
            let want = if entry.spec == PUB { 0o644 } else { 0o600 };
            assert!(
                matches!(entry.spec.uid, Id::Writer),
                "{sub}: the app user owns it"
            );
            let p = t.path().join(sub);
            let meta = std::fs::symlink_metadata(&p).unwrap();
            assert_eq!(
                (meta.uid(), meta.gid(), mode(&p)),
                (uid, gid, want),
                "{sub}"
            );
            assert!(report.created.contains(&p), "{sub}");
        }
        let auth = std::fs::read_to_string(t.path().join("nats/auth.conf")).unwrap();
        let hashes = crate::kinds::service_auth::previous_nats_hashes(&auth);
        let pw = std::fs::read_to_string(t.path().join("nats/storage.password")).unwrap();
        assert!(
            bcrypt::verify(&pw, &hashes["storage"]).unwrap(),
            "storage's NATS user"
        );
    }

    /// An install from before object storage (no storage secrets): the next run makes them and
    /// re-renders NATS's users, and every other file stays byte for byte, the never-overwrite
    /// ones included.
    #[test]
    fn upgrade_adds_storage_secrets_and_keeps_the_rest() {
        const STORAGE: [&str; 2] = ["nats/storage.password", "tokens/storage-internal.token"];
        let t = tmp();
        ensure_secrets(t.path(), &opts()).unwrap();
        for f in STORAGE {
            std::fs::remove_file(t.path().join(f)).unwrap();
        }
        // NATS's users as such an install rendered them: no storage user.
        let rendered = std::fs::read_to_string(t.path().join("nats/auth.conf")).unwrap();
        let start = rendered
            .find("    {\n      user: \"storage\"\n")
            .expect("a storage user in auth.conf");
        let end = start + rendered[start..].find("\n    }\n").unwrap() + "\n    }\n".len();
        let auth = format!("{}{}", &rendered[..start], &rendered[end..]);
        assert!(!crate::kinds::service_auth::previous_nats_hashes(&auth).contains_key("storage"));
        assert_eq!(
            crate::kinds::service_auth::previous_nats_hashes(&auth).len(),
            crate::kinds::stack_secrets::NATS_USERS.len() - 1
        );
        std::fs::write(t.path().join("nats/auth.conf"), &auth).unwrap();
        let before = snapshot(t.path());
        let r = ensure_secrets(t.path(), &opts()).unwrap();
        let mut created = r.created.clone();
        created.sort();
        assert_eq!(created, STORAGE.map(|f| t.path().join(f)));
        assert_eq!(r.rewritten, [t.path().join("nats/auth.conf")]);
        assert!(r.auth_files.nats && !r.auth_files.valkey);
        let after = snapshot(t.path());
        for (p, v) in &before {
            if p != Path::new("nats/auth.conf") {
                assert_eq!(after.get(p), Some(v), "{} changed", p.display());
            }
        }
        for kept in [
            "secrets/secrets.key",
            "gateway/internal-signing.pem",
            "gateway/internal-verify.pem",
            "auth/cookie.key",
            "neon/compute-jwt.pem",
            "neon/compute-jwt.pub",
            "auth/member-jwt.key",
            "auth/member-jwt.pub",
            "identity/secret",
            "identity/db.password",
            "tokens/auth-identity.token",
            "tokens/orchestrator-identity.token",
            "tokens/worker-identity.token",
            "tokens/bootstrap-identity.token",
        ] {
            assert_eq!(
                after.get(Path::new(kept)),
                before.get(Path::new(kept)),
                "{kept}"
            );
            assert!(before.contains_key(Path::new(kept)), "{kept}");
        }
        // The other users keep their passwords; storage's new one verifies.
        let new = String::from_utf8(after[Path::new("nats/auth.conf")].0.clone()).unwrap();
        assert_ne!(new, auth);
        let hashes = crate::kinds::service_auth::previous_nats_hashes(&new);
        for u in crate::kinds::stack_secrets::NATS_USERS {
            let pw = String::from_utf8(after[Path::new(&format!("nats/{u}.password"))].0.clone())
                .unwrap();
            assert!(bcrypt::verify(&pw, &hashes[u]).unwrap(), "{u}");
        }
        // A rerun changes nothing.
        let again = ensure_secrets(t.path(), &opts()).unwrap();
        assert!(
            again.created.is_empty() && again.rewritten.is_empty(),
            "{again:?}"
        );
        assert_eq!(snapshot(t.path()), after);
    }

    // m12 (#23): seeds from the compose .env.

    fn seeded(text: &str) -> InitSecrets {
        InitSecrets {
            seeds: Seeds::parse(text).unwrap(),
            ..opts()
        }
    }

    /// A seeds file at `name` in `dir` with `text`, at `mode`.
    fn seeds_file(dir: &Path, name: &str, text: &str, mode: u32) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, text).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        p
    }

    #[test]
    fn seeded_value_is_written_at_layout_mode() {
        let t = tmp();
        let value = "Chosen-Metadata-Password-0001";
        let o = seeded(&format!("CHELABASE_SEED_METADATA_DB_PASSWORD={value}\n"));
        let report = ensure_secrets(t.path(), &o).unwrap();
        let path = t.path().join("db/metadata.password");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), value);
        let want = o.owners.spec("db/metadata.password").resolve();
        let meta = std::fs::symlink_metadata(&path).unwrap();
        assert!(meta.is_file());
        assert_eq!(
            (meta.uid(), meta.gid(), mode(&path)),
            (want.uid, want.gid, want.mode)
        );
        assert_eq!(want.mode, 0o640, "the layout's group-readable mode");
        assert_eq!(report.seeded, std::slice::from_ref(&path));
        assert!(!report.created.contains(&path), "{report:?}");
        // Every other file is generated, as without seeds.
        for entry in LAYOUT.iter().filter(|e| e.path != "db/metadata.password") {
            let p = t.path().join(entry.path);
            assert!(p.is_file(), "{}", entry.path);
            assert_ne!(
                std::fs::read(&p).unwrap(),
                value.as_bytes(),
                "{}",
                entry.path
            );
            assert!(report.created.contains(&p), "{}", entry.path);
        }
        assert!(report.setup_code.is_some());
        let tmps: Vec<_> = snapshot(t.path())
            .into_keys()
            .filter(|p| p.to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(tmps.is_empty(), "{tmps:?}");
    }

    #[test]
    fn seeded_password_reaches_derived_files() {
        use sha2::Digest;
        let t = tmp();
        let (nats, valkey) = ("Chosen-Nats-Worker-0001", "Chosen-Valkey-Proxy-0001");
        let o = seeded(&format!(
            "CHELABASE_SEED_NATS_WORKER_PASSWORD={nats}\n\
             CHELABASE_SEED_VALKEY_PROXY_PASSWORD={valkey}\n"
        ));
        let report = ensure_secrets(t.path(), &o).unwrap();
        assert_eq!(report.seeded.len(), 2, "{report:?}");
        let read = |p: &str| std::fs::read_to_string(t.path().join(p)).unwrap();
        assert_eq!(read("nats/worker.password"), nats);
        assert_eq!(read("valkey/proxy.password"), valkey);
        let hashes = crate::kinds::service_auth::previous_nats_hashes(&read("nats/auth.conf"));
        assert!(bcrypt::verify(nats, &hashes["worker"]).unwrap());
        let acl = read("valkey/users.acl");
        let proxy = acl.lines().find(|l| l.starts_with("user proxy ")).unwrap();
        let hash = hex::encode(sha2::Sha256::digest(valkey.as_bytes()));
        assert!(proxy.contains(&format!("#{hash} ")), "{proxy}");
        assert!(!acl.contains(valkey) && !read("nats/auth.conf").contains(nats));
    }

    #[test]
    fn seed_rerun_changes_nothing() {
        let t = tmp();
        let text = "CHELABASE_SEED_METADATA_DB_PASSWORD=Chosen-Metadata-0001\n\
                    CHELABASE_SEED_S3_ACCESS_KEY=chosen-access-0001\n\
                    CHELABASE_SEED_NATS_RUNNER_PASSWORD=Chosen-Nats-Runner-0001\n";
        ensure_secrets(t.path(), &seeded(text)).unwrap();
        let before = snapshot(t.path());
        let again = ensure_secrets(t.path(), &seeded(text)).unwrap();
        assert_eq!(snapshot(t.path()), before);
        assert!(
            again.created.is_empty() && again.seeded.is_empty() && again.rewritten.is_empty(),
            "{again:?}"
        );
        assert!(again.setup_code.is_none());
        assert!(!again.auth_files.nats && !again.auth_files.valkey);
        assert!(again.kept.contains(&t.path().join("db/metadata.password")));
        assert_eq!(again.kept.len(), LAYOUT.len(), "{again:?}");
        // And a run without the seeds keeps them too (m1's rerun rule).
        ensure_secrets(t.path(), &opts()).unwrap();
        assert_eq!(snapshot(t.path()), before);
        // A seeded file an operator rewrote with a trailing line break still matches.
        let p = t.path().join("db/metadata.password");
        std::fs::write(&p, "Chosen-Metadata-0001\n").unwrap();
        let before = snapshot(t.path());
        ensure_secrets(t.path(), &seeded(text)).unwrap();
        assert_eq!(snapshot(t.path()), before);
    }

    #[test]
    fn changed_seed_refuses_and_writes_nothing() {
        let t = populated_with_two_missing();
        let existing = std::fs::read_to_string(t.path().join("db/metadata.password")).unwrap();
        let value = "Changed-Metadata-Value-0001";
        let o = seeded(&format!(
            "CHELABASE_SEED_METADATA_DB_PASSWORD={value}\n\
             CHELABASE_SEED_STORCON_DB_PASSWORD=Would-Be-Seeded-0001\n"
        ));
        let before = snapshot(t.path());
        let err = format!("{:#}", ensure_secrets(t.path(), &o).unwrap_err());
        assert!(err.contains("CHELABASE_SEED_METADATA_DB_PASSWORD"), "{err}");
        assert!(
            err.contains(&t.path().join("db/metadata.password").display().to_string()),
            "{err}"
        );
        assert!(err.contains("first created") && err.contains("#6"), "{err}");
        assert!(
            !err.contains(value) && !err.contains(existing.trim()),
            "{err}"
        );
        assert_eq!(
            snapshot(t.path()),
            before,
            "the refused run wrote something"
        );
        assert!(!t.path().join("setup/setup.code").exists());
        assert!(!t.path().join("db/storcon.password").exists());
        // A derived file's source too: a changed NATS seed doesn't re-render auth.conf.
        let o = seeded("CHELABASE_SEED_NATS_WORKER_PASSWORD=Changed-Nats-Worker-0001\n");
        let err = format!("{:#}", ensure_secrets(t.path(), &o).unwrap_err());
        assert!(err.contains("CHELABASE_SEED_NATS_WORKER_PASSWORD"), "{err}");
        assert_eq!(snapshot(t.path()), before);
    }

    #[test]
    fn invalid_seeds_refuse_and_write_nothing() {
        let short = "CHELABASE_SEED_APP_AUTH_TOKEN=too-short\n";
        let cases: Vec<(String, &str)> = vec![
            (
                "CHELABASE_SEED_UNKNOWN_THING=0123456789abcdefgh\n".into(),
                "an unknown CHELABASE_SEED_* key",
            ),
            (
                "CHELABASE_SEED_METADATA_DB_PASSWORD=\n".into(),
                "CHELABASE_SEED_METADATA_DB_PASSWORD",
            ),
            (short.into(), "CHELABASE_SEED_APP_AUTH_TOKEN"),
            (
                "CHELABASE_SEED_S3_ACCESS_KEY=ab\n".into(),
                "CHELABASE_SEED_S3_ACCESS_KEY",
            ),
            (
                "CHELABASE_SEED_DATA_KEYS_TOKEN=\"line-one-0123456\nline-two\"\n".into(),
                "CHELABASE_SEED_DATA_KEYS_TOKEN",
            ),
            (
                "CHELABASE_SEED_DATA_KEYS_TOKEN=first-0123456789ab\n\
                 CHELABASE_SEED_DATA_KEYS_TOKEN=second-0123456789ab\n"
                    .into(),
                "CHELABASE_SEED_DATA_KEYS_TOKEN",
            ),
        ];
        for (text, key) in cases {
            // Through the CLI, on a fresh root and on a populated one.
            let t = tmp();
            let file = seeds_file(t.path(), ".env", &text, 0o600);
            let fresh = t.path().join("fresh");
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let e =
                crate::cli::secrets(&fresh, &opts(), Some(&file), &mut out, &mut err).unwrap_err();
            let e = format!("{e:#}");
            assert!(e.contains(key), "{text:?}: {e}");
            assert!(!fresh.exists(), "{text:?}: the root was made");
            assert!(out.is_empty(), "{text:?}");

            let populated = populated_with_two_missing();
            let before = snapshot(populated.path());
            let e = crate::cli::secrets(populated.path(), &opts(), Some(&file), &mut out, &mut err)
                .unwrap_err();
            assert!(format!("{e:#}").contains(key), "{text:?}: {e:#}");
            assert_eq!(snapshot(populated.path()), before, "{text:?}");
            assert!(out.is_empty(), "{text:?}");
        }
    }

    #[test]
    fn seeds_file_symlink_refused_and_loose_mode_warns() {
        let t = tmp();
        let text = "CHELABASE_SEED_APP_AUTH_TOKEN=Chosen-App-Auth-0001\n";
        let real = seeds_file(t.path(), "real.env", text, 0o600);
        let link = t.path().join("link.env");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let dir = t.path().join("a-dir.env");
        std::fs::create_dir(&dir).unwrap();
        let fifo = t.path().join("fifo.env");
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        for (path, what) in [
            (&link, "symlink"),
            (&dir, "not a regular file"),
            (&fifo, "not a regular file"),
            (&t.path().join("missing.env"), "missing.env"),
        ] {
            let root = t.path().join("root");
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let e =
                crate::cli::secrets(&root, &opts(), Some(path), &mut out, &mut err).unwrap_err();
            let e = format!("{e:#}");
            assert!(e.contains(what), "{}: {e}", path.display());
            assert!(!root.exists(), "{}: the root was made", path.display());
        }
        // A tight file: no warning.
        let root = t.path().join("tight");
        let (mut out, mut err) = (Vec::new(), Vec::new());
        crate::cli::secrets(&root, &opts(), Some(&real), &mut out, &mut err).unwrap();
        let err = String::from_utf8(err).unwrap();
        assert!(!err.contains("warning"), "{err}");
        assert!(err.contains("seeded"), "{err}");
        // Group- or world-readable: it runs, with a warning to chmod 600.
        for m in [0o640, 0o604, 0o644] {
            let loose = seeds_file(t.path(), &format!("loose-{m:o}.env"), text, m);
            let root = t.path().join(format!("loose-{m:o}"));
            let (mut out, mut err) = (Vec::new(), Vec::new());
            crate::cli::secrets(&root, &opts(), Some(&loose), &mut out, &mut err).unwrap();
            let err = String::from_utf8(err).unwrap();
            assert!(
                err.contains("warning") && err.contains("chmod 600"),
                "{m:o}: {err}"
            );
            assert!(err.contains(&loose.display().to_string()), "{m:o}: {err}");
            assert_eq!(
                std::fs::read_to_string(root.join("tokens/app-auth.token")).unwrap(),
                "Chosen-App-Auth-0001"
            );
        }
    }

    #[test]
    fn seed_values_never_leak() {
        // A distinct sentinel per key, each one a valid value for its rule.
        let sentinel = |k: &crate::seed::SeedKey, n: u32| -> String {
            let base = format!("Sntl9Q{n}x{}", k.path.replace(['/', '.', '-'], "x"));
            format!("{base:Q<20}")
        };
        let all = |n: u32| -> (String, Vec<String>) {
            let mut text = String::from("# sentinels\n");
            let mut values = Vec::new();
            for k in crate::seed::SEED_KEYS {
                let v = sentinel(k, n);
                text.push_str(&format!("{}={v}\n", k.key));
                values.push(v);
            }
            (text, values)
        };
        let t = tmp();
        let root = t.path().join("root");
        let (first, mut values) = all(1);
        let (changed, more) = all(2);
        values.extend(more);
        // Failing inputs carry sentinels too.
        let invalid = [
            "CHELABASE_SEED_NOT_A_KEY=Sntl9QUnknownKeyValue0001\n".to_string(),
            "CHELABASE_SEED_APP_AUTH_TOKEN=Sntl9QShort\n".to_string(),
            "CHELABASE_SEED_STORCON_DB_PASSWORD=Sntl9QShortStor\n".to_string(),
            "CHELABASE_SEED_APP_AUTH_TOKEN=\"Sntl9QMultiLine000001\nSntl9QSecondLine00001\"\n"
                .into(),
            "CHELABASE_SEED_APP_AUTH_TOKEN=Sntl9QDuplicateOne0001\n\
             CHELABASE_SEED_APP_AUTH_TOKEN=Sntl9QDuplicateTwo0001\n"
                .into(),
            "CHELABASE_SEED_APP_AUTH_TOKEN=Sntl9QDollar$Interp0001\n".into(),
            "CHELABASE_SEED_APP_AUTH_TOKEN='Sntl9QUnterminated0001\n".into(),
            // Typos that put the value into what looks like the key.
            "CHELABASE_SEED_APP_AUTH_TOKEN Sntl9QNoEquals0001\n".into(),
            "CHELABASE_SEED_APP_AUTH_TOKEN:Sntl9QColon0001\n".into(),
            "CHELABASE_SEED_APP_AUTH_TOKEN Sntl9QSpaceKey0001=x\n".into(),
            "CHELABASE_SEED_APP_AUTH_TOKEN-Sntl9QDash0001=0123456789abcdef\n".into(),
            "my Sntl9QNotAKey0001 ='never closed\n".into(),
            // Audit B1 (m12): a value typed straight after a known key, with no `=` or with a
            // name character for it, and a value after each separator.
            "CHELABASE_SEED_APP_AUTH_TOKENSntl9QNoEqualsAlnum01\n".into(),
            "CHELABASE_SEED_APP_AUTH_TOKEN_Sntl9QUnderscore01=x\n".into(),
            "CHELABASE_SEED_APP_AUTH_TOKEN-Sntl9QDashNoEq0001\n".into(),
            "CHELABASE_SEED_APP_AUTH_TOKEN.Sntl9QDotNoEq0001\n".into(),
            "CHELABASE_SEED_APP_AUTH_TOKEN.Sntl9QDotEq0001=0123456789abcdef\n".into(),
            "CHELABASE_SEED_APP_AUTH_TOKEN:Sntl9QColonEq0001=0123456789abcdef\n".into(),
            "CHELABASE_SEED_APP_AUTH_TOKEN Sntl9QSpaceNoEq0001\n".into(),
            "CHELABASE_SEED_Sntl9QBareValue0001\n".into(),
            "CHELABASE_SEED_Sntl9QBareValueEq0001=x\n".into(),
            "CHELABASE_SEED_APP_AUTH_TOKENSntl9QQuoted0001='unclosed\n".into(),
        ];
        values.extend([
            "Sntl9QUnknownKeyValue0001".into(),
            "Sntl9QShort".into(),
            "Sntl9QMk".repeat(4)[..31].to_string(),
            "Sntl9QMultiLine000001".into(),
            "Sntl9QSecondLine00001".into(),
            "Sntl9QDuplicateOne0001".into(),
            "Sntl9QDuplicateTwo0001".into(),
            "Sntl9QDollar".into(),
            "Sntl9QUnterminated0001".into(),
            "Sntl9QNoEquals0001".into(),
            "Sntl9QColon0001".into(),
            "Sntl9QSpaceKey0001".into(),
            "Sntl9QDash0001".into(),
            "Sntl9QNotAKey0001".into(),
            "Sntl9QNoEqualsAlnum01".into(),
            "Sntl9QUnderscore01".into(),
            "Sntl9QDashNoEq0001".into(),
            "Sntl9QDotNoEq0001".into(),
            "Sntl9QDotEq0001".into(),
            "Sntl9QColonEq0001".into(),
            "Sntl9QSpaceNoEq0001".into(),
            "Sntl9QBareValue0001".into(),
            "Sntl9QBareValueEq0001".into(),
            "Sntl9QQuoted0001".into(),
        ]);

        let mut printed = String::new();
        let shown = |out: Vec<u8>, err: Vec<u8>, printed: &mut String| {
            printed.push_str(&String::from_utf8(out).unwrap());
            printed.push_str(&String::from_utf8(err).unwrap());
        };
        // The first run (loose mode, so the warning is printed too), a rerun, a changed seed,
        // then every invalid file.
        let file = seeds_file(t.path(), ".env", &first, 0o644);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        crate::cli::secrets(&root, &opts(), Some(&file), &mut out, &mut err).unwrap();
        let code = read_setup_code(&root).unwrap();
        let out = String::from_utf8(out)
            .unwrap()
            .replacen(&format!("Setup code: {code}\n"), "", 1)
            .into_bytes();
        shown(out, err, &mut printed);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        crate::cli::secrets(&root, &opts(), Some(&file), &mut out, &mut err).unwrap();
        shown(out, err, &mut printed);
        let file = seeds_file(t.path(), ".env", &changed, 0o600);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let e = crate::cli::secrets(&root, &opts(), Some(&file), &mut out, &mut err).unwrap_err();
        printed.push_str(&format!("{e:#}\n{e:?}\n"));
        shown(out, err, &mut printed);
        for text in &invalid {
            let file = seeds_file(t.path(), ".env", text, 0o600);
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let e =
                crate::cli::secrets(&root, &opts(), Some(&file), &mut out, &mut err).unwrap_err();
            printed.push_str(&format!("{e:#}\n{e:?}\n"));
            shown(out, err, &mut printed);
            let e = Seeds::parse(text).unwrap_err();
            printed.push_str(&format!("{e:#}\n{e:?}\n"));
        }
        // Debug of everything that holds seeds or a report.
        let o = seeded(&first);
        printed.push_str(&format!("{o:?}\n{o:#?}\n{:?}\n", o.seeds));
        let report = ensure_secrets(&root, &o).unwrap();
        printed.push_str(&format!("{report:?}\n{report:#?}\n"));
        let fresh = t.path().join("fresh");
        let report = ensure_secrets(&fresh, &o).unwrap();
        printed.push_str(&format!("{report:?}\n{report:#?}\n"));
        assert_eq!(report.seeded.len(), crate::seed::SEED_KEYS.len());
        let loaded = Seeds::load(&seeds_file(t.path(), "dbg.env", &first, 0o644)).unwrap();
        printed.push_str(&format!("{loaded:?}\n"));

        // The checks saw real output (keys and paths), and no sentinel anywhere.
        assert!(
            printed.contains("CHELABASE_SEED_APP_AUTH_TOKEN"),
            "{printed}"
        );
        assert!(printed.contains("chmod 600"), "{printed}");
        assert!(printed.contains("tokens/app-auth.token"), "{printed}");
        assert!(!printed.contains(code.as_str()));
        assert!(!printed.contains("Sntl9Q"), "a sentinel leaked: {printed}");
        for v in &values {
            assert!(
                v.starts_with("Sntl9Q") && !printed.contains(v.as_str()),
                "{v}"
            );
        }
        // The values did reach their files.
        for k in crate::seed::SEED_KEYS {
            assert_eq!(
                std::fs::read_to_string(root.join(k.path)).unwrap(),
                sentinel(k, 1),
                "{}",
                k.key
            );
        }
    }
}
