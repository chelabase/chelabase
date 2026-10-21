//! The worker's round-4 settings (F2, part 2): the refresh interval is at most half a token's
//! life, and realtime's client pair never shares the orchestrator's directory.

use std::collections::HashMap;
use std::time::Duration;

use crate::config::WorkerConfig;

const IMAGE: (&str, &str) = ("CHELABASE_COMPUTE_IMAGE", "chelabase/compute-node-v17:test");

fn config(pairs: &[(&str, &str)]) -> anyhow::Result<WorkerConfig> {
    let map: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    WorkerConfig::from_lookup(move |k| map.get(k).cloned())
}

// A pass that fails once must leave a retry before the token expires: the interval is at most
// half of `STORAGE_TOKEN_TTL` (24 h), so 43,200 s.
#[test]
fn the_refresh_interval_is_at_most_half_a_tokens_life() {
    const VAR: &str = "CHELABASE_STORAGE_TOKEN_REFRESH_SECS";
    assert_eq!(chelabase_neon::jwt::STORAGE_TOKEN_TTL.as_secs() / 2, 43_200);
    for ok in ["1", "3600", "43199", "43200"] {
        let c = config(&[IMAGE, (VAR, ok)]).unwrap_or_else(|e| panic!("{ok}: {e:#}"));
        assert_eq!(
            c.storage_token_refresh_every,
            Duration::from_secs(ok.parse().unwrap())
        );
    }
    for bad in ["43201", "50000", "86399"] {
        let err = config(&[IMAGE, (VAR, bad)]).expect_err(bad).to_string();
        assert!(err.contains(VAR), "{bad}: {err}");
    }
}

// The sweep writes realtime's pair (CN chelabase-realtime) and the orchestrator's pair into
// their own directories: one directory for both would make one pair replace the other.
#[test]
fn the_realtime_and_orchestrator_pair_directories_must_differ() {
    const REALTIME: &str = "CHELABASE_REALTIME_TLS_DIR";
    const COMPUTE: &str = "CHELABASE_COMPUTE_TLS_DIR";
    for (realtime, compute) in [
        ("/m/tls", "/m/tls"),
        ("/m/tls/", "/m/tls"),
        ("/m/./tls", "/m/tls"),
        ("/m//tls", "/m/tls"),
        (" /m/tls ", "/m/tls"),
        (".chelabase/x", ".chelabase/x"),
    ] {
        let err = config(&[IMAGE, (REALTIME, realtime), (COMPUTE, compute)])
            .expect_err(realtime)
            .to_string();
        assert!(
            err.contains(REALTIME) && err.contains(COMPUTE),
            "{realtime} / {compute}: {err}"
        );
    }
    // Only one set, to the other's default.
    let err = config(&[IMAGE, (REALTIME, ".chelabase/orchestrator-tls")])
        .expect_err("realtime on the orchestrator's default")
        .to_string();
    assert!(err.contains(REALTIME) && err.contains(COMPUTE), "{err}");
    let err = config(&[IMAGE, (COMPUTE, ".chelabase/realtime-tls")])
        .expect_err("the orchestrator on realtime's default")
        .to_string();
    assert!(err.contains(REALTIME) && err.contains(COMPUTE), "{err}");
    // Different directories, siblings or not, are fine.
    for (realtime, compute) in [
        ("/m/realtime-tls", "/m/orchestrator-tls"),
        ("/m/tls-a", "/m/tls"),
    ] {
        config(&[IMAGE, (REALTIME, realtime), (COMPUTE, compute)])
            .unwrap_or_else(|e| panic!("{realtime} / {compute}: {e:#}"));
    }
    config(&[IMAGE]).expect("the defaults differ");
}
