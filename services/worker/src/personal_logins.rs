//! Personal logins on a branch (members hub step 1, T6; spec Part 6 "Convergence",
//! "Memberships", D2, MH-10, MH-16, MH-17, MH-23, S9): what the member-role reconciler
//! ([`crate::member_roles`]) wants of each login and what the catalog has. The planner
//! ([`crate::member_roles::plan_member_roles`]) and the executor are the member roles' own:
//! a personal login is a [`DesiredMemberRole`] of kind [`RoleKind::Personal`].
//!
//! - **Desired:** each record of the branch's database whose member qualifies, unrevoked,
//!   unexpired, with a whole SCRAM verifier and a name a personal login may take. Its
//!   memberships are the safe roles held at Developer or above
//!   (`Effective::roles_at_least(Developer)`; a tenant admin's from
//!   `tenant_admin_effective()`), never the owner or an owner-level role. On a protected
//!   branch a non-Admin's login carries the DDL guard's marker.
//! - **Actual:** the catalog's marked roles (the round-4 predicate), never one under a name a
//!   personal login can't take (the owner, a reserved name): those are never dropped.
//! - **Never adopted:** a record whose name an unmarked role holds is left alone (logged, the
//!   run fails, the record kept); its personal row, if any, is removed.
//! - **Member-filtered runs (S9):** only that member's records and the marked roles they name;
//!   an orphan is never dropped there.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use chelabase_metadata::members::MemberRecord;
#[cfg(test)]
use chelabase_metadata::members::MemberStatus;
use chelabase_metadata::personal_logins::PersonalLoginRecord;
use chelabase_metadata::{BranchRecord, DatabaseRecord, RoleOrigin};
use chelabase_neon::ComputeBackend;
use chelabase_types::BranchId;
use chelabase_types::access::{Grant, GrantSources, PlatformRole, tenant_admin_effective};
use chelabase_types::role_name::check_login_role_name;
use chrono::{DateTime, Utc};
use tracing::{error, warn};
use uuid::Uuid;

use crate::Worker;
use crate::member_roles::{
    ActualMemberRole, DesiredMemberRole, MemberCatalog, RoleKind, granted_roles,
};
use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};

/// The personal logins of one reconcile run.
#[derive(Debug, Default)]
pub(crate) struct PersonalPlan {
    /// The logins wanted on the branch.
    pub desired: Vec<DesiredMemberRole>,
    /// The marked roles the run may change or drop.
    pub actual: Vec<ActualMemberRole>,
    /// The endpoint's personal rows in the run's scope: name → verifier.
    pub rows: BTreeMap<String, Option<String>>,
    /// The personal rows in scope whose login isn't wanted: removed before any role step.
    pub stale_rows: Vec<String>,
    /// T6-M3: rows of another origin (not the owner's) under a wanted login's name, left from
    /// before the login (a role made in SQL, then dropped): removed before any role step, so
    /// the login's own row can take the name.
    pub legacy_rows: Vec<String>,
    /// What keeps the run from converging (a name held by another role, a broken verifier).
    pub failures: Vec<String>,
}

/// Whether a record is wanted on a branch at `now`: its member qualifies, it isn't revoked or
/// expired, and it has a verifier (pure).
pub fn record_wanted(record: &PersonalLoginRecord, now: DateTime<Utc>) -> bool {
    record.qualifies
        && !record.revoked
        && record.valid_until > now
        && record.scram_verifier.is_some()
}

