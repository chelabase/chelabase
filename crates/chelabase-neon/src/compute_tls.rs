//! Compute TLS: the platform CA, each compute's server certificate, the orchestrator's client
//! certificate, the archive the worker uploads into a compute, and the client pair on disk that
//! the orchestrator loads (and reloads) for its connections to computes.
//!
//! Every key is ECDSA P-256 (PKCS#8 PEM) and every certificate is signed with
//! ecdsa-with-SHA256: the only kind compute_ctl reads.

use std::fmt;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, bail};
use arc_swap::ArcSwap;
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, PKCS_ECDSA_P256_SHA256, SanType, SerialNumber,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use sha2::{Digest, Sha256};

/// Where a compute finds its TLS files (compute_ctl's `compute_ctl_config.tls`).
pub const TLS_DIR: &str = "/var/db/chelabase/tls";
/// The directory the upload archive is extracted into; its entries start at `chelabase/`.
pub const TLS_UPLOAD_ROOT: &str = "/var/db";
pub const SERVER_KEY_PATH: &str = "/var/db/chelabase/tls/server.key";
pub const SERVER_CERT_PATH: &str = "/var/db/chelabase/tls/server.crt";
pub const CA_CERT_PATH: &str = "/var/db/chelabase/tls/ca.crt";
/// The copy of the certificate compute_ctl makes in `pgdata`: what Postgres serves.
pub const PGDATA_SERVER_CERT_PATH: &str = "/var/db/postgres/compute/server.crt";
/// A compute's server name is `<endpoint_id>.<SERVER_NAME_SUFFIX>`.
pub const SERVER_NAME_SUFFIX: &str = "compute.chelabase.internal";
/// The CN of the orchestrator's client certificate, the one CN the computes' ident map admits.
pub const ORCHESTRATOR_CN: &str = "chelabase-orchestrator";
/// The CN of realtime's client certificate (W31): the one CN the computes' ident map admits for
/// `chelabase_realtime`.
pub const REALTIME_CN: &str = "chelabase-realtime";
/// The CA's CN.
pub const CA_CN: &str = "Chelabase compute CA";

pub const SERVER_VALIDITY: Duration = Duration::from_secs(90 * 86_400);
pub const CLIENT_VALIDITY: Duration = Duration::from_secs(365 * 86_400);
pub const CA_VALIDITY: Duration = Duration::from_secs(3650 * 86_400);
/// A compute's certificate is re-issued when it has less than this left.
pub const SERVER_RENEW_BEFORE: Duration = Duration::from_secs(30 * 86_400);
/// The orchestrator's client certificate is re-issued when it has less than this left.
pub const CLIENT_RENEW_BEFORE: Duration = Duration::from_secs(60 * 86_400);
/// Certificates start this long before they're issued, for clock skew between machines.
const BACKDATE: Duration = Duration::from_secs(3600);

/// The client pair's file names, in each `v<secs>/` directory and so under `current/`.
pub const CLIENT_CERT_FILE: &str = "client.crt";
const CLIENT_KEY_FILE: &str = "client.key";
const CA_CERT_FILE: &str = "ca.crt";
const CURRENT: &str = "current";

/// `<endpoint_id>.compute.chelabase.internal`: the name in a compute's certificate, and the
/// name the orchestrator verifies it against.
pub fn server_name(endpoint_id: &str) -> String {
    format!("{endpoint_id}.{SERVER_NAME_SUFFIX}")
}

/// A certificate and its key, both PEM, and when the certificate expires.
#[derive(Clone)]
pub struct IssuedCert {
    pub cert_pem: String,
    pub key_pem: String,
    pub not_after: SystemTime,
}

impl fmt::Debug for IssuedCert {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IssuedCert")
            .field("not_after", &self.not_after)
            .finish_non_exhaustive()
    }
}

/// The platform CA: it issues the computes' server certificates and the orchestrator's client
/// certificate. Only the worker holds its key.
#[derive(Clone)]
pub struct ComputeCa {
    cert_pem: String,
    key_pem: String,
}

