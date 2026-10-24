//! T4 RED: `verify_privileges` (MH-24, S3), `all_rows_exposure` (MH-26) and
//! `all_rows_policy_name` (spec Part 4).

use super::*;
use crate::model::{NamedRole, Oid, RoleRef};
use chelabase_types::table_api::{
    Grantee, Privilege, SchemaPrivilege, SequencePrivilege, TableGrant, TableRef,
};

fn grant(grantor: &str, grantee: &str, privilege: &str) -> Grant {
    Grant {
        grantor: NamedRole {
            oid: Oid(10),
            name: grantor.into(),
        },
        grantee: RoleRef::Role {
            oid: Oid(20),
            name: grantee.into(),
        },
        privilege: privilege.into(),
        grantable: false,
    }
}

fn offered() -> Offered {
    Offered {
        table: vec![],
        sequence: vec![],
        schema: vec![],
        function: vec!["execute".into()],
    }
}

fn page(role: &str) -> PrivilegesPage {
    PrivilegesPage {
        role: role.into(),
        server_version_num: 170_005,
        offered: offered(),
        schemas: vec![],
        default_privileges: vec![],
        objects: vec![],
        next_after: None,
    }
}

fn object(
    kind: PrivilegeObjectKind,
    schema: &str,
    name: &str,
    oid: u32,
    grants: Vec<Grant>,
) -> ObjectPrivileges {
    ObjectPrivileges {
        kind,
        schema: schema.into(),
        name: name.into(),
        oid,
        identity: None,
        owner: "owner".into(),
        owner_settable: true,
        rls_enabled: false,
        grants,
        columns: vec![],
        all_rows: None,
        acl_hash: "0".repeat(64),
    }
}

fn table(schema: &str, name: &str) -> TableRef {
    TableRef {
        schema: schema.into(),
        name: name.into(),
    }
}

fn table_grant(privileges: Vec<Privilege>, grantee: &str) -> TableGrant {
    TableGrant {
        columns: vec![],
        privileges,
        grantee: Grantee::Role(grantee.into()),
        with_grant_option: false,
    }
}

fn sorted(mut v: Vec<PrivilegeMismatch>) -> Vec<PrivilegeMismatch> {
    v.sort();
    v
}

/// S3: the ops span two schemas, a sequence, a schema privilege and a default privilege; the
/// pages are the per-schema pages (the first one paged in two) plus the `schema: None` page.
/// Everything landed: no mismatch. Then one grant is missing from the second page of `app`.
#[test]
fn verify_reads_several_pages() {
    let ops = [
        Operation::Grant {
            table: table("app", "a"),
            grant: table_grant(vec![Privilege::Select], "readers"),
        },
        Operation::Grant {
            table: table("app", "z"),
            grant: table_grant(vec![Privilege::Insert], "readers"),
        },
        Operation::Grant {
            table: table("billing", "b"),
            grant: table_grant(vec![Privilege::Select], "readers"),
        },
        Operation::GrantSequence {
            sequence: table("app", "s"),
            grantee: Grantee::Role("readers".into()),
            privileges: vec![SequencePrivilege::Usage],
        },
        Operation::GrantSchema {
            schema: "app".into(),
            grantee: Grantee::Role("readers".into()),
            privileges: vec![SchemaPrivilege::Usage],
        },
        Operation::AlterDefaultPrivileges {
            for_role: "owner".into(),
            schema: "app".into(),
            object: DefaultObject::Tables,
            grant: true,
            grantee: Grantee::Role("readers".into()),
            privileges: vec![chelabase_types::table_api::PrivilegeName::Select],
        },
    ];
    let app_1 = PrivilegesPage {
        objects: vec![
            object(
                PrivilegeObjectKind::Table,
                "app",
                "a",
                101,
                vec![grant("owner", "readers", "SELECT")],
            ),
            object(
                PrivilegeObjectKind::Sequence,
                "app",
                "s",
                102,
                vec![grant("owner", "readers", "USAGE")],
            ),
        ],
        next_after: Some("s".into()),
        ..page("readers")
    };
    let app_2_missing = PrivilegesPage {
        objects: vec![object(PrivilegeObjectKind::Table, "app", "z", 103, vec![])],
        ..page("readers")
    };
    let billing = PrivilegesPage {
        objects: vec![object(
            PrivilegeObjectKind::Table,
            "billing",
            "b",
            201,
            vec![grant("owner", "readers", "SELECT")],
        )],
        ..page("readers")
    };
    let schemas = PrivilegesPage {
        schemas: vec![SchemaPrivileges {
            name: "app".into(),
            owner: "owner".into(),
            owner_settable: true,
            grants: vec![grant("owner", "readers", "USAGE")],
            acl_hash: "1".repeat(64),
        }],
        default_privileges: vec![DefaultPrivileges {
            for_role: "owner".into(),
            schema: "app".into(),
            object: DefaultObject::Tables,
            grants: vec![grant("owner", "readers", "SELECT")],
            acl_hash: "2".repeat(64),
        }],
        ..page("readers")
    };

    let missing = verify_privileges(
        &ops,
        &[
            app_1.clone(),
            app_2_missing,
            billing.clone(),
            schemas.clone(),
        ],
    );
    assert_eq!(
        missing,
        Err(vec![PrivilegeMismatch {
            object: PrivilegeTarget::Relation {
                schema: "app".into(),
                name: "z".into()
            },
            privilege: "INSERT".into(),
            grantee: "readers".into(),
            still_granted_by: None,
            column: None,
            wanted: true,
        }]),
        "the grant on app.z, on the second page of app, didn't land"
    );

    let app_2 = PrivilegesPage {
        objects: vec![object(
            PrivilegeObjectKind::Table,
            "app",
            "z",
            103,
            vec![grant("owner", "readers", "INSERT")],
        )],
        ..page("readers")
    };
    assert_eq!(
        verify_privileges(&ops, &[app_1, app_2, billing, schemas]),
        Ok(())
    );
}

