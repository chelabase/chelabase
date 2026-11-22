//! compute-network-reach m1: the stack's internal networks, their subnets, the engine version
//! check, the live check after `up`, and the move off the legacy networks (`<project>_compute`,
//! `<project>_control`).

use super::env::{
    NetworkSubnets, check_stack_subnets, compute_net, compute_network, control_net,
    control_network, legacy_networks,
};
use super::networks::{
    MigrationStep, check_engine_version, check_live_networks, legacy_member_ps_format, may_skip_up,
    plan_legacy_migration, precheck_existing_networks, worker_ps_args,
};

const P: &str = "chelabase-feat-x";

fn net(name: &str, subnets: &[&str]) -> NetworkSubnets {
    NetworkSubnets {
        name: name.to_string(),
        subnets: subnets.iter().map(|s| s.to_string()).collect(),
    }
}

#[test]
fn the_networks_have_their_new_names() {
    assert_eq!(compute_network(P), "chelabase-feat-x_compute-internal");
    assert_eq!(control_network(P), "chelabase-feat-x_control-internal");
    assert_eq!(
        legacy_networks(P),
        ["chelabase-feat-x_compute", "chelabase-feat-x_control"]
    );
}

#[test]
fn the_compute_subnet_follows_the_control_slot() {
    let c = compute_net(0);
    assert_eq!(
        (c.subnet.as_str(), c.range.as_str()),
        ("10.212.0.0/22", "10.212.2.0/23")
    );
    let c = compute_net(1);
    assert_eq!(
        (c.subnet.as_str(), c.range.as_str()),
        ("10.212.4.0/22", "10.212.6.0/23")
    );
    let c = compute_net(63);
    assert_eq!(
        (c.subnet.as_str(), c.range.as_str()),
        ("10.212.252.0/22", "10.212.254.0/23")
    );
    // Disjoint from every slot's control subnet and from every other slot's compute subnet.
    for a in 0..=63u8 {
        let ca = compute_net(a);
        assert!(
            super::env::cidr_overlaps(&ca.range, &ca.subnet),
            "slot {a}: range inside"
        );
        for b in 0..=63u8 {
            assert!(
                !super::env::cidr_overlaps(&ca.subnet, &control_net(b).subnet),
                "{a} vs control {b}"
            );
            if a != b {
                assert!(
                    !super::env::cidr_overlaps(&ca.subnet, &compute_net(b).subnet),
                    "{a} vs compute {b}"
                );
            }
        }
    }
}

#[test]
fn a_foreign_network_on_the_compute_subnet_is_named() {
    let nets = [
        net("bridge", &["172.17.0.0/16"]),
        net("squatter", &["10.212.5.0/24"]),
    ];
    let err = check_stack_subnets("feat-x", 1, P, &nets)
        .unwrap_err()
        .to_string();
    for needle in [
        "feat-x",
        "slot 1",
        "10.212.4.0/22",
        "squatter",
        "10.212.5.0/24",
    ] {
        assert!(err.contains(needle), "{needle:?} missing from: {err}");
    }
    // Still on the control subnet as before.
    let err = check_stack_subnets("feat-x", 1, P, &[net("other", &["10.213.4.0/24"])])
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("other") && err.contains("10.213.4.0/22"),
        "{err}"
    );
}

#[test]
fn the_stacks_own_legacy_networks_are_not_an_overlap() {
    let nets = [
        net("chelabase-feat-x_control", &["10.213.4.0/22"]),
        net("chelabase-feat-x_compute", &["10.212.4.0/22"]),
        net("chelabase-feat-x_control-internal", &["10.213.4.0/22"]),
        net("chelabase-feat-x_compute-internal", &["10.212.4.0/22"]),
    ];
    check_stack_subnets("feat-x", 1, P, &nets).unwrap();
    // Another stack's networks with the same suffixes are not ours.
    let err = check_stack_subnets(
        "feat-x",
        1,
        P,
        &[net("chelabase-feat-y_control", &["10.213.4.0/22"])],
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("chelabase-feat-y_control"), "{err}");
}

// --- the move off the legacy networks ---------------------------------------------------------