impl fmt::Debug for ComputeCa {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ComputeCa").finish_non_exhaustive()
    }
}

/// `params`' validity: from `now` (less [`BACKDATE`]) to `now + validity`.
fn set_validity(
    params: &mut CertificateParams,
    now: SystemTime,
    validity: Duration,
) -> anyhow::Result<()> {
    let secs = |t: SystemTime| -> anyhow::Result<i64> {
        let d = t
            .duration_since(SystemTime::UNIX_EPOCH)
            .context("a time before 1970")?;
        i64::try_from(d.as_secs()).context("a time too far ahead")
    };
    let at = |t: SystemTime| -> anyhow::Result<_> {
        Ok(x509_parser::time::ASN1Time::from_timestamp(secs(t)?)
            .context("a certificate time")?
            .to_datetime())
    };
    params.not_before = at(now.checked_sub(BACKDATE).unwrap_or(now))?;
    params.not_after = at(now.checked_add(validity).context("a time too far ahead")?)?;
    Ok(())
}

fn random_serial() -> SerialNumber {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("OS random number generator");
    // A positive integer (RFC 5280 §4.1.2.2).
    bytes[0] &= 0x7f;
    bytes[0] |= 0x01;
    SerialNumber::from_slice(&bytes)
}

fn distinguished_name(cn: &str) -> DistinguishedName {
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, cn);
    dn
}

/// A CN compute_ctl accepts in `pg_ident.conf`: `^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$`.
fn check_cn(cn: &str) -> anyhow::Result<()> {
    let mut bytes = cn.bytes();
    let first_ok = bytes.next().is_some_and(|b| b.is_ascii_alphanumeric());
    let rest_ok = bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if !first_ok || !rest_ok || cn.len() > 64 {
        bail!("client certificate CN {cn:?} isn't [A-Za-z0-9][A-Za-z0-9._-]{{0,63}}");
    }
    Ok(())
}

/// An endpoint id is one DNS label of the server name.
fn check_endpoint_id(endpoint_id: &str) -> anyhow::Result<()> {
    let ok = !endpoint_id.is_empty()
        && endpoint_id.len() <= 63
        && !endpoint_id.starts_with('-')
        && !endpoint_id.ends_with('-')
        && endpoint_id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !ok {
        bail!("endpoint id {endpoint_id:?} can't be a DNS label of a server name");
    }
    Ok(())
}

fn p256_key() -> anyhow::Result<KeyPair> {
    KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).context("generating a P-256 key")
}

impl ComputeCa {
    /// A new self-signed CA, valid for [`CA_VALIDITY`] from `now`.
    pub fn generate(now: SystemTime) -> anyhow::Result<Self> {
        let key = p256_key()?;
        let mut params = CertificateParams::default();
        params.distinguished_name = distinguished_name(CA_CN);
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        params.serial_number = Some(random_serial());
        set_validity(&mut params, now, CA_VALIDITY)?;
        let cert = params
            .self_signed(&key)
            .context("signing the CA certificate")?;
        Ok(Self {
            cert_pem: cert.pem(),
            key_pem: key.serialize_pem(),
        })
    }

    /// The CA from its PEM certificate and key; the key must be the certificate's, P-256.
    pub fn from_pem(cert_pem: &str, key_pem: &str) -> anyhow::Result<Self> {
        let key = KeyPair::from_pem(key_pem).context("reading the CA key")?;
        if key.algorithm() != &PKCS_ECDSA_P256_SHA256 {
            bail!("the CA key isn't ECDSA P-256");
        }
        let (_, pem) =
            x509_parser::pem::parse_x509_pem(cert_pem.as_bytes()).context("reading the CA PEM")?;
        let cert = pem.parse_x509().context("reading the CA certificate")?;
        if cert.public_key().subject_public_key.data.as_ref() != key.public_key_raw() {
            bail!("the CA key doesn't match the CA certificate");
        }
        if !cert.is_ca() {
            bail!("the CA certificate isn't a CA");
        }
        let validity = cert.validity();
        let now = x509_parser::time::ASN1Time::now();
        if now > validity.not_after {
            bail!("the CA certificate has expired");
        }
        if now < validity.not_before {
            bail!("the CA certificate isn't valid yet");
        }
        let may_sign = cert
            .key_usage()
            .context("reading the CA's key usage")?
            .is_some_and(|usage| usage.value.key_cert_sign());
        if !may_sign {
            bail!("the CA certificate doesn't allow keyCertSign");
        }
        // The issuer must be readable for signing.
        Issuer::from_ca_cert_pem(cert_pem, key).context("reading the CA as an issuer")?;
        Ok(Self {
            cert_pem: cert_pem.to_string(),
            key_pem: key_pem.to_string(),
        })
    }