/// The roles a personal login holds (pure; D1, MH-23): the safe roles of `effective`'s set held
/// at Developer or above, and whether it is locked on a protected branch (every non-Admin).
pub fn personal_memberships(
    effective: Option<&chelabase_types::access::Effective>,
    catalog: &MemberCatalog,
    protected: bool,
) -> (BTreeSet<String>, bool) {
    match effective {
        Some(e) => {
            let grant = Grant {
                platform_role: e.platform_role,
                roles: e.roles_at_least(PlatformRole::Developer),
            };
            let (roles, _) = granted_roles(&grant, catalog);
            (roles, protected && e.platform_role != PlatformRole::Admin)
        }
        // No source (never for a qualifying record): nothing to hold, and locked on a
        // protected branch (fail closed).
        None => (BTreeSet::new(), protected),
    }
}

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher>
    Worker<M, S, C, N, P>
{
    /// The personal logins of `branch` (only `member`'s, when given), from the database's
    /// records, the catalog and the endpoint's rows. `Err` when a read fails.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn personal_logins_wanted(
        &self,
        branch: BranchId,
        ep: &str,
        record: &BranchRecord,
        database: &DatabaseRecord,
        member: Option<Uuid>,
        catalog: &MemberCatalog,
        members: &[MemberRecord],
        grants: &HashMap<Uuid, GrantSources>,
    ) -> Result<PersonalPlan, String> {
        let records = self
            .metadata
            .personal_login_records(database.database_id.as_uuid())
            .await
            .map_err(|e| format!("reading the personal logins: {e}"))?;
        let (copies, _) = self
            .metadata
            .role_copies(ep)
            .await
            .map_err(|e| format!("reading the personal rows: {e}"))?;
        let now = Utc::now();
        let scoped: Vec<&PersonalLoginRecord> = records
            .iter()
            .filter(|r| member.is_none_or(|m| m == r.member_id))
            .collect();
        let names: BTreeSet<&str> = scoped.iter().map(|r| r.role_name.as_str()).collect();
        let in_scope = |name: &str| member.is_none() || names.contains(name);
        let mut plan = PersonalPlan::default();
        for r in scoped.iter().filter(|r| record_wanted(r, now)) {
            let name = r.role_name.as_str();
            if check_login_role_name(name).is_err() {
                error!(branch_id = %branch, role = name, "a personal login's record has a name no personal login may take; skipped");
                plan.failures.push(format!(
                    "the personal login {name} has a reserved or malformed name"
                ));
                continue;
            }
            let verifier = r.scram_verifier.as_deref().unwrap_or_default();
            if !chelabase_neon::scram::is_verifier(verifier) {
                error!(branch_id = %branch, role = name, "a personal login's record has no whole SCRAM verifier; skipped");
                plan.failures
                    .push(format!("the personal login {name} has no whole verifier"));
                continue;
            }
            if catalog.roles.contains(name) && !catalog.personal.contains_key(name) {
                warn!(branch_id = %branch, role = name, "a role that isn't a personal login holds a personal login's name; it is never adopted");
                plan.failures.push(format!(
                    "the name {name} is held by a role that isn't a personal login"
                ));
                continue;
            }
            let effective = members
                .iter()
                .find(|m| m.id == r.member_id && m.in_good_standing())
                .and_then(|m| {
                    if m.tenant_admin {
                        Some(tenant_admin_effective())
                    } else {
                        grants.get(&m.id).and_then(GrantSources::effective)
                    }
                });
            // T6-M1: the record says the member qualifies, but this run reads no source for it
            // (the reads aren't one snapshot): nothing to hold, so no login until they agree.
            if effective.is_none() {
                warn!(branch_id = %branch, role = name, "a qualifying personal login has no grant source in this run; skipped");
                continue;
            }
            let (roles, locked) =
                personal_memberships(effective.as_ref(), catalog, record.protected);
            plan.desired.push(DesiredMemberRole {
                member_id: r.member_id,
                role_name: name.to_string(),
                roles,
                verifier: verifier.to_string(),
                rotate: false,
                locked,
                kind: RoleKind::Personal {
                    valid_until: r.valid_until,
                },
            });
        }
        for a in catalog.personal.values().filter(|a| in_scope(&a.role_name)) {
            if check_login_role_name(&a.role_name).is_err() {
                warn!(branch_id = %branch, role = %a.role_name, "a marked role under a name no personal login may take is left alone");
                continue;
            }
            plan.actual.push(a.clone());
        }
        let wanted: BTreeSet<&str> = plan.desired.iter().map(|d| d.role_name.as_str()).collect();
        for c in copies
            .iter()
            .filter(|c| c.origin == RoleOrigin::Personal && in_scope(&c.name))
        {
            if !wanted.contains(c.name.as_str()) {
                plan.stale_rows.push(c.name.clone());
            }
            plan.rows.insert(c.name.clone(), c.scram_verifier.clone());
        }
        // T6-M3: the compute role under a wanted name is ours or absent (an unmarked one is
        // never adopted, above), so another origin's row there only routes a stale password.
        plan.legacy_rows = copies
            .iter()
            .filter(|c| {
                !matches!(c.origin, RoleOrigin::Personal | RoleOrigin::Owner)
                    && wanted.contains(c.name.as_str())
            })
            .map(|c| c.name.clone())
            .collect();
        Ok(plan)
    }
}

