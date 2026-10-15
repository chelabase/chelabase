//! F6 (fork round 3 final fix round): RED tests for T11 m1, the preflight for a control-subnet
//! overlap with an existing Docker network. Pinned in `workbench::env`:
//!
//! - `cidr_overlaps(a: &str, b: &str) -> bool`: IPv4 CIDRs only; host bits are ignored; IPv6 or
//!   unparsable input never overlaps.
//! - `struct NetworkSubnets { pub name: String, pub subnets: Vec<String> }` and
//!   `parse_network_subnets(json: &str) -> anyhow::Result<Vec<NetworkSubnets>>`: the output of
//!   `docker network inspect $(docker network ls -q)` (an array; `IPAM.Config` may be null).
//! - `check_control_subnet(slug: &str, slot: u8, own_network: &str, networks: &[NetworkSubnets])
//!   -> anyhow::Result<()>`: fails when another network's IPv4 subnet overlaps the slot's
//!   `control_net(slot).subnet`; the stack's own network (`own_network`, e.g.
//!   `chelabase-feat-x_control`) may hold it. The error names the stack, the slot, the wanted
//!   subnet, and the offending network with its subnet.

use super::env::{NetworkSubnets, parse_network_inspect, check_control_subnet, cidr_overlaps, control_net, parse_network_subnets};

fn net(name: &str, subnets: &[&str]) -> NetworkSubnets {
    NetworkSubnets {
        name: name.to_string(),
        subnets: subnets.iter().map(|s| s.to_string()).collect(),
    }
}

#[test]
fn cidr_overlap_cases() {
    assert!(cidr_overlaps("10.213.4.0/22", "10.213.4.0/22"), "equal");
    assert!(cidr_overlaps("10.213.4.0/22", "10.213.6.0/23"), "contains");
    assert!(cidr_overlaps("10.213.6.0/23", "10.213.4.0/22"), "contained");
    assert!(cidr_overlaps("10.0.0.0/8", "10.213.4.0/22"), "a wide network");
    assert!(cidr_overlaps("10.213.5.77/22", "10.213.4.0/22"), "host bits are ignored");
    assert!(!cidr_overlaps("10.213.4.0/22", "10.213.8.0/22"), "adjacent");
    assert!(!cidr_overlaps("10.213.4.0/22", "172.17.0.0/16"), "disjoint");
    assert!(cidr_overlaps("0.0.0.0/0", "10.213.4.0/22"), "everything");
    assert!(cidr_overlaps("10.213.4.1/32", "10.213.4.0/22"), "a single address");
    assert!(!cidr_overlaps("fd00::/8", "10.213.4.0/22"), "IPv6 is ignored");
    assert!(!cidr_overlaps("fd00::/8", "fd00::/8"), "IPv6 never overlaps here");
    assert!(!cidr_overlaps("not a cidr", "10.213.4.0/22"), "garbage");
    assert!(!cidr_overlaps("10.213.4.0/33", "10.213.4.0/22"), "a prefix over 32");
    assert!(!cidr_overlaps("10.213.4.0", "10.213.4.0/22"), "no prefix");
}

#[test]
fn docker_network_inspect_output_is_parsed() {
    let json = r#"[
      {"Name": "bridge", "IPAM": {"Driver": "default", "Config": [{"Subnet": "172.17.0.0/16", "Gateway": "172.17.0.1"}]}},
      {"Name": "host", "IPAM": {"Driver": "default", "Config": null}},
      {"Name": "none", "IPAM": {"Driver": "default"}},
      {"Name": "dual", "IPAM": {"Config": [{"Subnet": "10.5.0.0/16"}, {"Subnet": "fd12::/64"}]}},
      {"Name": "v6only", "IPAM": {"Config": [{"Subnet": "fd13::/64"}]}},
      {"Name": "ranged", "IPAM": {"Config": [{"IPRange": "10.9.0.0/24"}]}}
    ]"#;
    let nets = parse_network_subnets(json).unwrap();
    let get = |n: &str| nets.iter().find(|x| x.name == n).unwrap_or_else(|| panic!("{n} missing")).subnets.clone();
    assert_eq!(get("bridge"), ["172.17.0.0/16"]);
    assert!(get("host").is_empty(), "null Config");
    assert!(get("none").is_empty(), "no Config");
    assert_eq!(get("dual"), ["10.5.0.0/16", "fd12::/64"], "the parser keeps what Docker says");
    assert!(get("ranged").is_empty(), "an entry without a Subnet");
    assert_eq!(nets.len(), 6);
    assert!(parse_network_subnets("not json").is_err());
    assert!(parse_network_subnets("[]").unwrap().is_empty());
}