fn ps(rows: &[(&str, &str, &str, &str, &str, &str)]) -> String {
    rows.iter()
        .map(|(n, proj, svc, ep, stack, nets)| {
            format!("{n}\t{proj}\t{svc}\t{ep}\t{stack}\t{nets}\n")
        })
        .collect()
}

fn legacy() -> Vec<String> {
    legacy_networks(P).to_vec()
}

#[test]
fn the_ps_format_names_every_column_the_plan_reads() {
    let f = legacy_member_ps_format();
    for needle in [
        "{{.Names}}",
        "com.docker.compose.project",
        "com.docker.compose.service",
        chelabase_neon::compute::ENDPOINT_LABEL,
        chelabase_neon::compute::STACK_LABEL,
        "{{.Networks}}",
    ] {
        assert!(f.contains(needle), "{needle} missing from {f}");
    }
    assert_eq!(
        f.matches("\\t").count() + f.matches('\t').count(),
        5,
        "six tab-separated columns: {f}"
    );
}

#[test]
fn no_legacy_network_plans_nothing() {
    let out = ps(&[(
        "chelabase-feat-x-worker-1",
        P,
        "worker",
        "",
        "",
        "chelabase-feat-x_compute-internal",
    )]);
    assert!(
        plan_legacy_migration("feat-x", P, &out, &[])
            .unwrap()
            .is_empty()
    );
}

#[test]
fn computes_on_a_legacy_network_are_picked_for_removal() {
    let out = ps(&[
        (
            "ep-old",
            "",
            "",
            "ep-1",
            "feat-x",
            "chelabase-feat-x_compute,chelabase-feat-x_control",
        ),
        (
            "ep-control-only",
            "",
            "",
            "ep-2",
            "feat-x",
            "chelabase-feat-x_control",
        ),
        (
            "ep-new",
            "",
            "",
            "ep-3",
            "feat-x",
            "chelabase-feat-x_compute-internal,chelabase-feat-x_control-internal",
        ),
        (
            "ep-other-stack",
            "",
            "",
            "ep-4",
            "feat-y",
            "chelabase-feat-y_compute",
        ),
    ]);
    let plan = plan_legacy_migration("feat-x", P, &out, &legacy()).unwrap();
    let computes = plan
        .iter()
        .find_map(|s| match s {
            MigrationStep::RemoveComputes(c) => Some(c.clone()),
            _ => None,
        })
        .expect("a RemoveComputes step");
    assert_eq!(computes, ["ep-old", "ep-control-only"]);
}

#[test]
fn a_foreign_container_on_a_legacy_network_stops_up() {
    for (name, proj, svc, ep, stack) in [
        ("someone-elses", "", "", "", ""),
        ("other-project", "chelabase-feat-y", "worker", "", ""),
        ("other-stack-compute", "", "", "ep-9", "feat-y"),
        ("label-less-endpoint", "", "", "", "feat-x"),
    ] {
        let out = ps(&[
            (
                "chelabase-feat-x-worker-1",
                P,
                "worker",
                "",
                "",
                "chelabase-feat-x_compute",
            ),
            (
                name,
                proj,
                svc,
                ep,
                stack,
                "bridge,chelabase-feat-x_control",
            ),
        ]);
        let err = plan_legacy_migration("feat-x", P, &out, &legacy())
            .unwrap_err()
            .to_string();
        assert!(err.contains(name), "{name}: {err}");
        assert!(
            err.contains("chelabase-feat-x_control"),
            "{name}: names the network: {err}"
        );
    }
    // A container on a network whose name only starts like a legacy one is not a member.
    let out = ps(&[(
        "near",
        "",
        "",
        "",
        "",
        "chelabase-feat-x_compute-internal,chelabase-feat-x_compute2",
    )]);
    plan_legacy_migration("feat-x", P, &out, &legacy()).unwrap();
}