/// Edge tests beyond T6's RED set (the coder's).
#[cfg(test)]
mod tests {
    use super::*;
    use crate::OWNER_ROLE;
    use crate::fakes::{FakeLogin, FakeWorker, entries, position, ready_database, worker};
    use crate::member_roles::{ActualMembership, MemberRoleStep, plan_member_roles, render_step};
    use crate::resync::{
        KEY_ROLES_SQL, Observed, ObservedRole, parse_key_role_catalog, plan_key_roles, plan_resync,
    };
    use chelabase_metadata::members::GrantRow;
    use chelabase_metadata::{DataKeyRecord, KeyRoleFix, RoleAttributes, RoleCopy, RoleLevel};
    use chelabase_nats::MemberRolesReply;
    use chelabase_types::DatabaseId;
    use chelabase_types::data_key::DataKeyKind;
    use chelabase_types::table_api::SCHEMA_LOCKED_ROLE;

    const NEEDLE: &str = "chelabase: member roles";

    fn h(s: &str) -> String {
        hex::encode(s.as_bytes())
    }

    fn far() -> DateTime<Utc> {
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2099, 1, 2, 3, 4, 5).unwrap()
    }

    fn scram(password: &str) -> String {
        chelabase_neon::scram::verifier(password)
    }

    /// Catalog lines: plain groups (`@` LOGIN, `!` owner-level), the `app` database.
    fn groups(roles: &[&str]) -> String {
        let mut out = String::new();
        for r in roles {
            let (name, level) = r.strip_prefix('!').map_or((*r, "f"), |n| (n, "t"));
            let (name, login) = name.strip_prefix('@').map_or((name, "f"), |n| (n, "t"));
            out.push_str(&format!("r {} f {level} f {login}\n", h(name)));
        }
        out.push_str(&format!("d {} {}\n", h("app"), h(OWNER_ROLE)));
        out
    }

    /// A marked role's lines, its memberships ours and shaped.
    fn marked(name: &str, verifier: &str, until: DateTime<Utc>, memberships: &[&str]) -> String {
        let mut out = format!(
            "r {n} t f f t\nk {n}\np {n} {}\nu {n} {}\n",
            h(verifier),
            until.timestamp_micros(),
            n = h(name)
        );
        for m in memberships {
            out.push_str(&format!("m {} {} - t\n", h(name), h(m)));
        }
        out
    }

    /// A ready database's main branch, its endpoint, and an active Developer holding `billing`.
    async fn setup(w: &FakeWorker) -> (BranchId, String, DatabaseId, Uuid) {
        let id = ready_database(w).await;
        let main = w.metadata.main(id);
        let member = add_member(w, id);
        w.computes.sql.lock().unwrap().clear();
        (main.id, main.endpoint_id.unwrap(), id, member)
    }

    fn add_member(w: &FakeWorker, id: DatabaseId) -> Uuid {
        let db = w.metadata.database(id);
        let member = Uuid::new_v4();
        let mut state = w.metadata.state();
        state.members.push(MemberRecord {
            id: member,
            tenant_id: db.tenant_id,
            zitadel_user_id: format!("z-{member}"),
            email: None,
            display_name: "m".into(),
            tenant_admin: false,
            status: MemberStatus::Active,
            accepted_at: None,
            offboarded: false, suspended_at: None, suspended_by: None,
        });
        state.grants.insert(
            (member, db.project_id.as_uuid()),
            GrantRow {
                platform_role: PlatformRole::Developer,
                pg_roles: vec!["billing".into()],
            },
        );
        member
    }

    fn record(
        w: &FakeWorker,
        id: DatabaseId,
        member: Uuid,
        name: &str,
        verifier: Option<&str>,
        revoked: bool,
    ) {
        w.metadata
            .state()
            .personal_logins
            .push(PersonalLoginRecord {
                member_id: member,
                database_id: id.as_uuid(),
                role_name: name.into(),
                scram_verifier: verifier.map(str::to_string),
                valid_until: far(),
                revoked,
                qualifies: true,
            });
    }

    fn personal_row(w: &FakeWorker, ep: &str, name: &str, verifier: Option<&str>) {
        let mut state = w.metadata.state();
        let endpoint = state.endpoints.get_mut(ep).unwrap();
        endpoint.logins.insert(
            name.into(),
            FakeLogin::new(RoleOrigin::Personal, None, false),
        );
        endpoint.set_verifier(name, verifier);
    }

    /// Every script but the catalog read and the read-only checks, quotes removed.
    fn scripts(w: &FakeWorker) -> Vec<String> {
        w.computes
            .sql
            .lock()
            .unwrap()
            .iter()
            .map(|(_, _, s)| s.replace('"', ""))
            .filter(|s| {
                !s.contains(NEEDLE) && !s.starts_with(crate::schema_guard::GUARD_PRESENT_MARK)
            })
            .collect()
    }

    fn touches(w: &FakeWorker, role: &str) -> Vec<String> {
        scripts(w)
            .into_iter()
            .filter(|s| {
                s.contains(&format!(" {role} "))
                    || s.contains(&format!(" {role};"))
                    || s.contains(&format!("'{role}'"))
            })
            .collect()
    }

    // The checklist's "Postgres takes a non-verifier PASSWORD as a plaintext password": a record
    // whose verifier isn't a whole SCRAM verifier is never created (the run fails, the record
    // stays), and no row is written.
    #[tokio::test]
    async fn a_record_without_a_whole_verifier_is_never_created() {
        for verifier in [
            "SCRAM-SHA-256$4096:c2FsdA==$a:b",
            "md5000102030405060708090a0b0c0d0e0f",
            "hunter2",
        ] {
            let (w, journal) = worker();
            let (branch, ep, id, member) = setup(&w).await;
            record(&w, id, member, "alice", Some(verifier), false);
            w.computes
                .answer_at(&ep, NEEDLE, &groups(&["!postgres", "billing"]));
            journal.lock().unwrap().clear();
            let reply = w.reconcile_member_roles(branch, None).await;
            assert!(
                matches!(reply, MemberRolesReply::Failed { .. }),
                "{verifier}: {reply:?}"
            );
            assert!(
                touches(&w, "alice").is_empty(),
                "{verifier}: {:#?}",
                scripts(&w)
            );
            assert!(
                position(&journal, "metadata.upsert_personal_role_row").is_none(),
                "{:#?}",
                entries(&journal)
            );
        }
    }

    // A marked role under a name no personal login may take (the owner, a reserved name) is
    // never dropped, even with no record; a record under such a name is never created.
    #[tokio::test]
    async fn a_marked_owner_or_reserved_role_is_never_dropped_or_made() {
        let (w, _journal) = worker();
        let (branch, ep, id, member) = setup(&w).await;
        record(&w, id, member, "anon", Some(&scram("x")), false);
        w.computes.answer_at(
            &ep,
            NEEDLE,
            &format!(
                "{}{}{}",
                groups(&["billing"]),
                marked(OWNER_ROLE, &scram("o"), far(), &[]),
                marked("authenticator", &scram("a"), far(), &[])
            ),
        );
        w.reconcile_member_roles(branch, None).await;
        for role in [OWNER_ROLE, "authenticator", "anon"] {
            assert!(touches(&w, role).is_empty(), "{role}: {:#?}", scripts(&w));
        }
    }

    // A create that fails (an unmarked role appeared after the catalog read, say) runs nothing
    // else for the name: no grant, no row.
    #[tokio::test]
    async fn a_failed_create_runs_nothing_else_for_the_name() {
        let (w, journal) = worker();
        let (branch, ep, id, member) = setup(&w).await;
        record(&w, id, member, "alice", Some(&scram("pw")), false);
        w.computes
            .answer_at(&ep, NEEDLE, &groups(&["!postgres", "billing"]));
        w.computes.fail_when("CREATE ROLE alice ");
        w.computes.fail_when("CREATE ROLE \"alice\"");
        journal.lock().unwrap().clear();
        let reply = w.reconcile_member_roles(branch, None).await;
        assert!(
            matches!(reply, MemberRolesReply::Failed { .. }),
            "{reply:?}"
        );
        // The fake records only scripts that ran: the failed create isn't among them, and
        // nothing else for the name ran.
        assert!(touches(&w, "alice").is_empty(), "{:#?}", scripts(&w));
        assert!(
            position(&journal, "metadata.upsert_personal_role_row").is_none(),
            "{:#?}",
            entries(&journal)
        );
    }

    // MH-9, S9, S16: a revoked record's leftover row (its role already gone) is removed and the
    // cache told; a run for another member leaves it.
    #[tokio::test]
    async fn a_stale_row_is_removed_only_by_a_run_that_covers_it() {
        let (w, journal) = worker();
        let (branch, ep, id, member) = setup(&w).await;
        let other = add_member(&w, id);
        record(&w, id, other, "nina", None, true);
        personal_row(&w, &ep, "nina", None);
        w.computes
            .answer_at(&ep, NEEDLE, &groups(&["!postgres", "billing"]));
        journal.lock().unwrap().clear();
        w.reconcile_member_roles(branch, Some(member)).await;
        assert!(
            w.metadata.state().endpoints[&ep]
                .logins
                .contains_key("nina")
        );
        assert!(position(&journal, "metadata.delete_personal_role_row").is_none());
        w.reconcile_member_roles(branch, Some(other)).await;
        assert!(
            !w.metadata.state().endpoints[&ep]
                .logins
                .contains_key("nina")
        );
        let log = entries(&journal);
        let deleted = position(
            &journal,
            &format!("metadata.delete_personal_role_row {ep} nina"),
        )
        .unwrap_or_else(|| panic!("{log:#?}"));
        let told = log
            .iter()
            .position(|e| e == &format!("cache.password_updated {ep} nina"))
            .unwrap_or_else(|| panic!("{log:#?}"));
        assert!(deleted < told, "{log:#?}");
    }

    // The sweep's steady state: a converged login (role, memberships, expiry, row) runs no
    // script for it, writes no row and tells the cache nothing.
    #[tokio::test]
    async fn a_converged_login_writes_and_publishes_nothing() {
        let (w, journal) = worker();
        let (branch, ep, id, member) = setup(&w).await;
        let verifier = scram("pw");
        record(&w, id, member, "alice", Some(&verifier), false);
        personal_row(&w, &ep, "alice", Some(&verifier));
        w.computes.answer_at(
            &ep,
            NEEDLE,
            &format!(
                "{}{}",
                groups(&["!postgres", "billing"]),
                marked("alice", &verifier, far(), &["billing"])
            ),
        );
        journal.lock().unwrap().clear();
        let reply = w.reconcile_member_roles(branch, None).await;
        assert!(matches!(reply, MemberRolesReply::Done { .. }), "{reply:?}");
        assert!(touches(&w, "alice").is_empty(), "{:#?}", scripts(&w));
        let log = entries(&journal);
        assert!(
            !log.iter().any(|e| e.contains("personal_role_row")
                || (e.starts_with("cache.") && e.contains("alice"))),
            "{log:#?}"
        );
    }

    // A personal login whose attributes were changed (only a superuser can) gets them back
    // with its expiry, and its password set again (its sessions end); never `RESET ALL`, which
    // would take the marker off.
    #[test]
    fn wrong_attributes_are_set_back_and_sessions_end_without_reset_all() {
        let wanted = DesiredMemberRole {
            member_id: Uuid::nil(),
            role_name: "alice".into(),
            roles: BTreeSet::new(),
            verifier: "SCRAM-alice".into(),
            rotate: false,
            locked: false,
            kind: RoleKind::Personal { valid_until: far() },
        };
        let have = ActualMemberRole {
            role_name: "alice".into(),
            attributes_ok: false,
            settings: Vec::new(),
            verifier: Some("SCRAM-alice".into()),
            memberships: Vec::new(),
            valid_until: Some(far()),
        };
        let steps = plan_member_roles(&[wanted], &[have]);
        assert_eq!(
            steps,
            vec![
                MemberRoleStep::SetValidUntil {
                    role: "alice".into(),
                    valid_until: far()
                },
                MemberRoleStep::SetPassword {
                    role: "alice".into(),
                    verifier: "SCRAM-alice".into()
                },
            ]
        );
        let sql: String = steps.iter().map(render_step).collect::<Vec<_>>().join("\n");
        assert!(sql.contains("NOSUPERUSER LOGIN NOINHERIT"), "{sql}");
        assert!(sql.contains("pg_terminate_backend"), "{sql}");
        assert!(!sql.contains("RESET"), "{sql}");
    }

    // A member who became an Admin on a protected branch: the existing login loses the DDL
    // guard's marker, as a member role does.
    #[test]
    fn an_existing_login_of_a_member_who_became_admin_is_unlocked() {
        let wanted = DesiredMemberRole {
            member_id: Uuid::nil(),
            role_name: "alice".into(),
            roles: BTreeSet::new(),
            verifier: "SCRAM-alice".into(),
            rotate: false,
            locked: false,
            kind: RoleKind::Personal { valid_until: far() },
        };
        let have = ActualMemberRole {
            role_name: "alice".into(),
            attributes_ok: true,
            settings: Vec::new(),
            verifier: Some("SCRAM-alice".into()),
            memberships: vec![ActualMembership {
                role: SCHEMA_LOCKED_ROLE.into(),
                grantor: None,
                shaped: false,
            }],
            valid_until: Some(far()),
        };
        assert_eq!(
            plan_member_roles(&[wanted], &[have]),
            vec![MemberRoleStep::Unlock {
                role: "alice".into()
            }]
        );
    }

    fn copy(name: &str, origin: RoleOrigin) -> RoleCopy {
        RoleCopy {
            name: name.into(),
            origin,
            scram_verifier: Some(scram("x")),
            password_enc: (origin == RoleOrigin::Api).then(|| "sealed".to_string()),
            login: true,
            level: RoleLevel::Plain,
            api_owner_level: false,
            pending: false,
            attributes_pending: false,
            created_at: DateTime::<Utc>::UNIX_EPOCH,
            attributes: RoleAttributes::default(),
        }
    }

    // MH-10: a marked role is never `sql` or `api`: another origin's row of its name is
    // removed (the reconciler then writes the personal row), and nothing is repaired.
    #[test]
    fn the_resync_removes_another_origins_row_of_a_marked_role() {
        for origin in [RoleOrigin::Sql, RoleOrigin::Api] {
            let mut alice = ObservedRole::plain("alice");
            alice.password = Some(scram("x"));
            alice.marked = true;
            let observed = Observed {
                roles: vec![alice],
                databases: Vec::new(),
            };
            let (repairs, write) = plan_resync(OWNER_ROLE, &[copy("alice", origin)], &observed);
            assert!(repairs.is_empty(), "{origin:?}: {repairs:?}");
            assert!(
                write.upserts.iter().all(|u| u.name != "alice"),
                "{origin:?}: {write:?}"
            );
            assert_eq!(write.removals, ["alice"], "{origin:?}");
        }
    }

    // MH-12: the key-role read carries the marker, and a key whose role was renamed onto a
    // personal login's name is revoked, never followed.
    #[test]
    fn the_key_read_carries_the_marker_and_a_rename_onto_a_login_is_revoked() {
        assert!(
            KEY_ROLES_SQL.contains("'chelabase.managed_role='"),
            "{KEY_ROLES_SQL}"
        );
        let read = parse_key_role_catalog(&format!(
            "10 {} f t\n11 {} t\n12 {}\n",
            h("alice"),
            h("app"),
            h("old")
        ))
        .unwrap();
        assert_eq!(
            read,
            [
                (10, "alice".to_string(), false, true),
                (11, "app".to_string(), true, false),
                (12, "old".to_string(), false, false),
            ]
        );
        assert!(parse_key_role_catalog(&format!("10 {} f x\n", h("a"))).is_err());
        let now = "tl";
        let key = DataKeyRecord {
            role: "old".into(),
            role_oid: Some(10),
            role_timeline: Some(now.into()),
            ..sample_key()
        };
        let fixes = plan_key_roles(
            std::slice::from_ref(&key),
            &[(10, "alice".to_string())],
            now,
            &BTreeSet::from(["alice".to_string()]),
        );
        assert_eq!(
            fixes,
            [KeyRoleFix::Revoke {
                id: key.id,
                role: "old".into(),
                oid: Some(10)
            }]
        );
    }

    // MH-10: a resync that finds a personal row without its marked role (or a marked role
    // without its row) asks for the member-role reconcile, which converges it; one that finds
    // them matching doesn't.
    #[tokio::test(start_paused = true)]
    async fn a_resync_that_sees_personal_drift_asks_for_a_reconcile() {
        for (marked, row, asks) in [
            (false, true, true),
            (true, false, true),
            (true, true, false),
        ] {
            let (w, _journal) = worker();
            let id = ready_database(&w).await;
            let main = w.metadata.main(id);
            let ep = main.endpoint_id.clone().unwrap();
            if row {
                personal_row(&w, &ep, "alice", Some(&scram("pw")));
            }
            let db = w.metadata.state().db_rows[&main.id]
                .iter()
                .find(|r| r.main)
                .unwrap()
                .name
                .clone();
            let mut out = format!("d|{}||{}|f|||||||\n", h(&db), h(OWNER_ROLE));
            if marked {
                out.push_str(&crate::resync::role_line(&ObservedRole {
                    password: Some(scram("pw")),
                    ..ObservedRole::plain("alice")
                }));
                out.push_str(&format!("m|{}||||||||||\n", h("alice")));
            }
            w.computes.answer("FROM pg_authid", &out);
            let before = w.member_role_requests.sent().len();
            w.resync(main.id).await.unwrap();
            let asked = w.member_role_requests.sent().len() > before;
            assert_eq!(asked, asks, "marked {marked}, row {row}");
        }
    }

    fn sample_key() -> DataKeyRecord {
        DataKeyRecord {
            id: Uuid::new_v4(),
            branch_id: BranchId::from(Uuid::new_v4()),
            kind: DataKeyKind::Secret,
            role: String::new(),
            name: "k".into(),
            prefix: "p".into(),
            sealed: None,
            databases: None,
            created_at: DateTime::<Utc>::UNIX_EPOCH,
            revoked_at: None,
            last_used_at: None,
            role_oid: None,
            role_timeline: None,
            role_bypass_rls: false,
        }
    }
}
