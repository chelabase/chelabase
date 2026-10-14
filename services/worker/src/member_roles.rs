//! Member roles in Postgres (members-and-roles design, part 3; rulings R16, R17): one hidden
//! login role per (member, branch), `cb_m_<member uuid hex32>`, that holds no rights of its own
//! and can only `SET ROLE` into the roles the member may act as. The reconciler makes a
//! branch's member roles exactly what the metadata says through `admin_sql` (`cloud_admin`,
//! forwarding off), so the DDL receiver never sees them and they have no `neon_roles` row (no
//! proxy login).
//!
//! - **Desired:** for each active member with an effective grant on the branch's project
//!   (project ∩ Chelabase-database override), the effective roles minus the owner, that exist
//!   on the branch and are safe (see below). A tenant admin's `All` is every safe, non-reserved
//!   role. An empty set means no member role at all.
//! - **Granted roles bring their chain (R21)**, following Postgres' convention (R22): a member
//!   role (a person's LOGIN role) is only ever granted NOLOGIN group roles, and `SET ROLE`
//!   reaches what those reach. What a role can become ([`reach`]) follows `SET`-option
//!   memberships only, through member roles too, never stopping there (R23). What makes a chain
//!   unsafe ([`unsafe_node`]) walks every membership, whatever its options (I2): an
//!   `INHERIT`-only or `ADMIN`-only edge still carries privileges or the power to regrant.
//! - **Safe ([`safe_roles`]):** a granted role is left out entirely, with what only it reaches,
//!   when its chain (itself included, every edge) holds any of:
//!   - an owner-level role (R16, R23): SUPERUSER, REPLICATION, CREATEROLE, BYPASSRLS or
//!     CREATEDB, or a member of the owner, `neon_superuser` or `cloud_admin` through any chain.
//!     Only Admins act as those, through the owner connection;
//!   - a role that holds ADMIN on any role (N2, R30): it manages roles;
//!   - a LOGIN role (R22): acting as it, a member could change its password and keep a login
//!     after removal;
//!   - a reserved role (R23): `pg_*`, `cb_m_*`, `chelabase_*`, `authenticator`, the owner and
//!     the platform's own (`anon` and `authenticated` may be in a chain and be granted).
//! - **Actual:** the catalog ([`member_catalog_sql`]), read live at each run.
//! - **Passwords:** a role with no row on this branch (inherited from the parent, or new), a
//!   row that won't open, or a catalog verifier that isn't the row's password (a reset, a
//!   restore, a password the member changed itself) gets a fresh password, and its sessions
//!   end. Passwords reach SQL only as SCRAM verifiers, on psql's stdin.
//! - **Protected branches (TE-A P5–P7):** every member role of a non-Admin holds the DDL
//!   guard's marker role (`chelabase_schema_locked`, granted with no option: it gives
//!   nothing), and [`crate::schema_guard::schema_guard_sql`] runs first in every database
//!   where [`crate::schema_guard::schema_guard_check_sql`] doesn't find it current. The marker
//!   is never a granted role: [`MemberRoleStep::Lock`] / [`MemberRoleStep::Unlock`] alone
//!   manage it, and `Lock` comes before any grant of the role (fail closed). The guard is
//!   rendered with the branch's settings (PB: `guard_logins` and its exemptions). An
//!   unprotected branch carries no guard: where
//!   [`crate::schema_guard::schema_guard_present_sql`] finds one,
//!   [`crate::schema_guard::schema_unguard_sql`] removes it.
//! - **Order (I1):** each role's revocations (and the end of its sessions) run first, as their
//!   own script, so a failing grant never skips a revoke; then creates, passwords and grants;
//!   then drops.
//!
//! Each run holds the endpoint's lock, so it never interleaves with another run or a role
//! write on the same compute. Enqueued runs are retried with backoff; the periodic sweep
//! ([`Worker::sweep_member_roles_once`]) is the backstop for lost publishes.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use chelabase_metadata::members::{GrantRow, MemberBranchRole, MemberStatus, OverrideRow};
use chelabase_metadata::{BranchKind, BranchStatus};
use chelabase_nats::{
    MemberRoleApplied, MemberRoleHandOverRequest, MemberRolesReconcileRequest, MemberRolesReply,
};
use chelabase_types::role_name::member_hand_over_sql;
use chelabase_neon::ComputeBackend;
use chelabase_schema::sql::{ident, literal};
use chelabase_secrets::{open_member_role_password, seal_member_role_password};
use chelabase_types::BranchId;
use chelabase_types::access::{
    Grant, Narrowing, PgRole, PlatformRole, RoleSet, act_as_reserved, data_api_role, effective,
    tenant_admin_grant,
};
use chelabase_types::role_name::MEMBER_ROLE_PREFIX;
use chelabase_types::table_api::SCHEMA_LOCKED_ROLE;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};
use crate::resync::{PendingResyncs, SYSTEM_DATABASE};
use crate::schema_guard::{
    GuardSettings, schema_guard_check_sql, schema_guard_present_sql, schema_guard_sql,
    schema_unguard_sql,
};
use crate::{OWNER_ROLE, Worker, credentials};

/// How long an enqueued reconcile waits before it runs: a burst of changes is one run.
pub const MEMBER_ROLES_ENQUEUE_DELAY: Duration = Duration::from_secs(1);

/// Hard cap on one synchronous reconcile's answer, below the orchestrator's request timeout
/// (15 s). The run itself goes on under the endpoint's lock (M3).
pub const MEMBER_ROLES_DEADLINE: Duration = Duration::from_secs(10);

/// How many times an enqueued reconcile runs before the sweep takes over, and the first wait
/// between runs (doubled each time).
pub const MEMBER_ROLES_TRIES: u32 = 4;
pub const MEMBER_ROLES_RETRY_DELAY: Duration = Duration::from_secs(2);

/// A member role's attributes: it logs in, and can do nothing else of its own.
pub const MEMBER_ROLE_ATTRIBUTES: &str =
    "LOGIN NOINHERIT NOCREATEDB NOCREATEROLE NOBYPASSRLS NOREPLICATION";

/// The roles whose reach makes a role owner-level (R16), besides the owner itself.
pub const OWNER_LEVEL_ROLES: [&str; 2] = ["neon_superuser", "cloud_admin"];

/// The member role of `member` on any branch (R17), shared with the orchestrator, which logs in
/// as it.
pub use chelabase_types::role_name::member_role_name;

/// A member role as the metadata wants it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredMemberRole {
    pub member_id: Uuid,
    pub role_name: String,
    /// The roles it may `SET ROLE` to (already safe; the plan drops the owner and reserved
    /// names defensively).
    pub roles: BTreeSet<String>,
    /// Its password's SCRAM verifier: what a create sets, and what a rotation sets.
    pub verifier: String,
    /// The existing role's password is replaced (and its sessions end).
    pub rotate: bool,
    /// It holds the DDL guard's marker role ([`SCHEMA_LOCKED_ROLE`], TE-A P7): the branch is
    /// protected and the member isn't an Admin on the project.
    pub locked: bool,
}

/// One `pg_auth_members` row of a member role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActualMembership {
    /// The role it may become.
    pub role: String,
    /// `None`: granted by the bootstrap superuser, as the reconciler grants. `Some`: another
    /// grantor (never the reconciler's), revoked as that grantor.
    pub grantor: Option<String>,
    /// `INHERIT FALSE, SET TRUE, ADMIN FALSE`.
    pub shaped: bool,
}

/// A member role as the catalog has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActualMemberRole {
    pub role_name: String,
    /// It has exactly [`MEMBER_ROLE_ATTRIBUTES`] (and isn't a superuser).
    pub attributes_ok: bool,
    /// Its per-role settings (`ALTER ROLE … SET`): `None` for every database, else the
    /// database's name.
    pub settings: Vec<Option<String>>,
    /// `pg_authid.rolpassword`.
    pub verifier: Option<String>,
    pub memberships: Vec<ActualMembership>,
}