    /// `<dir>/ca.crt` and `<dir>/ca.key`.
    pub fn load(dir: &Path) -> anyhow::Result<Self> {
        let read = |name: &str| {
            let path = dir.join(name);
            std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))
        };
        Self::from_pem(&read("ca.crt")?, &read("ca.key")?)
            .with_context(|| format!("the compute CA in {}", dir.display()))
    }

    pub fn cert_pem(&self) -> &str {
        &self.cert_pem
    }

    pub fn key_pem(&self) -> &str {
        &self.key_pem
    }

    fn issue(
        &self,
        mut params: CertificateParams,
        now: SystemTime,
        validity: Duration,
    ) -> anyhow::Result<IssuedCert> {
        let ca_key = KeyPair::from_pem(&self.key_pem).context("reading the CA key")?;
        let issuer = Issuer::from_ca_cert_pem(&self.cert_pem, ca_key)
            .context("reading the CA certificate")?;
        let key = p256_key()?;
        params.is_ca = IsCa::ExplicitNoCa;
        params.serial_number = Some(random_serial());
        params.use_authority_key_identifier_extension = true;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        set_validity(&mut params, now, validity)?;
        let cert = params
            .signed_by(&key, &issuer)
            .context("signing the certificate")?;
        let cert_pem = cert.pem();
        Ok(IssuedCert {
            not_after: not_after(&cert_pem)?,
            cert_pem,
            key_pem: key.serialize_pem(),
        })
    }

    /// A compute's server certificate: CN and DNS SAN [`server_name`], EKU serverAuth,
    /// [`SERVER_VALIDITY`].
    pub fn issue_server(&self, endpoint_id: &str, now: SystemTime) -> anyhow::Result<IssuedCert> {
        check_endpoint_id(endpoint_id)?;
        let name = server_name(endpoint_id);
        let mut params = CertificateParams::default();
        params.distinguished_name = distinguished_name(&name);
        params.subject_alt_names = vec![SanType::DnsName(
            name.clone().try_into().context("the server name")?,
        )];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        self.issue(params, now, SERVER_VALIDITY)
            .with_context(|| format!("issuing the server certificate of {endpoint_id}"))
    }

    /// A client certificate with CN `cn` (the orchestrator's is [`ORCHESTRATOR_CN`]), EKU
    /// clientAuth, [`CLIENT_VALIDITY`].
    pub fn issue_client(&self, cn: &str, now: SystemTime) -> anyhow::Result<IssuedCert> {
        check_cn(cn)?;
        let mut params = CertificateParams::default();
        params.distinguished_name = distinguished_name(cn);
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        self.issue(params, now, CLIENT_VALIDITY)
            .with_context(|| format!("issuing the client certificate {cn}"))
    }
}

