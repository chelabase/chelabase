use super::*;
use std::collections::HashMap;

const WORKBENCH: &[(&str, &str)] = &[
    ("CHELABASE_PUBLIC_SCHEME", "http"),
    ("CHELABASE_STUDIO_HOST", "studio.chelabase.localhost:8000"),
    ("CHELABASE_API_HOST", "api.chelabase.localhost:8000"),
    ("CHELABASE_AUTH_HOST", "auth.chelabase.localhost:8000"),
];

/// WORKBENCH with `extra` on top (an `extra` entry overrides; value "-" removes the name).
fn env(extra: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let mut map: HashMap<String, String> =
        WORKBENCH.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    for (k, v) in extra {
        if *v == "-" {
            map.remove(*k);
        } else {
            map.insert(k.to_string(), v.to_string());
        }
    }
    move |name: &str| map.get(name).cloned()
}

fn err(extra: &[(&str, &str)]) -> String {
    Entry::from_lookup(env(extra)).unwrap_err().to_string()
}

#[test]
fn workbench_urls_are_derived_from_the_hosts() {
    let e = Entry::from_lookup(env(&[])).unwrap();
    assert_eq!(e.scheme(), Scheme::Http);
    assert_eq!(e.studio_host(), "studio.chelabase.localhost:8000");
    assert_eq!(e.studio_origin(), "http://studio.chelabase.localhost:8000");
    assert_eq!(e.api_base(), "http://api.chelabase.localhost:8000");
    assert_eq!(e.issuer(), "http://auth.chelabase.localhost:8000");
    assert_eq!((e.docs_host(), e.landing_host(), e.mail_host()), (None, None, None));
}

#[test]
fn the_scheme_defaults_to_https_and_is_checked() {
    let e = Entry::from_lookup(env(&[
        ("CHELABASE_PUBLIC_SCHEME", "-"),
        ("CHELABASE_STUDIO_HOST", "studio.acme.com"),
        ("CHELABASE_API_HOST", "api.acme.com"),
        ("CHELABASE_AUTH_HOST", "auth.acme.com"),
    ]))
    .unwrap();
    assert_eq!(e.scheme(), Scheme::Https);
    assert_eq!(e.studio_origin(), "https://studio.acme.com");
    assert_eq!(e.issuer(), "https://auth.acme.com");
    let msg = err(&[("CHELABASE_PUBLIC_SCHEME", "ftp")]);
    assert!(msg.contains("CHELABASE_PUBLIC_SCHEME"), "{msg}");
}

#[test]
fn each_required_host_is_named_when_missing_or_blank() {
    for name in ["CHELABASE_STUDIO_HOST", "CHELABASE_API_HOST", "CHELABASE_AUTH_HOST"] {
        for value in ["-", "  "] {
            let msg = err(&[(name, value)]);
            assert!(msg.contains(name), "{name}={value:?}: {msg}");
        }
    }
}

#[test]
fn invalid_hosts_are_refused_with_their_variable() {
    for bad in ["http://studio.chelabase.localhost", "studio chelabase.localhost", "studio/x", "::1", "[::1", "studio.chelabase.localhost:http"] {
        let msg = err(&[("CHELABASE_STUDIO_HOST", bad)]);
        assert!(msg.contains("CHELABASE_STUDIO_HOST"), "{bad}: {msg}");
    }
}

#[test]
fn hosts_are_normalized_and_keep_their_port() {
    let e = Entry::from_lookup(env(&[
        ("CHELABASE_STUDIO_HOST", "Studio.Chelabase.LocalHost.:8000"),
        ("CHELABASE_DOCS_HOST", "docs.chelabase.localhost:8000"),
        ("CHELABASE_LANDING_HOST", " "),
        ("CHELABASE_MAIL_HOST", "mail.chelabase.localhost:8000"),
    ]))
    .unwrap();
    assert_eq!(e.studio_host(), "studio.chelabase.localhost:8000");
    assert_eq!(e.studio_origin(), "http://studio.chelabase.localhost:8000");
    assert_eq!(e.docs_host(), Some("docs.chelabase.localhost:8000"));
    assert_eq!(e.landing_host(), None);
    assert_eq!(e.mail_host(), Some("mail.chelabase.localhost:8000"));
}

