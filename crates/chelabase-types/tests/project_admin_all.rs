//! User ruling R17 (2026-10-14): a project Admin's grant is every role on their project
//! (`RoleSet::All`), still narrowed per Chelabase database by overrides. Only a tenant admin
//! escapes narrowing, and that is decided from the tenant-admin flag, not from the grant's shape.

use std::collections::BTreeSet;

use chelabase_types::access::{effective, tenant_admin_grant, Grant, Narrowing, PgRole, PlatformRole, RoleSet};

fn only(names: &[&str]) -> RoleSet {
    RoleSet::Only(names.iter().map(|n| if *n == "owner" { PgRole::Owner } else { PgRole::Named((*n).to_string()) }).collect())
}

fn set(names: &[&str]) -> BTreeSet<PgRole> {
    names.iter().map(|n| PgRole::Named((*n).to_string())).collect()
}

fn admin_all() -> Grant {
    Grant { platform_role: PlatformRole::Admin, roles: RoleSet::All }
}

#[test]
fn a_project_admin_with_every_role_keeps_it_without_an_override() {
    assert_eq!(effective(&admin_all(), None), admin_all());
}

#[test]
fn a_roles_override_narrows_a_project_admins_every_role() {
    let roles = Narrowing { platform_role: None, roles: Some(set(&["sales"])) };
    assert_eq!(effective(&admin_all(), Some(&roles)), Grant { platform_role: PlatformRole::Admin, roles: only(&["owner", "sales"]) });
}

#[test]
fn a_level_override_below_admin_without_roles_leaves_no_roles() {
    let dev = Narrowing { platform_role: Some(PlatformRole::Developer), roles: None };
    assert_eq!(effective(&admin_all(), Some(&dev)), Grant { platform_role: PlatformRole::Developer, roles: only(&[]) });
    let both = Narrowing { platform_role: Some(PlatformRole::Developer), roles: Some(set(&["sales"])) };
    assert_eq!(effective(&admin_all(), Some(&both)), Grant { platform_role: PlatformRole::Developer, roles: only(&["sales"]) });
}

#[test]
fn the_tenant_admin_grant_is_still_admin_with_every_role() {
    assert_eq!(tenant_admin_grant(), admin_all());
}