/// MH-24: an owner's REVOKE leaves another grantor's grant in place; verify names that grantor,
/// for a table, a schema, a sequence, a function and a default privilege.
#[test]
fn verify_flags_a_revoke_another_grantor_still_holds() {
    let readers = || Grantee::Role("readers".into());
    let ops = [
        Operation::Revoke {
            table: table("app", "t"),
            grant: table_grant(vec![Privilege::Select], "readers"),
        },
        Operation::RevokeSchema {
            schema: "app".into(),
            grantee: readers(),
            privileges: vec![SchemaPrivilege::Usage],
            grant_option_only: false,
        },
        Operation::RevokeSequence {
            sequence: table("app", "s"),
            grantee: readers(),
            privileges: vec![SequencePrivilege::Update],
            grant_option_only: false,
        },
        Operation::RevokeFunction {
            function: chelabase_types::table_api::FunctionRef {
                oid: 16384,
                identity: "app.f(integer)".into(),
            },
            grantee: readers(),
            grant_option_only: false,
        },
        Operation::AlterDefaultPrivileges {
            for_role: "owner".into(),
            schema: "app".into(),
            object: DefaultObject::Sequences,
            grant: false,
            grantee: readers(),
            privileges: vec![chelabase_types::table_api::PrivilegeName::Usage],
        },
    ];
    let objects = PrivilegesPage {
        objects: vec![
            object(
                PrivilegeObjectKind::Table,
                "app",
                "t",
                101,
                vec![
                    grant("delegate", "readers", "SELECT"),
                    grant("owner", "readers", "INSERT"),
                ],
            ),
            object(
                PrivilegeObjectKind::Sequence,
                "app",
                "s",
                102,
                vec![grant("delegate", "readers", "UPDATE")],
            ),
            object(
                PrivilegeObjectKind::Function,
                "app",
                "f",
                16384,
                vec![grant("fn_owner", "readers", "EXECUTE")],
            ),
        ],
        ..page("readers")
    };
    let schemas = PrivilegesPage {
        schemas: vec![SchemaPrivileges {
            name: "app".into(),
            owner: "owner".into(),
            owner_settable: true,
            grants: vec![grant("other_admin", "readers", "USAGE")],
            acl_hash: "1".repeat(64),
        }],
        default_privileges: vec![DefaultPrivileges {
            for_role: "owner".into(),
            schema: "app".into(),
            object: DefaultObject::Sequences,
            grants: vec![grant("owner", "readers", "USAGE")],
            acl_hash: "2".repeat(64),
        }],
        ..page("readers")
    };
    let m = |object: PrivilegeTarget, privilege: &str, by: &str| PrivilegeMismatch {
        object,
        privilege: privilege.into(),
        grantee: "readers".into(),
        still_granted_by: Some(by.into()),
        column: None,
        wanted: false,
    };
    let got = verify_privileges(&ops, &[objects, schemas]).map_err(sorted);
    assert_eq!(
        got,
        Err(sorted(vec![
            m(
                PrivilegeTarget::Relation {
                    schema: "app".into(),
                    name: "t".into()
                },
                "SELECT",
                "delegate"
            ),
            m(
                PrivilegeTarget::Schema {
                    schema: "app".into()
                },
                "USAGE",
                "other_admin"
            ),
            m(
                PrivilegeTarget::Sequence {
                    schema: "app".into(),
                    name: "s".into()
                },
                "UPDATE",
                "delegate"
            ),
            m(
                PrivilegeTarget::Function { oid: 16384 },
                "EXECUTE",
                "fn_owner"
            ),
            m(
                PrivilegeTarget::DefaultAcl {
                    for_role: "owner".into(),
                    schema: "app".into(),
                    object: DefaultObject::Sequences
                },
                "USAGE",
                "owner",
            ),
        ])),
        "INSERT on app.t was not revoked and isn't a mismatch"
    );
}