#[test]
fn the_projects_own_services_are_expected_legacy_members() {
    let out = ps(&[
        (
            "chelabase-feat-x-orchestrator-1",
            P,
            "orchestrator",
            "",
            "",
            "chelabase-feat-x_default,chelabase-feat-x_control",
        ),
        (
            "chelabase-feat-x-worker-1",
            P,
            "worker",
            "",
            "",
            "chelabase-feat-x_compute",
        ),
        (
            "chelabase-feat-x-nats-1",
            P,
            "nats",
            "",
            "",
            "chelabase-feat-x_default",
        ),
        ("ep-a", "", "", "ep-a", "feat-x", "chelabase-feat-x_compute"),
    ]);
    let plan = plan_legacy_migration("feat-x", P, &out, &legacy()).unwrap();
    assert_eq!(
        plan,
        [
            MigrationStep::StopWorker(vec!["chelabase-feat-x-worker-1".into()]),
            MigrationStep::RemoveProjectContainers(vec![
                "chelabase-feat-x-orchestrator-1".into(),
                "chelabase-feat-x-worker-1".into(),
                "chelabase-feat-x-nats-1".into(),
            ]),
            MigrationStep::RemoveComputes(vec!["ep-a".into()]),
            MigrationStep::RemoveNetworks(legacy()),
        ]
    );
}

#[test]
fn the_worker_stops_before_legacy_computes_are_removed() {
    let out = ps(&[
        ("ep-a", "", "", "ep-a", "feat-x", "chelabase-feat-x_compute"),
        (
            "chelabase-feat-x-worker-1",
            P,
            "worker",
            "",
            "",
            "chelabase-feat-x_compute",
        ),
    ]);
    // Only the control network is left: it alone is removed, still after the computes.
    let present = vec!["chelabase-feat-x_control".to_string()];
    let plan = plan_legacy_migration("feat-x", P, &out, &present).unwrap();
    let pos = |f: fn(&MigrationStep) -> bool| plan.iter().position(f).expect("step present");
    let worker = pos(|s| matches!(s, MigrationStep::StopWorker(_)));
    let services = pos(|s| matches!(s, MigrationStep::RemoveProjectContainers(_)));
    let computes = pos(|s| matches!(s, MigrationStep::RemoveComputes(_)));
    let networks = pos(|s| matches!(s, MigrationStep::RemoveNetworks(_)));
    assert!(
        worker < services && services < computes && computes < networks,
        "{plan:?}"
    );
    assert_eq!(plan[networks], MigrationStep::RemoveNetworks(present));
    // A network that isn't one of the two legacy names is never removed.
    let err = plan_legacy_migration("feat-x", P, &out, &["bridge".to_string()]).unwrap_err();
    assert!(err.to_string().contains("bridge"), "{err}");
}

// --- the live check after `up` ------------------------------------------------------------------

fn inspect(name: &str, internal: bool, v4: Option<&str>, v6: Option<&str>) -> serde_json::Value {
    let mut options = serde_json::Map::new();
    if let Some(v) = v4 {
        options.insert(
            "com.docker.network.bridge.gateway_mode_ipv4".into(),
            v.into(),
        );
    }
    if let Some(v) = v6 {
        options.insert(
            "com.docker.network.bridge.gateway_mode_ipv6".into(),
            v.into(),
        );
    }
    serde_json::json!({"Name": name, "Internal": internal, "Options": options})
}

fn wanted() -> Vec<String> {
    vec![compute_network(P), control_network(P)]
}

#[test]
fn internal_isolated_live_networks_pass() {
    let json = serde_json::json!([
        inspect(
            &compute_network(P),
            true,
            Some("isolated"),
            Some("isolated")
        ),
        inspect(
            &control_network(P),
            true,
            Some("isolated"),
            Some("isolated")
        ),
    ]);
    check_live_networks(&json.to_string(), &wanted()).unwrap();
}