/// When the PEM certificate expires.
pub fn not_after(cert_pem: &str) -> anyhow::Result<SystemTime> {
    let (_, pem) =
        x509_parser::pem::parse_x509_pem(cert_pem.as_bytes()).context("reading a PEM block")?;
    let cert = pem.parse_x509().context("reading a certificate")?;
    let secs = cert.validity().not_after.timestamp();
    let secs = u64::try_from(secs).context("a certificate expiring before 1970")?;
    Ok(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
}

/// Whether `cert_pem` carries a valid signature by the key of `ca_pem`. A certificate or CA that
/// does not parse is simply not signed by it.
pub fn signed_by(cert_pem: &str, ca_pem: &str) -> bool {
    let (Ok((_, cert_block)), Ok((_, ca_block))) = (
        x509_parser::pem::parse_x509_pem(cert_pem.as_bytes()),
        x509_parser::pem::parse_x509_pem(ca_pem.as_bytes()),
    ) else {
        return false;
    };
    match (cert_block.parse_x509(), ca_block.parse_x509()) {
        (Ok(cert), Ok(ca)) => cert.verify_signature(Some(ca.public_key())).is_ok(),
        _ => false,
    }
}

/// Whether `key_pem` is the private key of the certificate `cert_pem` (same public key). An
/// unparsable key or certificate does not match.
pub fn key_matches_cert(cert_pem: &str, key_pem: &str) -> bool {
    let Ok(key) = KeyPair::from_pem(key_pem) else {
        return false;
    };
    let Ok((_, block)) = x509_parser::pem::parse_x509_pem(cert_pem.as_bytes()) else {
        return false;
    };
    block
        .parse_x509()
        .is_ok_and(|cert| cert.public_key().subject_public_key.data.as_ref() == key.public_key_raw())
}

/// Whether a certificate expiring at `not_after` has less than `before` left at `now` (an
/// expired one does).
pub fn needs_renewal(not_after: SystemTime, now: SystemTime, before: Duration) -> bool {
    match not_after.duration_since(now) {
        Ok(left) => left < before,
        Err(_) => true,
    }
}

fn tar_header(kind: tar::EntryType, mode: u32, size: u64, mtime: u64) -> tar::Header {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(kind);
    header.set_mode(mode);
    header.set_uid(0);
    header.set_gid(0);
    header.set_size(size);
    header.set_mtime(mtime);
    header
}

/// The archive the worker extracts at [`TLS_UPLOAD_ROOT`]: `chelabase/` and `chelabase/tls/`
/// (0755) and `server.crt`, `server.key`, `ca.crt` in it (0644), all root-owned. compute_ctl
/// copies the key into `pgdata` with mode 0600 for Postgres.
pub fn tls_files_tar(server: &IssuedCert, ca_pem: &str) -> anyhow::Result<Vec<u8>> {
    let mtime = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let mut builder = tar::Builder::new(Vec::new());
    for dir in ["chelabase/", "chelabase/tls/"] {
        let mut header = tar_header(tar::EntryType::Directory, 0o755, 0, mtime);
        builder
            .append_data(&mut header, dir, std::io::empty())
            .with_context(|| format!("adding {dir} to the TLS archive"))?;
    }
    // The key first: compute_ctl's watcher reloads on the certificate, so a reader that polls
    // between entries never pairs a new certificate with the old key.
    for (name, body) in [
        ("server.key", server.key_pem.as_str()),
        ("server.crt", server.cert_pem.as_str()),
        ("ca.crt", ca_pem),
    ] {
        let path = format!("chelabase/tls/{name}");
        let mut header = tar_header(tar::EntryType::Regular, 0o644, body.len() as u64, mtime);
        builder
            .append_data(&mut header, &path, body.as_bytes())
            .with_context(|| format!("adding {path} to the TLS archive"))?;
    }
    builder.into_inner().context("finishing the TLS archive")
}

fn write_file(path: &Path, body: &[u8], mode: u32) -> anyhow::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(body)
        .and_then(|()| file.sync_all())
        .with_context(|| format!("writing {}", path.display()))
}