/// One change to a branch's member roles, in [`plan_member_roles`]'s order: revokes; creates,
/// password and attribute fixes; grants; drops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemberRoleStep {
    Revoke {
        role: String,
        granted: String,
        grantor: Option<String>,
    },
    Create {
        role: String,
        verifier: String,
    },
    /// A rotation: the new password, then the role's sessions end.
    SetPassword {
        role: String,
        verifier: String,
    },
    /// Back to [`MEMBER_ROLE_ATTRIBUTES`], its settings reset (everywhere, and in `databases`).
    Reshape {
        role: String,
        databases: Vec<String>,
    },
    Grant {
        role: String,
        granted: String,
    },
    /// Rendered as the role's lock-out (NOLOGIN, sessions ended); the executor then runs
    /// [`drop_owned_sql`] in every database of the branch and [`drop_role_sql`].
    Drop {
        role: String,
    },
    /// The DDL guard's marker ([`SCHEMA_LOCKED_ROLE`], P7), created on first use and granted
    /// with no option: it gives nothing, the guard's event triggers only look for it. Planned
    /// straight after `Create` (first, for an existing role), before any `Grant`.
    Lock {
        role: String,
    },
    /// The marker revoked. Runs with the revokes.
    Unlock {
        role: String,
    },
}

impl MemberRoleStep {
    /// The member role the step changes.
    pub fn role(&self) -> &str {
        match self {
            Self::Revoke { role, .. }
            | Self::Create { role, .. }
            | Self::SetPassword { role, .. }
            | Self::Reshape { role, .. }
            | Self::Grant { role, .. }
            | Self::Drop { role }
            | Self::Lock { role }
            | Self::Unlock { role } => role,
        }
    }
}

/// A name a member role may ever be granted: not the owner, nor any reserved name or prefix
/// (`cb_m_`, `chelabase_`, `pg_`, `postgres`, …; `anon` and `authenticated` are allowed). The
/// DDL guard's marker is reserved too: only [`MemberRoleStep::Lock`] grants it.
pub fn grantable(role: &str) -> bool {
    role != OWNER_ROLE && !act_as_reserved(role)
}

/// The reconciler's own marker membership (no other grantor): [`plan_member_roles`] keeps or
/// removes it with [`MemberRoleStep::Lock`] / [`MemberRoleStep::Unlock`], never as a granted
/// role.
fn is_marker(m: &ActualMembership) -> bool {
    m.role == SCHEMA_LOCKED_ROLE && m.grantor.is_none()
}

/// The steps that make `actual` equal `desired` (pure). A converged pair plans nothing.
pub fn plan_member_roles(
    desired: &[DesiredMemberRole],
    actual: &[ActualMemberRole],
) -> Vec<MemberRoleStep> {
    let mut revokes = Vec::new();
    let mut creates = Vec::new();
    let mut grants = Vec::new();
    let mut drops = Vec::new();
    let mut desired: Vec<&DesiredMemberRole> = desired.iter().collect();
    desired.sort_by(|a, b| a.role_name.cmp(&b.role_name));
    for d in &desired {
        let wanted: BTreeSet<&String> = d.roles.iter().filter(|r| grantable(r)).collect();
        let role = d.role_name.clone();
        let have = actual.iter().find(|a| a.role_name == d.role_name);
        let marked = have.is_some_and(|a| a.memberships.iter().any(is_marker));
        // Fail closed (T7 review): the marker comes straight after the role exists (first, for
        // an existing role), so a later step that fails can't leave grants without it.
        let lock = (d.locked && !marked).then(|| MemberRoleStep::Lock { role: role.clone() });
        match have {
            None => {
                creates.push(MemberRoleStep::Create {
                    role: role.clone(),
                    verifier: d.verifier.clone(),
                });
                creates.extend(lock);
            }
            Some(a) => {
                creates.extend(lock);
                if d.rotate {
                    creates.push(MemberRoleStep::SetPassword {
                        role: role.clone(),
                        verifier: d.verifier.clone(),
                    });
                }
                if !a.attributes_ok || !a.settings.is_empty() {
                    let mut databases: Vec<String> = a.settings.iter().flatten().cloned().collect();
                    databases.sort();
                    databases.dedup();
                    creates.push(MemberRoleStep::Reshape {
                        role: role.clone(),
                        databases,
                    });
                }
            }
        }
        let mut memberships: Vec<&ActualMembership> = have
            .map(|a| a.memberships.iter().collect())
            .unwrap_or_default();
        memberships.sort_by(|a, b| (&a.role, &a.grantor).cmp(&(&b.role, &b.grantor)));
        if marked && !d.locked {
            revokes.push(MemberRoleStep::Unlock { role: role.clone() });
        }
        for m in memberships.iter().filter(|m| !is_marker(m)) {
            // Another grantor's membership never stays; one of ours stays while it's wanted
            // (a re-grant fixes its options).
            if m.grantor.is_some() || !wanted.contains(&m.role) {
                revokes.push(MemberRoleStep::Revoke {
                    role: role.clone(),
                    granted: m.role.clone(),
                    grantor: m.grantor.clone(),
                });
            }
        }
        for granted in &wanted {
            let held = memberships
                .iter()
                .any(|m| &m.role == *granted && m.grantor.is_none() && m.shaped);
            if !held {
                grants.push(MemberRoleStep::Grant {
                    role: role.clone(),
                    granted: (*granted).clone(),
                });
            }
        }
    }
    let mut gone: Vec<&ActualMemberRole> = actual
        .iter()
        .filter(|a| !desired.iter().any(|d| d.role_name == a.role_name))
        .collect();
    gone.sort_by(|a, b| a.role_name.cmp(&b.role_name));
    drops.extend(gone.into_iter().map(|a| MemberRoleStep::Drop {
        role: a.role_name.clone(),
    }));
    revokes
        .into_iter()
        .chain(creates)
        .chain(grants)
        .chain(drops)
        .collect()
}

/// The SQL of one step (pure). Names are quoted with [`ident`], strings with [`literal`]; a
/// password appears only as its SCRAM verifier.
pub fn render_step(step: &MemberRoleStep) -> String {
    match step {
        MemberRoleStep::Revoke {
            role,
            granted,
            grantor,
        } => {
            let revoke = match grantor {
                None => format!("REVOKE {} FROM {};", ident(granted), ident(role)),
                Some(g) => format!(
                    "REVOKE {} FROM {} GRANTED BY {} CASCADE;",
                    ident(granted),
                    ident(role),
                    ident(g)
                ),
            };
            format!("{revoke}\n{}", terminate_sql(role))
        }
        MemberRoleStep::Create { role, verifier } => format!(
            "CREATE ROLE {} {MEMBER_ROLE_ATTRIBUTES} PASSWORD {};",
            ident(role),
            literal(verifier)
        ),
        MemberRoleStep::SetPassword { role, verifier } => format!(
            "ALTER ROLE {} PASSWORD {};\n{}",
            ident(role),
            literal(verifier),
            terminate_sql(role)
        ),
        MemberRoleStep::Reshape { role, databases } => {
            let mut sql = format!(
                "ALTER ROLE {r} NOSUPERUSER {MEMBER_ROLE_ATTRIBUTES};\nALTER ROLE {r} RESET ALL;",
                r = ident(role)
            );
            for db in databases {
                sql.push_str(&format!(
                    "\nALTER ROLE {} IN DATABASE {} RESET ALL;",
                    ident(role),
                    ident(db)
                ));
            }
            sql
        }
        MemberRoleStep::Grant { role, granted } => format!(
            "GRANT {} TO {} WITH INHERIT FALSE, SET TRUE, ADMIN FALSE;",
            ident(granted),
            ident(role)
        ),
        MemberRoleStep::Drop { role } => {
            format!(
                "ALTER ROLE {} NOLOGIN;\n{}",
                ident(role),
                terminate_sql(role)
            )
        }
        MemberRoleStep::Lock { role } => format!(
            "DO $$BEGIN IF NOT EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname OPERATOR(pg_catalog.=) {marker_literal}) THEN CREATE ROLE {marker} NOLOGIN; END IF; END$$;\nGRANT {marker} TO {} WITH INHERIT FALSE, SET FALSE, ADMIN FALSE;",
            ident(role),
            marker = ident(SCHEMA_LOCKED_ROLE),
            marker_literal = literal(SCHEMA_LOCKED_ROLE),
        ),
        MemberRoleStep::Unlock { role } => format!(
            "REVOKE {} FROM {};",
            ident(SCHEMA_LOCKED_ROLE),
            ident(role)
        ),
    }
}

