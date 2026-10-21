//! The worker's round-4 settings: `CHELABASE_STORAGE_TOKEN_REFRESH_SECS`,
//! `CHELABASE_REALTIME_LOGIN_CIDRS` and `CHELABASE_REALTIME_TLS_DIR`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use chelabase_neon::spec::MemberLogins;

use crate::config::WorkerConfig;

const IMAGE: (&str, &str) = ("CHELABASE_COMPUTE_IMAGE", "chelabase/compute-node-v17:test");

fn config(pairs: &[(&str, &str)]) -> anyhow::Result<WorkerConfig> {
    let map: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    WorkerConfig::from_lookup(move |k| map.get(k).cloned())
}

#[test]
fn the_refresh_interval_defaults_to_an_hour() {
    let c = config(&[IMAGE]).unwrap();
    assert_eq!(c.storage_token_refresh_every, Duration::from_secs(3600));
    let blank = config(&[IMAGE, ("CHELABASE_STORAGE_TOKEN_REFRESH_SECS", "  ")]).unwrap();
    assert_eq!(blank.storage_token_refresh_every, Duration::from_secs(3600));
}

#[test]
fn the_refresh_interval_is_read_in_whole_seconds() {
    let c = config(&[IMAGE, ("CHELABASE_STORAGE_TOKEN_REFRESH_SECS", "600")]).unwrap();
    assert_eq!(c.storage_token_refresh_every, Duration::from_secs(600));
    let padded = config(&[IMAGE, ("CHELABASE_STORAGE_TOKEN_REFRESH_SECS", " 90\n")]).unwrap();
    assert_eq!(padded.storage_token_refresh_every, Duration::from_secs(90));
}

#[test]
fn a_refresh_interval_that_cannot_keep_the_tokens_alive_is_refused() {
    // Zero would spin; a token lives 24 h (`STORAGE_TOKEN_TTL`), so an interval of that or more
    // lets it expire before the next pass.
    for bad in ["0", "abc", "-5", "1.5", "86400", "999999999"] {
        let err = config(&[IMAGE, ("CHELABASE_STORAGE_TOKEN_REFRESH_SECS", bad)])
            .expect_err(bad)
            .to_string();
        assert!(
            err.contains("CHELABASE_STORAGE_TOKEN_REFRESH_SECS"),
            "{bad}: {err}"
        );
    }
}

#[test]
fn realtime_defaults() {
    let c = config(&[IMAGE]).unwrap();
    assert_eq!(
        c.realtime_login_cidrs, None,
        "unset: the worker refuses to start"
    );
    assert_eq!(c.realtime_tls_dir, PathBuf::from(".chelabase/realtime-tls"));
    let blank = config(&[IMAGE, ("CHELABASE_REALTIME_TLS_DIR", " ")]).unwrap();
    assert_eq!(
        blank.realtime_tls_dir,
        PathBuf::from(".chelabase/realtime-tls")
    );
}

#[test]
fn realtime_settings_are_read() {
    let c = config(&[
        IMAGE,
        ("CHELABASE_REALTIME_TLS_DIR", "/m/.chelabase/realtime-tls"),
        (
            "CHELABASE_REALTIME_LOGIN_CIDRS",
            "10.213.4.12/32, 10.213.4.11/32",
        ),
    ])
    .unwrap();
    assert_eq!(
        c.realtime_tls_dir,
        PathBuf::from("/m/.chelabase/realtime-tls")
    );
    assert_eq!(
        c.realtime_login_cidrs,
        Some(vec![
            "10.213.4.12/32".to_string(),
            "10.213.4.11/32".to_string()
        ])
    );
}

#[test]
fn empty_realtime_cidrs_mean_no_address_rule_and_bad_ones_are_refused() {
    let empty = config(&[IMAGE, ("CHELABASE_REALTIME_LOGIN_CIDRS", "")]).unwrap();
    assert_eq!(empty.realtime_login_cidrs, Some(vec![]));
    let blank = config(&[IMAGE, ("CHELABASE_REALTIME_LOGIN_CIDRS", "  ")]).unwrap();
    assert_eq!(blank.realtime_login_cidrs, Some(vec![]));
    for bad in [
        "nope",
        "10.0.0.1",
        "10.0.0.0/33",
        "10.0.0.1/32,",
        ",10.0.0.1/32",
    ] {
        let err = config(&[IMAGE, ("CHELABASE_REALTIME_LOGIN_CIDRS", bad)])
            .expect_err(bad)
            .to_string();
        assert!(
            err.contains("CHELABASE_REALTIME_LOGIN_CIDRS"),
            "{bad}: {err}"
        );
    }
}

#[test]
fn the_member_and_realtime_cidrs_are_separate_settings() {
    let only_member = config(&[IMAGE, ("CHELABASE_MEMBER_LOGIN_CIDRS", "10.213.4.10/32")]).unwrap();
    assert_eq!(only_member.realtime_login_cidrs, None);
    let only_realtime =
        config(&[IMAGE, ("CHELABASE_REALTIME_LOGIN_CIDRS", "10.213.4.12/32")]).unwrap();
    assert_eq!(only_realtime.member_login_cidrs, None);
}

#[test]
fn the_parsed_cidrs_become_the_spec_policys_realtime_logins() {
    // SP4: the value `build()` puts on the worker, replacing C1's interim empty list.
    let c = config(&[
        IMAGE,
        (
            "CHELABASE_REALTIME_LOGIN_CIDRS",
            "10.213.4.12/32,10.213.4.11/32",
        ),
    ])
    .unwrap();
    assert_eq!(
        c.realtime_logins().unwrap(),
        MemberLogins::realtime(vec!["10.213.4.12/32".into(), "10.213.4.11/32".into()])
    );
    let empty = config(&[IMAGE, ("CHELABASE_REALTIME_LOGIN_CIDRS", "")]).unwrap();
    assert_eq!(
        empty.realtime_logins().unwrap(),
        MemberLogins::realtime(Vec::new())
    );
}

#[test]
fn unset_realtime_cidrs_are_a_startup_error_naming_the_setting() {
    // As round 3 P13 for the member CIDRs: a missing setting must not silently mean "any
    // address".
    let c = config(&[IMAGE]).unwrap();
    let err = c.realtime_logins().unwrap_err().to_string();
    assert!(err.contains("CHELABASE_REALTIME_LOGIN_CIDRS"), "{err}");
    assert!(err.contains("unset"), "{err}");
}