/// Writes the client pair and the CA certificate into a new `<dir>/v<unix-secs>/` and points
/// `<dir>/current` at it with one atomic rename, so a reader never sees a key without its
/// certificate. `client.key` is 0600. Returns the new directory.
pub fn write_client_pair(
    dir: &Path,
    client: &IssuedCert,
    ca_pem: &str,
    now: SystemTime,
) -> anyhow::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let secs = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .context("a time before 1970")?
        .as_secs();
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    // One writer at a time across processes (the CLI and the sweep): the lock is on the
    // directory itself, released when `_lock` drops.
    let _lock = std::fs::File::open(dir).with_context(|| format!("opening {}", dir.display()))?;
    _lock
        .lock()
        .with_context(|| format!("locking {}", dir.display()))?;
    let previous = current_name(dir);
    // A second pair in the same second gets the next free name: `create` is the claim, so a
    // writer that isn't holding the lock still can't share a directory.
    let (name, version) = (0u32..)
        .find_map(|n| {
            let name = if n == 0 {
                format!("v{secs}")
            } else {
                format!("v{secs}.{n}")
            };
            let path = dir.join(&name);
            match std::fs::DirBuilder::new().mode(0o755).create(&path) {
                Ok(()) => Some(Ok((name, path))),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => None,
                Err(e) => Some(Err(e).with_context(|| format!("creating {}", path.display()))),
            }
        })
        .expect("an unbounded range")?;
    write_file(
        &version.join(CLIENT_CERT_FILE),
        client.cert_pem.as_bytes(),
        0o644,
    )?;
    write_file(
        &version.join(CLIENT_KEY_FILE),
        client.key_pem.as_bytes(),
        0o600,
    )?;
    write_file(&version.join(CA_CERT_FILE), ca_pem.as_bytes(), 0o644)?;
    // The entries are durable before `current` can point at them.
    std::fs::File::open(&version)
        .and_then(|d| d.sync_all())
        .with_context(|| format!("syncing {}", version.display()))?;
    // A name no other writer uses: the version directory's own name is unique in `dir`.
    let tmp = dir.join(format!("{CURRENT}.tmp.{}.{name}", std::process::id()));
    match std::fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("removing {}", tmp.display())),
    }
    std::os::unix::fs::symlink(&name, &tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    if let Err(e) = std::fs::rename(&tmp, dir.join(CURRENT)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("replacing {}/{CURRENT}", dir.display()));
    }
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    prune_versions(dir, &[Some(name.as_str()), previous.as_deref()]);
    Ok(version)
}

/// Writes a client pair with CN `cn` into `dir` (see [`write_client_pair`]) unless `dir/current`
/// already holds a complete one that is still good at `now`: all three files readable, `ca.crt`
/// the CA's certificate, the key matching the certificate, the certificate signed by `ca`,
/// carrying CN `cn`, and with at least [`CLIENT_RENEW_BEFORE`] left. A missing, corrupt or
/// foreign pair is reissued, not an error. Returns whether a pair was written; an invalid `cn`
/// is an error and nothing is written.
pub fn renew_client_pair(
    dir: &Path,
    ca: &ComputeCa,
    cn: &str,
    now: SystemTime,
) -> anyhow::Result<bool> {
    check_cn(cn)?;
    if client_pair_is_current(ca, &dir.join(CURRENT), cn, now)? {
        return Ok(false);
    }
    let client = ca.issue_client(cn, now)?;
    write_client_pair(dir, &client, ca.cert_pem(), now)?;
    Ok(true)
}

