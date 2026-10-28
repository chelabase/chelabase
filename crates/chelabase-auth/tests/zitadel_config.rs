use chelabase_auth::ZitadelAuthConfig;

fn get(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
    move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
}

#[test]
fn the_issuer_comes_from_the_auth_host() {
    let c = ZitadelAuthConfig::from_lookup(get(&[
        ("CHELABASE_PUBLIC_SCHEME", "http"),
        ("CHELABASE_AUTH_HOST", "auth.chelabase.localhost:8000"),
        ("CHELABASE_ZITADEL_AUDIENCES", "p1"),
    ]))
    .unwrap();
    assert_eq!(c.issuer, "http://auth.chelabase.localhost:8000");
    assert_eq!(c.jwks_url, "http://auth.chelabase.localhost:8000/oauth/v2/keys");
    let c = ZitadelAuthConfig::from_lookup(get(&[
        ("CHELABASE_AUTH_HOST", "auth.acme.com"),
        ("CHELABASE_ZITADEL_JWKS_URL", "http://zitadel:8080/oauth/v2/keys"),
        ("CHELABASE_ZITADEL_AUDIENCES", "p1"),
    ]))
    .unwrap();
    assert_eq!(c.issuer, "https://auth.acme.com");
    assert_eq!(c.jwks_url, "http://zitadel:8080/oauth/v2/keys");
}

#[test]
fn no_auth_host_or_a_lone_issuer_is_refused() {
    assert!(ZitadelAuthConfig::from_lookup(get(&[])).is_err());
    assert!(ZitadelAuthConfig::from_lookup(get(&[("CHELABASE_ZITADEL_ISSUER", "http://old-issuer.example")])).is_err());
}

fn lookup(pairs: Vec<(&'static str, String)>) -> impl Fn(&str) -> Option<String> {
    move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone())
}

fn https_no_port(internal: &str) -> Vec<(&'static str, String)> {
    vec![
        ("CHELABASE_PUBLIC_SCHEME", "https".into()),
        ("CHELABASE_AUTH_HOST", "auth.example.com".into()),
        ("CHELABASE_ZITADEL_INTERNAL_URL", internal.into()),
        ("CHELABASE_ZITADEL_AUDIENCES", "p1".into()),
    ]
}

// #24: the URL the keys are fetched from and the issuer a token must name are set apart.
#[test]
fn the_in_network_url_sets_the_key_fetch_and_not_the_issuer() {
    let c = ZitadelAuthConfig::from_lookup(lookup(https_no_port("http://zitadel:8080"))).unwrap();
    assert_eq!(c.issuer, "https://auth.example.com");
    assert_eq!(c.jwks_url, "http://zitadel:8080/oauth/v2/keys");
    // A trailing slash is the same origin.
    let c = ZitadelAuthConfig::from_lookup(lookup(https_no_port("http://zitadel:8080/"))).unwrap();
    assert_eq!(c.jwks_url, "http://zitadel:8080/oauth/v2/keys");
    assert_eq!(c.issuer, "https://auth.example.com");
    // Blank is unset: the keys come from the issuer, as before.
    let c = ZitadelAuthConfig::from_lookup(lookup(https_no_port(" "))).unwrap();
    assert_eq!(c.jwks_url, "https://auth.example.com/oauth/v2/keys");
}

#[test]
fn an_explicit_jwks_url_wins_over_the_in_network_url() {
    let mut pairs = https_no_port("http://zitadel:8080");
    pairs.push(("CHELABASE_ZITADEL_JWKS_URL", "http://keys.internal:9000/keys".into()));
    let c = ZitadelAuthConfig::from_lookup(lookup(pairs)).unwrap();
    assert_eq!(c.jwks_url, "http://keys.internal:9000/keys");
    assert_eq!(c.issuer, "https://auth.example.com");
}

// The in-network URL is an origin (http or https, a host, an optional port) and nothing else:
// no path, query, fragment or credentials. Anything else refuses the config (the service
// doesn't start) rather than fetching keys somewhere unexpected.
#[test]
fn a_malformed_in_network_url_is_refused() {
    for bad in [
        "zitadel:8080",
        "ftp://zitadel:8080",
        "file:///etc/keys",
        "http://",
        "http://zitadel:8080/oauth/v2/keys",
        "http://zitadel:8080/evil",
        "http://zitadel:8080?x=1",
        "http://zitadel:8080#x",
        "http://user:pass@zitadel:8080",
        "http://zitadel:0",
        "http://zitadel:99999",
        "http://zit adel:8080",
    ] {
        let err = ZitadelAuthConfig::from_lookup(lookup(https_no_port(bad)));
        match err {
            Err(e) => assert!(e.to_string().contains("CHELABASE_ZITADEL_INTERNAL_URL"), "{bad}: {e}"),
            Ok(c) => panic!("{bad:?} was accepted: keys from {}", c.jwks_url),
        }
    }
}
