use super::*;
use crate::fakes::{FakeWorker, ready_database, worker};
use chelabase_metadata::members::MemberRecord;
use chelabase_types::access::PlatformRole;
use proptest::prelude::*;

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|s| s.to_string()).collect()
}

fn ours(role: &str) -> ActualMembership {
    ActualMembership {
        role: role.into(),
        grantor: None,
        shaped: true,
    }
}

fn desired(role_name: &str, roles: &[&str], rotate: bool) -> DesiredMemberRole {
    DesiredMemberRole {
        member_id: Uuid::nil(),
        role_name: role_name.into(),
        roles: set(roles),
        verifier: format!("SCRAM-{role_name}"),
        rotate,
        locked: false,
        kind: RoleKind::Member,
    }
}

fn actual(role_name: &str, memberships: Vec<ActualMembership>) -> ActualMemberRole {
    ActualMemberRole {
        role_name: role_name.into(),
        attributes_ok: true,
        settings: Vec::new(),
        verifier: Some(format!("SCRAM-{role_name}")),
        memberships,
        valid_until: None,
    }
}

// --- Pure parts ---------------------------------------------------------------------------

#[test]
fn member_role_name_is_37_chars_and_stable() {
    let member = Uuid::parse_str("0123456789abcdef0123456789abcdef").unwrap();
    let name = member_role_name(member);
    assert_eq!(name, "cb_m_0123456789abcdef0123456789abcdef");
    assert_eq!(name.len(), 37);
    assert_eq!(member_role_name(member), name, "stable");
    assert_ne!(member_role_name(Uuid::new_v4()), name, "per member");
}

#[test]
fn plan_member_roles_creates_grants_revokes_drops() {
    let want = vec![
        desired("cb_m_new", &["sales", "billing"], false),
        desired("cb_m_kept", &["billing", "hr"], false),
        desired("cb_m_rotated", &["billing"], true),
    ];
    let have = vec![
        actual(
            "cb_m_kept",
            vec![
                ours("billing"),
                ours("sales"),
                // Another grantor's membership is revoked as that grantor, wanted or not.
                ActualMembership {
                    role: "hr".into(),
                    grantor: Some("postgres".into()),
                    shaped: true,
                },
            ],
        ),
        ActualMemberRole {
            role_name: "cb_m_rotated".into(),
            attributes_ok: false,
            settings: vec![None, Some("app".into())],
            verifier: Some("SCRAM-old".into()),
            valid_until: None,
            // Ours but with the wrong options: granted again (which fixes them).
            memberships: vec![ActualMembership {
                role: "billing".into(),
                grantor: None,
                shaped: false,
            }],
        },
        actual("cb_m_gone", vec![ours("billing")]),
    ];
    use MemberRoleStep::*;
    assert_eq!(
        plan_member_roles(&want, &have),
        vec![
            Revoke {
                role: "cb_m_kept".into(),
                granted: "hr".into(),
                grantor: Some("postgres".into())
            },
            Revoke {
                role: "cb_m_kept".into(),
                granted: "sales".into(),
                grantor: None
            },
            Create {
                role: "cb_m_new".into(),
                verifier: "SCRAM-cb_m_new".into()
            },
            SetPassword {
                role: "cb_m_rotated".into(),
                verifier: "SCRAM-cb_m_rotated".into()
            },
            Reshape {
                role: "cb_m_rotated".into(),
                databases: vec!["app".into()]
            },
            Grant {
                role: "cb_m_kept".into(),
                granted: "hr".into()
            },
            Grant {
                role: "cb_m_new".into(),
                granted: "billing".into()
            },
            Grant {
                role: "cb_m_new".into(),
                granted: "sales".into()
            },
            Grant {
                role: "cb_m_rotated".into(),
                granted: "billing".into()
            },
            Drop {
                role: "cb_m_gone".into()
            },
        ]
    );
}

#[test]
fn plan_is_empty_when_converged() {
    let want = vec![
        desired("cb_m_a", &["billing", "sales"], false),
        desired("cb_m_b", &["anon"], false),
    ];
    let have = vec![
        actual("cb_m_a", vec![ours("billing"), ours("sales")]),
        actual("cb_m_b", vec![ours("anon")]),
    ];
    assert_eq!(plan_member_roles(&want, &have), vec![]);
    assert_eq!(plan_member_roles(&[], &[]), vec![]);
}

#[test]
fn owner_is_never_granted() {
    let want = vec![desired(
        "cb_m_a",
        &[
            OWNER_ROLE,
            "owner",
            "OWNER",
            "cb_m_other",
            "chelabase_admin",
            "pg_read_all_data",
            "authenticator",
            "billing",
        ],
        false,
    )];
    let granted: Vec<String> = plan_member_roles(&want, &[])
        .into_iter()
        .filter_map(|s| match s {
            MemberRoleStep::Grant { granted, .. } => Some(granted),
            _ => None,
        })
        .collect();
    assert_eq!(granted, ["billing"]);
    // A membership in the owner found on a member role is revoked.
    let have = vec![actual("cb_m_a", vec![ours(OWNER_ROLE), ours("billing")])];
    let want = vec![desired("cb_m_a", &[OWNER_ROLE, "billing"], false)];
    assert_eq!(
        plan_member_roles(&want, &have),
        vec![MemberRoleStep::Revoke {
            role: "cb_m_a".into(),
            granted: OWNER_ROLE.into(),
            grantor: None
        }]
    );
    // A tenant admin's `All` never holds it, nor any owner-level role (R16).
    let catalog = MemberCatalog {
        roles: set(&[
            OWNER_ROLE,
            "billing",
            "cb_m_x",
            "anon",
            "authenticator",
            "neon_superuser",
            "pg_monitor",
            "app",
        ]),
        owner_level: set(&[OWNER_ROLE, "neon_superuser", "app"]),
        ..MemberCatalog::default()
    };
    let (roles, skipped) = granted_roles(&tenant_admin_grant(), &catalog);
    // Nor the data API's `anon` unless named (R18).
    assert_eq!(roles, set(&["billing"]));
    assert!(skipped.is_empty());
}

fn edges(pairs: &[(&str, &str)]) -> BTreeMap<String, BTreeSet<String>> {
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (from, to) in pairs {
        out.entry(from.to_string())
            .or_default()
            .insert(to.to_string());
    }
    out
}

#[test]
fn reach_follows_set_chains_and_cycles() {
    let e = edges(&[("a", "b"), ("b", "c"), ("c", "a"), ("d", "e")]);
    assert_eq!(reach(&e, "a"), set(&["a", "b", "c"]));
    assert_eq!(reach(&e, "d"), set(&["d", "e"]));
    assert_eq!(reach(&e, "z"), set(&["z"]));
}

fn chains() -> MemberCatalog {
    MemberCatalog {
        roles: set(&[
            "app",
            "billing",
            "billing_admin",
            "sales",
            "hr",
            "ops",
            "audit",
            "neon_superuser",
        ]),
        owner_level: set(&["app", "neon_superuser"]),
        // billing_admin can become billing; ops can become hr, which can become audit; sales
        // can become neon_superuser.
        set_edges: edges(&CHAIN_EDGES),
        all_edges: edges(&CHAIN_EDGES),
        ..MemberCatalog::default()
    }
}

const CHAIN_EDGES: [(&str, &str); 4] = [
    ("billing_admin", "billing"),
    ("ops", "hr"),
    ("hr", "audit"),
    ("sales", "neon_superuser"),
];

/// A membership with the SET option (and any others): both walks follow it.
fn link(catalog: &mut MemberCatalog, from: &str, to: &str) {
    catalog.set_edges.entry(from.into()).or_default().insert(to.into());
    catalog.all_edges.entry(from.into()).or_default().insert(to.into());
}

/// A membership without the SET option: only the safety walk follows it (I2).
fn link_no_set(catalog: &mut MemberCatalog, from: &str, to: &str) {
    catalog.all_edges.entry(from.into()).or_default().insert(to.into());
}

fn only(roles: &[&str]) -> Grant {
    Grant {
        platform_role: PlatformRole::Developer,
        roles: RoleSet::Only(roles.iter().map(|r| PgRole::parse(r)).collect()),
    }
}

/// R21: granting a role includes its chain. The member role is granted only the granted role;
/// what it reaches through `SET` comes with it.
#[test]
fn granted_role_brings_its_chain() {
    let catalog = chains();
    let (roles, skipped) = granted_roles(&only(&["billing_admin", "ops", "ghost"]), &catalog);
    assert_eq!(
        roles,
        set(&["billing_admin", "ops"]),
        "granted roles only, not their chains"
    );
    assert_eq!(skipped, vec![("ghost".to_string(), Skipped::Missing)]);
    assert_eq!(
        closure(&catalog.set_edges, &roles),
        set(&["audit", "billing", "billing_admin", "hr", "ops"])
    );
}

/// R21 with R16: a granted role whose chain reaches an owner-level role is left out entirely
/// (nor what it alone reaches); the other granted roles stay. A tenant admin's `All` is every
/// role whose chain is safe.
#[test]
fn chain_reaching_owner_level_excludes_the_granted_role() {
    let catalog = chains();
    let (roles, mut skipped) = granted_roles(&only(&["app", "sales", "billing"]), &catalog);
    assert_eq!(roles, set(&["billing"]));
    skipped.sort();
    assert_eq!(
        skipped,
        vec![
            ("app".to_string(), Skipped::Unsafe),
            ("sales".to_string(), Skipped::Unsafe)
        ]
    );
    let (all, _) = granted_roles(&tenant_admin_grant(), &catalog);
    assert_eq!(
        all,
        set(&["audit", "billing", "billing_admin", "hr", "ops"])
    );
}

/// R22: a member role is only ever a member of NOLOGIN group roles. A LOGIN role is never
/// given, by name or through `All`.
#[test]
fn login_roles_are_never_given() {
    let catalog = MemberCatalog {
        login: set(&["app_login"]),
        ..chains()
    };
    let mut catalog = catalog;
    catalog.roles.insert("app_login".into());
    let (roles, skipped) = granted_roles(&only(&["app_login", "billing"]), &catalog);
    assert_eq!(roles, set(&["billing"]));
    assert_eq!(skipped, vec![("app_login".to_string(), Skipped::Unsafe)]);
    let (all, _) = granted_roles(&tenant_admin_grant(), &catalog);
    assert!(!all.contains("app_login"), "{all:?}");
}

/// R22: a granted role whose chain reaches a LOGIN role is left out entirely (its password
/// could be changed as that role).
#[test]
fn chain_reaching_a_login_role_excludes_the_granted_role() {
    let mut catalog = chains();
    catalog.roles.insert("app_login".into());
    catalog.login.insert("app_login".into());
    link(&mut catalog, "hr", "app_login");
    let (roles, mut skipped) = granted_roles(&only(&["ops", "hr", "billing"]), &catalog);
    assert_eq!(roles, set(&["billing"]));
    skipped.sort();
    assert_eq!(
        skipped,
        vec![
            ("hr".to_string(), Skipped::Unsafe),
            ("ops".to_string(), Skipped::Unsafe)
        ]
    );
}

/// N2: a role that holds ADMIN on any role (directly or through its memberships) manages roles,
/// so it's never given, nor a role whose chain reaches it; an ADMIN-option edge counts as reach.
#[test]
fn admin_holders_are_never_given() {
    let mut catalog = chains();
    catalog.admin.insert("team_lead".into());
    catalog
        .roles
        .extend(["team_lead".to_string(), "lead_of_leads".to_string()]);
    link(&mut catalog, "lead_of_leads", "team_lead");
    let (roles, mut skipped) =
        granted_roles(&only(&["team_lead", "lead_of_leads", "billing"]), &catalog);
    assert_eq!(roles, set(&["billing"]));
    skipped.sort();
    assert_eq!(
        skipped,
        vec![
            ("lead_of_leads".to_string(), Skipped::Unsafe),
            ("team_lead".to_string(), Skipped::Unsafe)
        ]
    );
}

/// R23: a chain through a reserved role (`pg_*`, another member role, `chelabase_*`,
/// `authenticator`) excludes the granted role; `anon` and `authenticated` may be in a chain.
#[test]
fn a_chain_through_a_reserved_role_excludes_the_granted_role() {
    let mut catalog = chains();
    catalog.roles.extend(
        [
            "pg_monitor",
            "cb_m_other",
            "authenticator",
            "anon",
            "x",
            "y",
            "z",
            "web",
        ]
        .map(String::from),
    );
    for (from, to) in [
        ("x", "pg_monitor"),
        ("y", "cb_m_other"),
        ("z", "authenticator"),
        ("web", "anon"),
    ] {
        link(&mut catalog, from, to);
    }
    let (roles, _) = granted_roles(&only(&["x", "y", "z", "web", "billing"]), &catalog);
    assert_eq!(roles, set(&["billing", "web"]));
}

/// R23: reach follows a member role's grants too, never stopping there; the catalog read
/// keeps member roles' edges. I2: `e` lines are the SET-option edges only, `a` lines every edge.
#[test]
fn reach_follows_member_roles_and_the_catalog_reads_both_edge_kinds() {
    let e = edges(&[("x", "cb_m_y"), ("cb_m_y", "z")]);
    assert_eq!(reach(&e, "x"), set(&["cb_m_y", "x", "z"]));
    let sql = member_catalog_sql();
    assert!(sql.contains("WHERE a.set_option"), "{sql}");
    assert!(!sql.contains("a.set_option OR a.admin_option"), "{sql}");
    assert!(sql.contains("SELECT 'a ' || "), "{sql}");
}

/// I2: an edge without the SET option is not reach (the role can't become the group) but is
/// still in the chain: the granted role is unsafe when that group is.
#[test]
fn a_no_set_edge_is_not_reach_but_still_makes_the_chain_unsafe() {
    let mut catalog = chains();
    catalog.roles.extend(["viewer_grp".to_string(), "inherits_boss".to_string()]);
    link_no_set(&mut catalog, "inherits_boss", "app");
    link(&mut catalog, "inherits_boss", "billing");
    assert_eq!(reach(&catalog.set_edges, "inherits_boss"), set(&["billing", "inherits_boss"]));
    assert!(unsafe_node(&catalog, "inherits_boss"), "app is owner-level, reached by INHERIT");
    let (roles, skipped) = granted_roles(&only(&["inherits_boss", "billing"]), &catalog);
    assert_eq!(roles, set(&["billing"]));
    assert_eq!(skipped, vec![("inherits_boss".to_string(), Skipped::Unsafe)]);
    assert!(!unsafe_node(&catalog, "viewer_grp"));
}

/// R23: CREATEDB counts as owner-level, as 6a's `owner_level` does; holding ADMIN (directly or
/// through memberships) and LOGIN are read per role.
#[test]
fn createdb_counts_as_owner_level() {
    let sql = member_catalog_sql();
    assert!(
        sql.contains(
            "r.rolsuper OR r.rolreplication OR r.rolcreaterole OR r.rolbypassrls OR r.rolcreatedb"
        ),
        "{sql}"
    );
    assert!(
        sql.contains("a.admin_option AND a.member = r.oid"),
        "{sql}"
    );
    assert!(
        sql.contains("CASE WHEN r.rolcanlogin THEN 't' ELSE 'f' END"),
        "{sql}"
    );
}

/// The roles `SET ROLE` reaches from any of `roles`.
fn closure(e: &BTreeMap<String, BTreeSet<String>>, roles: &BTreeSet<String>) -> BTreeSet<String> {
    roles.iter().flat_map(|r| reach(e, r)).collect()
}

/// The catalog nodes of the properties: six plain names, a reserved `pg_` role and another
/// member role (R23: a chain through either excludes the granted role).
fn nodes() -> Vec<String> {
    (0..6)
        .map(|i| format!("r{i}"))
        .chain(["pg_monitor".to_string(), "cb_m_9".to_string()])
        .collect()
}

/// A reserved name, written out here apart from the worker's `grantable`.
fn reserved_name(n: &str) -> bool {
    n.starts_with("pg_") || n.starts_with("cb_m_") || n.starts_with("chelabase_") || n == OWNER_ROLE
}

/// A role no granted role's chain may hold (R16, R22, R23, N2), written apart from the worker.
fn bad(catalog: &MemberCatalog, n: &str) -> bool {
    reserved_name(n)
        || catalog.owner_level.contains(n)
        || catalog.admin.contains(n)
        || catalog.login.contains(n)
}

fn arb_catalog() -> impl Strategy<Value = MemberCatalog> {
    let names = nodes();
    let n = names.len();
    (
        proptest::collection::vec((any::<bool>(), any::<bool>(), any::<bool>()), n),
        proptest::collection::vec((0..n, 0..n, any::<bool>()), 0..12),
        proptest::collection::vec(any::<bool>(), n),
    )
        .prop_map(move |(flags, pairs, exists)| {
            let roles: BTreeSet<String> = names
                .iter()
                .zip(&exists)
                .filter(|(_, e)| **e)
                .map(|(n, _)| n.clone())
                .collect();
            // Every edge is in `all_edges`; the ones with the SET option are in `set_edges`.
            let mut set_edges: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
            let mut all_edges: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
            for (a, b, with_set) in pairs {
                let (a, b) = (&names[a], &names[b]);
                if a != b && roles.contains(a) && roles.contains(b) {
                    all_edges.entry(a.clone()).or_default().insert(b.clone());
                    if with_set {
                        set_edges.entry(a.clone()).or_default().insert(b.clone());
                    }
                }
            }
            let flagged = |pick: fn(&(bool, bool, bool)) -> bool| -> BTreeSet<String> {
                names
                    .iter()
                    .zip(&flags)
                    .filter(|(n, f)| roles.contains(*n) && pick(f))
                    .map(|(n, _)| n.clone())
                    .collect()
            };
            MemberCatalog {
                owner_level: flagged(|f| f.0),
                admin: flagged(|f| f.1),
                login: flagged(|f| f.2),
                roles: roles.clone(),
                set_edges,
                all_edges,
                ..MemberCatalog::default()
            }
        })
}

