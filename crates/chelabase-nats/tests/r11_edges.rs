//! RT1 T1 edge cases beyond the RED set: inbox prefixes, blank values, no password in Debug.

use chelabase_nats::NatsConfig;
use std::collections::HashMap;

fn lookup(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<String, String> = vars
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |k| map.get(k).cloned()
}

#[test]
fn an_inbox_prefix_with_a_wildcard_blank_or_empty_token_is_refused() {
    for bad in ["_INBOX_*", "_INBOX.>", "a..b", "a.", "_INBOX worker"] {
        assert!(
            NatsConfig::from_lookup(lookup(&[("CHELABASE_NATS_INBOX_PREFIX", bad)])).is_err(),
            "{bad:?}"
        );
    }
    let ok = NatsConfig::from_lookup(lookup(&[(
        "CHELABASE_NATS_INBOX_PREFIX",
        "_INBOX_realtime",
    )]))
    .unwrap();
    assert_eq!(ok.inbox_prefix.as_deref(), Some("_INBOX_realtime"));
}

#[test]
fn blank_values_count_as_unset() {
    let c = NatsConfig::from_lookup(lookup(&[
        ("CHELABASE_NATS_URL", "  "),
        ("CHELABASE_NATS_USER", ""),
        ("CHELABASE_NATS_PASSWORD", " "),
        ("CHELABASE_NATS_INBOX_PREFIX", ""),
    ]))
    .unwrap();
    assert_eq!(c.url, chelabase_nats::DEFAULT_NATS_URL);
    assert_eq!((c.user, c.password, c.inbox_prefix), (None, None, None));
}

#[test]
fn debug_never_shows_the_password() {
    let c = NatsConfig {
        url: "nats://nats:4222".into(),
        user: Some("worker".into()),
        password: Some("very-secret-pw".into()),
        inbox_prefix: None,
    };
    let text = format!("{c:?}");
    assert!(
        !text.contains("very-secret-pw") && text.contains("worker"),
        "{text}"
    );
}
