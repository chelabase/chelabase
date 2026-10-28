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
//! | `neon/compute-jwt.pem`, `neon/compute-jwt.pub` | the compute-JWT pair |
//! | `neon/compute-jwt.pub.pem` | derived: its public key as SPKI PEM |
//! | `neon/storage-tokens/*.jwt` | derived: the scoped storage tokens |
//! | `neon/storage-tokens/metrics.token` | derived once: the runner's metrics token |
//! | `compute-ca/ca.key`, `compute-ca/ca.crt` | the compute CA |
//! | `orchestrator-tls/`, `realtime-tls/` | client pairs (`current` -> `v<secs>/`), renewed when due |
//! | `nats/*.password`, `valkey/*.password` | NATS and Valkey user passwords |
//! | `realtime/{pg.password,ticket.key,internal.token}` | realtime's secrets |
//! | `tokens/*.token` | the internal bearers |
//!
//! The install part holds what each install has on its own (in the workbench it goes under
//! each stack's state dir; self-hosted, it is the same root):
//!
//! | Path | What |
//! |---|---|
//! | `nats/auth.conf`, `valkey/users.acl` | derived: the NATS users (bcrypt) and Valkey ACL (SHA-256) |
//! | `db/metadata.password` | the metadata database's `chelabase` user |
//! | `db/zitadel-user.password`, `db/zitadel-admin.password` | Zitadel's database user and admin (`postgres`) |
//! | `db/storcon.password` | the storage controller database's `storcon` user |
//! | `s3/access-key`, `s3/secret-key` | the object store's credentials |
//! | `zitadel/masterkey` | Zitadel's masterkey (32 characters) |
//! | `zitadel/steps.yaml` | Zitadel's `--steps` file: the FirstInstance human's random password, written nowhere else |
//! | `setup/setup.code` | the one-time setup code (`XXXX-XXXX-XXXX-XXXX-XXXX`) |
//!
//! # Rules
//!
//! - A missing file is created; an existing one is never rewritten. Derived files are rendered
//!   from the existing source files and rewritten only when that rendering differs.
//! - Every existing entry is checked before anything is written: an empty or unreadable file,
//!   a symlink anywhere in the layout, half a key pair, or an owner or mode other than the
//!   ownership map's fails the run, naming the file, and nothing is changed.
//! - Owners and modes come from [`LAYOUT`]'s `spec` column ([`OwnershipMap`] for overrides). A
//!   directory is 0750 with a consumer group when files below it are readable by one group,
//!   else 0700.
//! - Runs on the same root are serialised by a lock file (`.chelabase-init.lock`).
//! - Nothing here prints or logs a secret: the report carries file paths and the setup code only.

use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::fsx::{Dir, Kind, Perm};