/// The granted roles of `candidate` a member role holds (R21–R23, N2), computed apart from
/// `safe_roles`.
fn included(catalog: &MemberCatalog, candidate: &BTreeSet<String>) -> BTreeSet<String> {
    candidate
        .iter()
        .filter(|r| catalog.roles.contains(*r))
        .filter(|r| {
            closure(&catalog.all_edges, &BTreeSet::from([(*r).clone()]))
                .iter()
                .all(|n| !bad(catalog, n))
        })
        .cloned()
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// R21–R23 and N2, pure: [`safe_roles`] keeps exactly the candidates that exist and whose
    /// chain (SET and ADMIN edges, themselves included, through member roles too) holds no
    /// owner-level, ADMIN-holding, LOGIN or reserved role; the chain itself stays out of the set.
    #[test]
    fn safe_roles_keeps_exactly_the_candidates_with_a_safe_chain(
        catalog in arb_catalog(),
        pick in proptest::collection::vec(any::<bool>(), 9),
    ) {
        let all: Vec<String> = nodes().into_iter().chain(["ghost".to_string()]).collect();
        let candidate: BTreeSet<String> =
            all.iter().zip(&pick).filter(|(_, p)| **p).map(|(n, _)| n.clone()).collect();
        let got = safe_roles(&candidate, &catalog);
        prop_assert_eq!(&got, &included(&catalog, &candidate));
        prop_assert!(closure(&catalog.set_edges, &got).iter().all(|n| !bad(&catalog, n)));
    }

    /// Reach invariant (R21), through the plan: whatever the member roles hold (other grantors,
    /// owner-level roles, wrong options), after the plan's steps each holds exactly its included
    /// granted roles, shaped and ours; its `SET ROLE` closure equals the closure of those roles,
    /// and holds no owner-level role. Roles not desired are gone.
    #[test]
    fn after_the_plan_a_member_role_reaches_only_its_roles(
        catalog in arb_catalog(),
        grants in proptest::collection::vec(proptest::collection::vec(any::<bool>(), 6), 3),
        held in proptest::collection::vec(proptest::collection::vec((0..6usize, any::<bool>(), any::<bool>()), 0..5), 3),
        exists in proptest::collection::vec(any::<bool>(), 3),
    ) {
        let names: Vec<String> = (0..6).map(|i| format!("r{i}")).collect();
        let mut want = Vec::new();
        let mut have = Vec::new();
        for m in 0..3 {
            let role_name = format!("cb_m_{m}");
            let candidate: BTreeSet<String> =
                names.iter().zip(&grants[m]).filter(|(_, g)| **g).map(|(n, _)| n.clone()).collect();
            let roles = safe_roles(&candidate, &catalog);
            if !roles.is_empty() {
                want.push(desired(&role_name, &roles.iter().map(String::as_str).collect::<Vec<_>>(), false));
            }
            if exists[m] {
                // pg_auth_members holds one row per (role, member, grantor).
                let mut memberships: Vec<ActualMembership> = Vec::new();
                for (i, foreign, shaped) in &held[m] {
                    let membership = ActualMembership {
                        role: names[*i].clone(),
                        grantor: foreign.then(|| "postgres".to_string()),
                        shaped: *shaped,
                    };
                    if !memberships.iter().any(|h| (&h.role, &h.grantor) == (&membership.role, &membership.grantor)) {
                        memberships.push(membership);
                    }
                }
                have.push(actual(&role_name, memberships));
            }
        }
        let steps = plan_member_roles(&want, &have);
        // Apply the steps to a model of pg_auth_members.
        let mut state: BTreeMap<String, Vec<ActualMembership>> =
            have.iter().map(|a| (a.role_name.clone(), a.memberships.clone())).collect();
        for step in &steps {
            match step {
                MemberRoleStep::Create { role, .. } => {
                    prop_assert!(state.insert(role.clone(), Vec::new()).is_none(), "created twice");
                }
                MemberRoleStep::Grant { role, granted } => {
                    let held = state.get_mut(role).expect("granted to a role that exists");
                    held.retain(|m| !(m.role == *granted && m.grantor.is_none()));
                    held.push(ours(granted));
                }
                MemberRoleStep::Revoke { role, granted, grantor } => {
                    state.get_mut(role).unwrap().retain(|m| !(m.role == *granted && m.grantor == *grantor));
                }
                MemberRoleStep::Drop { role } => {
                    state.remove(role);
                }
                MemberRoleStep::SetPassword { .. } | MemberRoleStep::Reshape { .. } => {}
                // T7: the marker (never planned here: nothing is locked).
                MemberRoleStep::Lock { role } => state.get_mut(role).unwrap().push(marker(None)),
                MemberRoleStep::Unlock { role } => {
                    state.get_mut(role).unwrap().retain(|m| !(m.role == "chelabase_schema_locked" && m.grantor.is_none()));
                }
                // T6: personal steps are never planned for member roles.
                MemberRoleStep::CreatePersonal { .. } | MemberRoleStep::SetValidUntil { .. } | MemberRoleStep::ResetDatabaseSettings { .. } => {
                    prop_assert!(false, "a personal step for a member role: {step:?}");
                }
            }
        }
        // The included granted roles, computed apart from `safe_roles`.
        let granted = |m: usize| -> BTreeSet<String> {
            names.iter().zip(&grants[m]).filter(|(_, g)| **g).map(|(n, _)| n.clone()).collect()
        };
        let expected: BTreeMap<String, BTreeSet<String>> = (0..3)
            .map(|m| (format!("cb_m_{m}"), included(&catalog, &granted(m))))
            .filter(|(_, roles)| !roles.is_empty())
            .collect();
        prop_assert_eq!(state.keys().cloned().collect::<BTreeSet<_>>(), expected.keys().cloned().collect());
        for (role, held) in &state {
            prop_assert!(held.iter().all(|m| m.grantor.is_none() && m.shaped), "{role}: {held:?}");
            let direct: BTreeSet<String> = held.iter().map(|m| m.role.clone()).collect();
            prop_assert_eq!(&direct, &expected[role], "granted roles only");
            let reached = closure(&catalog.set_edges, &direct);
            prop_assert_eq!(&reached, &closure(&catalog.set_edges, &expected[role]));
            prop_assert!(reached.iter().all(|n| !bad(&catalog, n)), "{role} reaches {reached:?}");
        }
        // Converged: planning again changes nothing.
        let after: Vec<ActualMemberRole> = state.iter().map(|(r, m)| actual(r, m.clone())).collect();
        prop_assert_eq!(plan_member_roles(&want, &after), vec![]);
    }
}

fn any_name() -> impl Strategy<Value = String> {
    prop_oneof![
        "[a-z_][a-z0-9_]{0,20}",
        "[ -~]{1,30}",
        "\\PC{1,20}",
        Just("a\"b'c d".to_string()),
        Just("x\\y".to_string()),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]
    #[test]
    fn render_quotes_names_and_never_contains_plaintext_passwords(
        role in any_name(),
        granted in any_name(),
        grantor in any_name(),
        database in any_name(),
        password in "[A-Za-z0-9]{24}",
    ) {
        let verifier = chelabase_neon::scram::verifier(&password);
        let steps = [
            MemberRoleStep::Revoke { role: role.clone(), granted: granted.clone(), grantor: None },
            MemberRoleStep::Revoke { role: role.clone(), granted: granted.clone(), grantor: Some(grantor.clone()) },
            MemberRoleStep::Create { role: role.clone(), verifier: verifier.clone() },
            MemberRoleStep::SetPassword { role: role.clone(), verifier: verifier.clone() },
            MemberRoleStep::Reshape { role: role.clone(), databases: vec![database.clone()] },
            MemberRoleStep::Grant { role: role.clone(), granted: granted.clone() },
            MemberRoleStep::Drop { role: role.clone() },
        ];
        for step in &steps {
            let sql = render_step(step);
            prop_assert!(!sql.contains(&password), "{sql}");
            // Every value appears in its quoted form; once those are taken out (longest
            // first), no quote is left: nothing reached the SQL unquoted.
            let mut forms = vec![ident(&role)];
            if matches!(step, MemberRoleStep::Revoke { .. } | MemberRoleStep::Drop { .. } | MemberRoleStep::SetPassword { .. }) {
                forms.push(literal(&role));
            }
            if let MemberRoleStep::Grant { granted, .. } | MemberRoleStep::Revoke { granted, .. } = step {
                forms.push(ident(granted));
            }
            if let MemberRoleStep::Revoke { grantor: Some(g), .. } = step {
                forms.push(ident(g));
            }
            if let MemberRoleStep::Reshape { databases, .. } = step {
                forms.extend(databases.iter().map(|d| ident(d)));
            }
            if matches!(step, MemberRoleStep::Create { .. } | MemberRoleStep::SetPassword { .. }) {
                forms.push(literal(&verifier));
            }
            for form in &forms {
                prop_assert!(sql.contains(form.as_str()), "{form} in {sql}");
            }
            forms.sort_by_key(|f| std::cmp::Reverse(f.len()));
            let mut rest = sql.clone();
            for form in &forms {
                rest = rest.replace(form.as_str(), "");
            }
            prop_assert!(!rest.contains('"') && !rest.contains('\''), "an unquoted name in {sql}");
        }
    }
}

#[test]
fn render_uses_the_member_role_shape() {
    let terminate =
        "SELECT pg_terminate_backend(pid, 5000) FROM pg_stat_activity WHERE usename = 'cb_m_a';";
    assert_eq!(
        render_step(&MemberRoleStep::Create {
            role: "cb_m_a".into(),
            verifier: "SCRAM-SHA-256$x".into()
        }),
        "CREATE ROLE cb_m_a LOGIN NOINHERIT NOCREATEDB NOCREATEROLE NOBYPASSRLS NOREPLICATION PASSWORD 'SCRAM-SHA-256$x';"
    );
    assert_eq!(
        render_step(&MemberRoleStep::SetPassword {
            role: "cb_m_a".into(),
            verifier: "SCRAM-SHA-256$x".into()
        }),
        format!("ALTER ROLE cb_m_a PASSWORD 'SCRAM-SHA-256$x';\n{terminate}"),
        "a rotation ends the role's sessions"
    );
    assert_eq!(
        render_step(&MemberRoleStep::Grant {
            role: "cb_m_a".into(),
            granted: "billing".into()
        }),
        "GRANT billing TO cb_m_a WITH INHERIT FALSE, SET TRUE, ADMIN FALSE;"
    );
    assert_eq!(
        render_step(&MemberRoleStep::Revoke {
            role: "cb_m_a".into(),
            granted: "billing".into(),
            grantor: None
        }),
        format!("REVOKE billing FROM cb_m_a;\n{terminate}")
    );
    assert_eq!(
        render_step(&MemberRoleStep::Reshape {
            role: "cb_m_a".into(),
            databases: vec!["app".into()]
        }),
        "ALTER ROLE cb_m_a NOSUPERUSER LOGIN NOINHERIT NOCREATEDB NOCREATEROLE NOBYPASSRLS NOREPLICATION;\n\
         ALTER ROLE cb_m_a RESET ALL;\nALTER ROLE cb_m_a IN DATABASE app RESET ALL;"
    );
    assert_eq!(
        cleanup_script("cb_m_a", Some("billing")),
        format!(
            "{CLEANUP_MARK}\n{}",
            chelabase_types::role_name::member_cleanup_sql("cb_m_a", Some("billing"))
        )
    );
    let sql = member_catalog_sql();
    assert!(
        sql.contains("x.rolname IN ('postgres', 'neon_superuser', 'cloud_admin')"),
        "{sql}"
    );
    assert!(
        sql.contains("r.rolsuper OR r.rolreplication OR r.rolcreaterole OR r.rolbypassrls"),
        "{sql}"
    );
}

fn h(s: &str) -> String {
    hex::encode(s.as_bytes())
}

/// A catalog answer: roles (`!` marks owner-level, `@` LOGIN; member roles are LOGIN),
/// member-role memberships (ours, shaped), other roles' SET or ADMIN edges, member-role
/// passwords and the databases with their owners.
struct Cat<'a> {
    roles: &'a [&'a str],
    memberships: &'a [(&'a str, &'a str)],
    edges: &'a [(&'a str, &'a str)],
    passwords: &'a [(&'a str, &'a str)],
    databases: &'a [(&'a str, &'a str)],
}

impl Cat<'_> {
    fn answer(&self) -> String {
        let mut out = String::new();
        for r in self.roles {
            let (name, level) = match r.strip_prefix('!') {
                Some(name) => (name, "t"),
                None => (*r, "f"),
            };
            let (name, login) = match name.strip_prefix('@') {
                Some(name) => (name, "t"),
                None if name.starts_with("cb_m_") => (name, "t"),
                None => (name, "f"),
            };
            let attrs = if name.starts_with("cb_m_") { "t" } else { "f" };
            out.push_str(&format!("r {} {attrs} {level} f {login}\n", h(name)));
        }
        for (m, r) in self.memberships {
            out.push_str(&format!("m {} {} - t\n", h(m), h(r)));
        }
        for (a, b) in self.edges {
            out.push_str(&format!("e {} {}\na {} {}\n", h(a), h(b), h(a), h(b)));
        }
        for (r, v) in self.passwords {
            out.push_str(&format!("p {} {}\n", h(r), h(v)));
        }
        for (d, o) in self.databases {
            out.push_str(&format!("d {} {}\n", h(d), h(o)));
        }
        out
    }
}

fn cat(roles: &[&str]) -> String {
    Cat {
        roles,
        memberships: &[],
        edges: &[],
        passwords: &[],
        databases: &[("app", "postgres")],
    }
    .answer()
}

#[test]
fn the_catalog_answer_parses() {
    let out = format!(
        "r {} f t f t\nr {} t f f t\nr {} f f t f\nm {} {} - t\nm {} {} {} f\ne {} {}\na {} {}\na {} {}\ns {} -\ns {} {}\np {} {}\nd {} {}\nd {} {}\n",
        h("postgres"),
        h("cb_m_a"),
        h("billing"),
        h("cb_m_a"),
        h("billing"),
        h("cb_m_a"),
        h("hr"),
        h("postgres"),
        h("billing"),
        h("hr"),
        // `a` lines: the SET edge again, and an edge without SET (I2).
        h("billing"),
        h("hr"),
        h("hr"),
        h("billing"),
        h("cb_m_a"),
        h("cb_m_a"),
        h("app"),
        h("cb_m_a"),
        h("SCRAM-x"),
        h("app"),
        h("app_owner"),
        h("postgres"),
        h("cloud_admin"),
    );
    let catalog = parse_member_catalog(&out).unwrap();
    assert_eq!(catalog.roles, set(&["billing", "cb_m_a", "postgres"]));
    assert_eq!(catalog.owner_level, set(&["postgres"]));
    assert_eq!(catalog.admin, set(&["billing"]));
    assert_eq!(catalog.login, set(&["cb_m_a", "postgres"]));
    assert_eq!(catalog.set_edges, edges(&[("billing", "hr")]));
    assert_eq!(catalog.all_edges, edges(&[("billing", "hr"), ("hr", "billing")]));
    assert_eq!(
        catalog.databases,
        [
            ("app".to_string(), "app_owner".to_string()),
            ("postgres".to_string(), "cloud_admin".to_string())
        ]
    );
    assert_eq!(
        catalog.member_roles["cb_m_a"],
        ActualMemberRole {
            role_name: "cb_m_a".into(),
            attributes_ok: true,
            settings: vec![None, Some("app".into())],
            verifier: Some("SCRAM-x".into()),
            memberships: vec![
                ours("billing"),
                ActualMembership {
                    role: "hr".into(),
                    grantor: Some("postgres".into()),
                    shaped: false
                },
            ],
            valid_until: None,
        }
    );
    assert!(catalog.leftovers.is_empty());
    // I-A: where a member role still owns or holds something (`pg_shdepend`): a database, or
    // `-` for shared objects (any database does).
    let with = format!("{out}l {} {}\nl {} -\nl {} {}\n", h("cb_m_a"), h("app"), h("cb_m_a"), h("cb_m_b"), h("app"));
    let leftovers = parse_member_catalog(&with).unwrap().leftovers;
    assert_eq!(
        leftovers,
        BTreeMap::from([
            ("cb_m_a".to_string(), BTreeSet::from([None, Some("app".to_string())])),
            ("cb_m_b".to_string(), BTreeSet::from([Some("app".to_string())])),
        ])
    );
    assert!(member_catalog_sql().contains("FROM pg_shdepend"));
    assert!(parse_member_catalog("x 00").is_err());
    assert!(parse_member_catalog("r zz t f").is_err());
    assert!(
        parse_member_catalog(&format!("r {} t", h("a"))).is_err(),
        "a missing flag"
    );
    assert!(
        parse_member_catalog(&format!("r {} t f f f x", h("a"))).is_err(),
        "an extra field"
    );
}

// --- The reconcile over the fakes -------------------------------------------------------------

/// A ready database's main branch and endpoint, with `member` an active Developer holding
/// `roles` on its project.
async fn setup(w: &FakeWorker, roles: &[&str]) -> (BranchId, String, Uuid) {
    let id = ready_database(w).await;
    let main = w.metadata.main(id);
    let ep = main.endpoint_id.clone().unwrap();
    let member = add_member(w, id, roles);
    w.computes.sql.lock().unwrap().clear();
    (main.id, ep, member)
}

/// Another active Developer on `db`'s project holding `roles`.
fn add_member(w: &FakeWorker, db: chelabase_types::DatabaseId, roles: &[&str]) -> Uuid {
    let record = w.metadata.database(db);
    let member = Uuid::new_v4();
    let mut state = w.metadata.state();
    state.members.push(MemberRecord {
        id: member,
        tenant_id: record.tenant_id,
        zitadel_user_id: format!("z-{member}"),
        email: None,
        display_name: "m".into(),
        tenant_admin: false,
        status: MemberStatus::Active,
        accepted_at: None, offboarded: false, suspended_at: None, suspended_by: None,
    });
    state.grants.insert(
        (member, record.project_id.as_uuid()),
        GrantRow {
            platform_role: PlatformRole::Developer,
            pg_roles: roles.iter().map(|r| r.to_string()).collect(),
        },
    );
    member
}

/// A row on `branch` for `member` with `password`; the catalog verifier that matches it.
fn row(w: &FakeWorker, branch: BranchId, member: Uuid, password: &str, applied: &[&str]) -> String {
    let role = member_role_name(member);
    w.metadata.state().member_roles.push(MemberBranchRole {
        member_id: member,
        branch_id: branch.as_uuid(),
        role_name: role.clone(),
        password_enc: seal_member_role_password(&w.key, branch.as_uuid(), &role, password).unwrap(),
        applied_roles: applied.iter().map(|r| r.to_string()).collect(),
    });
    chelabase_neon::scram::verifier(password)
}

/// Every script but the catalog read, in order, with its database.
fn all_scripts(w: &FakeWorker) -> Vec<(String, String)> {
    w.computes
        .sql
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, _, s)| !s.contains("chelabase: member roles") && !s.contains(BRANCH_PROTECTED_CHECK_MARK))
        .map(|(_, db, s)| (db.clone(), s.clone()))
        .collect()
}

/// [`all_scripts`] without the read-only guard presence check every unprotected reconcile
/// runs in each database (PB S2): what changes anything.
fn changing_scripts(w: &FakeWorker) -> Vec<(String, String)> {
    all_scripts(w)
        .into_iter()
        .filter(|(_, s)| !s.starts_with(crate::schema_guard::GUARD_PRESENT_MARK))
        .collect()
}

/// [`changing_scripts`] without the per-database cleanups every reconcile runs (R31).
fn scripts(w: &FakeWorker) -> Vec<(String, String)> {
    changing_scripts(w)
        .into_iter()
        .filter(|(_, s)| !s.contains(CLEANUP_MARK))
        .collect()
}


fn texts(w: &FakeWorker) -> Vec<String> {
    scripts(w).into_iter().map(|(_, s)| s).collect()
}

fn done(reply: &MemberRolesReply) -> bool {
    matches!(reply, MemberRolesReply::Done { .. })
}

#[tokio::test]
async fn reconcile_creates_a_role_and_stores_its_sealed_password() {
    let (w, _journal) = worker();
    let (branch, ep, member) = setup(&w, &["billing", "ghost"]).await;
    w.computes.answer_at(
        &ep,
        "chelabase: member roles",
        &cat(&["!postgres", "billing", "anon"]),
    );
    let reply = w.reconcile_member_roles(branch, None).await;
    let role = member_role_name(member);
    assert_eq!(
        reply,
        MemberRolesReply::Done {
            roles: vec![MemberRoleApplied {
                member_id: member,
                role_name: role.clone(),
                applied_roles: vec!["billing".into()],
            }]
        }
    );
    let sql = texts(&w);
    assert_eq!(sql.len(), 1, "{sql:?}");
    assert!(
        sql[0].contains(&format!(
            "CREATE ROLE {role} {MEMBER_ROLE_ATTRIBUTES} PASSWORD 'SCRAM-SHA-256$"
        )),
        "{}",
        sql[0]
    );
    assert!(sql[0].contains(&format!(
        "GRANT billing TO {role} WITH INHERIT FALSE, SET TRUE, ADMIN FALSE;"
    )));
    assert!(
        !sql[0].contains("ghost"),
        "a role missing on the branch is skipped"
    );
    let rows = w.metadata.state().member_roles.clone();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].role_name, role);
    assert_eq!(rows[0].applied_roles, ["billing"]);
    let password =
        open_member_role_password(&w.key, branch.as_uuid(), &role, &rows[0].password_enc).unwrap();
    assert!(!sql[0].contains(&password), "only the verifier reaches SQL");
    let verifier = sql[0].split('\'').nth(1).unwrap();
    assert!(
        chelabase_neon::scram::verifies(verifier, &password),
        "the row holds the role's password"
    );
}

