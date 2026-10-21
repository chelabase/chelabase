//! C5 (neon fork round 4): `ensure_machine_keys` issues realtime's first client pair, CN
//! `chelabase-realtime`, into `realtime-tls/current` through `compute_tls::renew_client_pair`.
//!
//! Pinned: `ensure_machine_keys(gateway, secrets, auth, neon, compute_ca_dir, orchestrator_tls,
//! realtime_tls)` and `MachineKeys::realtime_tls` (the canonical directory, as `orchestrator_tls`).

use super::bootstrap::ensure_machine_keys;
use chelabase_neon::compute_tls::{ComputeCa, ORCHESTRATOR_CN, REALTIME_CN, renew_client_pair, signed_by};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::time::SystemTime;

struct Dirs {
    root: tempfile::TempDir,
}

impl Dirs {
    fn new() -> Self {
        Self { root: tempfile::tempdir().unwrap() }
    }
    fn p(&self, name: &str) -> std::path::PathBuf {
        self.root.path().join(name)
    }
    fn ensure(&self) -> super::bootstrap::MachineKeys {
        ensure_machine_keys(
            &self.p("gateway"),
            &self.p("secrets"),
            &self.p("auth"),
            &self.p("neon"),
            &self.p("compute-ca"),
            &self.p("orchestrator-tls"),
            &self.p("realtime-tls"),
        )
        .unwrap()
    }
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn the_first_realtime_pair_is_issued_with_its_own_cn() {
    let d = Dirs::new();
    let keys = d.ensure();
    let rt = keys.realtime_tls.clone();
    assert_eq!(rt, d.p("realtime-tls").canonicalize().unwrap(), "the canonical directory");
    let cur = rt.join("current");
    for f in ["client.crt", "client.key", "ca.crt"] {
        assert!(cur.join(f).is_file(), "realtime-tls/current/{f}");
    }
    // client.key is 0600 and owned by the user the bootstrap runs as (CHELABASE_UID: the stack's
    // containers run as that uid and read the pair through a read-only mount).
    assert_eq!(mode(&cur.join("client.key")), 0o600);
    let me = std::fs::metadata(d.root.path()).unwrap().uid();
    assert_eq!(std::fs::metadata(cur.join("client.key")).unwrap().uid(), me);
    let ca = ComputeCa::load(&d.p("compute-ca")).unwrap();
    let crt = std::fs::read_to_string(cur.join("client.crt")).unwrap();
    assert!(signed_by(&crt, ca.cert_pem()), "signed by the platform CA");
    assert_eq!(
        std::fs::read_to_string(cur.join("ca.crt")).unwrap().trim(),
        ca.cert_pem().trim()
    );
    // CN: `renew_client_pair` says "already current" only for a pair of that CN (and that CA).
    let now = SystemTime::now();
    assert!(!renew_client_pair(&rt, &ca, REALTIME_CN, now).unwrap(), "the pair carries CN chelabase-realtime");
    assert!(renew_client_pair(&rt, &ca, ORCHESTRATOR_CN, now).unwrap(), "and not the orchestrator's CN");
}

// Invariant: the two pairs are separate identities. The orchestrator keeps its own CN and key, the
// realtime pair is a different key, and a second run changes neither (the worker's sweep renews).
#[test]
fn the_orchestrator_and_realtime_pairs_are_separate_and_stable() {
    let d = Dirs::new();
    let keys = d.ensure();
    let ca = ComputeCa::load(&d.p("compute-ca")).unwrap();
    let now = SystemTime::now();
    assert!(
        !renew_client_pair(&keys.orchestrator_tls, &ca, ORCHESTRATOR_CN, now).unwrap(),
        "the orchestrator pair keeps CN chelabase-orchestrator"
    );
    let key = |dir: &Path| std::fs::read(dir.join("current/client.key")).unwrap();
    let (orch, rt) = (key(&keys.orchestrator_tls), key(&keys.realtime_tls));
    assert_ne!(orch, rt, "one key per identity");
    let (link_o, link_r) = (
        std::fs::read_link(keys.orchestrator_tls.join("current")).unwrap(),
        std::fs::read_link(keys.realtime_tls.join("current")).unwrap(),
    );

    let again = d.ensure();
    assert_eq!((key(&again.orchestrator_tls), key(&again.realtime_tls)), (orch, rt), "a second run keeps both");
    assert_eq!(std::fs::read_link(again.realtime_tls.join("current")).unwrap(), link_r);
    assert_eq!(std::fs::read_link(again.orchestrator_tls.join("current")).unwrap(), link_o);
}

// Edge: a realtime pair of another CA (a regenerated CA) or an old orchestrator-only keys dir
// (no realtime-tls yet) is issued; nothing else under the keys dir is disturbed.
#[test]
fn a_keys_dir_from_before_gets_the_realtime_pair_and_keeps_the_rest() {
    let d = Dirs::new();
    let first = d.ensure();
    let orch_key = std::fs::read(first.orchestrator_tls.join("current/client.key")).unwrap();
    let ca_key = std::fs::read(d.p("compute-ca/ca.key")).unwrap();
    std::fs::remove_dir_all(&first.realtime_tls).unwrap();
    let again = d.ensure();
    assert!(again.realtime_tls.join("current/client.crt").is_file(), "reissued");
    assert_eq!(std::fs::read(again.orchestrator_tls.join("current/client.key")).unwrap(), orch_key);
    assert_eq!(std::fs::read(d.p("compute-ca/ca.key")).unwrap(), ca_key, "the CA is never regenerated");
}
