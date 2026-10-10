//! I2: one rule, three implementations.
//!
//! **The invariant.** For any role graph (roles with LOGIN, owner-level and BYPASSRLS flags,
//! memberships with SET, INHERIT and ADMIN options, cycles and unlisted roles included):
//!
//! - *reach* (what a role can become) follows SET memberships only, and
//! - *refusal* (what makes a chain unsafe for a non-Admin member) walks every membership,
//!   whatever its options, and counts a role that holds ADMIN on another role as unsafe.
//!
//! The orchestrator's `RoleGraph::refusal` / `closure`, the worker's `unsafe_node` / `reach`
//! and Studio's `refusal` / `closure` (`apps/studio/src/lib/role-rules.ts`) give identical
//! answers on every graph. This file proves the first two against each other with a property
//! test, and writes the graphs with their expected answers to
//! `apps/studio/src/lib/role-rules.fixtures.json` (`CHELABASE_BLESS=1` rewrites it), which
//! Studio's test reads and must match. Without the variable the test fails when the file is out
//! of date.

use std::collections::{BTreeMap, BTreeSet};

use chelabase_metadata::{PasswordState, RoleAttributes, RoleLevel, RoleOrigin, RoleRow};
use orchestrator::access::{ChainRefusal, RoleGraph};
use proptest::prelude::*;
use serde_json::{Value, json};
use worker::member_roles::{MemberCatalog, reach, unsafe_node};

const OWNER: &str = "postgres";

/// Every name a graph may use. `ghost` is never listed: only memberships name it (as 6a omits
/// `neon_superuser`).
const POOL: [&str; 13] = [
    "r0", "r1", "r2", "r3", "r4", "r5", "pg_monitor", "cb_m_9", OWNER, "neon_superuser", "anon", "authenticated", "ghost",
];

#[derive(Clone, Debug)]
struct Node {
    name: &'static str,
    login: bool,
    owner_level: bool,
    bypass_rls: bool,
    /// `(group, set, inherit, admin)`, sorted by group, once each.
    member_of: Vec<(&'static str, bool, bool, bool)>,
}

/// A deterministic generator (splitmix64): a failure names its seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }

    fn pick(&mut self) -> &'static str {
        POOL[(self.next() % POOL.len() as u64) as usize]
    }
}

fn graph(seed: u64) -> Vec<Node> {
    let mut r = Rng(seed);
    let mut nodes: Vec<Node> = Vec::new();
    for name in POOL {
        if name == "ghost" || !(name == OWNER || r.chance(75)) {
            continue;
        }
        nodes.push(Node {
            name,
            login: name == OWNER || r.chance(15),
            owner_level: name == OWNER || r.chance(10),
            bypass_rls: r.chance(10),
            member_of: Vec::new(),
        });
    }
    for node in &mut nodes {
        let mut groups: BTreeMap<&'static str, (bool, bool, bool)> = BTreeMap::new();
        for _ in 0..(r.next() % 4) {
            let group = r.pick();
            if group != node.name {
                groups.insert(group, (r.chance(70), r.chance(60), r.chance(15)));
            }
        }
        node.member_of = groups.into_iter().map(|(g, (s, i, a))| (g, s, i, a)).collect();
    }
    nodes
}

fn orchestrator_graph(nodes: &[Node]) -> RoleGraph {
    let rows = nodes
        .iter()
        .map(|n| RoleRow {
            name: n.name.to_string(),
            origin: if n.name == OWNER { RoleOrigin::Owner } else { RoleOrigin::Sql },
            level: if n.owner_level { RoleLevel::OwnerLevel } else { RoleLevel::Plain },
            login: n.login,
            password: PasswordState::None,
            pending: false,
            created_at: chrono::DateTime::from_timestamp(0, 0).unwrap(),
            attributes: RoleAttributes {
                bypass_rls: n.bypass_rls,
                member_of: n.member_of.iter().map(|m| m.0.to_string()).collect(),
                member_no_set: n.member_of.iter().filter(|m| !m.1).map(|m| m.0.to_string()).collect(),
                member_admin: n.member_of.iter().filter(|m| m.3).map(|m| m.0.to_string()).collect(),
                ..RoleAttributes::default()
            },
        })
        .collect();
    RoleGraph::new(OWNER, rows)
}

/// What the worker's catalog read (`member_catalog_sql`) says about the same graph.
fn worker_catalog(nodes: &[Node]) -> MemberCatalog {
    let mut catalog = MemberCatalog::default();
    for n in nodes {
        catalog.roles.insert(n.name.to_string());
        if n.owner_level || n.bypass_rls {
            catalog.owner_level.insert(n.name.to_string());
        }
        if n.login {
            catalog.login.insert(n.name.to_string());
        }
        if n.member_of.iter().any(|m| m.3) {
            catalog.admin.insert(n.name.to_string());
        }
        for (group, set, _inherit, _admin) in &n.member_of {
            catalog.all_edges.entry(n.name.to_string()).or_default().insert(group.to_string());
            if *set {
                catalog.set_edges.entry(n.name.to_string()).or_default().insert(group.to_string());
            }
        }
    }
    catalog
}