#[tokio::test]
async fn an_inherited_role_gets_a_fresh_password_and_a_converged_one_nothing() {
    let (w, _journal) = worker();
    let (branch, ep, member) = setup(&w, &["billing"]).await;
    let role = member_role_name(member);
    let parent = chelabase_neon::scram::verifier("the parent's password");
    let catalog = |verifier: &str| {
        Cat {
            roles: &["!postgres", "billing", &role],
            memberships: &[(&role, "billing")],
            edges: &[],
            passwords: &[(&role, verifier)],
            databases: &[("app", "postgres")],
        }
        .answer()
    };
    w.computes
        .answer_once_at(&ep, "chelabase: member roles", &catalog(&parent));
    // No row on this branch: the role came from the parent, so its password is replaced and
    // its sessions end.
    assert!(done(&w.reconcile_member_roles(branch, None).await));
    let sql = texts(&w);
    assert_eq!(sql.len(), 1, "{sql:?}");
    assert!(
        sql[0].contains(&format!("ALTER ROLE {role} PASSWORD 'SCRAM-SHA-256$")),
        "{}",
        sql[0]
    );
    assert!(sql[0].contains("pg_terminate_backend"));
    assert!(!sql[0].contains("GRANT"));
    let stored = w.metadata.state().member_roles[0].clone();
    let password =
        open_member_role_password(&w.key, branch.as_uuid(), &role, &stored.password_enc).unwrap();
    // Now converged (the catalog holds the row's password): a second run changes nothing.
    w.computes.sql.lock().unwrap().clear();
    w.computes.answer_at(
        &ep,
        "chelabase: member roles",
        &catalog(&chelabase_neon::scram::verifier(&password)),
    );
    assert!(done(&w.reconcile_member_roles(branch, None).await));
    assert_eq!(texts(&w), Vec::<String>::new());
    assert_eq!(w.metadata.state().member_roles, vec![stored]);
}

/// C2 / staleness: a catalog password that isn't the row's (a restore or reset brought another,
/// or the member changed its own) is rotated, and the role's sessions end.
#[tokio::test]
async fn a_password_that_isnt_the_rows_is_rotated() {
    let (w, _journal) = worker();
    let (branch, ep, member) = setup(&w, &["billing"]).await;
    let role = member_role_name(member);
    row(&w, branch, member, "the row's password", &["billing"]);
    let other = chelabase_neon::scram::verifier("a password the member set itself");
    w.computes.answer_at(
        &ep,
        "chelabase: member roles",
        &Cat {
            roles: &["!postgres", "billing", &role],
            memberships: &[(&role, "billing")],
            edges: &[],
            passwords: &[(&role, &other)],
            databases: &[("app", "postgres")],
        }
        .answer(),
    );
    assert!(done(&w.reconcile_member_roles(branch, None).await));
    let sql = texts(&w);
    assert_eq!(sql.len(), 1, "{sql:?}");
    assert!(
        sql[0].starts_with(&format!(
            "SET search_path = pg_catalog, pg_temp;\nALTER ROLE {role} PASSWORD 'SCRAM-SHA-256$"
        )),
        "{}",
        sql[0]
    );
    assert!(sql[0].contains(&terminate_sql(&role)));
    let stored = w.metadata.state().member_roles[0].clone();
    let password =
        open_member_role_password(&w.key, branch.as_uuid(), &role, &stored.password_enc).unwrap();
    assert_ne!(password, "the row's password", "a fresh password");
    let verifier = sql[0].split('\'').nth(1).unwrap();
    assert!(chelabase_neon::scram::verifies(verifier, &password));
}

/// C1 (R16): an owner-level role in a grant is never given, and a membership in it is revoked
/// with the role's sessions ended.
#[tokio::test]
async fn owner_level_roles_are_never_granted_and_are_revoked() {
    let (w, ep, branch, member) = {
        let (w, _journal) = worker();
        let (branch, ep, member) = setup(&w, &["app", "billing"]).await;
        (w, ep, branch, member)
    };
    let role = member_role_name(member);
    let verifier = row(&w, branch, member, "pw", &["app", "billing"]);
    w.computes.answer_at(
        &ep,
        "chelabase: member roles",
        &Cat {
            roles: &["!postgres", "!app", "billing", &role],
            memberships: &[(&role, "app"), (&role, "billing")],
            edges: &[],
            passwords: &[(&role, &verifier)],
            databases: &[("app", "postgres")],
        }
        .answer(),
    );
    let reply = w.reconcile_member_roles(branch, None).await;
    let MemberRolesReply::Done { roles } = reply else {
        panic!("{reply:?}")
    };
    assert_eq!(roles[0].applied_roles, ["billing"]);
    let sql = texts(&w);
    assert_eq!(sql.len(), 1, "{sql:?}");
    assert!(
        sql[0].contains(&format!(
            "REVOKE app FROM {role};\n{}",
            terminate_sql(&role)
        )),
        "{}",
        sql[0]
    );
    assert!(!sql[0].contains("GRANT"));
    assert_eq!(
        w.metadata.state().member_roles[0].applied_roles,
        ["billing"]
    );
}

#[tokio::test]
async fn a_removed_member_role_is_dropped_everywhere_and_its_row_removed() {
    let (w, _journal) = worker();
    let (branch, ep, member) = setup(&w, &["billing"]).await;
    let role = member_role_name(member);
    w.metadata.state().grants.clear();
    let verifier = row(&w, branch, member, "pw", &["billing"]);
    let answer = Cat {
        roles: &["!postgres", "billing", &role],
        memberships: &[(&role, "billing")],
        edges: &[],
        passwords: &[(&role, &verifier)],
        databases: &[("app", "app_owner"), ("other", "app_owner"), ("postgres", "cloud_admin")],
    }
    .answer();
    // It left something in app and postgres, nothing in other.
    w.computes.answer_at(
        &ep,
        "chelabase: member roles",
        &format!("{answer}{}", leftovers(&[(&role, Some("app")), (&role, Some("postgres"))])),
    );
    // R31: the member last acted as billing.
    w.metadata.state().acted_roles.insert((member, branch.as_uuid()), "billing".into());
    assert!(done(&w.reconcile_member_roles(branch, None).await));
    let calls = changing_scripts(&w);
    assert_eq!(calls.len(), 4, "{calls:?}");
    assert!(calls[0].1.contains(&format!(
        "ALTER ROLE {role} NOLOGIN;\n{}",
        terminate_sql(&role)
    )));
    // In each database: what it still owns goes to the role it last acted as (it held it),
    // then the rest is dropped; never handed to the owner.
    assert_eq!(calls[1].0, "app");
    assert_eq!(calls[2].0, "postgres");
    for call in &calls[1..3] {
        assert_eq!(call.1, cleanup_script(&role, Some("billing")));
        assert!(
            !call.1.contains("TO \"app_owner\"") && !call.1.contains("TO \"cloud_admin\""),
            "{}",
            call.1
        );
    }
    assert!(calls[3].1.contains(&format!("DROP ROLE IF EXISTS {role};")));
    assert!(w.metadata.state().member_roles.is_empty());
}

/// I1 and M6: one member's failing grant neither skips its own revoke (revokes run first, as
/// their own script) nor stops another member's role; only the failed role's row stays
/// unwritten. A failed drop keeps its row.
#[tokio::test]
async fn failures_stay_with_their_role() {
    let (w, _journal) = worker();
    let (branch, ep, a) = setup(&w, &["sales"]).await;
    let db = w.metadata.branch(branch).database_id;
    let b = add_member(&w, db, &["billing"]);
    let gone = Uuid::new_v4();
    let (ra, rb, rgone) = (
        member_role_name(a),
        member_role_name(b),
        member_role_name(gone),
    );
    // a: held {billing, hr}, now {sales}; b: new; gone: no member any more.
    let va = row(&w, branch, a, "pa", &["billing", "hr"]);
    let vgone = row(&w, branch, gone, "pg", &["billing"]);
    w.computes.answer_at(
        &ep,
        "chelabase: member roles",
        &Cat {
            roles: &["!postgres", "billing", "hr", "sales", &ra, &rgone],
            memberships: &[(&ra, "billing"), (&ra, "hr"), (&rgone, "billing")],
            edges: &[],
            passwords: &[(&ra, &va), (&rgone, &vgone)],
            databases: &[("app", "postgres")],
        }
        .answer(),
    );
    w.computes.fail_when(&format!("GRANT sales TO {ra}"));
    w.computes
        .fail_when(&format!("DROP ROLE IF EXISTS {rgone}"));
    let reply = w.reconcile_member_roles(branch, None).await;
    let MemberRolesReply::Failed { message } = reply else {
        panic!("{reply:?}")
    };
    assert!(
        message.contains(&ra) && message.contains(&rgone),
        "{message}"
    );
    assert!(!message.contains(&rb), "{message}");
    // (The fake records only scripts that ran: the failing ones are absent.)
    let sql = texts(&w);
    let revoked = sql
        .iter()
        .find(|s| s.contains(&format!("REVOKE billing FROM {ra};")))
        .expect("a's revoke ran");
    assert!(revoked.contains(&format!("REVOKE hr FROM {ra};")));
    assert!(revoked.contains(&terminate_sql(&ra)));
    assert!(
        !revoked.contains("GRANT"),
        "revokes are their own script, so a failing grant can't skip them"
    );
    assert!(
        !sql.iter()
            .any(|s| s.contains(&format!("GRANT sales TO {ra}"))),
        "a's grant failed"
    );
    assert!(
        sql.iter().any(|s| s.contains(&format!("CREATE ROLE {rb}"))),
        "b is applied"
    );
    assert!(
        sql.iter()
            .any(|s| s.contains(&format!("ALTER ROLE {rgone} NOLOGIN;"))),
        "the drop began"
    );
    let rows = w.metadata.state().member_roles.clone();
    let row_of = |m: Uuid| rows.iter().find(|r| r.member_id == m).cloned();
    assert_eq!(
        row_of(a).unwrap().applied_roles,
        ["billing", "hr"],
        "a's row isn't rewritten"
    );
    assert_eq!(row_of(b).unwrap().applied_roles, ["billing"]);
    assert!(row_of(gone).is_some(), "a failed drop keeps its row");
}

#[tokio::test]
async fn one_members_reconcile_leaves_the_others_alone() {
    let (w, _journal) = worker();
    let (branch, ep, member) = setup(&w, &["billing"]).await;
    let other = member_role_name(Uuid::new_v4());
    w.computes.answer_at(
        &ep,
        "chelabase: member roles",
        &Cat {
            roles: &["!postgres", "billing", &other],
            memberships: &[(&other, "billing")],
            edges: &[],
            passwords: &[],
            databases: &[("app", "postgres")],
        }
        .answer(),
    );
    assert!(done(&w.reconcile_member_roles(branch, Some(member)).await));
    let sql = texts(&w);
    assert_eq!(sql.len(), 1, "only the member's own create: {sql:?}");
    assert!(!sql[0].contains(&other));
}

fn hand_over_ask(branch: BranchId, member: Uuid, acted: &str) -> MemberRoleHandOverRequest {
    MemberRoleHandOverRequest {
        branch_id: branch.as_uuid(),
        member_id: member,
        database: "app".into(),
        acted_role: acted.into(),
        backend_pid: Some(7),
        backend_start: Some("t0".into()),
    }
}

/// R5 (T9 review): the hand-over after an act-as run runs the shared hand-over script, as the
/// superuser, in the run's database, holding the endpoint's lock; then reads the member role's
/// password back (M-e).
#[tokio::test]
async fn hand_over_runs_in_the_runs_database_under_the_lock() {
    let (w, _journal) = worker();
    let (branch, ep, member) = setup(&w, &["billing"]).await;
    let role = member_role_name(member);
    let verifier = row(&w, branch, member, "pw", &["billing"]);
    w.computes.answer_at(&ep, PASSWORD_MARK, &verifier);
    let reply = w.hand_over_member_role(&hand_over_ask(branch, member, "billing")).await;
    assert_eq!(reply, MemberRolesReply::Done { roles: vec![] });
    assert_eq!(
        scripts(&w),
        vec![
            (
                "app".to_string(),
                chelabase_types::role_name::member_hand_over_sql(&role, Some("billing"), Some((7, "t0")))
            ),
            ("app".to_string(), member_password_sql(&role)),
        ]
    );
    // It waits for the endpoint's lock, as a reconcile does.
    let w = Arc::new(w);
    let held = w.metadata.lock_endpoint(&ep).await.unwrap();
    let ask = hand_over_ask(branch, member, "billing");
    let run = tokio::spawn({
        let w = w.clone();
        async move { w.hand_over_member_role(&ask).await }
    });
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(!run.is_finished(), "waits for the lock");
    drop(held);
    assert!(done(&run.await.unwrap()));
}

/// M-e: a run that changed the member role's password (`ALTER ROLE CURRENT_USER PASSWORD`) gets
/// the row's password back right after the hand-over; with no row, a reconcile is asked for.
#[tokio::test]
async fn hand_over_restores_a_password_the_run_changed() {
    let (w, _journal) = worker();
    let (branch, ep, member) = setup(&w, &["billing"]).await;
    let role = member_role_name(member);
    row(&w, branch, member, "pw", &["billing"]);
    w.computes
        .answer_at(&ep, PASSWORD_MARK, &chelabase_neon::scram::verifier("set by the script"));
    assert!(done(&w.hand_over_member_role(&hand_over_ask(branch, member, "billing")).await));
    let sql = texts(&w);
    assert_eq!(sql.len(), 3, "{sql:?}");
    let restore = &sql[2];
    assert!(restore.contains(&format!("ALTER ROLE {role} PASSWORD 'SCRAM-SHA-256$")), "{restore}");
    let verifier = restore.split('\'').nth(1).unwrap();
    assert!(chelabase_neon::scram::verifies(verifier, "pw"), "the row's password");
    // No row (a reset forgot it): a reconcile is asked for, nothing restored.
    w.computes.sql.lock().unwrap().clear();
    w.metadata.state().member_roles.clear();
    assert!(done(&w.hand_over_member_role(&hand_over_ask(branch, member, "billing")).await));
    assert_eq!(texts(&w).len(), 1, "{:?}", texts(&w));
    assert!(w.member_role_requests.sent().contains(&branch));
}

/// M-b: a name no member role may hold is never a target, but the rest of the hand-over (the
/// drop, the settings) still runs. A branch that isn't ready gets nothing.
#[tokio::test]
async fn a_refused_hand_over_skips_only_the_reassign() {
    let (w, _journal) = worker();
    let (branch, ep, member) = setup(&w, &["billing"]).await;
    let role = member_role_name(member);
    let verifier = row(&w, branch, member, "pw", &["billing"]);
    w.computes.answer_at(&ep, PASSWORD_MARK, &verifier);
    for acted in ["", "postgres", "owner", "pg_read_all_data", "cb_m_x", "chelabase_admin", "cloud_admin"] {
        w.computes.sql.lock().unwrap().clear();
        let reply = w.hand_over_member_role(&hand_over_ask(branch, member, acted)).await;
        assert!(done(&reply), "{acted}: {reply:?}");
        assert_eq!(
            texts(&w)[0],
            chelabase_types::role_name::member_hand_over_sql(&role, None, Some((7, "t0"))),
            "{acted}"
        );
    }
    w.computes.sql.lock().unwrap().clear();
    let reply = w.hand_over_member_role(&hand_over_ask(BranchId::new(), member, "billing")).await;
    assert!(matches!(reply, MemberRolesReply::Failed { .. }), "{reply:?}");
    assert!(scripts(&w).is_empty());
}