/// MH-24: a grant that isn't in the re-read (an empty object, a missing object, another
/// grantee's entry, a missing schema row) is a mismatch with `wanted: true`.
#[test]
fn verify_flags_a_grant_that_did_not_land() {
    let ops = [
        Operation::Grant {
            table: table("app", "t"),
            grant: table_grant(vec![Privilege::Select, Privilege::Delete], "readers"),
        },
        Operation::GrantFunction {
            function: chelabase_types::table_api::FunctionRef {
                oid: 16384,
                identity: "app.f(integer)".into(),
            },
            grantee: Grantee::Role("readers".into()),
        },
        Operation::GrantSchema {
            schema: "app".into(),
            grantee: Grantee::Role("readers".into()),
            privileges: vec![SchemaPrivilege::Create],
        },
    ];
    let objects = PrivilegesPage {
        objects: vec![
            object(
                PrivilegeObjectKind::Table,
                "app",
                "t",
                101,
                vec![grant("owner", "readers", "SELECT")],
            ),
            object(PrivilegeObjectKind::Function, "app", "f", 16384, vec![]),
        ],
        ..page("readers")
    };
    let schemas = PrivilegesPage {
        schemas: vec![SchemaPrivileges {
            name: "app".into(),
            owner: "owner".into(),
            owner_settable: true,
            grants: vec![grant("owner", "readers", "USAGE")],
            acl_hash: "1".repeat(64),
        }],
        ..page("readers")
    };
    let m = |object: PrivilegeTarget, privilege: &str| PrivilegeMismatch {
        object,
        privilege: privilege.into(),
        grantee: "readers".into(),
        still_granted_by: None,
        column: None,
        wanted: true,
    };
    let got = verify_privileges(&ops, &[objects, schemas]).map_err(sorted);
    assert_eq!(
        got,
        Err(sorted(vec![
            m(
                PrivilegeTarget::Relation {
                    schema: "app".into(),
                    name: "t".into()
                },
                "DELETE"
            ),
            m(PrivilegeTarget::Function { oid: 16384 }, "EXECUTE"),
            m(
                PrivilegeTarget::Schema {
                    schema: "app".into()
                },
                "CREATE"
            ),
        ]))
    );
    // An object missing from every page: its grant didn't land either.
    let gone = verify_privileges(&ops[..1], &[page("readers")]);
    assert!(gone.is_err(), "{gone:?}");
}

fn edges(pairs: &[(&str, &str)]) -> BTreeMap<String, BTreeSet<String>> {
    let mut m: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (member, of) in pairs {
        m.entry(member.to_string())
            .or_default()
            .insert(of.to_string());
    }
    m
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|s| s.to_string()).collect()
}

fn chain_set(chain: Option<Vec<String>>) -> Option<BTreeSet<String>> {
    chain.map(|c| c.into_iter().collect())
}

