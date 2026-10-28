//! Selfhost-install m5 fix round, B1: rest's internal listener (`/internal/console/*`,
//! `/internal/schema/*`) never starts on its public development bearer outside the workbench.

use rest::app::Config;

fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: std::collections::HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |k| map.get(k).cloned()
}

const CA: (&str, &str) = ("CHELABASE_COMPUTE_CA_FILE", "/nonexistent/ca.crt");
const DEV: &str = "chelabase-dev-rest-internal";

// The auditor's adversarial case, as written: the process environment, self-hosted, no switch.
#[test]
fn rest_refuses_dev_bearer_without_dev_defaults() {
    // SAFETY: nextest runs each test in its own process; nothing else reads the environment.
    unsafe {
        std::env::set_var("CHELABASE_MODE", "self-hosted");
        std::env::remove_var("CHELABASE_DEV_DEFAULTS");
        std::env::remove_var("CHELABASE_REST_INTERNAL_TOKEN");
        std::env::remove_var("CHELABASE_REST_INTERNAL_TOKEN_FILE");
        std::env::set_var("CHELABASE_COMPUTE_CA_FILE", "/nonexistent/ca.crt");
    }
    match Config::from_env() {
        Err(e) => assert!(
            format!("{e:#}").contains("CHELABASE_REST_INTERNAL_TOKEN"),
            "{e:#}"
        ),
        Ok(c) => panic!(
            "rest started its internal listener with the public dev bearer: internal_token == {:?}",
            c.internal_token
        ),
    }
}

// Unset, set to the default, `=0` and platform refuse (naming the variable, never the token);
// `=1` in self-hosted mode keeps today's behaviour; a generated token always starts.
#[test]
fn dev_bearer_refused_without_dev_defaults() {
    let token = ("CHELABASE_REST_INTERNAL_TOKEN", DEV);
    let refused: [&[(&str, &str)]; 5] = [
        &[CA],
        &[CA, token],
        &[CA, ("CHELABASE_DEV_DEFAULTS", "0")],
        &[
            CA,
            ("CHELABASE_MODE", "platform"),
            ("CHELABASE_DEV_DEFAULTS", "1"),
        ],
        // Another service's public default is no better.
        &[
            CA,
            ("CHELABASE_REST_INTERNAL_TOKEN", "chelabase-dev-data-keys"),
        ],
    ];
    for pairs in refused {
        let err = match Config::from_lookup(lookup(pairs)) {
            Err(e) => format!("{e:#}"),
            Ok(_) => panic!("{pairs:?}: rest started on a development bearer"),
        };
        assert!(
            err.contains("CHELABASE_REST_INTERNAL_TOKEN"),
            "{pairs:?}: {err}"
        );
        assert!(
            !err.contains("chelabase-dev-"),
            "{pairs:?}: the message repeats the token: {err}"
        );
    }
    let c = Config::from_lookup(lookup(&[CA, ("CHELABASE_DEV_DEFAULTS", "1")])).unwrap();
    assert_eq!(c.internal_token, DEV);
    let c = Config::from_lookup(lookup(&[
        CA,
        ("CHELABASE_REST_INTERNAL_TOKEN", "generated-0123"),
    ]))
    .unwrap();
    assert_eq!(c.internal_token, "generated-0123");
    let c = Config::from_lookup(lookup(&[
        CA,
        ("CHELABASE_MODE", "platform"),
        ("CHELABASE_REST_INTERNAL_TOKEN", "generated-0123"),
    ]))
    .unwrap();
    assert_eq!(c.internal_token, "generated-0123");
}
