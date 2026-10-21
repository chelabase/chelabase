//! Fork round 4, C4 (SP4, spec §10): the orchestrator's `CHELABASE_REALTIME_LOGIN_CIDRS`. Unset
//! is a startup error, an empty value is "no address rule", and the parsed value replaces C1's
//! interim empty list in every compute spec the orchestrator builds.

use orchestrator::internal::config::InternalConfig;

fn with(value: Option<&'static str>) -> anyhow::Result<InternalConfig> {
    InternalConfig::from_lookup(move |k| {
        (k == "CHELABASE_REALTIME_LOGIN_CIDRS")
            .then(|| value.map(str::to_string))
            .flatten()
    })
}

#[test]
fn unset_is_the_startup_error_naming_the_variable() {
    let c = with(None).unwrap();
    assert_eq!(c.realtime_login_cidrs, None, "read raw: unset stays None");
    let err = format!("{:#}", c.realtime_logins().unwrap_err());
    assert!(err.contains("CHELABASE_REALTIME_LOGIN_CIDRS"), "{err}");
}

#[test]
fn empty_or_blank_means_no_address_rule() {
    for raw in ["", "   "] {
        let c = with(Some(raw)).unwrap();
        assert_eq!(c.realtime_login_cidrs, Some(vec![]), "{raw:?}");
        let logins = c.realtime_logins().unwrap();
        assert!(logins.allowed_cidrs.is_empty(), "{raw:?}");
        assert_eq!(logins.client_cns, ["chelabase-realtime"], "{raw:?}");
    }
}

#[test]
fn a_list_is_parsed_and_carries_the_realtime_cn() {
    let logins = with(Some("10.213.4.12/32, 10.213.4.11/32"))
        .unwrap()
        .realtime_logins()
        .unwrap();
    assert_eq!(logins.allowed_cidrs, ["10.213.4.12/32", "10.213.4.11/32"]);
    assert_eq!(logins.client_cns, ["chelabase-realtime"]);
}

#[test]
fn a_bad_cidr_is_a_startup_error_naming_the_variable() {
    let err = format!("{:#}", with(Some("not-a-cidr")).map(|_| ()).expect_err("refused"));
    assert!(err.contains("CHELABASE_REALTIME_LOGIN_CIDRS"), "{err}");
    let err = format!(
        "{:#}",
        with(Some("10.0.0.0/8,,10.1.0.0/16"))
            .map(|_| ())
            .expect_err("an empty entry")
    );
    assert!(err.contains("CHELABASE_REALTIME_LOGIN_CIDRS"), "{err}");
}

// The member and realtime lists are separate settings: one doesn't stand in for the other.
#[test]
fn the_member_list_does_not_stand_in_for_the_realtime_list() {
    let c = InternalConfig::from_lookup(|k| {
        (k == "CHELABASE_MEMBER_LOGIN_CIDRS").then(|| "10.0.0.0/8".to_string())
    })
    .unwrap();
    assert!(c.member_logins().is_ok());
    assert!(c.realtime_logins().is_err());
}

// SP4: C1's interim value (an empty list that would mean "any address") must be gone from the
// two places that build a spec.
#[test]
fn no_interim_realtime_logins_are_left_in_the_spec_builders() {
    for (file, text) in [
        ("internal/spec.rs", include_str!("../src/internal/spec.rs")),
        (
            "internal/hooks.rs",
            include_str!("../src/internal/hooks.rs"),
        ),
    ] {
        assert!(
            !text.contains("Interim"),
            "{file} still has an interim value"
        );
        assert!(
            !text.contains("MemberLogins::realtime(Vec::new())"),
            "{file} still builds an empty realtime list"
        );
    }
}
