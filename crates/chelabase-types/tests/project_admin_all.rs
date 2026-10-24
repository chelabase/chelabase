//! User ruling R17 (2026-10-14): a project Admin's grant is every role on their project
//! (`RoleSet::All`), still narrowed per Chelabase database by overrides. Only a tenant admin
//! escapes narrowing, and that is decided from the tenant-admin flag, not from the grant's shape.

use std::collections::BTreeSet;

use chelabase_types::access::{tenant_admin_effective, Grant, GrantSources, Narrowing, PgRole, PlatformRole, RoleSet};

fn only(names: &[&str]) -> RoleSet {
    RoleSet::Only(names.iter().map(|n| if *n == "owner" { PgRole::Owner } else { PgRole::Named((*n).to_string()) }).collect())
}

fn set(names: &[&str]) -> BTreeSet<PgRole> {
    names.iter().map(|n| PgRole::Named((*n).to_string())).collect()
}

fn admin_all() -> Grant {
    Grant { platform_role: PlatformRole::Admin, roles: RoleSet::all() }
}

/// The effective shape of a project Admin's grant under `db`.
fn effective(db: Option<Narrowing>) -> Grant {
    let e = GrantSources::new(Some(admin_all()), Vec::new(), db).effective().expect("a source");
    Grant { platform_role: e.platform_role, roles: e.roles }
}

#[test]
fn a_project_admin_with_every_role_keeps_it_without_an_override() {
    assert_eq!(effective(None), admin_all());
}

#[test]
fn a_roles_override_narrows_a_project_admins_every_role() {
    let roles = Narrowing { platform_role: None, roles: Some(set(&["sales"])) };
    assert_eq!(effective(Some(roles)), Grant { platform_role: PlatformRole::Admin, roles: only(&["owner", "sales"]) });
}

#[test]
fn a_level_override_below_admin_without_roles_leaves_no_roles() {
    let dev = Narrowing { platform_role: Some(PlatformRole::Developer), roles: None };
    assert_eq!(effective(Some(dev)), Grant { platform_role: PlatformRole::Developer, roles: only(&[]) });
    let both = Narrowing { platform_role: Some(PlatformRole::Developer), roles: Some(set(&["sales"])) };
    assert_eq!(effective(Some(both)), Grant { platform_role: PlatformRole::Developer, roles: only(&["sales"]) });
}

#[test]
fn the_tenant_admin_effective_is_still_admin_with_every_role() {
    let e = tenant_admin_effective();
    assert_eq!(Grant { platform_role: e.platform_role, roles: e.roles }, admin_all());
}