/// M-c: an answer past the RPC's deadline is `Failed`, and the hand-over goes on to its end
/// holding the endpoint's lock (it is never cut short with its psql still running).
#[tokio::test]
async fn a_hand_over_past_the_deadline_goes_on() {
    let (w, _journal) = worker();
    let (branch, ep, member) = setup(&w, &["billing"]).await;
    let w = Arc::new(w);
    let held = w.metadata.lock_endpoint(&ep).await.unwrap();
    let payload = serde_json::to_vec(&hand_over_ask(branch, member, "billing")).unwrap();
    let reply = member_hand_over_reply(w.clone(), &payload, Duration::from_millis(50)).await;
    assert!(
        matches!(&reply, MemberRolesReply::Failed { message } if message.contains("timed out")),
        "{reply:?}"
    );
    assert!(scripts(&w).is_empty(), "still waiting for the lock");
    drop(held);
    for _ in 0..100 {
        if !scripts(&w).is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(!scripts(&w).is_empty(), "the hand-over finished");
    assert!(matches!(
        member_hand_over_reply(w, b"nope", MEMBER_ROLES_DEADLINE).await,
        MemberRolesReply::Failed { .. }
    ));
}

/// `l` lines of the catalog answer: `role` still owns or holds something in `db` (`None`:
/// shared objects).
fn leftovers(pairs: &[(&str, Option<&str>)]) -> String {
    pairs
        .iter()
        .map(|(r, db)| format!("l {} {}\n", h(r), db.map_or("-".to_string(), h)))
        .collect()
}

/// R31 (user), I-A, I-B: a reconcile cleans up only where `pg_shdepend` shows a member role
/// still owns or holds something, and does it first, before this reconcile's revokes: what it
/// owns goes to the role the member last acted as (it could still become it, though this
/// reconcile revokes it), then the rest goes (`DROP OWNED`). Shared objects (`-`) are cleaned
/// up from the system database. Never to the owner.
#[tokio::test]
async fn every_reconcile_hands_leftovers_to_the_last_acted_role_then_drops() {
    let (w, _journal) = worker();
    let (branch, ep, member) = setup(&w, &["billing", "reports"]).await;
    let role = member_role_name(member);
    let verifier = row(&w, branch, member, "pw", &["billing", "sales"]);
    // It last acted as sales, which this reconcile revokes: it held it before, so sales.
    w.metadata.state().acted_roles.insert((member, branch.as_uuid()), "sales".into());
    let answer = Cat {
        roles: &["!postgres", "billing", "sales", "reports", &role],
        memberships: &[(&role, "billing"), (&role, "sales")],
        edges: &[],
        passwords: &[(&role, &verifier)],
        databases: &[("app", "app_owner"), ("other", "app_owner"), ("postgres", "cloud_admin")],
    }
    .answer();
    w.computes.answer_once_at(
        &ep,
        "chelabase: member roles",
        &format!("{answer}{}", leftovers(&[(&role, Some("app")), (&role, None)])),
    );
    assert!(done(&w.reconcile_member_roles(branch, None).await));
    let calls = changing_scripts(&w);
    assert_eq!(calls.len(), 4, "{calls:?}");
    assert_eq!(calls[0], ("app".to_string(), cleanup_script(&role, Some("sales"))));
    assert_eq!(calls[1], ("postgres".to_string(), cleanup_script(&role, Some("sales"))));
    assert!(calls[2].1.contains(&format!("REVOKE sales FROM {role};")), "{}", calls[2].1);
    assert!(calls[3].1.contains(&format!("GRANT reports TO {role}")), "{}", calls[3].1);
    // Converged, with nothing left over: a sweep runs nothing at all.
    w.computes.sql.lock().unwrap().clear();
    w.computes.answer_at(
        &ep,
        "chelabase: member roles",
        &Cat {
            roles: &["!postgres", "billing", "sales", "reports", &role],
            memberships: &[(&role, "billing"), (&role, "reports")],
            edges: &[],
            passwords: &[(&role, &verifier)],
            databases: &[("app", "app_owner")],
        }
        .answer(),
    );
    assert!(done(&w.reconcile_member_roles(branch, None).await));
    assert_eq!(changing_scripts(&w), Vec::<(String, String)>::new());
}

/// R31, the sweep backstop: a converged member role whose hand-over never ran (leftovers, no
/// change to its memberships) is cleaned up by the next reconcile, and only there.
#[tokio::test]
async fn a_converged_reconcile_cleans_up_a_missed_hand_over() {
    let (w, _journal) = worker();
    let (branch, ep, member) = setup(&w, &["billing"]).await;
    let role = member_role_name(member);
    let verifier = row(&w, branch, member, "pw", &["billing"]);
    w.metadata.state().acted_roles.insert((member, branch.as_uuid()), "billing".into());
    let answer = Cat {
        roles: &["!postgres", "billing", &role],
        memberships: &[(&role, "billing")],
        edges: &[],
        passwords: &[(&role, &verifier)],
        databases: &[("app", "app_owner"), ("other", "app_owner")],
    }
    .answer();
    w.computes.answer_at(
        &ep,
        "chelabase: member roles",
        &format!("{answer}{}", leftovers(&[(&role, Some("other"))])),
    );
    assert!(done(&w.reconcile_member_roles(branch, None).await));
    assert_eq!(
        changing_scripts(&w),
        vec![("other".to_string(), cleanup_script(&role, Some("billing")))]
    );
}

fn catalog_of(roles: &[&str], memberships: &[(&str, &str)], edges: &[(&str, &str)]) -> MemberCatalog {
    parse_member_catalog(
        &Cat { roles, memberships, edges, passwords: &[], databases: &[("app", "postgres")] }.answer(),
    )
    .unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// R31, model: [`reassign_target`] gives the recorded role exactly when it exists, the
    /// member role's `SET` memberships (and their chains) reach it, and its chain holds no
    /// owner-level, ADMIN-holding, LOGIN or reserved role; so a reassignment never hands
    /// objects to a role the member couldn't already become, nor to anything owner-level.
    #[test]
    fn reassign_target_matches_the_model(
        catalog in arb_catalog(),
        held in proptest::collection::vec((0..8usize, any::<bool>()), 0..4),
        recorded in proptest::option::of(0..9usize),
    ) {
        let names: Vec<String> = nodes().into_iter().chain(["ghost".to_string()]).collect();
        let mut catalog = catalog;
        let m = "cb_m_t".to_string();
        catalog.roles.insert(m.clone());
        catalog.member_roles.insert(m.clone(), ActualMemberRole {
            role_name: m.clone(),
            attributes_ok: true,
            settings: vec![],
            verifier: None,
            memberships: held
                .iter()
                .filter(|(i, _)| catalog.roles.contains(&names[*i]))
                .map(|(i, shaped)| ActualMembership { role: names[*i].clone(), grantor: None, shaped: *shaped })
                .collect(),
            valid_until: None,
        });
        let recorded = recorded.map(|i| names[i].clone());
        let could: BTreeSet<String> = closure(
            &catalog.set_edges,
            &catalog.member_roles[&m].memberships.iter().filter(|x| x.shaped).map(|x| x.role.clone()).collect(),
        );
        let want = recorded.clone().filter(|r| {
            catalog.roles.contains(r)
                && could.contains(r)
                && closure(&catalog.all_edges, &BTreeSet::from([r.clone()])).iter().all(|n| !bad(&catalog, n))
        });
        prop_assert_eq!(reassign_target(&catalog, &m, recorded.as_deref()), want);
    }
}

/// R31's decision (pure): the recorded role, only when it exists, the member role could
/// `SET ROLE` to it before this reconcile (its own `SET` memberships and their chains), and
/// neither it nor its chain is owner-level, LOGIN, ADMIN-holding or reserved.
#[test]
fn reassign_target_decides() {
    let m = "cb_m_a";
    let held = catalog_of(
        &["!postgres", "billing", "billing_admin", "sales", "@app", "!power", "boss", m],
        &[(m, "billing_admin"), (m, "sales"), (m, "app"), (m, "power"), (m, "boss")],
        &[("billing_admin", "billing"), ("boss", "postgres")],
    );
    for (recorded, want) in [
        (None, None),
        (Some("billing_admin"), Some("billing_admin")),
        // Reached through a held role's chain.
        (Some("billing"), Some("billing")),
        // Held before this reconcile; that the reconcile revokes it doesn't matter.
        (Some("sales"), Some("sales")),
        // Gone from the branch.
        (Some("ghost"), None),
        // Refused: LOGIN, owner-level, a chain to the owner, reserved, the owner.
        (Some("app"), None),
        (Some("power"), None),
        (Some("boss"), None),
        (Some("pg_monitor"), None),
        (Some("postgres"), None),
        (Some("cb_m_b"), None),
    ] {
        assert_eq!(reassign_target(&held, m, recorded).as_deref(), want, "{recorded:?}");
    }
    // Not something the member role could become: never.
    let not_held = catalog_of(&["!postgres", "billing", "sales", m], &[(m, "billing")], &[]);
    assert_eq!(reassign_target(&not_held, m, Some("sales")), None);
    // A membership without SET (another grantor's, unshaped) doesn't count.
    let mut unshaped = not_held.clone();
    unshaped.member_roles.get_mut(m).unwrap().memberships.push(ActualMembership {
        role: "sales".into(),
        grantor: Some("someone".into()),
        shaped: false,
    });
    assert_eq!(reassign_target(&unshaped, m, Some("sales")), None);
    // The member role itself isn't on the branch: nothing to reassign from.
    assert_eq!(reassign_target(&held, "cb_m_zz", Some("billing")), None);
}

#[tokio::test]
async fn a_new_branch_asks_for_a_member_reconcile() {
    let (w, _journal) = worker();
    let id = ready_database(&w).await;
    let child = crate::fakes::ready_branch(&w, id, "dev", BranchId::from(id)).await;
    assert!(
        w.member_role_requests.sent().contains(&child),
        "{:?}",
        w.member_role_requests.sent()
    );
}

#[tokio::test]
async fn the_rpc_answers_failed_for_a_bad_request() {
    let (w, _journal) = worker();
    assert!(matches!(
        member_roles_reply(Arc::new(w), b"nope", MEMBER_ROLES_DEADLINE).await,
        MemberRolesReply::Failed { .. }
    ));
}

/// M3: a run past the RPC's deadline answers `Failed`, and goes on to its end (it isn't cut
/// short with its lock released while its psql may still run).
#[tokio::test]
async fn a_run_past_the_deadline_goes_on() {
    let (w, _journal) = worker();
    let (branch, ep, _member) = setup(&w, &["billing"]).await;
    w.computes.answer_at(
        &ep,
        "chelabase: member roles",
        &cat(&["!postgres", "billing"]),
    );
    let w = Arc::new(w);
    // Another holder of the endpoint's lock keeps the run waiting past the deadline.
    let held = w.metadata.lock_endpoint(&ep).await.unwrap();
    let payload = serde_json::to_vec(&MemberRolesReconcileRequest {
        branch_id: branch.as_uuid(),
        member_id: None,
        from_ddl: false,
    })
    .unwrap();
    let reply = member_roles_reply(w.clone(), &payload, Duration::from_millis(50)).await;
    assert!(
        matches!(&reply, MemberRolesReply::Failed { message } if message.contains("timed out")),
        "{reply:?}"
    );
    assert!(
        w.metadata.state().member_roles.is_empty(),
        "still waiting for the lock"
    );
    drop(held);
    for _ in 0..100 {
        if !w.metadata.state().member_roles.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(w.metadata.state().member_roles.len(), 1, "the run finished");
}

/// I2: a failed run is tried again with backoff.
#[tokio::test(start_paused = true)]
async fn a_failed_reconcile_is_retried() {
    let (w, _journal) = worker();
    let (branch, ep, _member) = setup(&w, &["billing"]).await;
    w.computes.answer_at(
        &ep,
        "chelabase: member roles",
        &cat(&["!postgres", "billing"]),
    );
    *w.computes.sql_failures_left.lock().unwrap() = 2;
    let started = tokio::time::Instant::now();
    assert!(done(
        &w.reconcile_member_roles_retrying(branch, 4, Duration::from_secs(2))
            .await
    ));
    assert_eq!(
        started.elapsed(),
        Duration::from_secs(2 + 4),
        "two waits, doubled"
    );
    assert_eq!(w.metadata.state().member_roles.len(), 1);
    *w.computes.sql_failures_left.lock().unwrap() = 10;
    assert!(matches!(
        w.reconcile_member_roles_retrying(branch, 2, Duration::from_secs(1))
            .await,
        MemberRolesReply::Failed { .. }
    ));
}

/// I2: the sweep reconciles every ready branch (PB end-of-step R12: members or not).
#[tokio::test]
async fn the_sweep_reconciles_every_branch_with_members() {
    let (w, _journal) = worker();
    let (branch, ep, _member) = setup(&w, &["billing"]).await;
    let db = w.metadata.branch(branch).database_id;
    let child = crate::fakes::ready_branch(&w, db, "dev", branch).await;
    let child_ep = w.metadata.branch(child).endpoint_id.unwrap();
    for e in [&ep, &child_ep] {
        w.computes.answer_at(
            e,
            "chelabase: member roles",
            &cat(&["!postgres", "billing"]),
        );
    }
    // A database of a tenant without members is visited too (R12: the guard's backstop).
    ready_database(&w).await;
    assert_eq!(w.sweep_member_roles_once().await, (3, 0));
    let rows = w.metadata.state().member_roles.clone();
    let mut branches: Vec<Uuid> = rows.iter().map(|r| r.branch_id).collect();
    branches.sort();
    let mut expected = vec![branch.as_uuid(), child.as_uuid()];
    expected.sort();
    assert_eq!(branches, expected);
}

fn locked(role_name: &str, roles: &[&str]) -> DesiredMemberRole {
    DesiredMemberRole { locked: true, ..desired(role_name, roles, false) }
}

#[test]
fn a_locked_member_role_gets_the_marker_and_keeps_it() {
    use MemberRoleStep::*;
    let have = vec![actual("cb_m_a", vec![ours("billing")])];
    assert_eq!(plan_member_roles(&[locked("cb_m_a", &["billing"])], &have), vec![Lock { role: "cb_m_a".into() }]);
    let marked = vec![actual("cb_m_a", vec![ours("billing"), ours("chelabase_schema_locked")])];
    assert_eq!(plan_member_roles(&[locked("cb_m_a", &["billing"])], &marked), vec![]);
    assert_eq!(plan_member_roles(&[desired("cb_m_a", &["billing"], false)], &marked), vec![Unlock { role: "cb_m_a".into() }]);
    // A new locked role: created, granted, then marked.
    let steps = plan_member_roles(&[locked("cb_m_b", &["billing"])], &[]);
    assert!(matches!(steps.first(), Some(Create { .. })), "{steps:?}");
    // T7 review, finding 3: the marker comes straight after Create.
    assert_eq!(steps.get(1), Some(&Lock { role: "cb_m_b".into() }), "{steps:?}");
    // The marker is never granted through the normal path.
    assert!(!steps.iter().any(|s| matches!(s, Grant { granted, .. } if granted == "chelabase_schema_locked")));
}

#[test]
fn lock_and_unlock_render() {
    let lock = render_step(&MemberRoleStep::Lock { role: "cb_m_a".into() });
    assert!(lock.contains("CREATE ROLE chelabase_schema_locked NOLOGIN"), "{lock}");
    assert!(lock.ends_with("GRANT chelabase_schema_locked TO cb_m_a WITH INHERIT FALSE, SET FALSE, ADMIN FALSE;"), "{lock}");
    assert_eq!(render_step(&MemberRoleStep::Unlock { role: "cb_m_a".into() }).lines().next(), Some("REVOKE chelabase_schema_locked FROM cb_m_a;"));
}

#[tokio::test]
async fn a_protected_branch_locks_developers_and_guards_every_database() {
    let (w, _journal) = worker();
    let (branch, ep, member) = setup(&w, &["billing"]).await;
    w.metadata.state().branches.get_mut(&branch).unwrap().protected = true;
    // U14: the compute already carries the protection flag (the flag tests cover the rest).
    w.computes.answer(BRANCH_PROTECTED_CHECK_MARK, "on");
    let answer = Cat { roles: &["!postgres", "billing"], memberships: &[], edges: &[], passwords: &[], databases: &[("app", "postgres"), ("analytics", "postgres")] }.answer();
    w.computes.answer_at(&ep, "chelabase: member roles", &answer);
    assert!(done(&w.reconcile_member_roles(branch, None).await));
    let all = all_scripts(&w);
    let guarded: Vec<&str> = all.iter().filter(|(_, s)| s.contains("chelabase_schema_guard_ddl")).map(|(db, _)| db.as_str()).collect();
    assert_eq!(guarded, ["analytics", "app"], "every database, in the catalog's order");
    let role = member_role_name(member);
    assert!(texts(&w).iter().any(|s| s.contains(&format!("GRANT chelabase_schema_locked TO {role}"))), "{:?}", texts(&w));
}

#[tokio::test]
async fn an_unprotected_branch_runs_no_guard() {
    let (w, _journal) = worker();
    let (branch, ep, _member) = setup(&w, &["billing"]).await;
    w.computes.answer_at(&ep, "chelabase: member roles", &cat(&["!postgres", "billing"]));
    assert!(done(&w.reconcile_member_roles(branch, None).await));
    // Only the read-only presence check (PB T3), which answers nothing here: no install, no removal.
    let all = all_scripts(&w);
    assert!(all.iter().filter(|(_, s)| !s.starts_with(crate::schema_guard::GUARD_PRESENT_MARK)).all(|(_, s)| !s.contains("chelabase_schema_guard") && !s.contains("chelabase_schema_locked")), "{all:?}");
}

// --- T7, the implementer's own -----------------------------------------------------------

/// The marker membership as the catalog shows it (`SET FALSE`: never shaped).
fn marker(grantor: Option<&str>) -> ActualMembership {
    ActualMembership {
        role: "chelabase_schema_locked".into(),
        grantor: grantor.map(str::to_string),
        shaped: false,
    }
}

#[test]
fn the_marker_is_never_a_usable_role() {
    let catalog = parse_member_catalog(
        &Cat {
            roles: &["!postgres", "billing", "chelabase_schema_locked"],
            memberships: &[],
            edges: &[],
            passwords: &[],
            databases: &[("app", "postgres")],
        }
        .answer(),
    )
    .unwrap();
    let marker_set = set(&["chelabase_schema_locked", "billing"]);
    assert_eq!(safe_roles(&marker_set, &catalog), set(&["billing"]));
    let all = Grant { platform_role: PlatformRole::Admin, roles: RoleSet::all() };
    assert!(!granted_roles(&all, &catalog).0.contains("chelabase_schema_locked"));
    // A grant naming it skips it as unsafe-or-reserved, never grants it.
    let named = Grant {
        platform_role: PlatformRole::Developer,
        roles: RoleSet::Only([PgRole::Named("chelabase_schema_locked".into())].into_iter().collect()),
    };
    assert!(granted_roles(&named, &catalog).0.is_empty());
    // Desired roles naming it never reach the normal grant path; an unlocked role holding it
    // only loses it through Unlock.
    let steps = plan_member_roles(&[desired("cb_m_a", &["billing", "chelabase_schema_locked"], false)], &[actual("cb_m_a", vec![ours("billing"), marker(None)])]);
    assert_eq!(steps, vec![MemberRoleStep::Unlock { role: "cb_m_a".into() }]);
}

#[test]
fn another_grantors_marker_is_revoked_as_that_grantor_and_ours_granted() {
    use MemberRoleStep::*;
    let have = vec![actual("cb_m_a", vec![ours("billing"), marker(Some("postgres"))])];
    assert_eq!(
        plan_member_roles(&[locked("cb_m_a", &["billing"])], &have),
        vec![
            Revoke { role: "cb_m_a".into(), granted: "chelabase_schema_locked".into(), grantor: Some("postgres".into()) },
            Lock { role: "cb_m_a".into() },
        ]
    );
    // Unlocked: only the foreign one goes (there is no marker of ours to unlock).
    assert_eq!(
        plan_member_roles(&[desired("cb_m_a", &["billing"], false)], &have),
        vec![Revoke { role: "cb_m_a".into(), granted: "chelabase_schema_locked".into(), grantor: Some("postgres".into()) }]
    );
}

#[test]
fn a_dropped_role_drops_with_its_marker() {
    let have = vec![actual("cb_m_a", vec![ours("billing"), marker(None)])];
    assert_eq!(plan_member_roles(&[], &have), vec![MemberRoleStep::Drop { role: "cb_m_a".into() }]);
}

#[test]
fn unlock_renders_alone_and_lock_quotes_its_role() {
    assert_eq!(render_step(&MemberRoleStep::Unlock { role: "cb_m_a".into() }), "REVOKE chelabase_schema_locked FROM cb_m_a;");
    let lock = render_step(&MemberRoleStep::Lock { role: "Odd Name".into() });
    assert_eq!(
        lock,
        "DO $$BEGIN IF NOT EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname OPERATOR(pg_catalog.=) 'chelabase_schema_locked') THEN CREATE ROLE chelabase_schema_locked NOLOGIN; END IF; END$$;\nGRANT chelabase_schema_locked TO \"Odd Name\" WITH INHERIT FALSE, SET FALSE, ADMIN FALSE;"
    );
    assert_eq!(MemberRoleStep::Lock { role: "r".into() }.role(), "r");
    assert_eq!(MemberRoleStep::Unlock { role: "r".into() }.role(), "r");
}

#[tokio::test]
async fn a_protected_branch_leaves_an_admin_unlocked_and_unlocks_one_who_became_admin() {
    let (w, _journal) = worker();
    let (branch, ep, member) = setup(&w, &["billing"]).await;
    w.metadata.state().branches.get_mut(&branch).unwrap().protected = true;
    // U14: the compute already carries the protection flag (the flag tests cover the rest).
    w.computes.answer(BRANCH_PROTECTED_CHECK_MARK, "on");
    // The member is an Admin now; its role still holds our marker.
    let db = w.metadata.state().branches.get(&branch).unwrap().database_id;
    let project = w.metadata.database(db).project_id.as_uuid();
    w.metadata.state().grants.get_mut(&(member, project)).unwrap().platform_role = PlatformRole::Admin;
    let role = member_role_name(member);
    let answer = Cat {
        roles: &["!postgres", "billing", "chelabase_schema_locked", &role],
        memberships: &[(&role, "billing"), (&role, "chelabase_schema_locked")],
        edges: &[],
        passwords: &[],
        databases: &[("app", "postgres")],
    }
    .answer();
    w.computes.answer_at(&ep, "chelabase: member roles", &answer);
    assert!(done(&w.reconcile_member_roles(branch, None).await));
    let texts = texts(&w);
    assert!(texts.iter().any(|s| s.contains(&format!("REVOKE chelabase_schema_locked FROM {role};"))), "{texts:?}");
    assert!(!texts.iter().any(|s| s.contains("GRANT chelabase_schema_locked")), "{texts:?}");
    // Still guarded: the branch is protected.
    assert!(all_scripts(&w).iter().any(|(db, s)| db == "app" && s.contains("chelabase_schema_guard_ddl")));
}

#[tokio::test]
async fn a_failed_guard_fails_the_run_but_the_roles_still_apply() {
    let (w, _journal) = worker();
    let (branch, ep, member) = setup(&w, &["billing"]).await;
    w.metadata.state().branches.get_mut(&branch).unwrap().protected = true;
    // U14: the compute already carries the protection flag (the flag tests cover the rest).
    w.computes.answer(BRANCH_PROTECTED_CHECK_MARK, "on");
    w.computes.answer_at(&ep, "chelabase: member roles", &cat(&["!postgres", "billing"]));
    w.computes.fail_when("CREATE EVENT TRIGGER chelabase_schema_guard_ddl");
    let reply = w.reconcile_member_roles(branch, None).await;
    assert!(matches!(&reply, MemberRolesReply::Failed { message } if message.contains("guarding app")), "{reply:?}");
    let role = member_role_name(member);
    assert!(texts(&w).iter().any(|s| s.contains(&format!("GRANT chelabase_schema_locked TO {role}"))));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// T7, P7: whatever a member role holds (our marker, another grantor's, granted roles of
    /// either kind), after the plan's steps it holds our marker exactly when it is locked, its
    /// other memberships are exactly its roles (ours, shaped), the marker never goes through
    /// `Grant`/our `Revoke`, `Unlock` runs with the revokes and `Lock` after the role's creation
    /// and grants; and planning again changes nothing.
    #[test]
    fn the_marker_follows_locked_and_nothing_else(
        roles in proptest::collection::vec(proptest::collection::btree_set(0..3usize, 1..3), 3),
        locked_flags in proptest::collection::vec(any::<bool>(), 3),
        wanted in proptest::collection::vec(any::<bool>(), 3),
        exists in proptest::collection::vec(any::<bool>(), 3),
        held in proptest::collection::vec(proptest::collection::vec((0..4usize, any::<bool>(), any::<bool>()), 0..6), 3),
    ) {
        let name = |i: usize| if i == 3 { "chelabase_schema_locked".to_string() } else { format!("r{i}") };
        let mut want = Vec::new();
        let mut have = Vec::new();
        for m in 0..3 {
            let role_name = format!("cb_m_{m}");
            if wanted[m] {
                let set: Vec<String> = roles[m].iter().map(|i| name(*i)).collect();
                let mut d = desired(&role_name, &set.iter().map(String::as_str).collect::<Vec<_>>(), false);
                d.locked = locked_flags[m];
                want.push(d);
            }
            if exists[m] {
                let mut memberships: Vec<ActualMembership> = Vec::new();
                for (i, foreign, shaped) in &held[m] {
                    let membership = ActualMembership {
                        role: name(*i),
                        grantor: foreign.then(|| "postgres".to_string()),
                        shaped: *shaped && *i != 3,
                    };
                    if !memberships.iter().any(|h| (&h.role, &h.grantor) == (&membership.role, &membership.grantor)) {
                        memberships.push(membership);
                    }
                }
                have.push(actual(&role_name, memberships));
            }
        }
        let steps = plan_member_roles(&want, &have);
        let first_change = steps.iter().position(|s| !matches!(s, MemberRoleStep::Revoke { .. } | MemberRoleStep::Unlock { .. })).unwrap_or(steps.len());
        let mut state: BTreeMap<String, Vec<ActualMembership>> =
            have.iter().map(|a| (a.role_name.clone(), a.memberships.clone())).collect();
        for (at, step) in steps.iter().enumerate() {
            match step {
                MemberRoleStep::Create { role, .. } => {
                    prop_assert!(state.insert(role.clone(), Vec::new()).is_none());
                }
                MemberRoleStep::Grant { role, granted } => {
                    prop_assert_ne!(granted.as_str(), "chelabase_schema_locked");
                    let held = state.get_mut(role).unwrap();
                    held.retain(|m| !(m.role == *granted && m.grantor.is_none()));
                    held.push(ours(granted));
                }
                MemberRoleStep::Revoke { role, granted, grantor } => {
                    prop_assert!(!(granted == "chelabase_schema_locked" && grantor.is_none()), "{step:?}");
                    state.get_mut(role).unwrap().retain(|m| !(m.role == *granted && m.grantor == *grantor));
                }
                MemberRoleStep::Unlock { role } => {
                    prop_assert!(at < first_change, "Unlock with the revokes: {steps:?}");
                    state.get_mut(role).unwrap().retain(|m| !(m.role == "chelabase_schema_locked" && m.grantor.is_none()));
                }
                MemberRoleStep::Lock { role } => {
                    // After the role's creation, before any of its grants (T7 review, finding 3:
                    // a failed grant must not leave the role granted but unlocked).
                    prop_assert!(!steps[at + 1..].iter().any(|s| s.role() == role && matches!(s, MemberRoleStep::Create { .. })), "{steps:?}");
                    prop_assert!(!steps[..at].iter().any(|s| s.role() == role && matches!(s, MemberRoleStep::Grant { .. })), "{steps:?}");
                    let held = state.get_mut(role).expect("locked a role that exists");
                    prop_assert!(!held.iter().any(|m| m.role == "chelabase_schema_locked" && m.grantor.is_none()), "locked twice");
                    held.push(marker(None));
                }
                MemberRoleStep::Drop { role } => {
                    state.remove(role);
                }
                MemberRoleStep::SetPassword { .. } | MemberRoleStep::Reshape { .. } => {}
                // T6: personal steps are never planned for member roles.
                MemberRoleStep::CreatePersonal { .. } | MemberRoleStep::SetValidUntil { .. } | MemberRoleStep::ResetDatabaseSettings { .. } => {
                    prop_assert!(false, "a personal step for a member role: {step:?}");
                }
            }
        }
        prop_assert_eq!(state.keys().cloned().collect::<BTreeSet<_>>(), want.iter().map(|d| d.role_name.clone()).collect::<BTreeSet<_>>());
        for d in &want {
            let held = &state[&d.role_name];
            prop_assert!(held.iter().all(|m| m.grantor.is_none()), "{held:?}");
            let marked = held.iter().filter(|m| m.role == "chelabase_schema_locked").count();
            prop_assert_eq!(marked, usize::from(d.locked), "{:?}", held);
            let others: BTreeSet<String> = held.iter().filter(|m| m.role != "chelabase_schema_locked").map(|m| m.role.clone()).collect();
            let expected: BTreeSet<String> = d.roles.iter().filter(|r| *r != "chelabase_schema_locked").cloned().collect();
            prop_assert_eq!(others, expected);
            prop_assert!(held.iter().filter(|m| m.role != "chelabase_schema_locked").all(|m| m.shaped));
        }
        let after: Vec<ActualMemberRole> = state.iter().map(|(r, m)| actual(r, m.clone())).collect();
        prop_assert_eq!(plan_member_roles(&want, &after), vec![]);
    }
}

// T7 review, finding 3: the marker comes straight after `Create`, before any grant, so a grant
// that fails can't leave a role with its grants but without the marker (fail closed).
#[test]
fn the_marker_comes_before_any_grant() {
    use MemberRoleStep::*;
    let steps = plan_member_roles(&[locked("cb_m_b", &["billing", "sales"])], &[]);
    assert!(matches!(steps.first(), Some(Create { .. })), "{steps:?}");
    assert_eq!(steps.get(1), Some(&Lock { role: "cb_m_b".into() }), "{steps:?}");
    // An existing role missing a grant: the marker still comes first.
    let have = vec![actual("cb_m_a", vec![ours("billing")])];
    let steps = plan_member_roles(&[locked("cb_m_a", &["billing", "sales"])], &have);
    assert_eq!(
        steps,
        vec![Lock { role: "cb_m_a".into() }, Grant { role: "cb_m_a".into(), granted: "sales".into() }]
    );
}

// T7 review, finding 7: the guard is reinstalled only where its check says it isn't current.
#[tokio::test]
async fn the_guard_is_installed_only_where_it_is_not_current() {
    let (w, _journal) = worker();
    let (branch, ep, _member) = setup(&w, &["billing"]).await;
    w.metadata.state().branches.get_mut(&branch).unwrap().protected = true;
    // U14: the compute already carries the protection flag (the flag tests cover the rest).
    w.computes.answer(BRANCH_PROTECTED_CHECK_MARK, "on");
    let answer = Cat { roles: &["!postgres", "billing"], memberships: &[], edges: &[], passwords: &[], databases: &[("app", "postgres"), ("analytics", "postgres")] }.answer();
    w.computes.answer_at(&ep, "chelabase: member roles", &answer);
    w.computes.answer_in("app", "chelabase: schema guard check", "t");
    w.computes.answer_in("analytics", "chelabase: schema guard check", "f");
    assert!(done(&w.reconcile_member_roles(branch, None).await));
    let all = all_scripts(&w);
    let checked: Vec<&str> = all.iter().filter(|(_, s)| s.contains("chelabase: schema guard check")).map(|(db, _)| db.as_str()).collect();
    assert_eq!(checked, ["analytics", "app"]);
    let installed: Vec<&str> = all.iter().filter(|(_, s)| s.contains("chelabase_schema_guard_ddl") && !s.contains("chelabase: schema guard check")).map(|(db, _)| db.as_str()).collect();
    assert_eq!(installed, ["analytics"], "app's guard is current");
}

// --- Protected branches: the login-role guard (PB, T3) ---------------------------------------

use crate::schema_guard::{
    GUARD_CHECK_MARK, GUARD_PRESENT_MARK, GuardSettings, UNGUARD_MARK, schema_unguard_sql,
};

fn guard_settings(on: bool, exempt: &[&str]) -> GuardSettings {
    GuardSettings { guard_logins: on, exempt: exempt.iter().map(|e| e.to_string()).collect() }
}

#[tokio::test]
async fn a_protected_branch_guards_with_its_settings() {
    for on in [true, false] {
        let (w, _journal) = worker();
        let (branch, ep, _member) = setup(&w, &["billing"]).await;
        {
            let mut state = w.metadata.state();
            let record = state.branches.get_mut(&branch).unwrap();
            record.protected = true;
            record.guard_logins = on;
            state.guard_exemptions.insert(branch, vec!["deploy".into(), "Ci Bot".into()]);
        }
        w.computes.answer(BRANCH_PROTECTED_CHECK_MARK, "on");
        // PB end-of-step R13: exemptions are honored only for roles that exist.
        let answer = Cat { roles: &["!postgres", "billing", "@deploy", "@Ci Bot"], memberships: &[], edges: &[], passwords: &[], databases: &[("app", "postgres")] }.answer();
        w.computes.answer_at(&ep, "chelabase: member roles", &answer);
        assert!(done(&w.reconcile_member_roles(branch, None).await));
        let settings = guard_settings(on, &["Ci Bot", "deploy"]);
        let all = all_scripts(&w);
        assert!(all.iter().any(|(db, s)| db == "app" && *s == schema_guard_check_sql(&settings)), "{on}: {all:?}");
        assert!(all.iter().any(|(db, s)| db == "app" && *s == schema_guard_sql(&settings)), "{on}: {all:?}");
        assert!(!all.iter().any(|(_, s)| s.starts_with(GUARD_PRESENT_MARK) || s.starts_with(UNGUARD_MARK)));
    }
}

#[tokio::test]
async fn an_unprotected_branch_removes_a_guard_where_it_finds_one() {
    let (w, _journal) = worker();
    let (branch, ep, _member) = setup(&w, &["billing"]).await;
    let answer = Cat { roles: &["!postgres", "billing"], memberships: &[], edges: &[], passwords: &[], databases: &[("app", "postgres"), ("analytics", "postgres")] }.answer();
    w.computes.answer_at(&ep, "chelabase: member roles", &answer);
    w.computes.answer_in("app", GUARD_PRESENT_MARK, "t");
    w.computes.answer_in("analytics", GUARD_PRESENT_MARK, "f");
    assert!(done(&w.reconcile_member_roles(branch, None).await));
    let all = all_scripts(&w);
    let checked: Vec<&str> = all.iter().filter(|(_, s)| s.starts_with(GUARD_PRESENT_MARK)).map(|(db, _)| db.as_str()).collect();
    assert_eq!(checked, ["analytics", "app"]);
    let removed: Vec<&str> = all.iter().filter(|(_, s)| *s == schema_unguard_sql()).map(|(db, _)| db.as_str()).collect();
    assert_eq!(removed, ["app"]);
    assert!(!all.iter().any(|(_, s)| s.contains("CREATE EVENT TRIGGER")));
}

#[tokio::test]
async fn a_failed_removal_fails_the_run_but_the_roles_still_apply() {
    let (w, _journal) = worker();
    let (branch, ep, member) = setup(&w, &["billing"]).await;
    w.computes.answer_at(&ep, "chelabase: member roles", &cat(&["!postgres", "billing"]));
    w.computes.answer(GUARD_PRESENT_MARK, "t");
    w.computes.fail_when(UNGUARD_MARK);
    let reply = w.reconcile_member_roles(branch, None).await;
    assert!(matches!(&reply, MemberRolesReply::Failed { message } if message.contains("unguarding")), "{reply:?}");
    let role = member_role_name(member);
    assert!(texts(&w).iter().any(|s| s.contains(&format!("CREATE ROLE {role}"))), "{:?}", texts(&w));
}

// --- Protected branches: end-of-step (R12, R13) -------------------------------------------

#[test]
fn the_catalog_reads_superusers_admin_edges_and_template1() {
    let out = format!(
        "{}S {}\nA {} {}\ng {}\n",
        cat(&["!postgres", "@esc", "deploy"]),
        h("cloud_admin"),
        h("esc"),
        h("deploy"),
        h("template1"),
    );
    let catalog = parse_member_catalog(&out).unwrap();
    assert_eq!(catalog.superusers, set(&["cloud_admin"]));
    assert_eq!(catalog.admin_edges, edges(&[("esc", "deploy")]));
    assert_eq!(catalog.guard_only_databases, ["template1"]);
    assert_eq!(catalog.databases, [("app".to_string(), "postgres".to_string())], "member-role work keeps its list");
    let sql = member_catalog_sql();
    assert!(sql.contains("SELECT 'S ' || "), "{sql}");
    assert!(sql.contains("WHERE a.admin_option"), "{sql}");
    assert!(sql.contains("d.datname = 'template1' AND d.datallowconn"), "{sql}");
}

fn exemption_catalog(login: &[&str], admin: &[(&str, &str)], member_of: &[(&str, &str)]) -> MemberCatalog {
    let mut catalog = MemberCatalog {
        roles: set(&["postgres", "cloud_admin", "cb_m_a", "deploy", "ci", "esc", "grp", "lonely"]),
        superusers: set(&["cloud_admin"]),
        login: set(login),
        admin_edges: edges(admin),
        all_edges: edges(member_of),
        ..MemberCatalog::default()
    };
    for (m, r) in admin {
        catalog.all_edges.entry(m.to_string()).or_default().insert(r.to_string());
    }
    catalog
}

fn names(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[test]
fn an_exemption_is_honored_only_for_an_existing_role_no_guarded_role_administers() {
    let logins = ["postgres", "cloud_admin", "cb_m_a", "deploy", "ci", "esc"];
    // Nobody administers them: both honored; a name with no role isn't.
    let catalog = exemption_catalog(&logins, &[], &[]);
    let (honored, skipped) = honored_exemptions(&catalog, &names(&["deploy", "ci", "ghost"]));
    assert_eq!(honored, set(&["ci", "deploy"]));
    assert_eq!(skipped, ["ghost"]);
    // A guarded login holds ADMIN on `deploy` directly.
    let catalog = exemption_catalog(&logins, &[("esc", "deploy")], &[]);
    let (honored, skipped) = honored_exemptions(&catalog, &names(&["deploy", "ci"]));
    assert_eq!((honored, skipped), (set(&["ci"]), names(&["deploy"])));
    // Through a chain: `esc` is a member of the NOLOGIN `grp`, which holds ADMIN on `ci`.
    let catalog = exemption_catalog(&logins, &[("grp", "ci")], &[("esc", "grp")]);
    let (honored, skipped) = honored_exemptions(&catalog, &names(&["deploy", "ci"]));
    assert_eq!((honored, skipped), (set(&["deploy"]), names(&["ci"])));
    // Another exemption is guarded too: `ci` administering `deploy` un-honors `deploy`.
    let catalog = exemption_catalog(&logins, &[("ci", "deploy")], &[]);
    let (honored, _) = honored_exemptions(&catalog, &names(&["deploy", "ci"]));
    assert_eq!(honored, set(&["ci"]));
}

#[test]
fn the_owner_superusers_member_logins_and_unreached_groups_may_administer_an_exemption() {
    let logins = ["postgres", "cloud_admin", "cb_m_a", "deploy", "esc"];
    for holder in ["postgres", "cloud_admin", "cb_m_a", "lonely"] {
        let catalog = exemption_catalog(&logins, &[(holder, "deploy")], &[]);
        let (honored, skipped) = honored_exemptions(&catalog, &names(&["deploy"]));
        assert_eq!(honored, set(&["deploy"]), "{holder}");
        assert!(skipped.is_empty(), "{holder}");
    }
    // A group no login reaches is harmless; once a guarded login joins it, it isn't.
    let catalog = exemption_catalog(&logins, &[("grp", "deploy")], &[]);
    assert_eq!(honored_exemptions(&catalog, &names(&["deploy"])).0, set(&["deploy"]));
    let catalog = exemption_catalog(&logins, &[("grp", "deploy")], &[("esc", "grp")]);
    assert!(honored_exemptions(&catalog, &names(&["deploy"])).0.is_empty());
    // A guarded login that is a member of the owner reaches what the owner administers.
    let catalog = exemption_catalog(&logins, &[("postgres", "deploy")], &[("esc", "postgres")]);
    assert!(honored_exemptions(&catalog, &names(&["deploy"])).0.is_empty());
}

#[tokio::test]
async fn the_guard_renders_only_honored_exemptions() {
    let (w, _journal) = worker();
    let (branch, ep, _member) = setup(&w, &["billing"]).await;
    {
        let mut state = w.metadata.state();
        let record = state.branches.get_mut(&branch).unwrap();
        record.protected = true;
        record.guard_logins = true;
        state.guard_exemptions.insert(branch, vec!["deploy".into(), "ghost".into(), "owned".into()]);
    }
    w.computes.answer(BRANCH_PROTECTED_CHECK_MARK, "on");
    let answer = format!(
        "{}A {} {}\n",
        Cat { roles: &["!postgres", "billing", "@deploy", "@owned", "@esc"], memberships: &[], edges: &[], passwords: &[], databases: &[("app", "postgres")] }.answer(),
        h("esc"),
        h("owned"),
    );
    w.computes.answer_at(&ep, "chelabase: member roles", &answer);
    assert!(done(&w.reconcile_member_roles(branch, None).await));
    let settings = guard_settings(true, &["deploy"]);
    let all = all_scripts(&w);
    assert!(all.iter().any(|(db, s)| db == "app" && *s == schema_guard_sql(&settings)), "{all:?}");
}

#[tokio::test]
async fn the_guard_goes_into_template1_and_leaves_it_with_protection() {
    for protected in [true, false] {
        let (w, _journal) = worker();
        let (branch, ep, _member) = setup(&w, &["billing"]).await;
        w.metadata.state().branches.get_mut(&branch).unwrap().protected = protected;
        w.computes.answer(BRANCH_PROTECTED_CHECK_MARK, if protected { "on" } else { "off" });
        let answer = format!("{}g {}\n", cat(&["!postgres", "billing"]), h("template1"));
        w.computes.answer_at(&ep, "chelabase: member roles", &answer);
        w.computes.answer(GUARD_PRESENT_MARK, "t");
        assert!(done(&w.reconcile_member_roles(branch, None).await));
        let all = all_scripts(&w);
        let settings = guard_settings(true, &[]);
        let in_template1: Vec<&str> = all.iter().filter(|(db, _)| db == "template1").map(|(_, s)| s.as_str()).collect();
        if protected {
            assert!(in_template1.contains(&schema_guard_sql(&settings).as_str()), "{all:?}");
        } else {
            assert!(in_template1.contains(&schema_unguard_sql().as_str()), "{all:?}");
        }
        // Member-role work (leftover cleanups, drops) never goes there.
        assert!(in_template1.iter().all(|s| s.starts_with(GUARD_CHECK_MARK) || s.starts_with(GUARD_PRESENT_MARK) || s.contains("chelabase_schema_guard_ddl") || *s == schema_unguard_sql()), "{in_template1:?}");
    }
}

// PB end-of-step R17: template1 is touched only in a run that had to install or remove the guard
// in another database (a session in template1 makes a concurrent CREATE DATABASE fail, and only
// a superuser can change template1's guard, so it can't drift on its own).
#[tokio::test]
async fn template1_is_touched_only_when_another_database_changed() {
    for (protected, needle, answer, touched) in [
        (true, GUARD_CHECK_MARK, "t", false),
        (true, GUARD_CHECK_MARK, "f", true),
        (false, GUARD_PRESENT_MARK, "f", false),
        (false, GUARD_PRESENT_MARK, "t", true),
    ] {
        let (w, _journal) = worker();
        let (branch, ep, _member) = setup(&w, &["billing"]).await;
        w.metadata.state().branches.get_mut(&branch).unwrap().protected = protected;
        w.computes.answer(BRANCH_PROTECTED_CHECK_MARK, if protected { "on" } else { "off" });
        let catalog = format!(
            "{}g {}\n",
            Cat { roles: &["!postgres", "billing"], memberships: &[], edges: &[], passwords: &[], databases: &[("app", "postgres")] }.answer(),
            h("template1")
        );
        w.computes.answer_at(&ep, "chelabase: member roles", &catalog);
        w.computes.answer_in("app", needle, answer);
        assert!(done(&w.reconcile_member_roles(branch, None).await), "{protected} {answer}");
        let in_template1: Vec<String> = all_scripts(&w).into_iter().filter(|(db, _)| db == "template1").map(|(_, s)| s).collect();
        assert_eq!(!in_template1.is_empty(), touched, "protected {protected}, app answered {answer}: {in_template1:?}");
    }
}

// R18 (user, 2026-10-14): every role (`All`, a project Admin's grant, R17) covers the branch's
// own roles, not the data API's `anon` and `authenticated`: the member login gets those only
// when a grant names them, so a member's SQL session can't `SET ROLE anon` without it.
#[test]
fn every_role_leaves_out_the_data_api_roles_unless_named() {
    let catalog = MemberCatalog { roles: set(&["anon", "authenticated", "billing", "sales"]), ..Default::default() };
    let all = Grant { platform_role: PlatformRole::Admin, roles: RoleSet::all() };
    assert_eq!(granted_roles(&all, &catalog).0, set(&["billing", "sales"]));
    let named = Grant {
        platform_role: PlatformRole::Developer,
        roles: RoleSet::Only([PgRole::Named("anon".into()), PgRole::Named("billing".into())].into_iter().collect()),
    };
    assert_eq!(granted_roles(&named, &catalog).0, set(&["anon", "billing"]));
}

fn closed(name: &str, allow: bool, template: bool, born: bool) -> ClosedDatabase {
    ClosedDatabase { name: name.into(), allow_connections: allow, template, born_closed: born }
}

#[test]
fn the_catalog_lists_closed_and_marked_databases() {
    let sql = member_catalog_sql();
    // (The marker is matched as a Postgres bool, so the exact spelling is no longer in the text;
    // `the_born_closed_marker_is_matched_as_a_postgres_bool` pins the accepted spellings.)
    assert!(sql.contains("neon.born_closed"), "{sql}");
    assert!(sql.contains("NOT d.datallowconn"), "{sql}");
    let h = |s: &str| hex::encode(s.as_bytes());
    let out = format!("c {} f f t\nc {} f f f\nc {} t f t\nc {} f t f\n", h("born"), h("shut"), h("opened"), h("template0"));
    let catalog = parse_member_catalog(&out).unwrap();
    assert_eq!(
        catalog.closed_databases,
        vec![
            closed("born", false, false, true),
            closed("opened", true, false, true),
            closed("shut", false, false, false),
            closed("template0", false, true, false),
        ]
    );
    assert!(parse_member_catalog(&format!("c {} f f", h("x"))).is_err(), "a short line");
}

#[test]
fn closed_databases_on_a_protected_branch_are_guarded_and_only_marked_ones_opened() {
    use ClosedStep::*;
    let dbs = [
        closed("born", false, false, true),
        closed("opened", true, false, true),
        closed("shut", false, false, false),
        closed("template0", false, true, false),
        closed("tmark", false, true, true),
    ];
    assert_eq!(
        plan_closed_databases(&dbs, true),
        vec![
            Guard("born".into()),
            Open("born".into()),
            ResetMarker("opened".into()),
            Guard("shut".into()),
            Skip("template0".into()),
            Skip("tmark".into()),
        ]
    );
    assert_eq!(
        plan_closed_databases(&dbs, false),
        vec![Open("born".into()), ResetMarker("opened".into()), Skip("tmark".into())]
    );
    assert!(plan_closed_databases(&[], true).is_empty());
}

#[test]
fn opening_resets_the_marker_with_quoted_names() {
    let sql = open_born_closed_sql("we\"ird");
    let open = sql.find("ALTER DATABASE \"we\"\"ird\" ALLOW_CONNECTIONS true;").expect(&sql);
    let reset = sql.find("ALTER DATABASE \"we\"\"ird\" RESET neon.born_closed;").expect(&sql);
    assert!(open < reset, "{sql}");
    assert_eq!(reset_born_closed_sql("db"), "ALTER DATABASE \"db\" RESET neon.born_closed;");
}

// Members hub T2 (MH-2): the reconcile reads `database_grant_sources` and grants the member
// role every role of `Effective::roles` (Viewer-level ones included: the member role serves
// Viewer act-as routes too; only personal logins filter by level, T6). A member with an
// override and no source gets nothing, never a default.
#[tokio::test]
async fn reconcile_reads_grant_sources() {
    let (w, _journal) = worker();
    let (branch, ep, member) = setup(&w, &["billing"]).await;
    let db = w.metadata.state().databases.keys().copied().next().expect("a database");
    let record = w.metadata.database(db);
    let project = record.project_id.as_uuid();
    // A team grant names `sales` at Viewer (step 2's source; the fake fills it).
    w.metadata.state().team_grants.insert(
        (member, project),
        vec![Grant {
            platform_role: PlatformRole::Viewer,
            roles: RoleSet::Only([PgRole::Named("sales".into())].into_iter().collect()),
        }],
    );
    // Another member holds only an override on the database: no source.
    let lonely = Uuid::new_v4();
    {
        let mut state = w.metadata.state();
        state.members.push(MemberRecord {
            id: lonely,
            tenant_id: record.tenant_id,
            zitadel_user_id: format!("z-{lonely}"),
            email: None,
            display_name: "o".into(),
            tenant_admin: false,
            status: MemberStatus::Active,
            accepted_at: None, offboarded: false, suspended_at: None, suspended_by: None,
        });
        state.overrides.insert(
            (lonely, db.as_uuid()),
            chelabase_metadata::members::OverrideRow {
                platform_role: Some(PlatformRole::Admin),
                pg_roles: Some(vec!["billing".into()]),
            },
        );
    }
    w.computes.answer_at(&ep, "chelabase: member roles", &cat(&["!postgres", "billing", "sales", "anon"]));
    let reply = w.reconcile_member_roles(branch, None).await;
    assert_eq!(
        reply,
        MemberRolesReply::Done {
            roles: vec![MemberRoleApplied {
                member_id: member,
                role_name: member_role_name(member),
                applied_roles: vec!["billing".into(), "sales".into()],
            }]
        }
    );
    let sql = texts(&w);
    assert_eq!(sql.len(), 1, "{sql:?}");
    assert!(!sql[0].contains(&member_role_name(lonely)), "an override alone is no access: {}", sql[0]);
}

// --- T6: personal logins (spec Part 6 "Convergence", "Memberships", "No privileges of its
// own"; D2; MH-10, MH-16, MH-17, MH-18, MH-23; seam S1, S9, S16) ------------------------------
//
// The catalog answer pins how [`member_catalog_sql`] reports marked roles (the round-4
// predicate, `chelabase.managed_role=` at `setdatabase = 0`): the role's `r` line as any role,
// a `k <name>` line (marked), its `p` line (password), a `u <name> <valid until>` line
// (microseconds since the epoch, `-` for none) and an `m` line per membership, as a member
// role's. A marked role is read into `MemberCatalog::marked` and `MemberCatalog::personal`.

use chelabase_metadata::personal_logins::PersonalLoginRecord;
use chelabase_types::role_name::mark_personal_login_sql;
use crate::fakes::{entries, position};

/// A fixed `VALID UNTIL` far ahead.
fn far() -> DateTime<Utc> {
    chrono::TimeZone::with_ymd_and_hms(&Utc, 2099, 1, 2, 3, 4, 5).unwrap()
}

fn days(n: i64) -> chrono::Duration {
    chrono::Duration::days(n)
}

/// A whole SCRAM verifier for `password`.
fn scram(password: &str) -> String {
    chelabase_neon::scram::verifier(password)
}

/// SQL with identifier quotes removed, so assertions read the same whether a plain name is
/// quoted or not.
fn plain(sql: &str) -> String {
    sql.replace('"', "")
}

fn personal_desired(role: &str, roles: &[&str], verifier: &str, locked: bool) -> DesiredMemberRole {
    DesiredMemberRole {
        member_id: Uuid::nil(),
        role_name: role.into(),
        roles: set(roles),
        verifier: verifier.into(),
        rotate: false,
        locked,
        kind: RoleKind::Personal { valid_until: far() },
    }
}

fn personal_actual(
    role: &str,
    memberships: Vec<ActualMembership>,
    verifier: &str,
    valid_until: Option<DateTime<Utc>>,
) -> ActualMemberRole {
    ActualMemberRole {
        role_name: role.into(),
        attributes_ok: true,
        settings: Vec::new(),
        verifier: Some(verifier.into()),
        memberships,
        valid_until,
    }
}

/// The catalog lines of a marked role (see the section's comment).
fn marked_lines(
    name: &str,
    verifier: &str,
    valid_until: Option<DateTime<Utc>>,
    memberships: &[&str],
) -> String {
    let until = valid_until.map_or("-".to_string(), |t| t.timestamp_micros().to_string());
    let mut out = format!(
        "r {n} t f f t\nk {n}\np {n} {v}\nu {n} {until}\n",
        n = h(name),
        v = h(verifier)
    );
    for m in memberships {
        out.push_str(&format!("m {} {} - t\n", h(name), h(m)));
    }
    out
}

/// A personal-login record of `member` on `db`.
#[allow(clippy::too_many_arguments)]
fn login_record(
    w: &FakeWorker,
    db: chelabase_types::DatabaseId,
    member: Uuid,
    name: &str,
    verifier: Option<&str>,
    valid_until: DateTime<Utc>,
    revoked: bool,
    qualifies: bool,
) {
    w.metadata.state().personal_logins.push(PersonalLoginRecord {
        member_id: member,
        database_id: db.as_uuid(),
        role_name: name.into(),
        scram_verifier: verifier.map(str::to_string),
        valid_until,
        revoked,
        qualifies,
    });
}

/// A live, qualifying record valid until [`far`].
fn live_record(w: &FakeWorker, db: chelabase_types::DatabaseId, member: Uuid, name: &str, verifier: &str) {
    login_record(w, db, member, name, Some(verifier), far(), false, true);
}

/// A personal row for `name` on `ep`, as the worker's upsert leaves it.
fn personal_row(w: &FakeWorker, ep: &str, name: &str, verifier: Option<&str>) {
    let mut state = w.metadata.state();
    let endpoint = state.endpoints.get_mut(ep).unwrap();
    endpoint.logins.insert(
        name.into(),
        crate::fakes::FakeLogin::new(chelabase_metadata::RoleOrigin::Personal, None, false),
    );
    endpoint.set_verifier(name, verifier);
}

/// The database of `branch`.
fn db_of(w: &FakeWorker, branch: BranchId) -> chelabase_types::DatabaseId {
    w.metadata.branch(branch).database_id
}

/// Every changing script (cleanups included), without identifier quotes, with its database.
fn plain_scripts(w: &FakeWorker) -> Vec<(String, String)> {
    changing_scripts(w)
        .into_iter()
        .map(|(db, s)| (db, plain(&s)))
        .collect()
}

/// Whether some script drops `role`.
fn dropped(w: &FakeWorker, role: &str) -> bool {
    plain_scripts(w).iter().any(|(_, s)| {
        s.contains(&format!("DROP ROLE IF EXISTS {role};")) || s.contains(&format!("DROP ROLE {role};"))
    })
}

/// Whether some script creates `role`.
fn created(w: &FakeWorker, role: &str) -> bool {
    plain_scripts(w)
        .iter()
        .any(|(_, s)| s.contains(&format!("CREATE ROLE {role} ")))
}

/// The index of the first script (of [`plain_scripts`]) containing `needle`.
fn script_at(scripts: &[(String, String)], needle: &str) -> usize {
    scripts
        .iter()
        .position(|(_, s)| s.contains(needle))
        .unwrap_or_else(|| panic!("no script contains {needle:?}: {scripts:#?}"))
}

/// The roles granted to `role` by every script (`GRANT x TO role …` lines), the DDL guard's
/// marker left out.
fn granted_to(w: &FakeWorker, role: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for (_, s) in plain_scripts(w) {
        for line in s.lines() {
            let Some(rest) = line.trim().strip_prefix("GRANT ") else { continue };
            let Some((granted, tail)) = rest.split_once(" TO ") else { continue };
            let grantee = tail.split([' ', ';']).next().unwrap_or("");
            if grantee == role && granted != SCHEMA_LOCKED_ROLE {
                out.insert(granted.to_string());
            }
        }
    }
    out
}

/// A tenant admin of `db`'s tenant, with no grant.
fn add_tenant_admin(w: &FakeWorker, db: chelabase_types::DatabaseId) -> Uuid {
    let record = w.metadata.database(db);
    let member = Uuid::new_v4();
    w.metadata.state().members.push(MemberRecord {
        id: member,
        tenant_id: record.tenant_id,
        zitadel_user_id: format!("z-{member}"),
        email: None,
        display_name: "admin".into(),
        tenant_admin: true,
        status: MemberStatus::Active,
        accepted_at: None, offboarded: false, suspended_at: None, suspended_by: None,
    });
    member
}

/// The `VALID UNTIL '<ts>'` instant of `sql` (RFC 3339).
fn valid_until_in(sql: &str) -> DateTime<Utc> {
    let at = sql.find("VALID UNTIL '").unwrap_or_else(|| panic!("no VALID UNTIL: {sql}"));
    let rest = &sql[at + "VALID UNTIL '".len()..];
    let literal = &rest[..rest.find('\'').expect(sql)];
    DateTime::parse_from_rfc3339(literal)
        .unwrap_or_else(|e| panic!("VALID UNTIL {literal:?} isn't RFC 3339: {e}"))
        .with_timezone(&Utc)
}

#[test]
fn the_catalog_reads_marked_roles() {
    let sql = member_catalog_sql();
    assert!(sql.contains("chelabase.managed_role="), "the round-4 predicate: {sql}");
    let out = format!(
        "{}{}{}",
        cat(&["!postgres", "billing"]),
        marked_lines("alice", "SCRAM-a", Some(far()), &["billing"]),
        leftovers(&[("alice", Some("app"))])
    );
    let catalog = parse_member_catalog(&out).unwrap();
    assert_eq!(catalog.marked, set(&["alice"]));
    assert!(catalog.login.contains("alice") && catalog.roles.contains("alice"));
    assert!(!catalog.member_roles.contains_key("alice"), "a personal login isn't a member role");
    assert_eq!(
        catalog.personal["alice"],
        personal_actual("alice", vec![ours("billing")], "SCRAM-a", Some(far()))
    );
    assert_eq!(catalog.leftovers["alice"], BTreeSet::from([Some("app".to_string())]));
    let none = parse_member_catalog(&marked_lines("bob", "SCRAM-b", None, &[])).unwrap();
    assert_eq!(none.personal["bob"].valid_until, None);
    assert!(parse_member_catalog(&format!("u {} soon\n", h("bob"))).is_err());
}

/// Spec Part 6 "Create order" (MH-16): a new personal login is one transaction as
/// `cloud_admin`: the role (the personal-login attributes, `VALID UNTIL`, the record's
/// verifier), the marker, then on a protected branch for a non-Admin the DDL guard's marker,
/// before any membership. An existing unlocked one is locked first.
#[test]
fn plan_creates_marks_and_locks_in_one_transaction() {
    use MemberRoleStep::*;
    let steps = plan_member_roles(&[personal_desired("alice", &["billing"], "SCRAM-alice", true)], &[]);
    assert_eq!(
        steps,
        vec![
            CreatePersonal {
                role: "alice".into(),
                verifier: "SCRAM-alice".into(),
                valid_until: far(),
                lock: true,
            },
            Grant { role: "alice".into(), granted: "billing".into() },
        ]
    );
    let raw = render_step(&steps[0]);
    let sql = plain(&raw);
    let begin = sql.find("BEGIN;").expect(&sql);
    let create = sql
        .find("CREATE ROLE alice LOGIN NOINHERIT NOCREATEDB NOCREATEROLE NOBYPASSRLS NOREPLICATION VALID UNTIL '")
        .expect(&sql);
    assert_eq!(valid_until_in(&sql), far());
    assert!(sql.contains("PASSWORD 'SCRAM-alice'"), "{sql}");
    let mark = raw.find(&mark_personal_login_sql("alice")).expect(&raw);
    let mark = plain(&raw[..mark]).len();
    let lock = sql.find("GRANT chelabase_schema_locked TO alice").expect(&sql);
    let commit = sql.rfind("COMMIT;").expect(&sql);
    assert!(begin < create && create < mark && mark < lock && lock < commit, "{sql}");
    assert!(sql.trim_end().ends_with("COMMIT;"), "{sql}");
    assert!(!sql.contains("GRANT billing"), "memberships are their own step: {sql}");
    // Unlocked: no guard marker.
    let unlocked = render_step(&CreatePersonal {
        role: "alice".into(),
        verifier: "SCRAM-alice".into(),
        valid_until: far(),
        lock: false,
    });
    assert!(!unlocked.contains(SCHEMA_LOCKED_ROLE), "{unlocked}");
    assert!(unlocked.contains(&mark_personal_login_sql("alice")), "{unlocked}");
    // Never the member role's `Create`.
    assert!(!steps.iter().any(|s| matches!(s, Create { .. })));
    // An existing login on a branch that became protected: the lock before any grant.
    let have = personal_actual("alice", vec![], "SCRAM-alice", Some(far()));
    let steps = plan_member_roles(&[personal_desired("alice", &["billing"], "SCRAM-alice", true)], &[have]);
    assert_eq!(
        steps,
        vec![
            Lock { role: "alice".into() },
            Grant { role: "alice".into(), granted: "billing".into() },
        ]
    );
}

/// D2: the record's `VALID UNTIL` is set back wherever the catalog differs (a copy, a restore,
/// an expiry set by hand); a converged login plans nothing.
#[test]
fn valid_until_drift_is_repaired() {
    use MemberRoleStep::*;
    let wanted = personal_desired("alice", &["billing"], "SCRAM-alice", false);
    for drifted in [Some(far() - days(1)), None] {
        let have = personal_actual("alice", vec![ours("billing")], "SCRAM-alice", drifted);
        let steps = plan_member_roles(std::slice::from_ref(&wanted), &[have]);
        assert_eq!(steps, vec![SetValidUntil { role: "alice".into(), valid_until: far() }], "{drifted:?}");
        let sql = plain(&render_step(&steps[0]));
        assert!(sql.contains("ALTER ROLE alice VALID UNTIL '"), "{sql}");
        assert_eq!(valid_until_in(&sql), far());
    }
    let converged = personal_actual("alice", vec![ours("billing")], "SCRAM-alice", Some(far()));
    assert_eq!(plan_member_roles(&[wanted], &[converged]), vec![]);
}

/// MH-23: a personal login never holds the owner, an owner-level role, a member role or the
/// guard's marker as a membership, whatever its desired set says; and a tenant admin's login
/// gets every safe role (`tenant_admin_effective()`, S16), the data API's roles, owner-level,
/// LOGIN (other personal logins included) and reserved roles left out.
#[tokio::test]
async fn plan_never_grants_the_owner_or_an_owner_level_role() {
    let (w, _journal) = worker();
    let id = ready_database(&w).await;
    let main = w.metadata.main(id);
    let ep = main.endpoint_id.clone().unwrap();
    let admin = add_tenant_admin(&w, id);
    live_record(&w, id, admin, "alice", &scram("pw"));
    w.computes.sql.lock().unwrap().clear();
    w.computes.answer_at(
        &ep,
        "chelabase: member roles",
        &format!(
            "{}{}",
            cat(&["!postgres", "!app", "billing", "sales", "anon", "authenticated", "@deploy"]),
            marked_lines("bob", &scram("bob"), Some(far()), &[])
        ),
    );
    w.reconcile_member_roles(main.id, None).await;
    assert!(created(&w, "alice"), "{:#?}", plain_scripts(&w));
    assert_eq!(granted_to(&w, "alice"), set(&["billing", "sales"]));

    // The plan itself drops the owner and reserved names from any desired set.
    let wanted = personal_desired(
        "alice",
        &[OWNER_ROLE, "billing", "cb_m_x", SCHEMA_LOCKED_ROLE, "pg_monitor"],
        "SCRAM-alice",
        false,
    );
    let grants: Vec<String> = plan_member_roles(&[wanted], &[])
        .into_iter()
        .filter_map(|s| match s {
            MemberRoleStep::Grant { granted, .. } => Some(granted),
            _ => None,
        })
        .collect();
    assert_eq!(grants, ["billing"]);
}

/// D1, spec Part 6 "Memberships": a personal login holds only the roles held at Developer or
/// above (`Effective::roles_at_least(Developer)`), safe ones only, `INHERIT FALSE, SET TRUE`
/// (R10); the member role keeps its Viewer-level roles.
#[tokio::test]
async fn plan_grants_only_developer_level_safe_roles() {
    let (w, _journal) = worker();
    let (branch, ep, member) = setup(&w, &["billing", "deploy"]).await;
    let id = db_of(&w, branch);
    let project = w.metadata.database(id).project_id.as_uuid();
    w.metadata.state().team_grants.insert(
        (member, project),
        vec![Grant {
            platform_role: PlatformRole::Viewer,
            roles: RoleSet::Only([PgRole::Named("reports".into())].into_iter().collect()),
        }],
    );
    live_record(&w, id, member, "alice", &scram("pw"));
    w.computes.answer_at(&ep, "chelabase: member roles", &cat(&["!postgres", "billing", "reports", "@deploy"]));
    w.reconcile_member_roles(branch, None).await;
    assert_eq!(granted_to(&w, "alice"), set(&["billing"]), "{:#?}", plain_scripts(&w));
    assert!(
        plain_scripts(&w)
            .iter()
            .any(|(_, s)| s.contains("GRANT billing TO alice WITH INHERIT FALSE, SET TRUE, ADMIN FALSE;")),
        "R10: {:#?}",
        plain_scripts(&w)
    );
    assert_eq!(
        granted_to(&w, &member_role_name(member)),
        set(&["billing", "reports"]),
        "the member role keeps Viewer-level roles"
    );
}

/// MH-14, D2: a copy, restore or protected child carries another verifier; the record's is
/// set back, and the login's sessions end.
#[tokio::test]
async fn plan_sets_the_record_verifier_back_after_a_copy() {
    let (w, _journal) = worker();
    let (branch, ep, member) = setup(&w, &["billing"]).await;
    let id = db_of(&w, branch);
    let record = scram("the record's password");
    live_record(&w, id, member, "alice", &record);
    w.computes.answer_at(
        &ep,
        "chelabase: member roles",
        &format!(
            "{}{}",
            cat(&["!postgres", "billing"]),
            marked_lines("alice", &scram("a parent's password"), Some(far()), &["billing"])
        ),
    );
    w.reconcile_member_roles(branch, None).await;
    let scripts = plain_scripts(&w);
    let at = script_at(&scripts, &format!("ALTER ROLE alice PASSWORD '{record}';"));
    assert!(scripts[at].1.contains(&terminate_sql("alice")), "{}", scripts[at].1);
    assert!(!created(&w, "alice") && !dropped(&w, "alice"), "{scripts:#?}");
}

/// MH-17: a rotation, a revoked membership and a drop of a personal login each end its
/// sessions.
#[tokio::test]
async fn a_rotation_or_drop_ends_sessions() {
    let (w, _journal) = worker();
    let (branch, ep, member) = setup(&w, &["billing"]).await;
    let id = db_of(&w, branch);
    let record = scram("new");
    live_record(&w, id, member, "alice", &record);
    w.computes.answer_at(
        &ep,
        "chelabase: member roles",
        &format!(
            "{}{}{}",
            cat(&["!postgres", "billing", "hr"]),
            marked_lines("alice", &scram("old"), Some(far()), &["billing", "hr"]),
            marked_lines("bob", &scram("bob"), Some(far()), &[])
        ),
    );
    w.reconcile_member_roles(branch, None).await;
    let scripts = plain_scripts(&w);
    let rotate = script_at(&scripts, &format!("ALTER ROLE alice PASSWORD '{record}';"));
    assert!(scripts[rotate].1.contains(&terminate_sql("alice")), "{}", scripts[rotate].1);
    let revoke = script_at(&scripts, "REVOKE hr FROM alice");
    assert!(scripts[revoke].1.contains(&terminate_sql("alice")), "{}", scripts[revoke].1);
    let lock_out = script_at(&scripts, "ALTER ROLE bob NOLOGIN;");
    assert!(scripts[lock_out].1.contains(&terminate_sql("bob")), "{}", scripts[lock_out].1);
    assert!(dropped(&w, "bob"), "{scripts:#?}");
}

/// MH-10, K6 H5, MH-18 (amended, T6-C1): a marked role without a record is dropped as
/// `cloud_admin`: locked out with its sessions ended, then what it owns goes to the powerless
/// quarantine role `chelabase_orphaned` (never the owner) and `DROP OWNED` takes the rest, then
/// the role.
#[tokio::test]
async fn plan_drops_an_orphaned_marked_role_after_cleanup() {
    let (w, _journal) = worker();
    let (branch, ep, _member) = setup(&w, &["billing"]).await;
    w.computes.answer_at(
        &ep,
        "chelabase: member roles",
        &format!(
            "{}{}{}",
            cat(&["!postgres", "billing"]),
            marked_lines("bob", &scram("bob"), Some(far()), &["billing"]),
            leftovers(&[("bob", Some("app"))])
        ),
    );
    w.reconcile_member_roles(branch, None).await;
    let scripts = plain_scripts(&w);
    let lock_out = script_at(&scripts, "ALTER ROLE bob NOLOGIN;");
    assert!(scripts[lock_out].1.contains(&terminate_sql("bob")));
    let cleanup = script_at(&scripts, "DROP OWNED BY bob");
    assert_eq!(scripts[cleanup].0, "app", "{scripts:#?}");
    assert!(
        scripts[cleanup].1.contains(&format!("REASSIGN OWNED BY bob TO {ORPHANED_ROLE};")),
        "MH-18 (amended): what it owns goes to the quarantine role: {}",
        scripts[cleanup].1
    );
    assert!(
        !scripts[cleanup].1.contains(&format!("REASSIGN OWNED BY bob TO {OWNER_ROLE}")),
        "never to the owner: {}",
        scripts[cleanup].1
    );
    let gone = scripts
        .iter()
        .position(|(_, s)| s.contains("DROP ROLE IF EXISTS bob;") || s.contains("DROP ROLE bob;"))
        .expect("bob is dropped");
    assert!(lock_out < cleanup && cleanup < gone, "{scripts:#?}");
}

/// MH-17: a record whose member no longer qualifies (whatever its revocation says) is not
/// desired: the login is dropped, its row deleted, and the proxy cache told now and later.
#[tokio::test]
async fn plan_drops_a_login_whose_member_no_longer_qualifies() {
    let (w, journal) = worker();
    let (branch, ep, member) = setup(&w, &["billing"]).await;
    let id = db_of(&w, branch);
    let verifier = scram("pw");
    login_record(&w, id, member, "alice", Some(&verifier), far(), false, false);
    personal_row(&w, &ep, "alice", Some(&verifier));
    w.computes.answer_at(
        &ep,
        "chelabase: member roles",
        &format!("{}{}", cat(&["!postgres", "billing"]), marked_lines("alice", &verifier, Some(far()), &["billing"])),
    );
    journal.lock().unwrap().clear();
    w.reconcile_member_roles(branch, None).await;
    assert!(dropped(&w, "alice"), "{:#?}", plain_scripts(&w));
    assert!(!w.metadata.state().endpoints[&ep].logins.contains_key("alice"));
    let log = entries(&journal);
    let deleted = position(&journal, &format!("metadata.delete_personal_role_row {ep} alice")).unwrap_or_else(|| panic!("{log:#?}"));
    let now = log
        .iter()
        .position(|e| e == &format!("cache.password_updated {ep} alice"))
        .unwrap_or_else(|| panic!("{log:#?}"));
    let later = log
        .iter()
        .position(|e| e.starts_with(&format!("cache.password_updated_later {ep} ")) && e.contains("alice"))
        .unwrap_or_else(|| panic!("{log:#?}"));
    assert!(deleted < now && deleted < later, "{log:#?}");
}

/// D4's backstop: an expired record is not desired; the login is dropped.
#[tokio::test]
async fn plan_drops_an_expired_login() {
    let (w, _journal) = worker();
    let (branch, ep, member) = setup(&w, &["billing"]).await;
    let id = db_of(&w, branch);
    let verifier = scram("pw");
    let past = Utc::now() - days(1);
    login_record(&w, id, member, "alice", Some(&verifier), past, false, true);
    w.computes.answer_at(
        &ep,
        "chelabase: member roles",
        &format!("{}{}", cat(&["!postgres", "billing"]), marked_lines("alice", &verifier, Some(past), &["billing"])),
    );
    w.reconcile_member_roles(branch, None).await;
    assert!(dropped(&w, "alice"), "{:#?}", plain_scripts(&w));
    assert!(!created(&w, "alice"));
}

/// MH-10 orphan, a restore: the branch went back to before the member deleted `alice` and made
/// `alice2`; the restored `alice` is dropped, `alice2` made.
#[tokio::test]
async fn orphan_from_a_restore() {
    let (w, _journal) = worker();
    let (branch, ep, member) = setup(&w, &["billing"]).await;
    let id = db_of(&w, branch);
    live_record(&w, id, member, "alice2", &scram("new"));
    w.computes.answer_at(
        &ep,
        "chelabase: member roles",
        &format!("{}{}", cat(&["!postgres", "billing"]), marked_lines("alice", &scram("old"), Some(far()), &["billing"])),
    );
    w.reconcile_member_roles(branch, None).await;
    assert!(dropped(&w, "alice"), "{:#?}", plain_scripts(&w));
    assert!(created(&w, "alice2"), "{:#?}", plain_scripts(&w));
    assert!(!created(&w, "alice"));
}

/// MH-10 orphan, a reset to the parent: the parent still has Bob's old `dave` (deleted since),
/// and Carol's login now holds the name. The role is Carol's: her verifier set back (Bob's
/// sessions end), Bob's memberships revoked, hers granted; never dropped or adopted as Bob's.
#[tokio::test]
async fn orphan_from_a_reset_to_parent() {
    let (w, _journal) = worker();
    let (branch, ep, carol) = setup(&w, &["billing"]).await;
    let id = db_of(&w, branch);
    let carols = scram("carol");
    live_record(&w, id, carol, "dave", &carols);
    w.computes.answer_at(
        &ep,
        "chelabase: member roles",
        &format!(
            "{}{}",
            cat(&["!postgres", "billing", "hr"]),
            marked_lines("dave", &scram("bob's old"), Some(far()), &["hr"])
        ),
    );
    w.reconcile_member_roles(branch, None).await;
    let scripts = plain_scripts(&w);
    let rotate = script_at(&scripts, &format!("ALTER ROLE dave PASSWORD '{carols}';"));
    assert!(scripts[rotate].1.contains(&terminate_sql("dave")));
    script_at(&scripts, "REVOKE hr FROM dave");
    assert_eq!(granted_to(&w, "dave"), set(&["billing"]));
    assert!(!dropped(&w, "dave") && !created(&w, "dave"), "{scripts:#?}");
}

/// MH-10/MH-15 orphan, a lost create answer: the record is written first, so a login whose
/// answer was lost is a live record with its marked role and no row yet. It is kept (never
/// dropped or made again) and its row written, with the cache told (S16).
#[tokio::test]
async fn orphan_from_a_lost_create_answer() {
    let (w, journal) = worker();
    let (branch, ep, member) = setup(&w, &["billing"]).await;
    let id = db_of(&w, branch);
    let verifier = scram("pw");
    live_record(&w, id, member, "alice", &verifier);
    w.computes.answer_at(
        &ep,
        "chelabase: member roles",
        &format!("{}{}", cat(&["!postgres", "billing"]), marked_lines("alice", &verifier, Some(far()), &["billing"])),
    );
    journal.lock().unwrap().clear();
    w.reconcile_member_roles(branch, None).await;
    assert!(
        position(&journal, &format!("metadata.upsert_personal_role_row {ep} alice")).is_some(),
        "{:#?}",
        entries(&journal)
    );
    assert!(!dropped(&w, "alice") && !created(&w, "alice"), "{:#?}", plain_scripts(&w));
    let state = w.metadata.state();
    let row = &state.endpoints[&ep].logins["alice"];
    assert_eq!(row.origin, chelabase_metadata::RoleOrigin::Personal);
    assert_eq!(state.endpoints[&ep].verifier("alice"), Some(verifier));
}

/// MH-10 orphan, a deleted record: its rows went with it (FK cascade); the role is dropped and
/// no row written for it.
#[tokio::test]
async fn orphan_from_a_deleted_record() {
    let (w, journal) = worker();
    let (branch, ep, _member) = setup(&w, &["billing"]).await;
    w.computes.answer_at(
        &ep,
        "chelabase: member roles",
        &format!("{}{}", cat(&["!postgres", "billing"]), marked_lines("alice", &scram("pw"), Some(far()), &["billing"])),
    );
    journal.lock().unwrap().clear();
    w.reconcile_member_roles(branch, None).await;
    assert!(dropped(&w, "alice"), "{:#?}", plain_scripts(&w));
    assert!(position(&journal, "metadata.upsert_personal_role_row").is_none(), "{:#?}", entries(&journal));
    assert!(!plain_scripts(&w).iter().any(|(_, s)| s.contains("GRANT billing TO alice")));
}

/// Spec "Create order": an unmarked role holding the record's name is a customer's and never
/// adopted: the create fails (Postgres refuses the name), nothing else touches the role, no row
/// is written, and the record stays.
#[tokio::test]
async fn an_unmarked_role_with_the_records_name_is_never_adopted() {
    let (w, journal) = worker();
    let (branch, ep, member) = setup(&w, &["billing"]).await;
    let id = db_of(&w, branch);
    live_record(&w, id, member, "alice", &scram("pw"));
    w.computes.fail_when("CREATE ROLE alice ");
    w.computes.fail_when("CREATE ROLE \"alice\"");
    w.computes.answer_at(&ep, "chelabase: member roles", &cat(&["!postgres", "billing", "@alice"]));
    journal.lock().unwrap().clear();
    w.reconcile_member_roles(branch, None).await;
    for (_, s) in plain_scripts(&w) {
        for needle in ["ALTER ROLE alice", "TO alice", "FROM alice", "DROP ROLE IF EXISTS alice", "DROP ROLE alice", "DROP OWNED BY alice"] {
            assert!(!s.contains(needle), "{needle}: {s}");
        }
    }
    assert!(position(&journal, "metadata.upsert_personal_role_row").is_none(), "{:#?}", entries(&journal));
    assert_eq!(w.metadata.state().personal_logins.len(), 1);
}

/// MH-18 (amended, T6-C1): a kept login that owns or holds something (`pg_shdepend`) is
/// cleaned up (what it owns to `chelabase_orphaned`, never the owner, then `DROP OWNED`) before
/// its memberships are applied.
#[tokio::test]
async fn a_kept_login_loses_direct_privileges_before_memberships() {
    let (w, _journal) = worker();
    let (branch, ep, member) = setup(&w, &["billing"]).await;
    let id = db_of(&w, branch);
    let verifier = scram("pw");
    live_record(&w, id, member, "alice", &verifier);
    w.computes.answer_at(
        &ep,
        "chelabase: member roles",
        &format!(
            "{}{}{}",
            cat(&["!postgres", "billing"]),
            marked_lines("alice", &verifier, Some(far()), &[]),
            leftovers(&[("alice", Some("app"))])
        ),
    );
    w.reconcile_member_roles(branch, None).await;
    let scripts = plain_scripts(&w);
    let cleanup = script_at(&scripts, "DROP OWNED BY alice");
    assert_eq!(scripts[cleanup].0, "app");
    assert!(scripts[cleanup].1.contains(&format!("REASSIGN OWNED BY alice TO {ORPHANED_ROLE};")), "{}", scripts[cleanup].1);
    assert!(!scripts[cleanup].1.contains(&format!("REASSIGN OWNED BY alice TO {OWNER_ROLE}")), "{}", scripts[cleanup].1);
    let grant = script_at(&scripts, "GRANT billing TO alice");
    assert!(cleanup < grant, "{scripts:#?}");
    assert!(!dropped(&w, "alice"));
}

/// MH-16 through the reconcile: on a protected branch a Developer's new login is created
/// with the guard's marker in the create transaction, before its memberships; a tenant admin's
/// isn't (with `guard_logins` on the guard itself covers every login).
#[tokio::test]
async fn a_protected_branch_creates_a_developers_login_locked_and_an_admins_unlocked() {
    let (w, _journal) = worker();
    let (branch, ep, member) = setup(&w, &["billing"]).await;
    let id = db_of(&w, branch);
    w.metadata.state().branches.get_mut(&branch).unwrap().protected = true;
    w.computes.answer(BRANCH_PROTECTED_CHECK_MARK, "on");
    let admin = add_tenant_admin(&w, id);
    live_record(&w, id, member, "alice", &scram("a"));
    live_record(&w, id, admin, "root_ann", &scram("b"));
    w.computes.answer_at(&ep, "chelabase: member roles", &cat(&["!postgres", "billing"]));
    w.reconcile_member_roles(branch, None).await;
    let scripts = plain_scripts(&w);
    let create = script_at(&scripts, "CREATE ROLE alice ");
    let script = &scripts[create].1;
    let lock = script.find("GRANT chelabase_schema_locked TO alice").expect(script);
    assert!(script.find("CREATE ROLE alice ").unwrap() < lock, "{script}");
    let grant = script_at(&scripts, "GRANT billing TO alice");
    assert!(create < grant || (create == grant && lock < script.find("GRANT billing TO alice").unwrap()), "{scripts:#?}");
    let admins = script_at(&scripts, "CREATE ROLE root_ann ");
    assert!(!scripts[admins].1.contains(SCHEMA_LOCKED_ROLE), "{}", scripts[admins].1);
}

/// S16: every personal row the worker writes clears the proxy cache now and again later.
#[tokio::test]
async fn personal_row_writes_clear_the_proxy_cache_twice() {
    let (w, journal) = worker();
    let (branch, ep, member) = setup(&w, &["billing"]).await;
    let id = db_of(&w, branch);
    let verifier = scram("pw");
    live_record(&w, id, member, "alice", &verifier);
    w.computes.answer_at(&ep, "chelabase: member roles", &cat(&["!postgres", "billing"]));
    journal.lock().unwrap().clear();
    w.reconcile_member_roles(branch, None).await;
    let log = entries(&journal);
    let upsert = position(&journal, &format!("metadata.upsert_personal_role_row {ep} alice")).unwrap_or_else(|| panic!("{log:#?}"));
    let now = log
        .iter()
        .position(|e| e == &format!("cache.password_updated {ep} alice"))
        .unwrap_or_else(|| panic!("{log:#?}"));
    let later = log
        .iter()
        .position(|e| e.starts_with(&format!("cache.password_updated_later {ep} ")) && e.contains("alice"))
        .unwrap_or_else(|| panic!("{log:#?}"));
    assert!(upsert < now && upsert < later, "{log:#?}");
    assert_eq!(w.metadata.state().endpoints[&ep].verifier("alice"), Some(verifier));
}

/// S9: a run for one member plans that member's personal logins (create) and leaves every
/// other login alone, and never drops an orphan (it can't be attributed); the unfiltered run
/// does.
#[tokio::test]
async fn a_member_filtered_run_creates_that_members_login_and_drops_no_orphan() {
    let (w, _journal) = worker();
    let (branch, ep, m) = setup(&w, &["billing"]).await;
    let id = db_of(&w, branch);
    let n = add_member(&w, id, &["billing"]);
    live_record(&w, id, m, "alice", &scram("a"));
    live_record(&w, id, n, "bob", &scram("b"));
    // Nina's record no longer qualifies; her login is still there.
    login_record(&w, id, n, "nina", Some(&scram("c")), far(), false, false);
    w.computes.answer_at(
        &ep,
        "chelabase: member roles",
        &format!(
            "{}{}{}",
            cat(&["!postgres", "billing"]),
            marked_lines("zed", &scram("z"), Some(far()), &[]),
            marked_lines("nina", &scram("c"), Some(far()), &["billing"])
        ),
    );
    w.reconcile_member_roles(branch, Some(m)).await;
    assert!(created(&w, "alice"), "{:#?}", plain_scripts(&w));
    for other in ["bob", "zed", "nina"] {
        assert!(
            !plain_scripts(&w).iter().any(|(_, s)| s.contains(&format!(" {other}"))),
            "{other} is left alone: {:#?}",
            plain_scripts(&w)
        );
    }
    w.computes.sql.lock().unwrap().clear();
    w.reconcile_member_roles(branch, None).await;
    assert!(dropped(&w, "zed") && dropped(&w, "nina"), "{:#?}", plain_scripts(&w));
    assert!(created(&w, "bob"), "{:#?}", plain_scripts(&w));
}

/// The convergence rule (spec Part 6, MH-10, MH-17, S9), exhaustively over one login: after a
/// run, the login is made exactly when its record is live (qualifies, unrevoked, unexpired)
/// and it's missing, and dropped exactly when it's present and not wanted, for an unfiltered
/// run or one for its own member when it has a record; a run for another member, or for its
/// member without a record (an orphan), touches nothing.
#[tokio::test]
async fn convergence_follows_the_desired_set_exhaustively() {
    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Filter {
        None,
        Own,
        Other,
    }
    let mut cases = Vec::new();
    for has_record in [false, true] {
        let flags: Vec<(bool, bool, bool)> = if has_record {
            let mut v = Vec::new();
            for q in [false, true] {
                for r in [false, true] {
                    for e in [false, true] {
                        v.push((q, r, e));
                    }
                }
            }
            v
        } else {
            vec![(false, false, false)]
        };
        for (qualifies, revoked, expired) in flags {
            for in_catalog in [false, true] {
                for filter in [Filter::None, Filter::Own, Filter::Other] {
                    cases.push((has_record, qualifies, revoked, expired, in_catalog, filter));
                }
            }
        }
    }
    for (has_record, qualifies, revoked, expired, in_catalog, filter) in cases {
        let case = format!("record {has_record}, qualifies {qualifies}, revoked {revoked}, expired {expired}, in catalog {in_catalog}, run {filter:?}");
        let (w, _journal) = worker();
        let (branch, ep, m) = setup(&w, &["billing"]).await;
        let id = db_of(&w, branch);
        let other = add_member(&w, id, &["billing"]);
        let verifier = scram("pw");
        let until = if expired { Utc::now() - days(1) } else { far() };
        if has_record {
            let kept = (!revoked).then_some(verifier.as_str());
            login_record(&w, id, m, "alice", kept, until, revoked, qualifies);
        }
        let mut answer = cat(&["!postgres", "billing"]);
        if in_catalog {
            answer.push_str(&marked_lines("alice", &verifier, Some(until), &["billing"]));
        }
        w.computes.answer_at(&ep, "chelabase: member roles", &answer);
        let run_for = match filter {
            Filter::None => None,
            Filter::Own => Some(m),
            Filter::Other => Some(other),
        };
        w.reconcile_member_roles(branch, run_for).await;
        let wanted = has_record && qualifies && !revoked && !expired;
        let touched = filter == Filter::None || (filter == Filter::Own && has_record);
        assert_eq!(created(&w, "alice"), touched && wanted && !in_catalog, "create: {case}: {:#?}", plain_scripts(&w));
        assert_eq!(dropped(&w, "alice"), touched && !wanted && in_catalog, "drop: {case}: {:#?}", plain_scripts(&w));
    }
}

// --- T6 fix round, C1 (spec Part 6 "No privileges of its own", MH-18 as amended by the user
// 2026-10-23): a personal login's objects are quarantined, never handed to the owner. The
// canonical procedure with a powerless successor: the fixed, reserved NOLOGIN role
// `chelabase_orphaned` (no members, no privileges) is made when missing, in the same script and
// before the reassign; `REASSIGN OWNED BY <login> TO chelabase_orphaned`, then `DROP OWNED BY
// <login>`, in every database with leftovers; EXECUTE is revoked from PUBLIC on the functions
// moved to it. Both for a kept login's cleanup and for a login's drop.

/// The quarantine role (spec Part 6, MH-18 amended).
const ORPHANED_ROLE: &str = "chelabase_orphaned";

/// The statement that makes the quarantine role, from `CREATE ROLE` to its end (`;` or the
/// closing quote of a dynamic `EXECUTE '…'`), quotes removed.
fn orphaned_create(script: &str) -> Option<String> {
    let script = plain(script);
    let at = script.find(&format!("CREATE ROLE {ORPHANED_ROLE}"))?;
    let rest = &script[at..];
    let end = rest.find([';', '\'']).unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

/// Checks one personal cleanup script (spec Part 6, MH-18 amended) for `role`.
fn assert_quarantines(script: &str, role: &str) {
    let p = plain(script);
    assert!(script.starts_with(CLEANUP_MARK), "still a cleanup (R31's filters read the mark): {script}");
    // The quarantine role is made when missing, before the reassign, in this script.
    let create = orphaned_create(script)
        .unwrap_or_else(|| panic!("the script makes {ORPHANED_ROLE} when missing: {script}"));
    let words: Vec<&str> = create.split_whitespace().collect();
    assert!(words.contains(&"NOLOGIN"), "NOLOGIN: {create}");
    for forbidden in [
        "LOGIN", "SUPERUSER", "CREATEDB", "CREATEROLE", "REPLICATION", "BYPASSRLS", "INHERIT",
        "PASSWORD", "IN", "ADMIN", "USER", "SYSID",
    ] {
        assert!(!words.contains(&forbidden), "no {forbidden}: no members, no privileges: {create}");
    }
    assert_eq!(
        words.iter().filter(|w| **w == "ROLE").count(),
        1,
        "no `ROLE …` member clause: {create}"
    );
    assert!(
        p.contains(&format!("'{ORPHANED_ROLE}'")) || p.contains("duplicate_object"),
        "made only when missing (an existence check, or `duplicate_object` caught): {script}"
    );
    let created_at = p.find(&format!("CREATE ROLE {ORPHANED_ROLE}")).unwrap();
    // Every reassign targets the quarantine role, never the owner or anyone else.
    let reassign = format!("REASSIGN OWNED BY {role} TO {ORPHANED_ROLE};");
    let reassigned_at = p
        .find(&reassign)
        .unwrap_or_else(|| panic!("{reassign:?} in {script}"));
    assert_eq!(p.matches("REASSIGN OWNED BY").count(), 1, "one reassign: {script}");
    assert!(!p.contains(&format!("TO {OWNER_ROLE};")), "never to the owner: {script}");
    assert!(!p.contains("TO cloud_admin"), "never to a superuser: {script}");
    let dropped_at = p
        .find(&format!("DROP OWNED BY {role};"))
        .unwrap_or_else(|| panic!("DROP OWNED follows: {script}"));
    assert!(created_at < reassigned_at && reassigned_at < dropped_at, "create, reassign, drop owned: {script}");
    // EXECUTE is revoked from PUBLIC on the functions the quarantine role holds.
    assert!(
        p.lines().any(|l| l.contains("REVOKE") && l.contains("EXECUTE") && l.contains("FROM PUBLIC")),
        "EXECUTE revoked from PUBLIC on the quarantined functions: {script}"
    );
    assert!(
        p.contains("pg_proc") || p.contains("ALL FUNCTIONS") || p.contains("ALL ROUTINES"),
        "the revoke reaches the functions themselves: {script}"
    );
    // Nothing grants the quarantine role to anyone or anything to it.
    assert!(!p.contains(&format!("GRANT {ORPHANED_ROLE}")), "never a member: {script}");
    for line in p.lines().filter(|l| l.contains("GRANT ") && !l.contains("REVOKE")) {
        assert!(!line.contains(&format!("TO {ORPHANED_ROLE}")), "never granted anything: {line}");
    }
}

/// MH-18 (amended), C1: the rendered script, for a plain name and one that needs quoting.
#[test]
fn personal_cleanup_quarantines_to_a_powerless_role() {
    assert_quarantines(&personal_cleanup_script("alice"), "alice");
    let odd = personal_cleanup_script("Al\"ice x");
    assert!(
        odd.contains("REASSIGN OWNED BY \"Al\"\"ice x\" TO "),
        "the login is quoted: {odd}"
    );
    assert!(odd.contains("DROP OWNED BY \"Al\"\"ice x\";"), "{odd}");
    assert!(
        orphaned_create(&odd).is_some() && plain(&odd).contains(&format!("TO {ORPHANED_ROLE};")),
        "{odd}"
    );
    // The timeouts R31's cleanup runs under are kept.
    assert!(odd.contains("SET lock_timeout") && odd.contains("SET statement_timeout"), "{odd}");
}

/// MH-18 (amended): a kept login's cleanup runs in every database where it owns or holds
/// something, shared objects from the system database, each one the whole quarantine script.
#[tokio::test]
async fn a_kept_logins_cleanup_quarantines_in_every_database() {
    let (w, _journal) = worker();
    let (branch, ep, member) = setup(&w, &["billing"]).await;
    let id = db_of(&w, branch);
    let verifier = scram("pw");
    live_record(&w, id, member, "alice", &verifier);
    let answer = Cat {
        roles: &["!postgres", "billing"],
        memberships: &[],
        edges: &[],
        passwords: &[],
        databases: &[("app", "postgres"), ("other", "postgres"), ("postgres", "cloud_admin")],
    }
    .answer();
    w.computes.answer_at(
        &ep,
        "chelabase: member roles",
        &format!(
            "{answer}{}{}",
            marked_lines("alice", &verifier, Some(far()), &["billing"]),
            leftovers(&[("alice", Some("app")), ("alice", Some("other")), ("alice", None)])
        ),
    );
    w.reconcile_member_roles(branch, None).await;
    let cleanups: Vec<(String, String)> = changing_scripts(&w)
        .into_iter()
        .filter(|(_, s)| plain(s).contains("DROP OWNED BY alice"))
        .collect();
    for (db, s) in &cleanups {
        assert_quarantines(s, "alice");
        assert!(!db.is_empty(), "{s}");
    }
    let dbs: BTreeSet<&str> = cleanups.iter().map(|(db, _)| db.as_str()).collect();
    assert_eq!(dbs, BTreeSet::from(["app", "other", "postgres"]), "{cleanups:#?}");
    assert!(!dropped(&w, "alice"));
}

/// MH-18 (amended), MH-10: a dropped login's objects are quarantined in every database with
/// leftovers, before its `DROP ROLE`.
#[tokio::test]
async fn a_dropped_login_is_quarantined_in_every_database_before_the_drop() {
    let (w, _journal) = worker();
    let (branch, ep, _member) = setup(&w, &["billing"]).await;
    let answer = Cat {
        roles: &["!postgres", "billing"],
        memberships: &[],
        edges: &[],
        passwords: &[],
        databases: &[("app", "postgres"), ("other", "postgres")],
    }
    .answer();
    w.computes.answer_at(
        &ep,
        "chelabase: member roles",
        &format!(
            "{answer}{}{}",
            marked_lines("bob", &scram("bob"), Some(far()), &["billing"]),
            leftovers(&[("bob", Some("app")), ("bob", Some("other"))])
        ),
    );
    w.reconcile_member_roles(branch, None).await;
    let scripts = plain_scripts(&w);
    let cleanups: Vec<usize> = scripts
        .iter()
        .enumerate()
        .filter(|(_, (_, s))| s.contains("DROP OWNED BY bob"))
        .map(|(i, _)| i)
        .collect();
    let dbs: BTreeSet<&str> = cleanups.iter().map(|i| scripts[*i].0.as_str()).collect();
    assert_eq!(dbs, BTreeSet::from(["app", "other"]), "{scripts:#?}");
    for i in &cleanups {
        assert_quarantines(&scripts[*i].1, "bob");
    }
    let gone = scripts
        .iter()
        .position(|(_, s)| s.contains("DROP ROLE IF EXISTS bob;") || s.contains("DROP ROLE bob;"))
        .expect("bob is dropped");
    assert!(cleanups.iter().all(|i| *i < gone), "{scripts:#?}");
}

/// MH-18 (amended): `chelabase_orphaned` is reserved: never a role a member role or personal
/// login is granted (a grant naming it, the role in the catalog), never act-as, and a
/// membership in it found in the catalog is revoked.
#[tokio::test]
async fn the_quarantine_role_is_never_granted() {
    assert!(!grantable(ORPHANED_ROLE));
    assert!(!grantable("Chelabase_Orphaned"));
    assert!(chelabase_types::access::act_as_reserved(ORPHANED_ROLE));
    let (w, _journal) = worker();
    let (branch, ep, member) = setup(&w, &["billing", ORPHANED_ROLE]).await;
    let id = db_of(&w, branch);
    let role = member_role_name(member);
    let verifier = row(&w, branch, member, "pw", &["billing"]);
    let login = scram("pl");
    live_record(&w, id, member, "alice", &login);
    let answer = Cat {
        roles: &["!postgres", "billing", ORPHANED_ROLE, &role],
        memberships: &[(&role, "billing"), (&role, ORPHANED_ROLE)],
        edges: &[],
        passwords: &[(&role, &verifier)],
        databases: &[("app", "postgres")],
    }
    .answer();
    w.computes.answer_at(
        &ep,
        "chelabase: member roles",
        &format!("{answer}{}", marked_lines("alice", &login, Some(far()), &[ORPHANED_ROLE])),
    );
    w.reconcile_member_roles(branch, None).await;
    assert!(!granted_to(&w, &role).contains(ORPHANED_ROLE), "{:#?}", plain_scripts(&w));
    assert!(!granted_to(&w, "alice").contains(ORPHANED_ROLE), "{:#?}", plain_scripts(&w));
    let scripts = plain_scripts(&w);
    script_at(&scripts, &format!("REVOKE {ORPHANED_ROLE} FROM {role}"));
    script_at(&scripts, &format!("REVOKE {ORPHANED_ROLE} FROM alice"));
}

/// M5, plan P16 (b), S2: "Revoke Postgres access now" empties the member's
/// `member_branch_roles.password_enc`; the next reconcile rotates the `cb_m_*` password and
/// ends its sessions, and the row carries the fresh sealed password.
#[tokio::test]
async fn an_emptied_member_role_password_is_rotated_with_sessions_ended() {
    let (w, _journal) = worker();
    let (branch, ep, member) = setup(&w, &["billing"]).await;
    let role = member_role_name(member);
    let old = row(&w, branch, member, "the old password", &["billing"]);
    w.metadata.state().member_roles[0].password_enc = String::new();
    w.computes.answer_at(
        &ep,
        "chelabase: member roles",
        &Cat {
            roles: &["!postgres", "billing", &role],
            memberships: &[(&role, "billing")],
            edges: &[],
            passwords: &[(&role, &old)],
            databases: &[("app", "postgres")],
        }
        .answer(),
    );
    assert!(done(&w.reconcile_member_roles(branch, None).await));
    let sql = texts(&w);
    let rotate = sql
        .iter()
        .find(|s| s.contains(&format!("ALTER ROLE {role} PASSWORD 'SCRAM-SHA-256$")))
        .unwrap_or_else(|| panic!("the password is rotated: {sql:#?}"));
    assert!(rotate.contains(&terminate_sql(&role)), "its sessions end: {rotate}");
    let stored = w.metadata.state().member_roles[0].clone();
    assert!(!stored.password_enc.is_empty(), "the row is sealed again");
    let password =
        open_member_role_password(&w.key, branch.as_uuid(), &role, &stored.password_enc).unwrap();
    assert_ne!(password, "the old password");
    let verifier = rotate.split('\'').nth(1).unwrap();
    assert!(chelabase_neon::scram::verifies(verifier, &password));
}
