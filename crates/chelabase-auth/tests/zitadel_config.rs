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