/// Ends every session of `role`, waiting up to 5 s for each (M2).
pub fn terminate_sql(role: &str) -> String {
    format!(
        "SELECT pg_terminate_backend(pid, 5000) FROM pg_stat_activity WHERE usename = {};",
        literal(role)
    )
}

/// Marks a member role's per-database cleanup script (R31).
pub const CLEANUP_MARK: &str = "-- chelabase: member role cleanup";

/// R31, I-B, per (member role, database): one DO
/// ([`chelabase_types::role_name::member_cleanup_sql`]) that hands what `role` still owns to
/// `reassign_to` (from [`reassign_target`]) while `role` can still `SET ROLE` to it, then
/// `DROP OWNED` removes the rest and every privilege, default privilege and policy naming it.
/// Never `REASSIGN … TO` the database's owner: a member role's objects are code the member
/// wrote (a `SECURITY DEFINER` function, a view), and the owner would run it with the owner's
/// rights.
pub fn cleanup_script(role: &str, reassign_to: Option<&str>) -> String {
    format!(
        "{CLEANUP_MARK}\n{}",
        chelabase_types::role_name::member_cleanup_sql(role, reassign_to)
    )
}

/// Marks the member role password read after a hand-over (M-e).
pub const PASSWORD_MARK: &str = "-- chelabase: member role password";

/// The member role's `rolpassword` (M-e: a run may have changed it).
pub fn member_password_sql(role: &str) -> String {
    format!(
        "{PASSWORD_MARK}\nSELECT coalesce(rolpassword, '') FROM pg_catalog.pg_authid WHERE rolname = {};",
        literal(role)
    )
}

/// R31 (pure): the role a member role's leftovers go to: `recorded` (the role the member last
/// acted as), only when it exists on the branch, the member role could `SET ROLE` to it per
/// `catalog` (read before this reconcile's revokes: its own `SET` memberships and their
/// chains), and neither it nor its chain holds an owner-level, ADMIN-holding, LOGIN or
/// reserved role ([`unsafe_node`], the same rule [`safe_roles`] applies; the orchestrator's
/// equivalent is its role graph's refusal). Anything else: `None`, and the leftovers are
/// dropped.
pub fn reassign_target(
    catalog: &MemberCatalog,
    member_role: &str,
    recorded: Option<&str>,
) -> Option<String> {
    let recorded = recorded?;
    let member = catalog.member_roles.get(member_role)?;
    if !catalog.roles.contains(recorded) {
        return None;
    }
    let could = member
        .memberships
        .iter()
        .filter(|m| m.shaped)
        .any(|m| reach(&catalog.set_edges, &m.role).contains(recorded));
    (could && !unsafe_node(catalog, recorded)).then(|| recorded.to_string())
}

pub fn drop_role_sql(role: &str) -> String {
    format!("DROP ROLE IF EXISTS {};", ident(role))
}

/// Every role with its owner-level flag (R16), the member roles with their attributes,
/// settings, password and memberships, every other role's `SET` memberships, the
/// connectable databases with their owners, and for the protected-branch guard (PB R12, R13)
/// the superusers, every ADMIN membership and `template1` (when it takes connections). The comment is the fakes' needle. Role grants made
/// by a superuser are recorded as made by the bootstrap superuser (OID 10): those are the
/// reconciler's own.
pub fn member_catalog_sql() -> String {
    let h = |x: &str| format!("encode(convert_to({x}, 'UTF8'), 'hex')");
    let owner_level: Vec<String> = std::iter::once(OWNER_ROLE)
        .chain(OWNER_LEVEL_ROLES)
        .map(literal)
        .collect();
    format!(
        "SET search_path = pg_catalog, pg_temp;\n\
SELECT /* chelabase: member roles */ 'r ' || {rname} || ' ' || \
CASE WHEN r.rolcanlogin AND NOT r.rolsuper AND NOT r.rolinherit AND NOT r.rolcreatedb \
AND NOT r.rolcreaterole AND NOT r.rolbypassrls AND NOT r.rolreplication THEN 't' ELSE 'f' END || ' ' || \
CASE WHEN r.rolsuper OR r.rolreplication OR r.rolcreaterole OR r.rolbypassrls OR r.rolcreatedb OR EXISTS \
(SELECT 1 FROM pg_roles x WHERE x.rolname IN ({owner_level}) AND pg_has_role(r.oid, x.oid, 'MEMBER')) \
THEN 't' ELSE 'f' END || ' ' || \
CASE WHEN EXISTS (SELECT 1 FROM pg_auth_members a WHERE a.admin_option AND a.member = r.oid) \
THEN 't' ELSE 'f' END || ' ' || \
CASE WHEN r.rolcanlogin THEN 't' ELSE 'f' END FROM pg_roles r \
UNION ALL \
SELECT 'm ' || {mname} || ' ' || {gname} || ' ' || \
CASE WHEN a.grantor = 10 THEN '-' ELSE {grname} END || ' ' || \
CASE WHEN a.set_option AND NOT a.inherit_option AND NOT a.admin_option THEN 't' ELSE 'f' END \
FROM pg_auth_members a JOIN pg_roles m ON m.oid = a.member JOIN pg_roles g ON g.oid = a.roleid \
JOIN pg_roles gr ON gr.oid = a.grantor WHERE starts_with(m.rolname::text, 'cb_m_') \
UNION ALL \
SELECT 'e ' || {mname} || ' ' || {gname} \
FROM pg_auth_members a JOIN pg_roles m ON m.oid = a.member JOIN pg_roles g ON g.oid = a.roleid \
WHERE a.set_option \
UNION ALL \
SELECT 'a ' || {mname} || ' ' || {gname} \
FROM pg_auth_members a JOIN pg_roles m ON m.oid = a.member JOIN pg_roles g ON g.oid = a.roleid \
UNION ALL \
SELECT 's ' || {rname} || ' ' || CASE WHEN s.setdatabase = 0 THEN '-' ELSE coalesce({dname}, '-') END \
FROM pg_db_role_setting s JOIN pg_roles r ON r.oid = s.setrole \
LEFT JOIN pg_database d ON d.oid = s.setdatabase WHERE starts_with(r.rolname::text, 'cb_m_') \
UNION ALL \
SELECT 'p ' || {aname} || ' ' || coalesce({apass}, '-') FROM pg_authid a \
WHERE starts_with(a.rolname::text, 'cb_m_') \
UNION ALL \
SELECT 'd ' || {dname} || ' ' || {downer} FROM pg_database d \
WHERE d.datallowconn AND NOT d.datistemplate \
UNION ALL \
SELECT DISTINCT 'l ' || {rname} || ' ' || CASE WHEN s.dbid = 0 THEN '-' ELSE coalesce({dname}, '-') END \
FROM pg_shdepend s JOIN pg_roles r ON r.oid = s.refobjid LEFT JOIN pg_database d ON d.oid = s.dbid \
WHERE s.refclassid = 'pg_authid'::regclass AND starts_with(r.rolname::text, 'cb_m_') \
UNION ALL \
SELECT 'S ' || {rname} FROM pg_roles r WHERE r.rolsuper \
UNION ALL \
SELECT 'A ' || {mname} || ' ' || {gname} \
FROM pg_auth_members a JOIN pg_roles m ON m.oid = a.member JOIN pg_roles g ON g.oid = a.roleid \
WHERE a.admin_option \
UNION ALL \
SELECT 'g ' || {dname} FROM pg_database d WHERE d.datname = 'template1' AND d.datallowconn;",
        rname = h("r.rolname"),
        mname = h("m.rolname"),
        gname = h("g.rolname"),
        grname = h("gr.rolname"),
        dname = h("d.datname"),
        aname = h("a.rolname"),
        apass = h("a.rolpassword"),
        downer = h("pg_get_userbyid(d.datdba)"),
        owner_level = owner_level.join(", "),
    )
}