#[test]
fn removed_variables_are_refused_with_their_replacement() {
    for (old, new) in [
        ("CHELABASE_API_HOSTS", "CHELABASE_API_HOST"),
        ("CHELABASE_STUDIO_ORIGINS", "CHELABASE_STUDIO_HOST"),
        ("CHELABASE_STUDIO_URL", "CHELABASE_STUDIO_HOST"),
    ] {
        let msg = err(&[(old, "http://old.example")]);
        assert!(msg.contains(old) && msg.contains(new), "{old}: {msg}");
        // Blank counts as unset.
        assert!(Entry::from_lookup(env(&[(old, " ")])).is_ok(), "{old} blank");
    }
}

#[test]
fn an_explicit_issuer_must_equal_the_derived_one() {
    assert!(Entry::from_lookup(env(&[("CHELABASE_ZITADEL_ISSUER", "http://auth.chelabase.localhost:8000")])).is_ok());
    assert!(Entry::from_lookup(env(&[("CHELABASE_ZITADEL_ISSUER", "http://auth.chelabase.localhost:8000/")])).is_ok());
    let msg = err(&[("CHELABASE_ZITADEL_ISSUER", "http://old-issuer.example")]);
    assert!(msg.contains("CHELABASE_ZITADEL_ISSUER") && msg.contains("CHELABASE_AUTH_HOST"), "{msg}");
}

#[test]
fn identical_hosts_are_refused() {
    let msg = err(&[("CHELABASE_AUTH_HOST", "API.chelabase.localhost:8000")]);
    assert!(msg.contains("CHELABASE_AUTH_HOST") && msg.contains("CHELABASE_API_HOST"), "{msg}");
    // A different port is a different host (P1).
    assert!(Entry::from_lookup(env(&[("CHELABASE_AUTH_HOST", "api.chelabase.localhost:9000")])).is_ok());
}

#[test]
fn studio_and_the_api_must_be_one_site() {
    // Bare *.localhost names are each their own site: sign-in cookies (SameSite=Strict) would drop.
    let msg = err(&[("CHELABASE_STUDIO_HOST", "studio.localhost:8000"), ("CHELABASE_API_HOST", "api.localhost:8000")]);
    assert!(msg.contains("CHELABASE_STUDIO_HOST") && msg.contains("CHELABASE_API_HOST"), "{msg}");
    let msg = err(&[("CHELABASE_API_HOST", "api.other.localhost:8000")]);
    assert!(msg.contains("CHELABASE_API_HOST"), "{msg}");
    // Production shape, and IP literals that are the same address.
    assert!(Entry::from_lookup(env(&[("CHELABASE_STUDIO_HOST", "studio.acme.com"), ("CHELABASE_API_HOST", "api.acme.com")])).is_ok());
    assert!(Entry::from_lookup(env(&[("CHELABASE_STUDIO_HOST", "127.0.0.1:3999"), ("CHELABASE_API_HOST", "127.0.0.1")])).is_ok());
    assert!(Entry::from_lookup(env(&[("CHELABASE_STUDIO_HOST", "127.0.0.2"), ("CHELABASE_API_HOST", "127.0.0.1")])).is_err());
    assert!(Entry::from_lookup(env(&[("CHELABASE_STUDIO_HOST", "studio.acme.com"), ("CHELABASE_API_HOST", "127.0.0.1")])).is_err());
}

#[test]
fn issuer_from_lookup_follows_p3() {
    let get = |pairs: &'static [(&'static str, &'static str)]| {
        move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
    };
    assert_eq!(issuer_from_lookup(get(&[])).unwrap(), None);
    assert_eq!(
        issuer_from_lookup(get(&[("CHELABASE_PUBLIC_SCHEME", "http"), ("CHELABASE_AUTH_HOST", "auth.chelabase.localhost:8000")])).unwrap(),
        Some("http://auth.chelabase.localhost:8000".to_string())
    );
    let msg = issuer_from_lookup(get(&[("CHELABASE_ZITADEL_ISSUER", "http://old-issuer.example")])).unwrap_err().to_string();
    assert!(msg.contains("CHELABASE_AUTH_HOST"), "{msg}");
    let msg = issuer_from_lookup(get(&[
        ("CHELABASE_AUTH_HOST", "auth.acme.com"),
        ("CHELABASE_ZITADEL_ISSUER", "http://old-issuer.example"),
    ]))
    .unwrap_err()
    .to_string();
    assert!(msg.contains("CHELABASE_ZITADEL_ISSUER"), "{msg}");
}