#[test]
fn a_live_network_that_is_not_internal_fails_up() {
    let cases = [
        (
            inspect(
                &control_network(P),
                false,
                Some("isolated"),
                Some("isolated"),
            ),
            "internal",
        ),
        (
            inspect(&control_network(P), true, None, Some("isolated")),
            "gateway_mode_ipv4",
        ),
        (
            inspect(&control_network(P), true, Some("nat"), Some("isolated")),
            "gateway_mode_ipv4",
        ),
        (
            inspect(&control_network(P), true, Some("isolated"), None),
            "gateway_mode_ipv6",
        ),
    ];
    for (bad, needle) in cases {
        let json = serde_json::json!([
            inspect(
                &compute_network(P),
                true,
                Some("isolated"),
                Some("isolated")
            ),
            bad
        ]);
        let err = check_live_networks(&json.to_string(), &wanted())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(&control_network(P)) && err.contains(needle),
            "{needle}: {err}"
        );
    }
    // A network that isn't there at all fails too.
    let json = serde_json::json!([inspect(
        &compute_network(P),
        true,
        Some("isolated"),
        Some("isolated")
    )]);
    let err = check_live_networks(&json.to_string(), &wanted())
        .unwrap_err()
        .to_string();
    assert!(err.contains(&control_network(P)), "{err}");
    assert!(check_live_networks("not json", &wanted()).is_err());
}

// --- the engine version -------------------------------------------------------------------------

#[test]
fn an_engine_older_than_28_is_refused() {
    for old in ["27.5.1", "26.1.5+dfsg1", "20.10.24", "1.13.1"] {
        let err = check_engine_version(old).unwrap_err().to_string();
        assert!(err.contains(old) && err.contains("28"), "{old}: {err}");
    }
    for ok in [
        "28.0.0",
        "28.0.4",
        "28.5.2",
        "29.9.3",
        "29.10.0-rc.1",
        "100.0.0",
    ] {
        check_engine_version(ok).unwrap_or_else(|e| panic!("{ok}: {e}"));
    }
}

#[test]
fn an_unparsable_engine_version_is_refused() {
    for bad in [
        "",
        " ",
        "dev",
        "master-dockerproject-2026",
        "v29.1.0",
        "29",
        "29.",
        ".1.0",
        "x.1.0",
        "28a.1",
    ] {
        let err = check_engine_version(bad).unwrap_err().to_string();
        assert!(err.contains("28"), "{bad:?}: {err}");
    }
}

fn listing(names: &[String]) -> String {
    let mut all = vec!["bridge".to_string(), "chelabase-feat-x_default".to_string()];
    all.extend(names.iter().cloned());
    all.join("\n")
}

#[test]
fn a_pre_existing_wrong_network_stops_up_before_anything_starts() {
    let bad = serde_json::json!([
        inspect(&compute_network(P), true, Some("isolated"), Some("isolated")),
        inspect(&control_network(P), false, None, None),
    ])
    .to_string();
    let names = listing(&wanted());
    let err = precheck_existing_networks(P, &names, |asked| {
        assert_eq!(asked, wanted().as_slice());
        Ok(bad.clone())
    })
    .unwrap_err()
    .to_string();
    assert!(err.contains(&control_network(P)) && err.contains("internal"), "{err}");
    // Only the one that exists is inspected, and a good one passes.
    let one = serde_json::json!([inspect(
        &compute_network(P),
        true,
        Some("isolated"),
        Some("isolated")
    )])
    .to_string();
    precheck_existing_networks(P, &listing(&[compute_network(P)]), |asked| {
        assert_eq!(asked, [compute_network(P)]);
        Ok(one.clone())
    })
    .unwrap();
    // Another project's network of the same suffix is not this stack's.
    precheck_existing_networks(P, "chelabase-feat-x2_control-internal", |_| {
        panic!("nothing of this stack exists")
    })
    .unwrap();
    // Neither exists: nothing is inspected.
    precheck_existing_networks(P, &listing(&[]), |_| panic!("nothing to inspect")).unwrap();
}

#[test]
fn a_stack_on_legacy_networks_goes_through_up() {
    let none: Vec<String> = Vec::new();
    assert!(may_skip_up(true, &none, &none));
    assert!(!may_skip_up(false, &none, &none));
    assert!(!may_skip_up(true, &["worker".to_string()], &none));
    assert!(!may_skip_up(true, &none, &legacy()), "legacy networks need up's migration");
}

#[test]
fn the_worker_is_found_by_project_and_service_label() {
    let args = worker_ps_args(P).join(" ");
    assert!(
        args.contains(&format!("label=com.docker.compose.project={P}"))
            && args.contains("label=com.docker.compose.service=worker"),
        "{args}"
    );
}