/// What [`member_catalog_sql`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemberCatalog {
    /// Every role on the branch.
    pub roles: BTreeSet<String>,
    /// The owner-level roles (R16; R23: CREATEDB too).
    pub owner_level: BTreeSet<String>,
    /// The roles that hold ADMIN on some role (N2, R30), themselves: [`unsafe_node`] reaches
    /// the ones above through [`Self::all_edges`].
    pub admin: BTreeSet<String>,
    /// The LOGIN roles (R22).
    pub login: BTreeSet<String>,
    /// `member → roles it may SET ROLE to directly` (memberships with the SET option), for
    /// every role: what [`reach`] follows.
    pub set_edges: BTreeMap<String, BTreeSet<String>>,
    /// `member → roles it is a direct member of`, whatever the options: what [`unsafe_node`]
    /// walks. Holds [`Self::set_edges`].
    pub all_edges: BTreeMap<String, BTreeSet<String>>,
    /// The member roles (`cb_m_…`), by name.
    pub member_roles: BTreeMap<String, ActualMemberRole>,
    /// The connectable databases and their owners, by name.
    pub databases: Vec<(String, String)>,
    /// I-A: where each member role still owns or holds something (`pg_shdepend`): a database
    /// by name, or `None` for shared objects (any database cleans those up).
    pub leftovers: BTreeMap<String, BTreeSet<Option<String>>>,
    /// The superusers (PB R13: never guarded).
    pub superusers: BTreeSet<String>,
    /// `member → roles it holds ADMIN on` (PB R13).
    pub admin_edges: BTreeMap<String, BTreeSet<String>>,
    /// Databases that get the protected-branch guard but no member-role work: `template1`, so
    /// a `CREATE DATABASE` copies the guard (PB R12).
    pub guard_only_databases: Vec<String>,
}

fn unhex(field: &str) -> Result<String, String> {
    hex::decode(field)
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .ok_or_else(|| format!("an unexpected name {field:?} in the member-role catalog"))
}

fn flag(field: Option<&str>) -> Result<bool, String> {
    match field {
        Some("t") => Ok(true),
        Some("f") => Ok(false),
        other => Err(format!(
            "an unexpected flag {other:?} in the member-role catalog"
        )),
    }
}

/// `-` or a hex name.
fn optional(field: Option<&str>) -> Result<Option<String>, String> {
    match field {
        Some("-") => Ok(None),
        Some(f) => unhex(f).map(Some),
        None => Err("a member-role catalog line is missing a field".into()),
    }
}

/// [`member_catalog_sql`]'s answer.
pub fn parse_member_catalog(out: &str) -> Result<MemberCatalog, String> {
    let mut catalog = MemberCatalog::default();
    let mut memberships: Vec<(String, ActualMembership)> = Vec::new();
    let mut settings: Vec<(String, Option<String>)> = Vec::new();
    let mut passwords: Vec<(String, Option<String>)> = Vec::new();
    let field = |f: Option<&str>| {
        f.map(unhex)
            .unwrap_or_else(|| Err("a missing field".into()))
    };
    for line in out.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let mut fields = line.split(' ');
        match fields.next() {
            Some("r") => {
                let name = field(fields.next())?;
                let attributes_ok = flag(fields.next())?;
                if flag(fields.next())? {
                    catalog.owner_level.insert(name.clone());
                }
                if flag(fields.next())? {
                    catalog.admin.insert(name.clone());
                }
                if flag(fields.next())? {
                    catalog.login.insert(name.clone());
                }
                if name.starts_with(MEMBER_ROLE_PREFIX) {
                    catalog.member_roles.insert(
                        name.clone(),
                        ActualMemberRole {
                            role_name: name.clone(),
                            attributes_ok,
                            settings: Vec::new(),
                            verifier: None,
                            memberships: Vec::new(),
                        },
                    );
                }
                catalog.roles.insert(name);
            }
            Some("m") => {
                let member = field(fields.next())?;
                let role = field(fields.next())?;
                let grantor = optional(fields.next())?;
                let shaped = flag(fields.next())?;
                memberships.push((
                    member,
                    ActualMembership {
                        role,
                        grantor,
                        shaped,
                    },
                ));
            }
            Some("e") => {
                let member = field(fields.next())?;
                let role = field(fields.next())?;
                catalog.set_edges.entry(member).or_default().insert(role);
            }
            Some("a") => {
                let member = field(fields.next())?;
                let role = field(fields.next())?;
                catalog.all_edges.entry(member).or_default().insert(role);
            }
            Some("s") => {
                let role = field(fields.next())?;
                settings.push((role, optional(fields.next())?));
            }
            Some("p") => {
                let role = field(fields.next())?;
                passwords.push((role, optional(fields.next())?));
            }
            Some("d") => {
                let name = field(fields.next())?;
                let owner = field(fields.next())?;
                catalog.databases.push((name, owner));
            }
            Some("l") => {
                let role = field(fields.next())?;
                let database = optional(fields.next())?;
                catalog.leftovers.entry(role).or_default().insert(database);
            }
            Some("S") => {
                catalog.superusers.insert(field(fields.next())?);
            }
            Some("A") => {
                let member = field(fields.next())?;
                let role = field(fields.next())?;
                catalog.admin_edges.entry(member).or_default().insert(role);
            }
            Some("g") => {
                catalog.guard_only_databases.push(field(fields.next())?);
            }
            _ => {
                return Err(format!(
                    "an unexpected line {line:?} in the member-role catalog"
                ));
            }
        }
        if fields.next().is_some() {
            return Err(format!(
                "a member-role catalog line with extra fields: {line:?}"
            ));
        }
    }
    for (member, membership) in memberships {
        if let Some(role) = catalog.member_roles.get_mut(&member) {
            role.memberships.push(membership);
        }
    }
    for (member, database) in settings {
        if let Some(role) = catalog.member_roles.get_mut(&member) {
            role.settings.push(database);
        }
    }
    for (member, verifier) in passwords {
        if let Some(role) = catalog.member_roles.get_mut(&member) {
            role.verifier = verifier;
        }
    }
    for role in catalog.member_roles.values_mut() {
        role.memberships
            .sort_by(|a, b| (&a.role, &a.grantor).cmp(&(&b.role, &b.grantor)));
        role.settings.sort();
    }
    catalog.databases.sort();
    catalog.guard_only_databases.sort();
    Ok(catalog)
}

/// PB R13 (pure): the exemptions the guard honors, and the names it doesn't. An exemption is
/// honored only for a role that exists and on which no guarded role holds ADMIN, directly or
/// through any chain of memberships (whatever their options: Postgres finds ADMIN through every
/// membership). Guarded here: every login role that isn't a superuser, the owner or a member
/// role (`cb_m_`, guarded by the marker; its granted roles hold no ADMIN), the exemptions
/// included, with every role it reaches. Otherwise a guarded role could take over an exempt
/// role (a password, `SET ROLE`) or make one under an exempt name.
pub fn honored_exemptions(
    catalog: &MemberCatalog,
    exempt: &[String],
) -> (BTreeSet<String>, Vec<String>) {
    let mut reached: BTreeSet<String> = BTreeSet::new();
    for login in catalog.login.iter().filter(|r| {
        !catalog.superusers.contains(*r)
            && r.as_str() != OWNER_ROLE
            && !r.starts_with(MEMBER_ROLE_PREFIX)
    }) {
        if !reached.contains(login) {
            reached.extend(reach(&catalog.all_edges, login));
        }
    }
    let administered: BTreeSet<&String> = reached
        .iter()
        .filter_map(|r| catalog.admin_edges.get(r))
        .flatten()
        .collect();
    let mut honored = BTreeSet::new();
    let mut skipped = Vec::new();
    for name in exempt {
        if catalog.roles.contains(name) && !administered.contains(name) {
            honored.insert(name.clone());
        } else {
            skipped.push(name.clone());
        }
    }
    (honored, skipped)
}