/// MH-26: `role` is inherited by `anon`/`authenticated`/`authenticator` through a chain of
/// any length and any membership option; the target itself being one of them; PUBLIC.
#[test]
fn exposure_finds_anon_through_any_membership_chain() {
    let none = BTreeSet::new();
    let g = edges(&[("authenticated", "mid"), ("mid", "billing_reader")]);
    assert_eq!(
        chain_set(all_rows_exposure(&g, "billing_reader", &none)),
        Some(set(&["authenticated", "mid", "billing_reader"])),
        "authenticated → mid → billing_reader"
    );
    let g = edges(&[("anon", "a1"), ("a1", "a2"), ("a2", "a3"), ("a3", "target")]);
    assert_eq!(
        chain_set(all_rows_exposure(&g, "target", &none)),
        Some(set(&["anon", "a1", "a2", "a3", "target"]))
    );
    let g = edges(&[("authenticator", "target")]);
    assert_eq!(
        chain_set(all_rows_exposure(&g, "target", &none)),
        Some(set(&["authenticator", "target"]))
    );
    for direct in ["anon", "authenticated", "authenticator"] {
        let got = all_rows_exposure(&BTreeMap::new(), direct, &none);
        assert!(
            got.as_ref().is_some_and(|c| c.iter().any(|r| r == direct)),
            "{direct}: {got:?}"
        );
    }
    assert!(
        all_rows_exposure(&BTreeMap::new(), "public", &none).is_some(),
        "PUBLIC"
    );
    assert!(
        all_rows_exposure(&BTreeMap::new(), "PUBLIC", &none).is_some(),
        "PUBLIC, any case"
    );

    // The other direction is safe: billing_reader inherits anon, anon doesn't inherit it.
    let g = edges(&[
        ("billing_reader", "anon"),
        ("billing_reader", "authenticated"),
    ]);
    assert_eq!(all_rows_exposure(&g, "billing_reader", &none), None);
    // Unrelated roles, and a cycle that never reaches anon, terminate as safe.
    let g = edges(&[("a", "b"), ("b", "a"), ("anon", "c")]);
    assert_eq!(all_rows_exposure(&g, "a", &none), None);
    assert_eq!(
        all_rows_exposure(&BTreeMap::new(), "billing_reader", &none),
        None
    );
}

/// MH-26: a role bound to a live secret key counts as the data API.
#[test]
fn exposure_finds_a_key_role() {
    let keys = set(&["svc_key"]);
    let g = edges(&[("svc_key", "ops"), ("ops", "billing_reader")]);
    assert_eq!(
        chain_set(all_rows_exposure(&g, "billing_reader", &keys)),
        Some(set(&["svc_key", "ops", "billing_reader"]))
    );
    assert!(
        all_rows_exposure(&BTreeMap::new(), "svc_key", &keys).is_some(),
        "the key role itself"
    );
    assert_eq!(
        all_rows_exposure(&g, "billing_reader", &BTreeSet::new()),
        None,
        "no key: safe"
    );
}

/// T4-8 (P31): the exposure check from a given starting set. With `{anon, authenticated}` a
/// role only `authenticator` inherits isn't flagged, while the all-rows check (its full
/// starting set) still refuses it.
#[test]
fn exposure_chain_starts_from_the_given_roles_only() {
    let p31 = set(&["anon", "authenticated"]);
    let g = edges(&[
        ("anon", "a1"),
        ("a1", "via_anon"),
        ("authenticator", "via_authenticator"),
        ("authenticator", "anon"),
        ("svc_key", "via_key"),
    ]);
    assert_eq!(
        exposure_chain(&g, "via_anon", &p31),
        Some(vec!["anon".to_string(), "a1".into(), "via_anon".into()]),
        "anon → a1 → via_anon, from the starting role to the role"
    );
    assert_eq!(
        exposure_chain(&g, "via_authenticator", &p31),
        None,
        "only authenticator inherits it: not P31's flag"
    );
    assert_eq!(
        all_rows_exposure(&g, "via_authenticator", &BTreeSet::new()),
        Some(vec![
            "authenticator".to_string(),
            "via_authenticator".into()
        ]),
        "the all-rows check still refuses it"
    );
    assert_eq!(
        exposure_chain(&g, "via_key", &p31),
        None,
        "a key role isn't in P31's starting set"
    );
    assert!(
        all_rows_exposure(&g, "via_key", &set(&["svc_key"])).is_some(),
        "the all-rows check with the key role"
    );
    assert_eq!(
        exposure_chain(&g, "authenticator", &p31),
        None,
        "authenticator inherits anon, not the other way"
    );
    for direct in ["anon", "authenticated"] {
        assert_eq!(
            exposure_chain(&BTreeMap::new(), direct, &p31),
            Some(vec![direct.to_string()]),
            "{direct} itself"
        );
    }
}