#[test]
fn no_networks_or_no_overlap_passes() {
    let wanted = control_net(1).subnet;
    assert_eq!(wanted, "10.213.4.0/22");
    check_control_subnet("feat-x", 1, "chelabase-feat-x_control", &[]).unwrap();
    check_control_subnet(
        "feat-x",
        1,
        "chelabase-feat-x_control",
        &[net("bridge", &["172.17.0.0/16"]), net("other_control", &["10.213.8.0/22"]), net("empty", &[])],
    )
    .unwrap();
}

#[test]
fn an_overlapping_network_is_named_with_the_stack_and_slot() {
    let nets = [net("bridge", &["172.17.0.0/16"]), net("legacy_default", &["10.213.0.0/16"])];
    let err = check_control_subnet("feat-x", 1, "chelabase-feat-x_control", &nets)
        .unwrap_err()
        .to_string();
    for needle in ["feat-x", "slot 1", "10.213.4.0/22", "legacy_default", "10.213.0.0/16"] {
        assert!(err.contains(needle), "{needle:?} missing from: {err}");
    }
}

#[test]
fn another_stacks_control_network_is_named() {
    // Slot 2 is 10.213.8.0/22; a stray network from a registry that was reset holds half of it.
    let nets = [net("chelabase-old_control", &["10.213.10.0/23"])];
    let err = check_control_subnet("feat-y", 2, "chelabase-feat-y_control", &nets)
        .unwrap_err()
        .to_string();
    assert!(err.contains("chelabase-old_control") && err.contains("slot 2") && err.contains("feat-y"), "{err}");
}

#[test]
fn the_stacks_own_control_network_may_hold_its_subnet() {
    // A re-run of `up` finds the network Compose made last time.
    let nets = [net("chelabase-feat-x_control", &["10.213.4.0/22"])];
    check_control_subnet("feat-x", 1, "chelabase-feat-x_control", &nets).unwrap();
    // But not when the same subnet is held by a different network too.
    let nets = [
        net("chelabase-feat-x_control", &["10.213.4.0/22"]),
        net("squatter", &["10.213.4.0/24"]),
    ];
    let err = check_control_subnet("feat-x", 1, "chelabase-feat-x_control", &nets).unwrap_err().to_string();
    assert!(err.contains("squatter"), "{err}");
}

#[test]
fn ipv6_only_and_non_overlapping_networks_are_ignored() {
    let nets = [
        net("v6", &["fd00::/8"]),
        net("dual", &["192.168.0.0/16", "fd00:10:213::/48"]),
    ];
    check_control_subnet("feat-x", 1, "chelabase-feat-x_control", &nets).unwrap();
}

#[test]
fn every_slot_is_checked_against_its_own_subnet() {
    // Slot 63 is the last: 10.213.252.0/22.
    let nets = [net("n", &["10.213.255.0/24"])];
    assert!(check_control_subnet("s", 63, "p_control", &nets).is_err());
    assert!(check_control_subnet("s", 62, "p_control", &nets).is_ok());
    assert!(check_control_subnet("s", 0, "p_control", &[net("n", &["10.213.0.0/24"])]).is_err());
}

// Audit minor (commands.rs, `check_control_network`): each network is inspected on its own, so a
// network deleted between `network ls` and `network inspect` doesn't fail `up`. Pinned in
// `workbench::env`: `parse_network_inspect(id: &str, ok: bool, stdout: &str, stderr: &str)
// -> anyhow::Result<Vec<NetworkSubnets>>`: a successful inspect is parsed like
// `parse_network_subnets`; a failed one whose stderr says "No such network" (any case) yields no
// networks; any other failure is an error naming the id and the stderr.
#[test]
fn a_network_deleted_before_its_inspect_is_skipped() {
    let gone = "Error response from daemon: network abc123 not found";
    let gone2 = "Error: No such network: abc123";
    assert!(parse_network_inspect("abc123", false, "[]", gone2).unwrap().is_empty());
    assert!(parse_network_inspect("abc123", false, "", &gone2.to_lowercase()).unwrap().is_empty());
    // Docker's wording for a removed network differs by version; both forms are skipped.
    assert!(parse_network_inspect("abc123", false, "", gone).unwrap().is_empty(), "{gone}");
}

#[test]
fn other_inspect_failures_still_fail_and_name_the_network() {
    let err = parse_network_inspect("abc123", false, "", "Cannot connect to the Docker daemon")
        .unwrap_err()
        .to_string();
    assert!(err.contains("abc123") && err.contains("Cannot connect"), "{err}");
    assert!(parse_network_inspect("abc123", true, "not json", "").is_err());
}

#[test]
fn a_successful_inspect_is_parsed() {
    let json = r#"[{"Name": "n", "IPAM": {"Config": [{"Subnet": "10.1.0.0/16"}]}}]"#;
    let nets = parse_network_inspect("abc123", true, json, "").unwrap();
    assert_eq!((nets[0].name.as_str(), nets[0].subnets.as_slice()), ("n", ["10.1.0.0/16".to_string()].as_slice()));
}