fn why(r: ChainRefusal) -> &'static str {
    match r {
        ChainRefusal::OwnerLevel => "owner_level",
        ChainRefusal::Login => "login",
        ChainRefusal::Admin => "admin",
        ChainRefusal::Reserved => "reserved",
    }
}

fn agree(seed: u64) {
    let nodes = graph(seed);
    let graph = orchestrator_graph(&nodes);
    let catalog = worker_catalog(&nodes);
    for name in POOL {
        let refused = graph.refusal(name).is_some();
        assert_eq!(refused, unsafe_node(&catalog, name), "seed {seed}: refusal of {name}");
        let closure: BTreeSet<String> = graph.closure(name).into_iter().collect();
        assert_eq!(closure, reach(&catalog.set_edges, name), "seed {seed}: reach of {name}");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4000))]

    /// The invariant, orchestrator against worker, on random graphs.
    #[test]
    fn orchestrator_and_worker_agree_on_every_graph(seed in any::<u64>()) {
        agree(seed);
    }
}

#[test]
fn the_fixture_seeds_agree() {
    for seed in 1..=FIXTURE_GRAPHS {
        agree(seed);
    }
}

/// An INHERIT-only or ADMIN-only membership is no reach but is in the chain; a role holding
/// ADMIN is unsafe by itself. The same cases as the worker's and Studio's unit tests.
#[test]
fn the_named_cases() {
    let node = |name, member_of: Vec<(&'static str, bool, bool, bool)>| Node {
        name,
        login: false,
        owner_level: false,
        bypass_rls: false,
        member_of,
    };
    let nodes = vec![
        node(OWNER, vec![]),
        node("r0", vec![("r1", false, true, false)]),
        node("r1", vec![("r2", true, false, true)]),
        node("r2", vec![]),
        node("r3", vec![("r0", true, true, false)]),
    ];
    let graph = orchestrator_graph(&nodes);
    // r0 only inherits r1: no reach, but r1 holds ADMIN on r2, so r0 is refused.
    assert_eq!(graph.closure("r0"), ["r0".to_string()].into_iter().collect());
    let (chain, reason) = graph.refusal("r0").unwrap();
    assert_eq!((chain, why(reason)), (vec!["r0".to_string(), "r1".to_string()], "admin"));
    // r2 is a plain group; r1 can become it (SET) but holds ADMIN on it.
    assert!(graph.refusal("r2").is_none());
    assert_eq!(graph.refusal("r3").unwrap().0, ["r3", "r0", "r1"]);
    let catalog = worker_catalog(&nodes);
    assert!(unsafe_node(&catalog, "r0") && unsafe_node(&catalog, "r3") && !unsafe_node(&catalog, "r2"));
}

const FIXTURE_GRAPHS: u64 = 150;

fn fixture() -> Value {
    let graphs: Vec<Value> = (1..=FIXTURE_GRAPHS)
        .map(|seed| {
            let nodes = graph(seed);
            let g = orchestrator_graph(&nodes);
            let rows: Vec<Value> = nodes
                .iter()
                .map(|n| {
                    json!({
                        "name": n.name,
                        "origin": if n.name == OWNER { "owner" } else { "sql" },
                        "level": if n.owner_level { "owner_level" } else { "plain" },
                        "login": n.login,
                        "bypass_rls": n.bypass_rls,
                        "member_of": n.member_of.iter().map(|m| m.0).collect::<Vec<_>>(),
                        "member_no_set": n.member_of.iter().filter(|m| !m.1).map(|m| m.0).collect::<Vec<_>>(),
                        "member_admin": n.member_of.iter().filter(|m| m.3).map(|m| m.0).collect::<Vec<_>>(),
                    })
                })
                .collect();
            let expected: BTreeMap<&str, Value> = POOL
                .iter()
                .map(|name| {
                    let refusal = g.refusal(name).map(|(chain, r)| json!({ "chain": chain, "why": why(r) }));
                    let mut closure: Vec<String> = g.closure(name).into_iter().collect();
                    closure.sort();
                    (*name, json!({ "refusal": refusal, "closure": closure }))
                })
                .collect();
            json!({ "seed": seed, "rows": rows, "expected": expected })
        })
        .collect();
    json!({ "owner": OWNER, "names": POOL, "graphs": graphs })
}

/// `CHELABASE_BLESS=1 cargo test -p orchestrator --test role_rules_agree` rewrites the file
/// Studio's `role-rules.test.ts` reads.
#[test]
fn the_studio_fixture_is_current() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/studio/src/lib/role-rules.fixtures.json");
    let want = serde_json::to_string(&fixture()).unwrap() + "\n";
    if std::env::var_os("CHELABASE_BLESS").is_some() {
        std::fs::write(&path, want).unwrap();
        return;
    }
    let have = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(have == want, "{} is out of date: run with CHELABASE_BLESS=1", path.display());
}