/// T4-8: the chain is one of the shortest, from whichever starting role is nearest; cycles
/// end; an empty starting set flags nothing; any starting set works, not just the data API's.
#[test]
fn exposure_chain_is_shortest_and_terminates() {
    let p31 = set(&["anon", "authenticated"]);
    let g = edges(&[
        ("anon", "a"),
        ("a", "b"),
        ("b", "target"),
        ("authenticated", "target"),
        ("x", "y"),
        ("y", "x"),
        ("anon", "x"),
    ]);
    assert_eq!(
        exposure_chain(&g, "target", &p31),
        Some(vec!["authenticated".to_string(), "target".into()]),
        "the shortest chain wins"
    );
    assert_eq!(
        exposure_chain(&g, "y", &p31),
        Some(vec!["anon".to_string(), "x".into(), "y".into()]),
        "through a cycle"
    );
    assert_eq!(exposure_chain(&g, "unrelated", &p31), None);
    let g2 = edges(&[("p", "q"), ("q", "p")]);
    assert_eq!(
        exposure_chain(&g2, "r", &set(&["p"])),
        None,
        "a cycle never reaching the role ends"
    );
    assert_eq!(
        exposure_chain(&g, "target", &BTreeSet::new()),
        None,
        "no starting role: nothing is exposed"
    );
    assert_eq!(
        exposure_chain(&g, "target", &set(&["b"])),
        Some(vec!["b".to_string(), "target".into()]),
        "any starting set"
    );
}

/// Spec Part 4: `<role>_all_rows`, at most 63 bytes, an 8-hex suffix on a clash, never a name
/// in `taken`, valid UTF-8 when the role's name is long and multibyte, deterministic.
#[test]
fn all_rows_policy_name_fits_63_bytes_and_avoids_clashes() {
    assert_eq!(
        all_rows_policy_name("readers", &BTreeSet::new()),
        "readers_all_rows"
    );
    let is_suffixed = |n: &str| {
        let (_, tail) = n.rsplit_once('_').unwrap_or(("", ""));
        tail.len() == 8
            && tail
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    };
    let taken = set(&["readers_all_rows"]);
    let clash = all_rows_policy_name("readers", &taken);
    assert!(
        !taken.contains(&clash) && clash.len() <= 63 && is_suffixed(&clash),
        "{clash}"
    );
    assert_eq!(
        all_rows_policy_name("readers", &taken),
        clash,
        "deterministic"
    );
    let more = set(&["readers_all_rows", &clash]);
    let again = all_rows_policy_name("readers", &more);
    assert!(!more.contains(&again) && again.len() <= 63, "{again}");

    for role in [
        "r".repeat(63),
        "é".repeat(31),
        format!("x{}", "é".repeat(31)),
        "中".repeat(21),
    ] {
        let n = all_rows_policy_name(&role, &BTreeSet::new());
        assert!(
            !n.is_empty() && n.len() <= 63,
            "{role}: {n} ({} bytes)",
            n.len()
        );
        let t = set(&[&n]);
        let c = all_rows_policy_name(&role, &t);
        assert!(
            c.len() <= 63 && !t.contains(&c) && is_suffixed(&c),
            "{role}: {c}"
        );
    }
}

// --- Final round, area B -------------------------------------------------------------------

fn grantable_grant(grantor: &str, grantee: &str, privilege: &str, grantable: bool) -> Grant {
    Grant {
        grantable,
        ..grant(grantor, grantee, privilege)
    }
}

fn app_t() -> PrivilegeTarget {
    PrivilegeTarget::Relation {
        schema: "app".into(),
        name: "t".into(),
    }
}