use crate::kinds::service_auth::AuthFilesChanged;
pub use crate::owners::{Id, OwnershipMap, Spec};

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
    /// 32 random bytes, raw.
    Raw32,
    /// One half of a pair (`private` true for the private key).
    Pair { pair: Pair, private: bool },
    /// A random [`Gen::Text32`], or `dev` for a workbench stack (`dev_fixed`).
    Password { dev: &'static str },
    /// `len` random letters and digits, or `dev` for a workbench stack.
    Alnum { len: usize, dev: &'static str },
    /// Zitadel's steps file.
    Steps,
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
/// `ghcr.io/zitadel/zitadel` runs as 1000:1000.
pub const ZITADEL_GID: u32 = 1000;
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
/// files need no group; the files of the postgres, Valkey, Zitadel and Neon images are 0640 with
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
    e("tokens/proxy-api.token", M, Gen::Text32, P),
    e("tokens/storcon-hook.token", M, Gen::Text32, P),
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
        "db/zitadel-user.password",
        I,
        Gen::Password { dev: "zitadel" },
        Spec::group(ZITADEL_GID),
    ),
    e(
        "db/zitadel-admin.password",
        I,
        Gen::Password { dev: "postgres" },
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
    e(
        "zitadel/masterkey",
        I,
        Gen::Alnum {
            len: 32,
            dev: "MasterkeyNeedsToHave32Characters",
        },
        Spec::group(ZITADEL_GID),
    ),
    e(
        "zitadel/steps.yaml",
        I,
        Gen::Steps,
        Spec::group(ZITADEL_GID),
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
    /// The workbench's fixed dev values for the database passwords, the S3 credentials and the
    /// Zitadel masterkey (its stacks' volumes were made with them).
    pub dev_fixed: bool,
    /// Where the install part goes; `None` is the root itself.
    pub install_dir: Option<PathBuf>,
    pub owners: OwnershipMap,
    /// Accept a directory or public file that already exists with group bits other than the
    /// map's (never world-writable, executable or set-id), as the workbench's shared keys dir
    /// has them from before this check (made under the user's umask). Secrets stay exact.
    pub legacy_modes: bool,
}

impl InitSecrets {
    /// A self-hosted install: one root, random values, the given ownership map.
    pub fn self_hosted(owners: OwnershipMap) -> Self {
        InitSecrets {
            dev_fixed: false,
            install_dir: None,
            owners,
            legacy_modes: false,
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
    pub created: Vec<PathBuf>,
    pub rewritten: Vec<PathBuf>,
    pub setup_code: Option<SetupCode>,
    /// Whether `nats/auth.conf` or `valkey/users.acl` changed (a running server must restart).
    pub auth_files: AuthFilesChanged,
}

/// Creates what is missing under `dir` (see the module doc).
pub fn ensure_secrets(dir: &Path, opts: &InitSecrets) -> Result<Report> {
    let machine = Dir::open_root(dir)?;
    let machine_path = dir
        .canonicalize()
        .with_context(|| format!("resolving {}", dir.display()))?;
    let install_given = opts.install_dir.as_deref().unwrap_or(dir);
    let install = Dir::open_root(install_given)?;
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
        report: Report::default(),
    };
    run.check()?;
    run.create_sources()?;
    run.client_pairs()?;
    run.derive()?;
    Ok(run.report)
}

/// The setup code stored under `dir` (the install root).
pub fn read_setup_code(dir: &Path) -> Result<SetupCode> {
    let root = Dir::open_root(dir)?;
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

/// Zitadel's `--steps` file: the FirstInstance human's password, random, here only. (Its
/// complexity policy wants a lower and upper case letter, a digit and a symbol.)
fn render_steps() -> String {
    let password = format!(
        "{}-Aa1",
        crate::random::from_alphabet(crate::random::ALPHANUMERIC, 40)
    );
    format!(
        "# Generated by chelabase-init: Zitadel's FirstInstance setup (`--steps`). The first\n\
         # human's password is random and stored nowhere else; the bootstrap deactivates the user.\n\
         FirstInstance:\n  Org:\n    Human:\n      Password: \"{password}\"\n"
    )
}

/// One `ensure_secrets` run.
struct Run<'a> {
    opts: &'a InitSecrets,
    /// The machine root and the install root (maybe the same directory), with their paths.
    roots: [(Dir, PathBuf); 2],
    /// The bytes of every layout file that exists (read in the check, or written by this run).
    have: BTreeMap<&'static str, Vec<u8>>,
    report: Report,
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

    /// A directory's owner and mode, from the files below it: 0750 with their group when the
    /// group-readable ones share one, else 0700.
    fn dir_perm(&self, part: Part, rel: &str) -> Perm {
        let (wuid, wgid) = crate::fsx::writer();
        let prefix = format!("{rel}/");
        let files: Vec<Perm> = LAYOUT
            .iter()
            .filter(|e| e.part == part && e.path.starts_with(&prefix))
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
        for part in [Part::Machine, Part::Install] {
            let mut dirs = layout_dirs(part);
            if part == Part::Machine {
                dirs.extend(CLIENT_PAIRS.iter().map(|(d, _)| d.to_string()));
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
            Pair::ComputeJwt,
            Pair::ComputeCa,
        ] {
            let (private, public) = pair_paths(pair);
            if self.have.contains_key(private) != self.have.contains_key(public) {
                let path = self.abs(Part::Machine, private);
                problems.push(format!(
                    "only one of {private} / {public} exists in {}; delete both to regenerate",
                    path.parent().unwrap_or(&path).display()
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
        Ok(())
    }

    /// Writes `rel` as a new file at its map entry; one that appears meanwhile is an error.
    fn put_new(&mut self, entry: &Entry, bytes: Vec<u8>) -> Result<()> {
        let dir = self.parent_created(entry.part, entry.path)?;
        let (_, name) = split(entry.path);
        let path = self.abs(entry.part, entry.path);
        if !dir.create_new(name, &bytes, self.perm(entry.path))? {
            bail!("{} appeared during the run; run it again", path.display());
        }
        self.report.created.push(path);
        self.have.insert(entry.path, bytes);
        Ok(())
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
            let bytes = match entry.make {
                Gen::SecretKey => chelabase_secrets::SecretKey::generate()
                    .to_base64()
                    .into_bytes(),
                Gen::Text32 => crate::random::text32().into_bytes(),
                Gen::Raw32 => crate::random::bytes::<32>().to_vec(),
                Gen::Password { dev: fixed } if dev => fixed.as_bytes().to_vec(),
                Gen::Password { .. } => crate::random::text32().into_bytes(),
                Gen::Alnum { dev: fixed, .. } if dev => fixed.as_bytes().to_vec(),
                Gen::Alnum { len, .. } => {
                    crate::random::from_alphabet(crate::random::ALPHANUMERIC, len).into_bytes()
                }
                Gen::Steps => render_steps().into_bytes(),
                Gen::SetupCode => {
                    let code = new_setup_code();
                    self.put_new(entry, code.as_str().as_bytes().to_vec())?;
                    self.report.setup_code = Some(code);
                    continue;
                }
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

    /// Both halves of a pair; if the public half can't be written, the private one goes again.
    fn create_pair(&mut self, pair: Pair) -> Result<()> {
        let (private, public) = match pair {
            Pair::Internal | Pair::AppJwt => chelabase_auth::internal::generate_keypair_pem(),
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
                    self.report.rewritten.push(self.abs(entry.part, entry.path));
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

    /// The files compose's top-level `secrets:` block reads from `${CHELABASE_KEYS_DIR}`; the
    /// others are Zitadel's PATs in the stack dir, which Zitadel writes.
    fn compose_secret_files() -> Vec<String> {
        let compose =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../infra/compose/docker-compose.yml");
        let text = std::fs::read_to_string(compose).unwrap();
        let block = text
            .split("\nsecrets:\n")
            .nth(1)
            .expect("a top-level secrets block");
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
            } else {
                others.push(file.to_string());
            }
        }
        assert_eq!(
            others,
            [
                "${CHELABASE_STACK_DIR}/zitadel/admin.pat",
                "${CHELABASE_STACK_DIR}/zitadel/login-client.pat"
            ],
            "a compose secret outside the keys dir that chelabase-init doesn't make"
        );
        assert!(keys.len() > 30, "{keys:?}");
        keys
    }

    fn mode(p: &Path) -> u32 {
        std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn creates_full_layout() {
        let t = tempfile::tempdir().unwrap();
        let o = opts();
        let report = ensure_secrets(t.path(), &o).unwrap();
        for file in compose_secret_files() {
            let p = t.path().join(&file);
            assert!(p.is_file(), "{file} (named by compose) was not created");
            assert!(std::fs::metadata(&p).unwrap().len() > 0, "{file} is empty");
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
            "zitadel",
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
        assert_eq!(read("zitadel/masterkey").len(), 32);
        assert!(read("zitadel/steps.yaml").contains("FirstInstance:"));
        assert!(
            chelabase_secrets::SecretKey::from_file(&t.path().join("secrets/secrets.key")).is_ok()
        );
        let code = report.setup_code.expect("created on this run");
        assert_eq!(read("setup/setup.code"), code.as_str());
        assert!(report.auth_files.nats && report.auth_files.valkey);
        assert!(!t.path().join("nats/auth.conf").read_link().is_ok());
    }

    #[test]
    fn rerun_changes_nothing() {
        let t = tempfile::tempdir().unwrap();
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
        let t = tempfile::tempdir().unwrap();
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
        let t = tempfile::tempdir().unwrap();
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
        let t = tempfile::tempdir().unwrap();
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
        let t = tempfile::tempdir().unwrap();
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
        let elsewhere = tempfile::tempdir().unwrap();
        // A symlink in place of a file.
        let t = tempfile::tempdir().unwrap();
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
            let t = tempfile::tempdir().unwrap();
            let target = tempfile::tempdir().unwrap();
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
        let t = tempfile::tempdir().unwrap();
        let link = t.path().join("link");
        std::os::unix::fs::symlink(elsewhere.path(), &link).unwrap();
        assert!(ensure_secrets(&link, &opts()).is_err());
        assert_eq!(std::fs::read_dir(elsewhere.path()).unwrap().count(), 0);
    }

    #[test]
    fn setup_code_reprint() {
        let t = tempfile::tempdir().unwrap();
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
        assert!(read_setup_code(tempfile::tempdir().unwrap().path()).is_err());
    }

    #[test]
    fn workbench_stacks_get_own_setup_code() {
        let t = tempfile::tempdir().unwrap();
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
        assert_eq!(
            read(a.join("zitadel/masterkey")),
            "MasterkeyNeedsToHave32Characters"
        );
        assert_eq!(mode(&a.join("db/metadata.password")), 0o600);
        assert_ne!(
            read(a.join("zitadel/steps.yaml")),
            read(b.join("zitadel/steps.yaml"))
        );
        assert!(a.join("nats/auth.conf").is_file() && b.join("valkey/users.acl").is_file());
    }

    #[test]
    fn workbench_accepts_its_umask_made_keys_dir() {
        // The shared keys dir from before: public files and dirs made under umask 002.
        let t = tempfile::tempdir().unwrap();
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
            let t = tempfile::tempdir().unwrap();
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
                org_id: "o".into(),
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
        let t = tempfile::tempdir().unwrap();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        crate::cli::secrets(t.path(), &opts(), &mut out, &mut err).unwrap();
        let code = read_setup_code(t.path()).unwrap();
        let out1 = String::from_utf8(out).unwrap();
        assert_eq!(out1, format!("Setup code: {code}\n"));
        let (mut out, mut err2) = (Vec::new(), Vec::new());
        crate::cli::secrets(t.path(), &opts(), &mut out, &mut err2).unwrap();
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
        let t = tempfile::tempdir().unwrap();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        crate::cli::secrets(t.path(), &opts(), &mut out, &mut err).unwrap();
        std::fs::remove_file(t.path().join("valkey/proxy.password")).unwrap();
        crate::cli::secrets(t.path(), &opts(), &mut out, &mut err).unwrap();
        // A failing run's message too.
        std::fs::write(t.path().join("tokens/app-auth.token"), "").unwrap();
        let failure = format!(
            "{:#}",
            crate::cli::secrets(t.path(), &opts(), &mut out, &mut err).unwrap_err()
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
}