#[test]
fn host_helpers_keep_the_gateway_rules() {
    assert_eq!(normalize_host("[::1]:8000"), "[::1]");
    assert_eq!(normalize_host("API.Example.com.:8000"), "api.example.com");
    assert_eq!(host_port("a.b:8000"), Some(8000));
    assert_eq!(host_port("[::1]:8000"), Some(8000));
    assert_eq!(host_port("[::1]"), None);
    assert_eq!(host_port("a.b"), None);
    for good in ["api.example.com", "API.example.com:443", "[::1]", "127.0.0.1"] {
        assert!(validate_host(good).is_ok(), "{good}");
    }
    let msg = validate_host("::1").unwrap_err();
    assert!(msg.contains("bracket"), "{msg}");
}

// --- T1 review fix round: the host-setting invariant -----------------------------------------
//
// A host setting is accepted iff it is `name[:port]` where
// - name is an IPv4 literal, a bracketed IPv6 literal, or a DNS name: 1+ labels of 1–63
//   characters from [a-zA-Z0-9-], none starting or ending with `-`, at most one trailing dot,
//   and a last label that isn't all digits (`999.1.1.1` is neither an IP nor a name);
// - port is 1–65535 in plain digits (no sign, no leading zero).
// The same-site parent (P2) is the name minus its first label and must itself be ≥2 labels.

fn model_valid(entry: &str) -> bool {
    let (name, port) = if let Some(rest) = entry.strip_prefix('[') {
        let Some((addr, after)) = rest.split_once(']') else { return false };
        if addr.parse::<std::net::Ipv6Addr>().is_err() {
            return false;
        }
        match after {
            "" => return true,
            p => match p.strip_prefix(':') {
                Some(p) => return model_port(p),
                None => return false,
            },
        }
    } else {
        match entry.rsplit_once(':') {
            Some((n, p)) => (n, Some(p)),
            None => (entry, None),
        }
    };
    if port.is_some_and(|p| !model_port(p)) {
        return false;
    }
    if name.parse::<std::net::Ipv4Addr>().is_ok() {
        return true;
    }
    let name = name.strip_suffix('.').unwrap_or(name);
    let labels: Vec<&str> = name.split('.').collect();
    let label_ok = |l: &&str| {
        !l.is_empty()
            && l.len() <= 63
            && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            && !l.starts_with('-')
            && !l.ends_with('-')
    };
    labels.iter().all(label_ok) && !labels.last().unwrap().chars().all(|c| c.is_ascii_digit())
}

fn model_port(p: &str) -> bool {
    !p.is_empty()
        && p.chars().all(|c| c.is_ascii_digit())
        && !p.starts_with('0')
        && p.parse::<u32>().is_ok_and(|n| (1..=65535).contains(&n))
}

#[test]
fn validate_host_matches_the_model_on_hand_picked_cases() {
    for case in [
        "api.example.com", "API.example.com:443", "[::1]", "[::1]:8000", "127.0.0.1", "127.0.0.1:3999",
        "localhost", "a.com.", "a.com.:8000", "xn--bcher-kva.example",
        "studio..x", ".x.com", "a-.b", "-a.b", "a.com..", "a.b:0", "a.b:+80", "a.b:080", "a.b:65536",
        "999.1.1.1", "1.2.3", "a.123", "[::1]8000", "[zz]", "::1", "a b.c", "a_b.c", "",
        &format!("{}.com", "a".repeat(63)), &format!("{}.com", "a".repeat(64)),
    ] {
        assert_eq!(validate_host(case).is_ok(), model_valid(case), "{case:?}");
    }
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(4000))]
    #[test]
    fn validate_host_matches_the_model(entry in "[a-c0-9.:\\[\\]+_-]{0,14}") {
        proptest::prop_assert_eq!(validate_host(&entry).is_ok(), model_valid(&entry), "{:?}", entry);
    }
}

