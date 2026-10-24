//! Members hub step 1, T2: `decide` on an [`Effective`] (per-role levels, D1/MH-1) and the
//! suspended member (P14).

use std::collections::BTreeSet;

use super::*;
use crate::mode::Mode;
use crate::mode_policy::ModePolicy;
use PlatformRole::{Admin, Developer, Viewer};

fn only(level: PlatformRole, names: &[&str]) -> Grant {
    Grant { platform_role: level, roles: RoleSet::Only(names.iter().map(|n| PgRole::parse(n)).collect()) }
}

fn effective_of(own: Grant, teams: Vec<Grant>) -> Effective {
    GrantSources::new(Some(own), teams, None).effective().expect("a source")
}

fn member() -> CallerFacts {
    CallerFacts { member: MemberFacts::Active { tenant_admin: false, has_any_grant: true }, in_operator_org: false }
}

fn forbidden(required: &str) -> Result<(), Deny> {
    Err(Deny::Forbidden { required: required.to_string() })
}

#[test]
fn decide_checks_the_requested_roles_own_level() {
    // Developer through the own grant (`y`); `x` comes only from a Viewer source.
    let e = effective_of(only(Developer, &["y"]), vec![only(Viewer, &["x"])]);
    assert_eq!(e.platform_role, Developer);
    assert_eq!(e.level_of("x"), Some(Viewer));
    assert_eq!(e.level_of("y"), Some(Developer));
    let policy = ModePolicy::new(Mode::SelfHosted);
    let caller = member();
    let act = |name: &str, access: Access| {
        decide(&policy, &caller, Some(&e), Some(false), Some(Requested::Role(name)), Some("postgres"), access)
    };
    // A Developer route as `x`: refused on `x`'s own level, not the member's.
    assert_eq!(act("x", Access::ActAs(Developer)), forbidden("role:x"));
    assert_eq!(act("x", Access::ActAsBranch { unprotected: Developer, protected: Admin }), forbidden("role:x"));
    // A Viewer route as `x` passes; `y` passes at Developer.
    assert_eq!(act("x", Access::ActAs(Viewer)), Ok(()));
    assert_eq!(act("y", Access::ActAs(Developer)), Ok(()));
    // The member's level still decides first (`forbidden("admin")`, not the role).
    assert_eq!(act("y", Access::ActAs(Admin)), forbidden("admin"));
    // A role no source names.
    assert_eq!(act("z", Access::ActAs(Viewer)), forbidden("role:z"));
    // Routes without act-as read the platform level.
    let project = |min| decide(&policy, &caller, Some(&e), Some(false), None, None, Access::Project(min));
    assert_eq!(project(Developer), Ok(()));
    assert_eq!(project(Admin), forbidden("admin"));
    // The owner still needs Admin.
    let owner = decide(&policy, &caller, Some(&e), Some(false), None, Some("postgres"), Access::ActAs(Viewer));
    assert_eq!(owner, forbidden("owner"));
}

#[test]
fn decide_checks_a_named_data_api_roles_own_level_under_every_role() {
    // MH-3 with MH-1: an Admin team gives every role; `anon` comes from the own Viewer grant.
    let e = effective_of(only(Viewer, &["anon"]), vec![Grant { platform_role: Admin, roles: RoleSet::All { plus: BTreeSet::new() } }]);
    assert_eq!(e.platform_role, Admin);
    let policy = ModePolicy::new(Mode::SelfHosted);
    let caller = member();
    let act = |name: &str, access: Access| {
        decide(&policy, &caller, Some(&e), Some(true), Some(Requested::Role(name)), Some("postgres"), access)
    };
    assert_eq!(act("anon", Access::ActAs(Viewer)), Ok(()));
    assert_eq!(act("anon", Access::ActAs(Developer)), forbidden("role:anon"));
    assert_eq!(act("authenticated", Access::ActAs(Viewer)), forbidden("role:authenticated"));
    assert_eq!(act("billing", Access::ActAs(Admin)), Ok(()));
    assert_eq!(act("billing", Access::ActAsBranch { unprotected: Developer, protected: Admin }), Ok(()));
    assert_eq!(act("authenticator", Access::ActAs(Viewer)), forbidden("role:authenticator"));
}

#[test]
fn a_tenant_admin_is_judged_with_the_tenant_admin_effective_whatever_is_passed() {
    let policy = ModePolicy::new(Mode::SelfHosted);
    let caller = CallerFacts { member: MemberFacts::Active { tenant_admin: true, has_any_grant: false }, in_operator_org: false };
    let narrowed = GrantSources::new(Some(only(Viewer, &["anon"])), vec![], Some(Narrowing { platform_role: Some(Viewer), roles: None }))
        .effective()
        .unwrap();
    for g in [None, Some(&narrowed)] {
        let act = |r: Option<Requested<'_>>, access| decide(&policy, &caller, g, Some(true), r, Some("postgres"), access);
        assert_eq!(act(None, Access::ActAs(Admin)), Ok(()));
        assert_eq!(act(Some(Requested::Role("billing")), Access::ActAs(Admin)), Ok(()));
        // Never narrowed, and R18 still holds: not the data API roles.
        assert_eq!(act(Some(Requested::Role("anon")), Access::ActAs(Viewer)), forbidden("role:anon"));
        assert_eq!(act(None, Access::Project(Admin)), Ok(()));
    }
}

/// Every access kind, as `decide_matrix_exhaustive` lists them.
fn every_access() -> Vec<Access> {
    let roles = [Viewer, Developer, Admin];
    let mut all = vec![Access::Public, Access::DataKey, Access::SignedIn, Access::Instance, Access::TenantMember, Access::TenantAdmin];
    all.extend(roles.map(Access::Project));
    all.extend(roles.map(Access::ActAs));
    for unprotected in roles {
        for protected in roles {
            all.push(Access::Branch { unprotected, protected });
            all.push(Access::ActAsBranch { unprotected, protected });
        }
    }
    all
}

#[test]
fn a_suspended_member_is_refused_member_suspended() {
    let admin = tenant_admin_effective();
    for mode in [Mode::SelfHosted, Mode::Platform] {
        let policy = ModePolicy::new(mode);
        for operator in [false, true] {
            let caller = CallerFacts { member: MemberFacts::Suspended, in_operator_org: operator };
            for access in every_access() {
                // A platform operator's `Instance` routes don't depend on tenant membership
                // (as for a removed operator); not pinned here.
                if access == Access::Instance && operator && mode == Mode::Platform {
                    continue;
                }
                for grant in [None, Some(&admin)] {
                    for requested in [None, Some(Requested::Role("billing"))] {
                        let got = decide(&policy, &caller, grant, Some(false), requested, Some("postgres"), access);
                        // T2-3 (fixed in T8), a deliberate change: `SignedIn` routes refuse a
                        // suspended member too (P14); `/v1/me`, which answers them with their
                        // status, doesn't go through `decide`.
                        let want = match access {
                            Access::Public | Access::DataKey => Ok(()),
                            _ => Err(Deny::Suspended),
                        };
                        assert_eq!(got, want, "{mode:?} operator={operator} {access:?} grant={} {requested:?}", grant.is_some());
                    }
                }
            }
        }
    }
}