/// T4-2, as an invariant (the second column-coverage case): a table-wide revoke of a privilege
/// covers the table's own entry and every column entry of it, whatever the grant option asked.
/// A full revoke leaves no entry; `REVOKE GRANT OPTION FOR` leaves no grantable one (Postgres
/// takes the option off the column entries too). Exhaustive over the request (full or option
/// only) and what the re-read still shows on the table and on a column (absent, plain,
/// grantable); every entry left as a mismatch by its column, the others none.
#[test]
fn a_table_wide_revoke_covers_column_entries_for_both_revoke_kinds() {
    let states = [None, Some(false), Some(true)];
    for option_only in [false, true] {
        for own in states {
            for column in states {
                let ops = [Operation::Revoke {
                    table: table("app", "t"),
                    grant: TableGrant {
                        with_grant_option: option_only,
                        ..table_grant(vec![Privilege::Select], "readers")
                    },
                }];
                let mut t = object(
                    PrivilegeObjectKind::Table,
                    "app",
                    "t",
                    101,
                    own.map(|g| grantable_grant("owner", "readers", "SELECT", g))
                        .into_iter()
                        .collect(),
                );
                t.columns = vec![
                    ColumnPrivileges {
                        name: "body".into(),
                        grants: column
                            .map(|g| grantable_grant("owner", "readers", "SELECT", g))
                            .into_iter()
                            .collect(),
                    },
                    ColumnPrivileges {
                        name: "id".into(),
                        grants: vec![],
                    },
                ];
                let pages = [PrivilegesPage {
                    objects: vec![t],
                    ..page("readers")
                }];
                let left = |state: Option<bool>| match state {
                    None => false,
                    Some(grantable) => !option_only || grantable,
                };
                let mut want: Vec<PrivilegeMismatch> = Vec::new();
                for (state, col) in [(own, None), (column, Some("body"))] {
                    if left(state) {
                        want.push(PrivilegeMismatch {
                            object: app_t(),
                            privilege: "SELECT".into(),
                            grantee: "readers".into(),
                            still_granted_by: Some("owner".into()),
                            column: col.map(String::from),
                            wanted: false,
                        });
                    }
                }
                let got = verify_privileges(&ops, &pages).map_err(sorted);
                let want = if want.is_empty() {
                    Ok(())
                } else {
                    Err(sorted(want))
                };
                assert_eq!(
                    got, want,
                    "option_only={option_only} own={own:?} column={column:?}"
                );
            }
        }
    }
}

/// T4-3: verify covers `SetAllRows`. The re-read's object carries the hub's all-rows policy for
/// the page's role (`page.role`): on wants it there, off wants it gone, and the last op on a
/// (table, role) decides. A difference is a mismatch on the table with privilege `ALL_ROWS`,
/// the op's role as grantee, no grantor and no column; `wanted` says which way. Another role's
/// policy on the same table never counts. A table no page lists has no policy.
#[test]
fn verify_checks_the_all_rows_policy_for_the_ops_role() {
    let set_all = |enabled: bool| Operation::SetAllRows {
        table: table("app", "t"),
        role: "readers".into(),
        enabled,
    };
    let pages = |readers_policy: bool| {
        let with = |policy: bool| {
            let mut t = object(PrivilegeObjectKind::Table, "app", "t", 101, vec![]);
            t.rls_enabled = true;
            t.all_rows = policy.then(|| AllRows {
                policy: "p_all_rows".into(),
            });
            t
        };
        vec![
            PrivilegesPage {
                objects: vec![with(readers_policy)],
                ..page("readers")
            },
            PrivilegesPage {
                objects: vec![with(true)],
                ..page("other")
            },
        ]
    };
    let mismatch = |wanted: bool| {
        Err(vec![PrivilegeMismatch {
            object: app_t(),
            privilege: "ALL_ROWS".into(),
            grantee: "readers".into(),
            still_granted_by: None,
            column: None,
            wanted,
        }])
    };
    for (ops, present, want) in [
        (vec![set_all(true)], false, mismatch(true)),
        (vec![set_all(true)], true, Ok(())),
        (vec![set_all(false)], true, mismatch(false)),
        (vec![set_all(false)], false, Ok(())),
        (vec![set_all(true), set_all(false)], false, Ok(())),
        (vec![set_all(true), set_all(false)], true, mismatch(false)),
        (vec![set_all(false), set_all(true)], false, mismatch(true)),
    ] {
        assert_eq!(
            verify_privileges(&ops, &pages(present)),
            want,
            "{ops:?} with the policy {present}"
        );
    }
    assert_eq!(
        verify_privileges(&[set_all(true)], &[page("readers")]),
        mismatch(true),
        "a table no page lists"
    );
    assert_eq!(
        verify_privileges(&[set_all(false)], &[page("readers")]),
        Ok(())
    );
}