/// Every role reached from `from` through `edges`, `from` included. With a catalog's
/// `set_edges` that is what `SET ROLE` reaches.
pub fn reach(edges: &BTreeMap<String, BTreeSet<String>>, from: &str) -> BTreeSet<String> {
    let mut seen: BTreeSet<String> = BTreeSet::from([from.to_string()]);
    let mut queue: Vec<String> = vec![from.to_string()];
    while let Some(role) = queue.pop() {
        for next in edges.get(&role).into_iter().flatten() {
            if seen.insert(next.clone()) {
                queue.push(next.clone());
            }
        }
    }
    seen
}

/// A role no granted role's chain may hold, by itself: a reserved name (R23: `pg_*`, `cb_m_*`,
/// `chelabase_*`, `authenticator`, the owner, …; `anon` and `authenticated` may be), an
/// owner-level role (R16, R23), one that holds ADMIN on any role (N2, R30) or a LOGIN role
/// (R22).
fn bad_alone(catalog: &MemberCatalog, role: &str) -> bool {
    !grantable(role)
        || catalog.owner_level.contains(role)
        || catalog.admin.contains(role)
        || catalog.login.contains(role)
}

/// Whether `role` or any role it is a member of, through every edge whatever its options
/// (I2), is [`bad_alone`]: the worker's counterpart of the orchestrator's `RoleGraph::refusal`
/// (`services/orchestrator/tests/role_rules_agree.rs` holds them equal on random graphs).
pub fn unsafe_node(catalog: &MemberCatalog, role: &str) -> bool {
    reach(&catalog.all_edges, role)
        .iter()
        .any(|n| bad_alone(catalog, n))
}

/// The granted roles a member role holds (pure; R16, R21–R23, N2): those of `candidate` that
/// exist and whose chain (themselves included) holds no unsafe role ([`unsafe_node`]). The
/// chain follows every membership, whatever its options, through member roles too. Granting a role includes its chain: the member role is granted
/// only these NOLOGIN group roles, and Postgres' own `SET` chaining gives what they reach. A
/// role whose chain reaches an unsafe role is left out entirely, with what only it reaches.
pub fn safe_roles(candidate: &BTreeSet<String>, catalog: &MemberCatalog) -> BTreeSet<String> {
    candidate
        .iter()
        .filter(|r| catalog.roles.contains(*r))
        .filter(|r| !unsafe_node(catalog, r))
        .cloned()
        .collect()
}

/// Why a role named by a grant isn't given to the member role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Skipped {
    /// It doesn't exist on the branch.
    Missing,
    /// It, or a role in its chain, is owner-level, holds ADMIN, is LOGIN or is reserved (R16,
    /// R21-R23, N2).
    Unsafe,
}

/// The roles a member role gets for `grant` on a branch: the owner and reserved names never,
/// then [`safe_roles`]. An Admin's `All` (a project Admin's, R17, or a tenant admin's) starts
/// from every role on the branch except the data API's `anon` and `authenticated`, which a
/// member gets only when its grant names them (R18). Each named role left out is returned
/// with why.
pub fn granted_roles(
    grant: &Grant,
    catalog: &MemberCatalog,
) -> (BTreeSet<String>, Vec<(String, Skipped)>) {
    let named: BTreeSet<String> = match &grant.roles {
        RoleSet::All => catalog.roles.iter().filter(|r| !data_api_role(r)).cloned().collect(),
        RoleSet::Only(set) => set
            .iter()
            .filter_map(|r| match r {
                PgRole::Owner => None,
                PgRole::Named(name) => Some(name.clone()),
            })
            .collect(),
    };
    let candidate: BTreeSet<String> = named.iter().filter(|r| grantable(r)).cloned().collect();
    let roles = safe_roles(&candidate, catalog);
    let skipped = match &grant.roles {
        RoleSet::All => Vec::new(),
        RoleSet::Only(_) => candidate
            .iter()
            .filter(|r| !roles.contains(*r))
            .map(|r| {
                let why = if catalog.roles.contains(r) {
                    Skipped::Unsafe
                } else {
                    Skipped::Missing
                };
                (r.clone(), why)
            })
            .collect(),
    };
    (roles, skipped)
}

/// R17: an Admin grant is every role on the project.
fn grant_of(row: &GrantRow) -> Grant {
    row.to_grant()
}

fn narrowing_of(row: &OverrideRow) -> Narrowing {
    Narrowing {
        platform_role: row.platform_role,
        roles: row
            .pg_roles
            .as_ref()
            .map(|roles| roles.iter().map(|r| PgRole::parse(r)).collect()),
    }
}

/// Where member-role reconciles are asked for (`chelabase.member_roles.reconcile`). The
/// process connects it to NATS once; until then (and in tests) requests are only recorded. A
/// failed publish is logged: the periodic sweep is the backstop.
#[derive(Default)]
pub struct MemberRoleRequests {
    client: std::sync::OnceLock<async_nats::Client>,
    sent: std::sync::Mutex<Vec<BranchId>>,
}

impl MemberRoleRequests {
    pub fn connect(&self, client: async_nats::Client) {
        let _ = self.client.set(client);
    }

    /// Asks for a reconcile of `branch`, off the caller's path.
    pub fn request(&self, branch: BranchId) {
        self.sent
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(branch);
        let Some(client) = self.client.get().cloned() else {
            return;
        };
        tokio::spawn(async move {
            let body = match serde_json::to_vec(&MemberRolesReconcileRequest {
                branch_id: branch.as_uuid(),
                member_id: None,
            }) {
                Ok(body) => body,
                Err(e) => {
                    warn!(branch_id = %branch, "member-role reconcile request not encoded: {e}");
                    return;
                }
            };
            if let Err(e) = client
                .publish(chelabase_nats::SUBJECT_MEMBER_ROLES_ENQUEUE, body.into())
                .await
            {
                warn!(branch_id = %branch, "member-role reconcile request not published: {e}");
            }
        });
    }