/// Whether `current/` holds a complete pair of `ca` for `cn` that isn't due for renewal. A
/// missing file or an unparsable certificate means "not current"; other I/O errors fail.
fn client_pair_is_current(
    ca: &ComputeCa,
    current: &Path,
    cn: &str,
    now: SystemTime,
) -> anyhow::Result<bool> {
    let read = |name: &str| -> anyhow::Result<Option<String>> {
        let path = current.join(name);
        match std::fs::read_to_string(&path) {
            Ok(s) => Ok(Some(s)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            // A regular file where `current`'s directory belongs reads as absent too.
            Err(e) if e.kind() == std::io::ErrorKind::NotADirectory => Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => Ok(None),
            // A symlink loop where `current` belongs (ELOOP) holds no pair either: a new one
            // replaces the link.
            Err(e) if is_symlink_loop(&e) => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    };
    let (Some(crt), Some(key), Some(ca_crt)) =
        (read(CLIENT_CERT_FILE)?, read(CLIENT_KEY_FILE)?, read(CA_CERT_FILE)?)
    else {
        return Ok(false);
    };
    if ca_crt.trim() != ca.cert_pem().trim()
        || !key_matches_cert(&crt, &key)
        || !signed_by(&crt, ca.cert_pem())
        || certificate_cn(&crt).as_deref() != Some(cn)
    {
        return Ok(false);
    }
    Ok(not_after(&crt).is_ok_and(|expires| !needs_renewal(expires, now, CLIENT_RENEW_BEFORE)))
}

/// Whether `e` is ELOOP (too many levels of symbolic links). `ErrorKind::FilesystemLoop` isn't
/// stable, so the errno is compared.
fn is_symlink_loop(e: &std::io::Error) -> bool {
    e.raw_os_error() == Some(libc::ELOOP)
}

/// The subject CN of a PEM certificate, if it parses and has exactly one.
fn certificate_cn(cert_pem: &str) -> Option<String> {
    let (_, pem) = x509_parser::pem::parse_x509_pem(cert_pem.as_bytes()).ok()?;
    let cert = pem.parse_x509().ok()?;
    let mut cns = cert.subject().iter_common_name();
    let cn = cns.next()?.as_str().ok()?.to_string();
    cns.next().is_none().then_some(cn)
}

/// The name of the version directory `<dir>/current` points at.
fn current_name(dir: &Path) -> Option<String> {
    let target = std::fs::read_link(dir.join(CURRENT)).ok()?;
    Some(target.file_name()?.to_string_lossy().into_owned())
}

/// Removes every `v*` directory of `dir` except the `keep`ers and whatever `current` points
/// at (a clock that steps back gives the newest pair the lowest name, so names don't say).
/// Best effort: a directory that stays is pruned by the next write.
fn prune_versions(dir: &Path, keep: &[Option<&str>]) {
    let current = current_name(dir);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // A temporary link a crashed writer left (ours was renamed away already).
        if name.starts_with(&format!("{CURRENT}.tmp")) {
            let _ = std::fs::remove_file(entry.path());
            continue;
        }
        if !name.starts_with('v')
            || current.as_deref() == Some(name.as_str())
            || keep.contains(&Some(name.as_str()))
        {
            continue;
        }
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// One loaded client pair: the rustls config built from it and its certificate's fingerprint.
struct Loaded {
    config: Arc<rustls::ClientConfig>,
    fingerprint: [u8; 32],
}

/// The orchestrator's client side of compute TLS: its client certificate, the CA it verifies
/// computes against, swapped whole when the pair on disk changes.
pub struct ClientTls {
    dir: PathBuf,
    loaded: ArcSwap<Loaded>,
}

impl fmt::Debug for ClientTls {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientTls")
            .field("dir", &self.dir)
            .field("fingerprint", &hex::encode(self.fingerprint()))
            .finish()
    }
}

/// The directory `<dir>/current` points at, read once so the three files come from one pair.
fn current_version(dir: &Path) -> anyhow::Result<PathBuf> {
    let link = dir.join(CURRENT);
    let target =
        std::fs::read_link(&link).with_context(|| format!("reading {}", link.display()))?;
    Ok(if target.is_absolute() {
        target
    } else {
        dir.join(target)
    })
}

fn read(path: &Path) -> anyhow::Result<Vec<u8>> {
    std::fs::read(path).with_context(|| format!("reading {}", path.display()))
}

fn fingerprint(cert: &[u8]) -> [u8; 32] {
    Sha256::digest(cert).into()
}

fn load_pair(dir: &Path) -> anyhow::Result<Loaded> {
    let version = current_version(dir)?;
    let cert = read(&version.join(CLIENT_CERT_FILE))?;
    let key = read(&version.join(CLIENT_KEY_FILE))?;
    let ca = read(&version.join(CA_CERT_FILE))?;
    let certs = CertificateDer::pem_slice_iter(&cert)
        .collect::<Result<Vec<_>, _>>()
        .context("reading client.crt")?;
    if certs.is_empty() {
        bail!("no certificate in client.crt");
    }
    let key = PrivateKeyDer::from_pem_slice(&key).context("reading client.key")?;
    let mut roots = rustls::RootCertStore::empty();
    for ca in CertificateDer::pem_slice_iter(&ca) {
        roots
            .add(ca.context("reading ca.crt")?)
            .context("adding the compute CA")?;
    }
    if roots.is_empty() {
        bail!("no certificate in ca.crt");
    }
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .context("TLS protocol versions")?
        .with_root_certificates(roots)
        .with_client_auth_cert(certs, key)
        .context("the client certificate and key")?;
    Ok(Loaded {
        config: Arc::new(config),
        fingerprint: fingerprint(&cert),
    })
}

impl ClientTls {
    /// The pair `<dir>/current/` points at.
    pub fn load(dir: &Path) -> anyhow::Result<Self> {
        let loaded = load_pair(dir)
            .with_context(|| format!("loading the compute client pair from {}", dir.display()))?;
        Ok(Self {
            dir: dir.to_path_buf(),
            loaded: ArcSwap::from_pointee(loaded),
        })
    }

    /// SHA-256 of the loaded `client.crt`'s bytes.
    pub fn fingerprint(&self) -> [u8; 32] {
        self.loaded.load().fingerprint
    }

    /// Loads the pair `current/` points at when its certificate differs from the loaded one;
    /// `true` when it swapped. On an error the loaded pair stays.
    pub fn reload_if_changed(&self) -> anyhow::Result<bool> {
        let version = current_version(&self.dir)?;
        let cert = read(&version.join(CLIENT_CERT_FILE))?;
        if fingerprint(&cert) == self.fingerprint() {
            return Ok(false);
        }
        let loaded = load_pair(&self.dir).with_context(|| {
            format!(
                "reloading the compute client pair from {}",
                self.dir.display()
            )
        })?;
        self.loaded.store(Arc::new(loaded));
        Ok(true)
    }

    /// A connector with the current pair, for tokio-postgres (`host` = the compute's
    /// [`server_name`]).
    pub fn connector(&self) -> tokio_postgres_rustls_improved::MakeRustlsConnect {
        self.snapshot().connector()
    }

    /// The current pair, held whole: its fingerprint and its connector come from the same
    /// pair whatever reloads happen after (a pool keyed by the fingerprint gets that pair's
    /// connector, never a newer one's).
    pub fn snapshot(&self) -> ClientTlsSnapshot {
        ClientTlsSnapshot {
            loaded: self.loaded.load_full(),
        }
    }
}

/// One client pair as [`ClientTls::snapshot`] took it.
#[derive(Clone)]
pub struct ClientTlsSnapshot {
    loaded: Arc<Loaded>,
}

impl fmt::Debug for ClientTlsSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientTlsSnapshot")
            .field("fingerprint", &hex::encode(self.fingerprint()))
            .finish()
    }
}

