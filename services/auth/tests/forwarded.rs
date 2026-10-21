//! RT1 T1 ruling (e): auth now sits on the compute network, so a compute can reach it directly.
//! `X-Forwarded-For` (which the rate limits key on) is believed only when the TCP peer is the
//! gateway; from anyone else the peer address is the client.

use auth::guard::client_ip_from;
use http::HeaderMap;
use std::net::IpAddr;

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

fn forwarded(values: &[&str]) -> HeaderMap {
    let mut h = HeaderMap::new();
    for v in values {
        h.append("x-forwarded-for", v.parse().unwrap());
    }
    h
}

const GATEWAY: &str = "172.30.0.5";

#[test]
fn the_gateways_rightmost_entry_is_the_client() {
    let h = forwarded(&["6.6.6.6, 203.0.113.9"]);
    assert_eq!(
        client_ip_from(&h, ip(GATEWAY), &[ip(GATEWAY)]),
        "203.0.113.9"
    );
    // Several headers: still the rightmost entry overall (the one the gateway appended).
    let h = forwarded(&["6.6.6.6", "203.0.113.10"]);
    assert_eq!(
        client_ip_from(&h, ip(GATEWAY), &[ip(GATEWAY)]),
        "203.0.113.10"
    );
}

#[test]
fn the_gateway_without_a_header_is_its_own_address() {
    assert_eq!(
        client_ip_from(&HeaderMap::new(), ip(GATEWAY), &[ip(GATEWAY)]),
        GATEWAY
    );
}

#[test]
fn anyone_else_is_their_peer_address_whatever_they_forward() {
    let compute = "10.213.6.17";
    let h = forwarded(&["203.0.113.9"]);
    assert_eq!(client_ip_from(&h, ip(compute), &[ip(GATEWAY)]), compute);
    assert_eq!(
        client_ip_from(&HeaderMap::new(), ip(compute), &[ip(GATEWAY)]),
        compute
    );
    // Nothing trusted: forwarded headers never count.
    assert_eq!(client_ip_from(&h, ip(GATEWAY), &[]), GATEWAY);
}

#[test]
fn ipv6_peers_are_written_plainly() {
    let h = forwarded(&["203.0.113.9"]);
    assert_eq!(client_ip_from(&h, ip("fd00::7"), &[ip(GATEWAY)]), "fd00::7");
    assert_eq!(
        client_ip_from(&h, ip("fd00::5"), &[ip("fd00::5")]),
        "203.0.113.9"
    );
}
