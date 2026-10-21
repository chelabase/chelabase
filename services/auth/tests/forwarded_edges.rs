//! RT1 T1 ruling (e) edges beyond the RED set: the trusted-proxy setting names a proxy (a switch
//! is refused), host names resolve (an unresolvable one trusts nobody), and IPv4-mapped peers
//! count as IPv4.

use auth::guard::{TrustedProxies, client_ip_from};
use http::HeaderMap;
use std::net::IpAddr;

#[test]
fn a_switch_value_is_refused_and_a_bad_entry_named() {
    for raw in ["true", "1", "yes", "on", "TRUE"] {
        let err = TrustedProxies::parse(raw).err().expect(raw);
        assert!(err.contains("CHELABASE_AUTH_TRUSTED_PROXY"), "{err}");
    }
    assert!(TrustedProxies::parse("not a host!").is_err());
    assert!(TrustedProxies::parse("").unwrap().is_empty());
    assert!(!TrustedProxies::parse("gateway").unwrap().is_empty());
    assert!(
        !TrustedProxies::parse("10.0.0.5, [fd00::5]")
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn names_resolve_and_addresses_are_kept() {
    let t = TrustedProxies::parse("localhost,10.9.8.7").unwrap();
    let now = t.current().await;
    assert!(
        now.contains(&"10.9.8.7".parse::<IpAddr>().unwrap()),
        "{now:?}"
    );
    assert!(
        now.iter().any(IpAddr::is_loopback),
        "localhost resolves: {now:?}"
    );
    // An unresolvable name trusts nobody extra (fail closed), never everyone.
    let none = TrustedProxies::parse("does-not-exist.invalid").unwrap();
    assert!(none.current().await.is_empty());
}

#[test]
fn an_ipv4_mapped_peer_counts_as_its_ipv4_address() {
    let mut h = HeaderMap::new();
    h.insert("x-forwarded-for", "203.0.113.9".parse().unwrap());
    let gateway: IpAddr = "172.30.0.5".parse().unwrap();
    let mapped: IpAddr = "::ffff:172.30.0.5".parse().unwrap();
    assert_eq!(client_ip_from(&h, mapped, &[gateway]), "203.0.113.9");
    let other: IpAddr = "::ffff:10.0.0.1".parse().unwrap();
    assert_eq!(client_ip_from(&h, other, &[gateway]), "10.0.0.1");
    // A trusted peer whose header is empty is its own address.
    let mut blank = HeaderMap::new();
    blank.insert("x-forwarded-for", " , ".parse().unwrap());
    assert_eq!(client_ip_from(&blank, gateway, &[gateway]), "172.30.0.5");
}