#[test]
fn the_same_site_parent_needs_two_non_empty_labels() {
    // Each refused by the host rule or by P2; none may start.
    for (studio, api) in [("a..b", "c..b"), ("studio.x", "api.x"), ("studio.localhost:8000", "api.localhost:8000")] {
        assert!(
            Entry::from_lookup(env(&[("CHELABASE_STUDIO_HOST", studio), ("CHELABASE_API_HOST", api)])).is_err(),
            "{studio} {api}"
        );
    }
}

#[test]
fn the_scheme_and_an_explicit_issuer_ignore_case_and_one_trailing_slash() {
    let e = Entry::from_lookup(env(&[("CHELABASE_PUBLIC_SCHEME", "HTTP")])).unwrap();
    assert_eq!(e.scheme(), Scheme::Http);
    assert!(Entry::from_lookup(env(&[("CHELABASE_ZITADEL_ISSUER", "HTTP://Auth.Chelabase.Localhost:8000")])).is_ok());
    assert!(Entry::from_lookup(env(&[("CHELABASE_ZITADEL_ISSUER", "http://auth.chelabase.localhost:8000//")])).is_err());
}

// T2 review: browsers drop a scheme's default port from Origin and Host, so a host setting
// carrying it could never match a request; it is refused.
#[test]
fn a_host_with_its_schemes_default_port_is_refused() {
    let msg = err(&[("CHELABASE_PUBLIC_SCHEME", "https"), ("CHELABASE_STUDIO_HOST", "studio.chelabase.localhost:443")]);
    assert!(msg.contains("CHELABASE_STUDIO_HOST") && msg.contains("443"), "{msg}");
    let msg = err(&[("CHELABASE_API_HOST", "api.chelabase.localhost:80")]);
    assert!(msg.contains("CHELABASE_API_HOST"), "{msg}");
    assert!(Entry::from_lookup(env(&[("CHELABASE_PUBLIC_SCHEME", "https"), ("CHELABASE_API_HOST", "api.chelabase.localhost:80")])).is_ok());
}

// End-of-step review: the loopback upstream defaults live in one place, here, for the gateway
// and the CLI bootstrap (which reaches Zitadel without the gateway).
#[test]
fn upstream_defaults_live_here_and_env_wins() {
    assert_eq!(zitadel_upstream(|_| None), DEFAULT_ZITADEL_UPSTREAM);
    assert_eq!(
        zitadel_upstream(|k| (k == "CHELABASE_ZITADEL_UPSTREAM").then(|| "http://zitadel:8080".to_string())),
        "http://zitadel:8080"
    );
    assert_eq!(zitadel_upstream(|k| (k == "CHELABASE_ZITADEL_UPSTREAM").then(|| " ".to_string())), DEFAULT_ZITADEL_UPSTREAM);
    for default in [DEFAULT_STUDIO_UPSTREAM, DEFAULT_ZITADEL_UPSTREAM, DEFAULT_DOCS_UPSTREAM, DEFAULT_LANDING_UPSTREAM, DEFAULT_MAIL_UPSTREAM] {
        assert!(default.starts_with("http://127.0.0.1:"), "{default}");
    }
}

// End-of-step review (user, 2026-10-13): platform mode refuses a plain-http public entry.
#[test]
fn platform_mode_refuses_an_http_entry() {
    use crate::mode::Mode;
    let http = Entry::from_lookup(env(&[])).unwrap();
    assert!(http.check_mode(Mode::SelfHosted).is_ok());
    let msg = http.check_mode(Mode::Platform).unwrap_err().to_string();
    assert!(msg.contains("CHELABASE_PUBLIC_SCHEME"), "{msg}");
    let https = Entry::from_lookup(env(&[("CHELABASE_PUBLIC_SCHEME", "https")])).unwrap();
    assert!(https.check_mode(Mode::Platform).is_ok());
}