    /// The branches asked for so far.
    pub fn sent(&self) -> Vec<BranchId> {
        self.sent.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// A desired role with its sealed password and whether its row needs writing once applied.
struct Wanted {
    desired: DesiredMemberRole,
    sealed: String,
    write_row: bool,
}

fn failed(message: impl Into<String>) -> MemberRolesReply {
    MemberRolesReply::Failed {
        message: message.into(),
    }
}

/// Decodes a reconcile request: the branch and the member, or the answer to give.
pub fn parse_member_roles_request(
    payload: &[u8],
) -> Result<(BranchId, Option<Uuid>), MemberRolesReply> {
    serde_json::from_slice::<MemberRolesReconcileRequest>(payload)
        .map(|r| (BranchId::from(r.branch_id), r.member_id))
        .map_err(|e| failed(format!("bad request: {e}")))
}

/// The synchronous reconcile (`chelabase.rpc.member_roles.reconcile`): the run is spawned, and
/// a run past `deadline` answers `Failed` while it goes on to its end under the endpoint's lock
/// (M3: no psql is left running after its lock is released).
pub async fn member_roles_reply<M, S, C, N, P>(
    worker: Arc<Worker<M, S, C, N, P>>,
    payload: &[u8],
    deadline: Duration,
) -> MemberRolesReply
where
    M: WorkerMetadata,
    S: Storage + Send + Sync + 'static,
    C: ComputeBackend + Send + Sync + 'static,
    N: CacheNotifier + Send + Sync + 'static,
    P: SpecPusher + Send + Sync + 'static,
{
    let (branch, member) = match parse_member_roles_request(payload) {
        Ok(request) => request,
        Err(reply) => return reply,
    };
    let run = tokio::spawn(async move { worker.reconcile_member_roles(branch, member).await });
    match tokio::time::timeout(deadline, run).await {
        Ok(Ok(reply)) => reply,
        Ok(Err(e)) => failed(format!("the reconcile stopped: {e}")),
        Err(_) => {
            warn!(branch_id = %branch, "member-role reconcile past its deadline; it goes on");
            failed("timed out; the reconcile goes on, and a retry is safe")
        }
    }
}

/// The hand-over RPC (`chelabase.rpc.member_roles.hand_over`), as [`member_roles_reply`]: the
/// hand-over is spawned, and one past `deadline` answers `Failed` while it goes on to its end
/// under the endpoint's lock (M-c: the lock is never released with its psql still running;
/// the SQL's own `statement_timeout` ends it).
pub async fn member_hand_over_reply<M, S, C, N, P>(
    worker: Arc<Worker<M, S, C, N, P>>,
    payload: &[u8],
    deadline: Duration,
) -> MemberRolesReply
where
    M: WorkerMetadata,
    S: Storage + Send + Sync + 'static,
    C: ComputeBackend + Send + Sync + 'static,
    N: CacheNotifier + Send + Sync + 'static,
    P: SpecPusher + Send + Sync + 'static,
{
    let request = match serde_json::from_slice::<MemberRoleHandOverRequest>(payload) {
        Ok(request) => request,
        Err(e) => return failed(format!("bad request: {e}")),
    };
    let branch = request.branch_id;
    let run = tokio::spawn(async move { worker.hand_over_member_role(&request).await });
    match tokio::time::timeout(deadline, run).await {
        Ok(Ok(reply)) => reply,
        Ok(Err(e)) => failed(format!("the hand-over stopped: {e}")),
        Err(_) => {
            warn!(branch_id = %branch, "member-role hand-over past its deadline; it goes on");
            failed("timed out; the hand-over goes on")
        }
    }
}

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher>
    Worker<M, S, C, N, P>
{
    /// Makes the branch's member roles (only `member`'s, when given) exactly what the metadata
    /// says, under the endpoint's lock. Any failure is `Failed` (with what failed); the roles
    /// that did apply keep their rows, the others are retried by the next run.
    pub async fn reconcile_member_roles(
        &self,
        branch: BranchId,
        member: Option<Uuid>,
    ) -> MemberRolesReply {
        let record = match self.metadata.get_branch(branch).await {
            Ok(Some(b)) => b,
            Ok(None) => return failed("the branch doesn't exist"),
            Err(e) => return failed(format!("reading the branch: {e}")),
        };
        let ep = match (&record.status, &record.kind, &record.endpoint_id) {
            (BranchStatus::Ready, BranchKind::Normal, Some(ep)) => ep.clone(),
            _ => return failed("the branch isn't ready"),
        };
        let database = match self.metadata.get_database(record.database_id).await {
            Ok(Some(d)) => d,
            Ok(None) => return failed("the database doesn't exist"),
            Err(e) => return failed(format!("reading the database: {e}")),
        };
        let _lock = match self.metadata.lock_endpoint(&ep).await {
            Ok(lock) => lock,
            Err(e) => return failed(format!("locking the endpoint: {e}")),
        };
        let catalog = match self
            .computes
            .admin_sql(&ep, SYSTEM_DATABASE, &member_catalog_sql())
            .await
            .map_err(|e| format!("{e:#}"))
            .and_then(|out| parse_member_catalog(&out))
        {
            Ok(catalog) => catalog,
            Err(e) => return failed(format!("reading the member roles: {e}")),
        };
        let members = match self.metadata.list_members(database.tenant_id).await {
            Ok(members) => members,
            Err(e) => return failed(format!("reading the members: {e}")),
        };
        let grants: HashMap<Uuid, chelabase_metadata::members::GrantRows> = match self
            .metadata
            .database_grant_rows(
                database.project_id.as_uuid(),
                database.database_id.as_uuid(),
            )
            .await
        {
            Ok(rows) => rows.into_iter().collect(),
            Err(e) => return failed(format!("reading the grants: {e}")),
        };
        let rows: HashMap<Uuid, MemberBranchRole> =
            match self.metadata.member_branch_roles(branch.as_uuid()).await {
                Ok(rows) => rows
                    .into_iter()
                    .filter(|r| member.is_none_or(|m| m == r.member_id))
                    .map(|r| (r.member_id, r))
                    .collect(),
                Err(e) => return failed(format!("reading the member roles' rows: {e}")),
            };
        // PB: a protected branch's guard settings (S3: installed whatever the switch), with
        // only the exemptions no guarded role could take over (R13).
        let guard_settings = if record.protected {
            match self.metadata.guard_exemptions(branch).await {
                Ok(exempt) => {
                    let (honored, skipped) = honored_exemptions(&catalog, &exempt);
                    for name in skipped {
                        warn!(branch_id = %branch, exemption = name, "protected branch: an exemption isn't honored (no such role, or a guarded role administers it)");
                    }
                    Some(GuardSettings {
                        guard_logins: record.guard_logins,
                        exempt: honored,
                    })
                }
                Err(e) => return failed(format!("reading the guard exemptions: {e}")),
            }
        } else {
            None
        };
        // R12: the guard also goes into `template1` (a `CREATE DATABASE` copies it), and
        // leaves it with protection; member-role work never goes there. R17: only a superuser
        // can change template1's guard, so it can't drift: it is touched (checked, then
        // installed or removed) only in a run that installs or removes the guard in another
        // database. Those come last (`true`: guard-only).
        let guard_databases: Vec<(&str, bool)> = catalog
            .databases
            .iter()
            .map(|(db, _)| (db.as_str(), false))
            .chain(catalog.guard_only_databases.iter().map(|db| (db.as_str(), true)))
            .collect();
        let mut guard_changed = false;

        // Desired, with passwords.
        let mut wanted: Vec<Wanted> = Vec::new();
        for m in members
            .iter()
            .filter(|m| m.status == MemberStatus::Active)
            .filter(|m| member.is_none_or(|id| id == m.id))
        {
            let grant = if m.tenant_admin {
                Some(tenant_admin_grant())
            } else {
                grants.get(&m.id).and_then(|levels| {
                    levels.project.as_ref().map(|project| {
                        effective(
                            &grant_of(project),
                            levels.database.as_ref().map(narrowing_of).as_ref(),
                        )
                    })
                })
            };
            let Some(grant) = grant else { continue };
            // P7: on a protected branch, everyone but an Admin (tenant admins are) is marked.
            let locked = record.protected && grant.platform_role != PlatformRole::Admin;
            let (roles, skipped) = granted_roles(&grant, &catalog);
            for (role, why) in skipped {
                debug!(branch_id = %branch, member_id = %m.id, role, ?why, "member roles: a granted role is left out");
            }
            if roles.is_empty() {
                continue;
            }
            let role_name = member_role_name(m.id);
            let current = catalog.member_roles.get(&role_name);
            let row = rows.get(&m.id).filter(|r| r.role_name == role_name);
            let kept = row.and_then(|r| {
                match open_member_role_password(&self.key, branch.as_uuid(), &role_name, &r.password_enc) {
                    Ok(password) => Some((password, r.password_enc.clone())),
                    Err(e) => {
                        warn!(branch_id = %branch, role = role_name, "a member role's sealed password won't open ({e}); a fresh one replaces it");
                        None
                    }
                }
            });
            let catalog_verifier = current.and_then(|c| c.verifier.clone());
            let (verifier, sealed, rotate, fresh) = match (kept, current) {
                // The row's password, and the catalog holds it: nothing changes.
                (Some((password, sealed)), Some(_))
                    if catalog_verifier
                        .as_deref()
                        .is_some_and(|v| chelabase_neon::scram::verifies(v, &password)) =>
                {
                    (catalog_verifier.unwrap_or_default(), sealed, false, false)
                }
                // The row's password, and the role is gone: made again with it.
                (Some((password, sealed)), None) => (
                    chelabase_neon::scram::verifier(&password),
                    sealed,
                    false,
                    false,
                ),
                // No row (new, or inherited from the parent), a row that won't open, or a
                // catalog password that isn't the row's (a reset, a restore, a self-change):
                // a fresh password.
                _ => {
                    let password = credentials::generate_password();
                    let sealed = match seal_member_role_password(
                        &self.key,
                        branch.as_uuid(),
                        &role_name,
                        &password,
                    ) {
                        Ok(sealed) => sealed,
                        Err(e) => return failed(format!("sealing a member role's password: {e}")),
                    };
                    (
                        chelabase_neon::scram::verifier(&password),
                        sealed,
                        current.is_some(),
                        true,
                    )
                }
            };
            let applied: Vec<String> = roles.iter().cloned().collect();
            let write_row = fresh || row.is_none_or(|r| r.applied_roles != applied);
            wanted.push(Wanted {
                desired: DesiredMemberRole {
                    member_id: m.id,
                    role_name,
                    roles,
                    verifier,
                    rotate,
                    locked,
                },
                sealed,
                write_row,
            });
        }

        let only = member.map(member_role_name);
        let actual: Vec<ActualMemberRole> = catalog
            .member_roles
            .values()
            .filter(|a| only.as_ref().is_none_or(|name| &a.role_name == name))
            .cloned()
            .collect();
        let desired: Vec<DesiredMemberRole> = wanted.iter().map(|w| w.desired.clone()).collect();
        let steps = plan_member_roles(&desired, &actual);

        // Apply (I1): each role's revocations first, as their own script; then its creates,
        // passwords and grants; then the drops. psql stops at a script's first error, so a
        // role's failure stops only that script.
        let mut failures: Vec<String> = Vec::new();
        let mut failed_roles: BTreeSet<String> = BTreeSet::new();
        // P6: on a protected branch the DDL guard goes into every database before the role
        // steps: checked on every run, installed wherever the check doesn't answer that the
        // current guard is in place (a database made since gets it; a dropped or changed
        // trigger comes back; a settings change rewrites the bodies). PB S2: an unprotected
        // branch carries no guard (its login rule would refuse login roles there): a
        // read-only presence check in every database, and the guard removed where found.
        if let Some(settings) = &guard_settings {
            let check = schema_guard_check_sql(settings);
            let guard = schema_guard_sql(settings);
            for &(db, guard_only) in &guard_databases {
                if guard_only && !guard_changed {
                    continue;
                }
                match self.computes.admin_sql(&ep, db, &check).await {
                    Ok(out) if out.lines().any(|l| l.trim() == "t") => continue,
                    Ok(_) => {}
                    Err(e) => {
                        debug!(branch_id = %branch, database = db, "member roles: checking the DDL guard failed ({e:#}); installing it");
                    }
                }
                guard_changed = true;
                if let Err(e) = self.computes.admin_sql(&ep, db, &guard).await {
                    warn!(branch_id = %branch, database = db, "member roles: installing the DDL guard failed: {e:#}");
                    failures.push(format!("guarding {db}: {e:#}"));
                }
            }
        } else {
            let present = schema_guard_present_sql();
            let unguard = schema_unguard_sql();
            for &(db, guard_only) in &guard_databases {
                if guard_only && !guard_changed {
                    continue;
                }
                match self.computes.admin_sql(&ep, db, &present).await {
                    Ok(out) if out.lines().any(|l| l.trim() == "t") => {}
                    Ok(_) => continue,
                    Err(e) => {
                        debug!(branch_id = %branch, database = db, "member roles: checking for a DDL guard failed ({e:#}); removing any");
                    }
                }
                guard_changed = true;
                if let Err(e) = self.computes.admin_sql(&ep, db, &unguard).await {
                    warn!(branch_id = %branch, database = db, "member roles: removing the DDL guard failed: {e:#}");
                    failures.push(format!("unguarding {db}: {e:#}"));
                }
            }
        }
        let mut revokes: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        let mut changes: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        let mut drops: Vec<&str> = Vec::new();
        for step in &steps {
            match step {
                MemberRoleStep::Drop { role } => drops.push(role),
                MemberRoleStep::Revoke { .. } | MemberRoleStep::Unlock { .. } => revokes
                    .entry(step.role())
                    .or_default()
                    .push(render_step(step)),
                _ => changes
                    .entry(step.role())
                    .or_default()
                    .push(render_step(step)),
            }
        }
        // R31: where each member role's leftovers go, decided on the catalog as it was before
        // this reconcile's revokes.
        let acted: HashMap<String, String> = match self.metadata.member_acted_roles(branch.as_uuid()).await {
            Ok(rows) => rows
                .into_iter()
                .map(|(member, role)| (member_role_name(member), role))
                .collect(),
            Err(e) => return failed(format!("reading the last acted roles: {e}")),
        };
        let target = |role: &str| reassign_target(&catalog, role, acted.get(role).map(String::as_str));
        // R31, I-A, I-B: every member role that stays, on every reconcile, but only where
        // `pg_shdepend` shows it still owns or holds something (a converged sweep runs nothing),
        // and before the revokes (its memberships still let it become the target): leftovers to
        // the role it last acted as (when allowed), then the rest dropped.
        for role in actual.iter().map(|a| a.role_name.as_str()) {
            if drops.contains(&role) {
                continue;
            }
            if let Err(e) = self
                .clean_up_leftovers(&ep, &catalog, role, target(role).as_deref())
                .await
            {
                warn!(branch_id = %branch, role, "member roles: cleaning up failed: {e:#}");
                failures.push(format!("cleaning up {role}: {e:#}"));
                failed_roles.insert(role.to_string());
            }
        }
        for (role, statements) in &revokes {
            let script = format!(
                "SET search_path = pg_catalog, pg_temp;\n{}",
                statements.join("\n")
            );
            if let Err(e) = self.computes.admin_sql(&ep, SYSTEM_DATABASE, &script).await {
                warn!(branch_id = %branch, role, "member roles: revoking failed: {e:#}");
                failures.push(format!("revoking {role}: {e:#}"));
                failed_roles.insert(role.to_string());
            }
        }
        for (role, statements) in &changes {
            let script = format!(
                "SET search_path = pg_catalog, pg_temp;\n{}",
                statements.join("\n")
            );
            if let Err(e) = self.computes.admin_sql(&ep, SYSTEM_DATABASE, &script).await {
                warn!(branch_id = %branch, role, "member roles: applying failed: {e:#}");
                failures.push(format!("applying {role}: {e:#}"));
                failed_roles.insert(role.to_string());
            }
        }
        for role in &drops {
            if let Err(e) = self
                .drop_member_role(&ep, &catalog, role, target(role).as_deref())
                .await
            {
                warn!(branch_id = %branch, role, "member roles: dropping failed: {e:#}");
                failures.push(format!("dropping {role}: {e:#}"));
                failed_roles.insert(role.to_string());
            }
        }

        // Rows: written for each applied role, removed for each role that is gone.
        for w in wanted
            .iter()
            .filter(|w| w.write_row && !failed_roles.contains(&w.desired.role_name))
        {
            let row = MemberBranchRole {
                member_id: w.desired.member_id,
                branch_id: branch.as_uuid(),
                role_name: w.desired.role_name.clone(),
                password_enc: w.sealed.clone(),
                applied_roles: w.desired.roles.iter().cloned().collect(),
            };
            if let Err(e) = self.metadata.upsert_member_branch_role(&row).await {
                failures.push(format!("storing {}: {e}", w.desired.role_name));
            }
        }
        for row in rows.values() {
            let still_wanted = wanted.iter().any(|w| w.desired.member_id == row.member_id);
            let role_left = catalog.member_roles.contains_key(&row.role_name)
                && failed_roles.contains(&row.role_name);
            if still_wanted || role_left {
                continue;
            }
            if let Err(e) = self
                .metadata
                .delete_member_branch_role(row.member_id, branch.as_uuid())
                .await
            {
                failures.push(format!("removing the row of {}: {e}", row.role_name));
            }
        }

        if !failures.is_empty() {
            return failed(format!("member roles: {}", failures.join("; ")));
        }
        if !steps.is_empty() {
            info!(branch_id = %branch, steps = steps.len(), "member roles reconciled");
        }
        MemberRolesReply::Done {
            roles: wanted
                .into_iter()
                .map(|w| MemberRoleApplied {
                    member_id: w.desired.member_id,
                    role_name: w.desired.role_name,
                    applied_roles: w.desired.roles.into_iter().collect(),
                })
                .collect(),
        }
    }

    /// A member role gone: locked out and its sessions ended, then in each database its
    /// leftovers to `reassign_to` and the rest dropped ([`cleanup_script`], R31), then the role.
    /// Each part is safe to run again.
    async fn drop_member_role(
        &self,
        ep: &str,
        catalog: &MemberCatalog,
        role: &str,
        reassign_to: Option<&str>,
    ) -> anyhow::Result<()> {
        let lock_out = format!(
            "SET search_path = pg_catalog, pg_temp;\n{}",
            render_step(&MemberRoleStep::Drop {
                role: role.to_string()
            })
        );
        self.computes
            .admin_sql(ep, SYSTEM_DATABASE, &lock_out)
            .await?;
        self.clean_up_leftovers(ep, catalog, role, reassign_to).await?;
        self.computes
            .admin_sql(
                ep,
                SYSTEM_DATABASE,
                &format!(
                    "SET search_path = pg_catalog, pg_temp;\n{}",
                    drop_role_sql(role)
                ),
            )
            .await?;
        Ok(())
    }

    /// [`cleanup_script`] for `role` in each database where `pg_shdepend` shows it still owns
    /// or holds something (I-A), shared objects from the system database; nothing at all for
    /// a role with no leftovers.
    async fn clean_up_leftovers(
        &self,
        ep: &str,
        catalog: &MemberCatalog,
        role: &str,
        reassign_to: Option<&str>,
    ) -> anyhow::Result<()> {
        let Some(found) = catalog.leftovers.get(role) else {
            return Ok(());
        };
        let mut databases: Vec<&str> = found.iter().flatten().map(String::as_str).collect();
        if found.contains(&None) && !databases.contains(&SYSTEM_DATABASE) {
            databases.push(SYSTEM_DATABASE);
        }
        for db in databases {
            self.computes
                .admin_sql(ep, db, &cleanup_script(role, reassign_to))
                .await
                .map_err(|e| anyhow::anyhow!("in {db}: {e:#}"))?;
        }
        Ok(())
    }

    /// The hand-over after an act-as run (R5; [`member_hand_over_sql`] states the invariant it
    /// keeps), in the run's database under the endpoint's lock. Refused for a name no member
    /// role may hold.
    pub async fn hand_over_member_role(&self, request: &MemberRoleHandOverRequest) -> MemberRolesReply {
        let acted = request.acted_role.as_str();
        // M-b: a name no member role may hold is never a target; the rest still runs.
        let target = (!acted.is_empty() && grantable(acted)).then_some(acted);
        if request.database.is_empty() {
            return failed("no database");
        }
        let branch = BranchId::from(request.branch_id);
        let record = match self.metadata.get_branch(branch).await {
            Ok(Some(b)) => b,
            Ok(None) => return failed("the branch doesn't exist"),
            Err(e) => return failed(format!("reading the branch: {e}")),
        };
        let ep = match (&record.status, &record.kind, &record.endpoint_id) {
            (BranchStatus::Ready, BranchKind::Normal, Some(ep)) => ep.clone(),
            _ => return failed("the branch isn't ready"),
        };
        let _lock = match self.metadata.lock_endpoint(&ep).await {
            Ok(lock) => lock,
            Err(e) => return failed(format!("locking the endpoint: {e}")),
        };
        let role = member_role_name(request.member_id);
        let backend = match (request.backend_pid, request.backend_start.as_deref()) {
            (Some(pid), Some(started)) => Some((pid, started)),
            _ => None,
        };
        let sql = member_hand_over_sql(&role, target, backend);
        if let Err(e) = self.computes.admin_sql(&ep, &request.database, &sql).await {
            warn!(branch_id = %branch, role, acted, "member role hand-over failed: {e:#}");
            return failed(format!("hand-over: {e:#}"));
        }
        // M-e: a run may have changed the member role's password (a role may change its own):
        // the row's password is put back at once.
        if let Err(e) = self.restore_member_password(&ep, &request.database, branch, request.member_id, &role).await {
            warn!(branch_id = %branch, role, "member role password check failed: {e:#}");
            self.member_role_requests.request(branch);
        }
        MemberRolesReply::Done { roles: vec![] }
    }

    /// M-e: compares the member role's `rolpassword` with its row's password and, when they
    /// differ, sets the row's password again (its SCRAM verifier). With no row, a reconcile is
    /// asked for (it makes a fresh one).
    async fn restore_member_password(
        &self,
        ep: &str,
        database: &str,
        branch: BranchId,
        member: Uuid,
        role: &str,
    ) -> anyhow::Result<()> {
        let rows = self.metadata.member_branch_roles(branch.as_uuid()).await?;
        let Some(row) = rows.into_iter().find(|r| r.member_id == member && r.role_name == role) else {
            self.member_role_requests.request(branch);
            return Ok(());
        };
        let password = open_member_role_password(&self.key, branch.as_uuid(), role, &row.password_enc)
            .map_err(|e| anyhow::anyhow!("the row's password won't open: {e}"))?;
        let current = self.computes.admin_sql(ep, database, &member_password_sql(role)).await?;
        if chelabase_neon::scram::verifies(current.trim(), &password) {
            return Ok(());
        }
        warn!(branch_id = %branch, role, "a run changed its member role's password; restoring it");
        let restore = format!(
            "SET search_path = pg_catalog, pg_temp;\nALTER ROLE {} PASSWORD {};",
            ident(role),
            literal(&chelabase_neon::scram::verifier(&password))
        );
        self.computes.admin_sql(ep, database, &restore).await?;
        Ok(())
    }

    /// Runs [`Self::reconcile_member_roles`] up to `tries` times, waiting `first_delay` after
    /// the first failure and twice as long after each next one. The last answer.
    pub async fn reconcile_member_roles_retrying(
        &self,
        branch: BranchId,
        tries: u32,
        first_delay: Duration,
    ) -> MemberRolesReply {
        let mut delay = first_delay;
        let mut reply = failed("never tried");
        for attempt in 1..=tries.max(1) {
            reply = self.reconcile_member_roles(branch, None).await;
            let MemberRolesReply::Failed { message } = &reply else {
                return reply;
            };
            if attempt < tries {
                info!(branch_id = %branch, attempt, "member-role reconcile failed; trying again: {message}");
                tokio::time::sleep(delay).await;
                delay *= 2;
            }
        }
        reply
    }

    /// An enqueued reconcile (`chelabase.member_roles.reconcile`): waits `delay` (a burst is
    /// one run), then reconciles the whole branch, retried with backoff
    /// ([`MEMBER_ROLES_TRIES`]). A call while another for the branch waits returns at once:
    /// the waiting one covers it.
    pub async fn reconcile_member_roles_soon(
        &self,
        pending: &PendingResyncs,
        branch: BranchId,
        delay: Duration,
    ) -> MemberRolesReply {
        if !pending.claim(branch) {
            return MemberRolesReply::Done { roles: Vec::new() };
        }
        tokio::time::sleep(delay).await;
        pending.release(branch);
        self.reconcile_member_roles_retrying(branch, MEMBER_ROLES_TRIES, MEMBER_ROLES_RETRY_DELAY)
            .await
    }

    /// I2: one pass of the periodic sweep, the backstop for lost or failed reconciles: every
    /// ready normal branch of a live database is reconciled once (PB R12: members or not, as
    /// the protected-branch guard's backstop too).
    /// Returns `(done, failed)`.
    pub async fn sweep_member_roles_once(&self) -> (usize, usize) {
        let branches = match self.metadata.member_role_branches().await {
            Ok(branches) => branches,
            Err(e) => {
                warn!("member-role sweep: listing the branches: {e}");
                return (0, 0);
            }
        };
        let (mut done, mut failures) = (0, 0);
        for branch in branches {
            let branch = BranchId::from(branch);
            match self.reconcile_member_roles(branch, None).await {
                MemberRolesReply::Done { .. } => done += 1,
                MemberRolesReply::Failed { message } => {
                    failures += 1;
                    warn!(branch_id = %branch, "member-role sweep: {message}");
                }
            }
        }
        (done, failures)
    }
}

#[cfg(test)]
mod tests;
