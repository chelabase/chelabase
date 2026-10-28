//! RT1 final fix round, batch A (T7 hardening): a platform instance refuses to start with a
//! public development bearer. M9 already refuses the app-auth default; realtime's
//! (`chelabase-dev-realtime-internal`, the default of `CHELABASE_REALTIME_INTERNAL_TOKEN`) was
//! accepted, so anyone who knows the public default could call `/internal/realtime/*` (member
//! resolution, `/visible`) and the connection-info and key lookups realtime is allowed.
//!
//! Second finding in the area (app-auth was the first), so the invariant is tested for every
//! bearer the internal listener checks: in platform mode `check_mode` refuses each development
//! default, names its variable, and never repeats the token; self-hosted mode keeps them only
//! with `CHELABASE_DEV_DEFAULTS=1` (selfhost-install m5: the workbench's switch).

use chelabase_types::mode::Mode;
use chelabase_types::mode_policy::ModePolicy;
use orchestrator::internal::config::InternalConfig;

/// Every bearer the internal listener accepts, with its development default.
const BEARERS: [(&str, &str); 7] = [
    ("CHELABASE_PROXY_API_TOKEN", "chelabase-dev-proxy-api"),
    ("CHELABASE_STORCON_HOOK_TOKEN", "chelabase-dev-storcon-hook"),
    ("CHELABASE_DATA_KEYS_TOKEN", "chelabase-dev-data-keys"),
    (
        "CHELABASE_REST_INTERNAL_TOKEN",
        "chelabase-dev-rest-internal",
    ),
    (
        "CHELABASE_REALTIME_INTERNAL_TOKEN",
        "chelabase-dev-realtime-internal",
    ),
    (
        "CHELABASE_APP_AUTH_INTERNAL_TOKEN",
        "chelabase-dev-app-auth",
    ),
    (
        "CHELABASE_APP_SESSIONS_INTERNAL_TOKEN",
        "chelabase-dev-app-sessions",
    ),
];

/// A config where every bearer is its own secret except `left` (None: all own secrets), which
/// stays unset (its development default). The workbench's `CHELABASE_DEV_DEFAULTS=1` is set:
/// these tests are about platform mode, which refuses the defaults whatever it says.
fn config_without(left: Option<&'static str>) -> InternalConfig {
    InternalConfig::from_lookup(move |name| {
        if name == "CHELABASE_DEV_DEFAULTS" {
            return Some("1".to_string());
        }
        BEARERS
            .iter()
            .find(|(var, _)| *var == name && Some(*var) != left)
            .map(|(var, _)| format!("own-secret-for-{var}-0123456789abcdef"))
    })
    .unwrap()
}

#[test]
fn platform_refuses_the_default_realtime_bearer() {
    let c = config_without(Some("CHELABASE_REALTIME_INTERNAL_TOKEN"));
    assert_eq!(c.realtime_token, "chelabase-dev-realtime-internal");
    let err = c
        .check_mode(&ModePolicy::new(Mode::Platform))
        .expect_err("platform mode must refuse the public realtime bearer");
    let message = format!("{err:#}");
    assert!(
        message.contains("CHELABASE_REALTIME_INTERNAL_TOKEN"),
        "{message}"
    );
    assert!(
        !message.contains("chelabase-dev-realtime-internal"),
        "the message repeats the token: {message}"
    );
    // Self-hosted keeps the development default.
    assert!(c.check_mode(&ModePolicy::new(Mode::SelfHosted)).is_ok());
}

#[test]
fn platform_refuses_every_default_bearer_and_names_it() {
    let platform = ModePolicy::new(Mode::Platform);
    let self_hosted = ModePolicy::new(Mode::SelfHosted);
    // All own secrets: both modes start.
    let own = config_without(None);
    assert!(own.check_mode(&platform).is_ok());
    assert!(own.check_mode(&self_hosted).is_ok());
    for (var, default) in BEARERS {
        let c = config_without(Some(var));
        let err = c
            .check_mode(&platform)
            .err()
            .unwrap_or_else(|| panic!("platform mode accepted the public default of {var}"));
        let message = format!("{err:#}");
        assert!(message.contains(var), "{var}: {message}");
        assert!(
            !message.contains(default),
            "{var}: the message repeats the token: {message}"
        );
        assert!(
            c.check_mode(&self_hosted).is_ok(),
            "{var}: self-hosted keeps its default"
        );
    }
}

#[test]
fn a_secret_set_to_a_development_value_counts_as_the_default() {
    // Setting the variable to the public value is no better than leaving it unset.
    let c = InternalConfig::from_lookup(|name| {
        BEARERS
            .iter()
            .find(|(var, _)| *var == name)
            .map(|(var, default)| {
                if *var == "CHELABASE_REALTIME_INTERNAL_TOKEN" {
                    default.to_string()
                } else {
                    format!("own-secret-for-{var}-0123456789abcdef")
                }
            })
    })
    .unwrap();
    assert!(c.check_mode(&ModePolicy::new(Mode::Platform)).is_err());
}