impl ClientTlsSnapshot {
    /// SHA-256 of this pair's `client.crt` bytes.
    pub fn fingerprint(&self) -> [u8; 32] {
        self.loaded.fingerprint
    }

    /// A connector with this pair, for tokio-postgres (`host` = the compute's
    /// [`server_name`]).
    pub fn connector(&self) -> tokio_postgres_rustls_improved::MakeRustlsConnect {
        tokio_postgres_rustls_improved::MakeRustlsConnect::new((*self.loaded.config).clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cns_and_endpoint_ids_are_checked() {
        for good in ["chelabase-orchestrator", "a", "A.b_c-9"] {
            check_cn(good).unwrap();
        }
        for bad in ["", "-a", "a b", "a#b", "a/b", "a'b", &"a".repeat(65)] {
            assert!(check_cn(bad).is_err(), "{bad:?}");
        }
        check_cn(&"a".repeat(64)).unwrap();
        for good in ["ep-abc123", "a"] {
            check_endpoint_id(good).unwrap();
        }
        for bad in ["", "ep.a", "EP-A", "-a", "a-", &"a".repeat(64)] {
            assert!(check_endpoint_id(bad).is_err(), "{bad:?}");
        }
        let ca = ComputeCa::generate(SystemTime::now()).unwrap();
        assert!(ca.issue_client("a b", SystemTime::now()).is_err());
        assert!(ca.issue_server("ep.evil", SystemTime::now()).is_err());
    }

    #[test]
    fn a_ca_key_that_isnt_the_certificates_is_refused() {
        let now = SystemTime::now();
        let a = ComputeCa::generate(now).unwrap();
        let b = ComputeCa::generate(now).unwrap();
        assert!(ComputeCa::from_pem(a.cert_pem(), b.key_pem()).is_err());
        // A leaf certificate isn't a CA.
        let leaf = a.issue_client(ORCHESTRATOR_CN, now).unwrap();
        assert!(ComputeCa::from_pem(&leaf.cert_pem, &leaf.key_pem).is_err());
    }

    #[test]
    fn a_failed_reload_keeps_the_loaded_pair() {
        let now = SystemTime::now();
        let ca = ComputeCa::generate(now).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let pair = ca.issue_client(ORCHESTRATOR_CN, now).unwrap();
        write_client_pair(dir.path(), &pair, ca.cert_pem(), now).unwrap();
        let tls = ClientTls::load(dir.path()).unwrap();
        let before = tls.fingerprint();
        // Two pairs in the same second land in two directories.
        let second = ca.issue_client(ORCHESTRATOR_CN, now).unwrap();
        let v2 = write_client_pair(dir.path(), &second, ca.cert_pem(), now).unwrap();
        std::fs::remove_file(v2.join(CLIENT_KEY_FILE)).unwrap();
        assert!(tls.reload_if_changed().is_err());
        assert_eq!(tls.fingerprint(), before);
    }

    #[test]
    fn a_ca_that_isnt_valid_yet_is_refused() {
        let future = SystemTime::now() + Duration::from_secs(10 * 86_400);
        let ca = ComputeCa::generate(future).unwrap();
        let err = ComputeCa::from_pem(ca.cert_pem(), ca.key_pem()).unwrap_err();
        assert!(format!("{err:#}").contains("valid"), "{err:#}");
    }

    #[test]
    fn a_stray_version_directory_is_pruned_and_current_is_kept() {
        let now = SystemTime::now();
        let ca = ComputeCa::generate(now).unwrap();
        let dir = tempfile::tempdir().unwrap();
        // What a writer that crashed between creating its directory and switching left.
        std::fs::create_dir(dir.path().join("v1")).unwrap();
        let pair = ca.issue_client(ORCHESTRATOR_CN, now).unwrap();
        let v = write_client_pair(dir.path(), &pair, ca.cert_pem(), now).unwrap();
        assert!(!dir.path().join("v1").exists());
        assert!(v.exists());
        ClientTls::load(dir.path()).unwrap();
    }

    #[test]
    fn the_tar_holds_the_key_the_certificate_and_the_ca() {
        let now = SystemTime::now();
        let ca = ComputeCa::generate(now).unwrap();
        let server = ca.issue_server("ep-a", now).unwrap();
        let tar = tls_files_tar(&server, ca.cert_pem()).unwrap();
        let mut archive = tar::Archive::new(tar.as_slice());
        let mut bodies = std::collections::BTreeMap::new();
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let name = entry.path().unwrap().to_string_lossy().into_owned();
            let mut body = String::new();
            std::io::Read::read_to_string(&mut entry, &mut body).unwrap();
            bodies.insert(name, body);
        }
        assert_eq!(bodies["chelabase/tls/server.key"], server.key_pem);
        assert_eq!(bodies["chelabase/tls/server.crt"], server.cert_pem);
        assert_eq!(bodies["chelabase/tls/ca.crt"], ca.cert_pem());
    }
}

#[cfg(test)]
#[path = "compute_tls_f4_tests.rs"]
mod f4_tests;
