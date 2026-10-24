//! The privilege operations of `POST …/schema-changes` (members hub, spec Part 4 "After the
//! red-team"): what a request holding one checks and runs beyond the other operations.
//!
//! - Before any compute: no personal login is named as a role (MH-18, MH-28).
//! - In the transaction, before the statements: the read's ACL hashes (`expect_acl`, MH-33),
//!   the functions by OID (MH-27, P11), the objects' owners for an owner run (D6, P27a/P30),
//!   the all-rows exposure check (MH-26) and the data API exposure flag (P31).
//! - Around each statement run as an object's owner: the run-as sequence (P26).
//! - After the statements: the re-read and the requested end state compared (MH-24).
//! - After the commit: one `privileges_changed` event (P13, MH-30), never SQL.

use std::collections::{BTreeMap, BTreeSet};

use chelabase_metadata::members::AccessAction;
use chelabase_metadata::personal_logins::AccessEventInput;
use chelabase_schema::ddl::{self, RenderContext};
use chelabase_schema::privileges::{
    PrivilegesPage, acl_hashes_sql, all_rows_exposure, exposure_chain, owner_settable_predicate,
    parse_privileges, privileges_sql, verify_privileges,
};
use chelabase_types::api_error::{ApiError, ErrorCode};
use chelabase_types::role_name::quote_ident;
use chelabase_types::table_api::{
    DataApiExposure, ExpectedAcl, Grantee, Operation, PrivilegeTarget,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_postgres::types::ToSql;
use tracing::{error, warn};
use uuid::Uuid;

use super::changes::{Notices, collecting, on_dry, read_failure, simple};
use crate::privileges::{JsonText, Jsonb};
use crate::sql_editor::run::Connected;
use crate::{AppState, AuthPrincipal, JobPort, MetadataPort};

/// Objects per page of the verify-after re-read (the read's own cap).
const REREAD_LIMIT: i32 = 1000;
/// The re-read stops after this many pages of one (grantee, schema): a catalog that large is
/// refused rather than read without end.
const REREAD_MAX_PAGES: usize = 1000;

/// Whether `op` changes an ACL or the hub's all-rows policy (a privilege op).
pub(super) fn is_privilege_op(op: &Operation) -> bool {
    ddl::privilege_target(op).is_some()
}

/// The snake-case name of `op` (its JSON tag).
fn op_name(op: &Operation) -> String {
    match serde_json::to_value(op) {
        Ok(Value::Object(map)) => map.keys().next().cloned().unwrap_or_default(),
        Ok(Value::String(s)) => s,
        _ => String::new(),
    }
}

/// The role names `op` hands Postgres as roles: its grantee, `SetAllRows`' role and the
/// default privileges' `FOR ROLE`.
fn named_roles(op: &Operation) -> Vec<&str> {
    fn grantee(g: &Grantee) -> Option<&str> {
        match g {
            Grantee::Role(r) => Some(r.as_str()),
            Grantee::Public => None,
        }
    }
    match op {
        Operation::Grant { grant, .. } | Operation::Revoke { grant, .. } => {
            grantee(&grant.grantee).into_iter().collect()
        }
        Operation::GrantSchema { grantee: g, .. }
        | Operation::RevokeSchema { grantee: g, .. }
        | Operation::GrantSequence { grantee: g, .. }
        | Operation::RevokeSequence { grantee: g, .. }
        | Operation::GrantFunction { grantee: g, .. }
        | Operation::RevokeFunction { grantee: g, .. } => grantee(g).into_iter().collect(),
        Operation::AlterDefaultPrivileges {
            for_role,
            grantee: g,
            ..
        } => grantee(g).into_iter().chain([for_role.as_str()]).collect(),
        Operation::SetAllRows { role, .. } => vec![role.as_str()],
        _ => Vec::new(),
    }
}

/// MH-18 / MH-28: a personal login of the database named as a role by any privilege op is
/// refused (400 `invalid_request` with `op_index`), before any compute.
pub(super) async fn refuse_personal_logins<M, J>(
    state: &AppState<M, J>,
    database: Uuid,
    ops: &[Operation],
) -> Result<(), ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    for (op_index, op) in ops.iter().enumerate() {
        for name in named_roles(op) {
            let personal = state
                .metadata
                .is_personal_login_name(database, name)
                .await
                .map_err(|err| {
                    error!("schema change: reading personal logins: {err}");
                    ApiError::internal()
                })?;
            if personal {
                return Err(ApiError::invalid_request(
                    400,
                    format!(
                        "operation {op_index}: {name:?} is a personal login; privileges are never granted to one"
                    ),
                )
                .with_extra("op_index", json!(op_index)));
            }
        }
    }
    Ok(())
}

/// Whether the request turns an all-rows policy on (its check needs the branch's key roles).
pub(super) fn needs_key_roles(ops: &[Operation]) -> bool {
    ops.iter()
        .any(|op| matches!(op, Operation::SetAllRows { enabled: true, .. }))
}

/// The roles of the branch's live data API keys (MH-26).
pub(super) async fn key_roles<M, J>(
    state: &AppState<M, J>,
    branch: chelabase_types::BranchId,
) -> Result<BTreeSet<String>, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let keys = state.metadata.list_data_keys(branch).await.map_err(|err| {
        error!("schema change: reading the branch's keys: {err}");
        ApiError::internal()
    })?;
    Ok(keys
        .into_iter()
        .filter(|k| k.revoked_at.is_none())
        .map(|k| k.role)
        .collect())
}

/// What one privilege request runs with, beyond the other operations'.
pub(super) struct PrivilegeChecks<'a> {
    pub(super) expect_acl: &'a [ExpectedAcl],
    /// The roles of the branch's live keys (read only when the request turns all rows on).
    pub(super) key_roles: &'a BTreeSet<String>,
}

/// What the in-transaction checks found that the rest of the request needs.
#[derive(Default)]
pub(super) struct Prepared {
    /// The schema of each function the request names (the re-read's pages).
    function_schemas: BTreeMap<u32, String>,
    /// P31's flag, in operation order.
    pub(super) exposure: Vec<DataApiExposure>,
}

/// Rows of one of our own parameterised queries in the transaction (its notices dropped).
async fn rows(
    conn: &mut Connected,
    sql: &str,
    params: &[&(dyn ToSql + Sync)],
) -> Result<Vec<tokio_postgres::Row>, tokio_postgres::Error> {
    let (client, queue) = conn.client_and_notices();
    collecting(client.query(sql, params), queue, &mut Notices::new()).await
}

/// The one JSON value of one of our own queries.
async fn json_value(
    conn: &mut Connected,
    sql: &str,
    params: &[&(dyn ToSql + Sync)],
) -> Result<String, tokio_postgres::Error> {
    let (client, queue) = conn.client_and_notices();
    let row = collecting(client.query_one(sql, params), queue, &mut Notices::new()).await?;
    Ok(row.get::<_, JsonText>(0).0)
}

fn unreadable(what: &str, err: impl std::fmt::Display) -> ApiError {
    error!("schema change, {what}: {err}");
    ApiError::internal()
}

/// 409 `privileges_changed_since_read` naming `objects`.
fn changed_since_read(objects: Vec<PrivilegeTarget>, dry_run: bool) -> ApiError {
    on_dry(
        ApiError::new(
            409,
            ErrorCode::PrivilegesChangedSinceRead,
            "the privileges of what the request changes changed since they were read; nothing was changed. Read them again and review the change",
        )
        .with_extra("objects", serde_json::to_value(objects).unwrap_or_default()),
        dry_run,
    )
}

/// Steps before the statements, in the transaction, once the request's locks are held (T5-M2):
/// `ctx` gains the resolved functions, the
/// objects' owners (an owner run only), the server version; refusals as the spec answers them.
/// `owner_run` is the database owner's own session (no `SET ROLE`); `ctx.session_role` names
/// the role the statements run as.
pub(super) async fn prepare(
    conn: &mut Connected,
    ops: &[Operation],
    checks: &PrivilegeChecks<'_>,
    owner_run: bool,
    ctx: &mut RenderContext,
    dry: bool,
) -> Result<Prepared, ApiError> {
    let mut prepared = Prepared::default();
    refuse_extension_schemas(conn, ops, dry).await?;
    check_acl_hashes(conn, ops, checks.expect_acl, dry).await?;
    resolve_functions(conn, ops, ctx, &mut prepared, dry).await?;
    if owner_run {
        ctx.owners = owners(conn, ops, &ctx.session_role, dry).await?;
    }
    let grants_a_role = ops.iter().any(|op| exposure_grantee(op).is_some());
    if grants_a_role || needs_key_roles(ops) {
        let edges = memberships(conn, dry).await?;
        for (op_index, op) in ops.iter().enumerate() {
            if let Operation::SetAllRows {
                role,
                enabled: true,
                ..
            } = op
                && let Some(chain) = all_rows_exposure(&edges, role, checks.key_roles)
            {
                return Err(on_dry(
                    ApiError::new(
                        422,
                        ErrorCode::ExposesDataApi,
                        format!(
                            "operation {op_index}: every row for {role:?} would reach the data API through {}; nothing was changed",
                            chain.join(" → ")
                        ),
                    )
                    .with_extra("op_index", json!(op_index))
                    .with_extra("chain", json!(chain)),
                    dry,
                ));
            }
        }
        let data_api: BTreeSet<String> = ["anon", "authenticated"].map(String::from).into();
        for (op_index, op) in ops.iter().enumerate() {
            if let Some(grantee) = exposure_grantee(op)
                && let Some(chain) = exposure_chain(&edges, grantee, &data_api)
            {
                prepared.exposure.push(DataApiExposure {
                    op_index,
                    grantee: grantee.to_string(),
                    chain,
                });
            }
        }
    }
    Ok(prepared)
}

/// The role a grant op gives privileges to (P31: grants only, default privileges included).
fn exposure_grantee(op: &Operation) -> Option<&str> {
    let grantee = match op {
        Operation::Grant { grant, .. } => &grant.grantee,
        Operation::GrantSchema { grantee, .. }
        | Operation::GrantSequence { grantee, .. }
        | Operation::GrantFunction { grantee, .. } => grantee,
        Operation::AlterDefaultPrivileges {
            grant: true,
            grantee,
            ..
        } => grantee,
        _ => return None,
    };
    match grantee {
        Grantee::Role(r) => Some(r),
        Grantee::Public => None,
    }
}

/// MH-33 / S3: each `expect_acl` entry naming an object the request changes must be the
/// object's current hash (a missing object has none); the others are ignored.
async fn check_acl_hashes(
    conn: &mut Connected,
    ops: &[Operation],
    expect_acl: &[ExpectedAcl],
    dry: bool,
) -> Result<(), ApiError> {
    let touched: BTreeSet<PrivilegeTarget> = ops.iter().filter_map(ddl::privilege_target).collect();
    let expected: Vec<&ExpectedAcl> = expect_acl
        .iter()
        .filter(|e| touched.contains(&e.object))
        .collect();
    if expected.is_empty() {
        return Ok(());
    }
    let objects: Vec<&PrivilegeTarget> = expected.iter().map(|e| &e.object).collect();
    let param = Jsonb(serde_json::to_string(&objects).unwrap_or_default());
    let json = json_value(conn, acl_hashes_sql(), &[&param])
        .await
        .map_err(|e| read_failure(&e, "reading the ACL hashes", dry))?;
    #[derive(Deserialize)]
    struct Current {
        object: PrivilegeTarget,
        acl_hash: String,
    }
    let current: Vec<Current> =
        serde_json::from_str(&json).map_err(|e| unreadable("the ACL hashes' JSON", e))?;
    let mut stale: Vec<PrivilegeTarget> = Vec::new();
    for e in expected {
        let now = current
            .iter()
            .find(|c| c.object == e.object)
            .map(|c| c.acl_hash.as_str());
        if now != Some(e.acl_hash.as_str()) && !stale.contains(&e.object) {
            stale.push(e.object.clone());
        }
    }
    if stale.is_empty() {
        Ok(())
    } else {
        Err(changed_since_read(stale, dry))
    }
}

/// The routine (by OID) each function op names: its identity as the read gives it, its schema,
/// its kind and whether its schema belongs to an extension; no row when it's gone.
const FUNCTION_SQL: &str = "SELECT \
     pg_catalog.format('%I.%I(%s)', n.nspname, p.proname, pg_catalog.pg_get_function_identity_arguments(p.oid)), \
     n.nspname::pg_catalog.text, p.prokind::pg_catalog.text, \
     EXISTS (SELECT FROM pg_catalog.pg_depend d \
             WHERE d.classid = 'pg_catalog.pg_namespace'::pg_catalog.regclass AND d.objid = n.oid AND d.deptype = 'e') \
     FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
     WHERE p.oid = $1::pg_catalog.int8::pg_catalog.oid";

/// T5-I1: a routine's `regprocedure` text, read under `search_path = ''` so it's
/// schema-qualified whoever then runs it.
const REGPROCEDURE_SQL: &str = "SELECT p.oid::pg_catalog.regprocedure::pg_catalog.text \
     FROM pg_catalog.pg_proc p WHERE p.oid = $1::pg_catalog.int8::pg_catalog.oid";

/// T4-5: 400 `invalid_request` for a privilege op on a schema the hub never shows.
fn hidden_schema_refusal(op_index: usize, schema: &str, dry: bool) -> ApiError {
    on_dry(
        ApiError::invalid_request(
            400,
            ddl::DdlError {
                op_index,
                kind: ddl::DdlErrorKind::HiddenSchema(schema.to_string()),
            }
            .to_string(),
        )
        .with_extra("op_index", json!(op_index)),
        dry,
    )
}

/// T4-5: the extension schemas (schemas that are members of an extension) among the schemas
/// the privilege ops name are refused like the hidden ones (the function ops' schemas are
/// checked as they resolve).
async fn refuse_extension_schemas(
    conn: &mut Connected,
    ops: &[Operation],
    dry: bool,
) -> Result<(), ApiError> {
    const SQL: &str = "SELECT n.nspname::pg_catalog.text FROM pg_catalog.pg_namespace n \
         WHERE n.nspname = ANY ($1::pg_catalog.text[]) \
           AND EXISTS (SELECT FROM pg_catalog.pg_depend d \
                       WHERE d.classid = 'pg_catalog.pg_namespace'::pg_catalog.regclass \
                         AND d.objid = n.oid AND d.deptype = 'e')";
    let named: Vec<(usize, &str)> = ops
        .iter()
        .enumerate()
        .filter_map(|(i, op)| ddl::privilege_op_schema(op).map(|s| (i, s)))
        .collect();
    if named.is_empty() {
        return Ok(());
    }
    let schemas: Vec<String> = named
        .iter()
        .map(|(_, s)| s.to_string())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let found: BTreeSet<String> = rows(conn, SQL, &[&schemas])
        .await
        .map_err(|e| read_failure(&e, "reading the extension schemas", dry))?
        .iter()
        .map(|r| r.get(0))
        .collect();
    match named.into_iter().find(|(_, s)| found.contains(*s)) {
        Some((op_index, schema)) => Err(hidden_schema_refusal(op_index, schema, dry)),
        None => Ok(()),
    }
}

/// MH-27 / P11: each function op's OID resolves now to the identity the read gave (else 409
/// `privileges_changed_since_read`); a routine that's gone is 404 with `op_index`; an aggregate
/// or window function (the editor offers functions and procedures only), or a routine in a
/// hidden or extension schema (T4-5), is 400 `invalid_request` with `op_index` (T5-M5). Only
/// the server's own `regprocedure` text is rendered, read under `search_path = ''` (T5-I1).
async fn resolve_functions(
    conn: &mut Connected,
    ops: &[Operation],
    ctx: &mut RenderContext,
    prepared: &mut Prepared,
    dry: bool,
) -> Result<(), ApiError> {
    let mut oids: Vec<u32> = Vec::new();
    for (op_index, op) in ops.iter().enumerate() {
        let function = match op {
            Operation::GrantFunction { function, .. }
            | Operation::RevokeFunction { function, .. } => function,
            _ => continue,
        };
        let oid = i64::from(function.oid);
        let found = rows(conn, FUNCTION_SQL, &[&oid])
            .await
            .map_err(|e| read_failure(&e, "resolving a function", dry))?;
        let Some(row) = found.first() else {
            return Err(on_dry(
                ApiError::not_found(format!(
                    "operation {op_index}: the function {} doesn't exist",
                    function.oid
                ))
                .with_extra("op_index", json!(op_index)),
                dry,
            ));
        };
        let (identity, schema, kind, in_extension): (String, String, String, bool) =
            (row.get(0), row.get(1), row.get(2), row.get(3));
        if ddl::hidden_schema(&schema) || in_extension {
            return Err(hidden_schema_refusal(op_index, &schema, dry));
        }
        if kind != "f" && kind != "p" {
            let what = if kind == "a" {
                "an aggregate"
            } else {
                "a window function"
            };
            return Err(on_dry(
                ApiError::invalid_request(
                    400,
                    format!(
                        "operation {op_index}: {} is {what}; its privileges are changed in SQL",
                        function.identity
                    ),
                )
                .with_extra("op_index", json!(op_index)),
                dry,
            ));
        }
        if identity != function.identity {
            return Err(changed_since_read(
                vec![PrivilegeTarget::Function { oid: function.oid }],
                dry,
            )
            .with_extra("op_index", json!(op_index)));
        }
        oids.push(function.oid);
        prepared.function_schemas.insert(function.oid, schema);
    }
    if oids.is_empty() {
        return Ok(());
    }
    // T5-I1: the statement may run as the function's owner, whose search path differs.
    let saved: String = rows(
        conn,
        "SELECT pg_catalog.current_setting('search_path')",
        &[],
    )
    .await
    .map_err(|e| read_failure(&e, "reading the search path", dry))?
    .first()
    .map(|r| r.get(0))
    .unwrap_or_default();
    simple(conn, "SET LOCAL search_path = ''")
        .await
        .map_err(|e| read_failure(&e, "clearing the search path", dry))?;
    for oid in oids {
        let found = rows(conn, REGPROCEDURE_SQL, &[&i64::from(oid)])
            .await
            .map_err(|e| read_failure(&e, "resolving a function", dry))?;
        if let Some(row) = found.first() {
            ctx.resolved_functions.insert(oid, row.get(0));
        }
    }
    rows(
        conn,
        "SELECT pg_catalog.set_config('search_path', $1::pg_catalog.text, true)",
        &[&saved],
    )
    .await
    .map_err(|e| read_failure(&e, "restoring the search path", dry))?;
    Ok(())
}

/// The owner of `target` now (`None` when there's no such object). A default ACL's "owner" is
/// its `FOR ROLE` (P28).
async fn owner_of(
    conn: &mut Connected,
    target: &PrivilegeTarget,
    dry: bool,
) -> Result<Option<String>, ApiError> {
    const SCHEMA: &str = "SELECT pg_catalog.pg_get_userbyid(nspowner)::pg_catalog.text \
         FROM pg_catalog.pg_namespace WHERE nspname = $1::pg_catalog.text";
    const RELATION: &str = "SELECT pg_catalog.pg_get_userbyid(c.relowner)::pg_catalog.text \
         FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = $1::pg_catalog.text AND c.relname = $2::pg_catalog.text \
           AND c.relkind IN ('r', 'v', 'm', 'p', 'f')";
    const SEQUENCE: &str = "SELECT pg_catalog.pg_get_userbyid(c.relowner)::pg_catalog.text \
         FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = $1::pg_catalog.text AND c.relname = $2::pg_catalog.text AND c.relkind = 'S'";
    const FUNCTION: &str = "SELECT pg_catalog.pg_get_userbyid(proowner)::pg_catalog.text \
         FROM pg_catalog.pg_proc WHERE oid = $1::pg_catalog.int8::pg_catalog.oid";
    let found = match target {
        PrivilegeTarget::DefaultAcl { for_role, .. } => return Ok(Some(for_role.clone())),
        PrivilegeTarget::Schema { schema } => rows(conn, SCHEMA, &[schema]).await,
        PrivilegeTarget::Relation { schema, name } => rows(conn, RELATION, &[schema, name]).await,
        PrivilegeTarget::Sequence { schema, name } => rows(conn, SEQUENCE, &[schema, name]).await,
        PrivilegeTarget::Function { oid } => rows(conn, FUNCTION, &[&i64::from(*oid)]).await,
    }
    .map_err(|e| read_failure(&e, "reading an owner", dry))?;
    Ok(found.first().map(|r| r.get(0)))
}

/// Whether `op` only revokes from an object (not its default privileges): P30's revokes run as
/// the database owner when the object's owner can't be reached.
fn plain_revoke(op: &Operation) -> bool {
    matches!(
        op,
        Operation::Revoke { .. }
            | Operation::RevokeSchema { .. }
            | Operation::RevokeSequence { .. }
            | Operation::RevokeFunction { .. }
    )
}

/// D6 with P27a and P30: the owner of every object the request changes. An object the
/// database owner owns (or `pg_database_owner`, which it is in its database) maps to the
/// session role (no run-as). An owner the run-as sequence can't reach refuses the request
/// (422 `invalid_request`, `extra.owner`), unless every op on that object is a revoke, which
/// then runs as the database owner (P30: grants recorded with grantor `neon_superuser` on
/// `cloud_admin`'s objects), checked by the re-read like any other.
async fn owners(
    conn: &mut Connected,
    ops: &[Operation],
    session_role: &str,
    dry: bool,
) -> Result<BTreeMap<PrivilegeTarget, String>, ApiError> {
    let mut owners: BTreeMap<PrivilegeTarget, String> = BTreeMap::new();
    for target in ops.iter().filter_map(ddl::privilege_target) {
        if owners.contains_key(&target) {
            continue;
        }
        if let Some(owner) = owner_of(conn, &target, dry).await? {
            let owner = if owner == "pg_database_owner" {
                session_role.to_string()
            } else {
                owner
            };
            owners.insert(target, owner);
        }
    }
    let others: Vec<String> = owners
        .values()
        .filter(|o| *o != session_role)
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let settable = settable_roles(conn, &others, dry).await?;
    for (op_index, op) in ops.iter().enumerate() {
        let Some(target) = ddl::privilege_target(op) else {
            continue;
        };
        let Some(owner) = owners.get(&target) else {
            continue;
        };
        if owner == session_role || settable.contains(owner) {
            continue;
        }
        let only_revokes = ops
            .iter()
            .filter(|o| ddl::privilege_target(o).as_ref() == Some(&target))
            .all(plain_revoke);
        if !only_revokes {
            return Err(on_dry(
                ApiError::invalid_request(
                    422,
                    format!(
                        "operation {op_index}: the object is owned by {owner:?}, which the database owner can't act as; change it in SQL as {owner:?}"
                    ),
                )
                .with_extra("op_index", json!(op_index))
                .with_extra("owner", json!(owner)),
                dry,
            ));
        }
    }
    // P30: the remaining unreachable owners' objects (revokes only) run as the database owner.
    owners.retain(|_, owner| owner == session_role || settable.contains(owner));
    Ok(owners)
}

/// The roles of `roles` the session role (the database owner) can act as (P27a).
async fn settable_roles(
    conn: &mut Connected,
    roles: &[String],
    dry: bool,
) -> Result<BTreeSet<String>, ApiError> {
    if roles.is_empty() {
        return Ok(BTreeSet::new());
    }
    let sql = format!(
        "SELECT r.rolname::pg_catalog.text FROM pg_catalog.pg_roles r \
         WHERE r.rolname = ANY ($1::pg_catalog.text[]) AND {}",
        owner_settable_predicate(
            "r.oid",
            "(SELECT me.oid FROM pg_catalog.pg_roles me WHERE me.rolname = current_user)"
        )
    );
    let found = rows(conn, &sql, &[&roles])
        .await
        .map_err(|e| read_failure(&e, "reading the owners' memberships", dry))?;
    Ok(found.iter().map(|r| r.get(0)).collect())
}

/// Every membership as member → the roles it is a member of (any option, MH-26).
async fn memberships(
    conn: &mut Connected,
    dry: bool,
) -> Result<BTreeMap<String, BTreeSet<String>>, ApiError> {
    let found = rows(conn, chelabase_schema::privileges::MEMBERSHIPS_SQL, &[])
        .await
        .map_err(|e| read_failure(&e, "reading the memberships", dry))?;
    let mut edges: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for row in found {
        edges.entry(row.get(0)).or_default().insert(row.get(1));
    }
    Ok(edges)
}

/// How the run-as sequence (P26) reaches a role X.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reach {
    /// The owner can already SET to X: nothing to grant.
    Set,
    /// A membership row the owner granted itself exists: SET on it, then the option revoked
    /// (the row as it was).
    OwnRow,
    /// A new row (`INHERIT FALSE, SET TRUE`), revoked after.
    Transient,
}

/// A statement running as an object's owner: what to undo after it.
pub(super) struct RunAs {
    role: String,
    reach: Reach,
    /// The session role's own `pg_auth_members` rows in X before (P26 step 4; T5-M3: another
    /// role's membership of X, granted meanwhile, isn't the sequence's doing).
    before: String,
}

/// The session role's memberships in X and how it reaches X.
const REACH_SQL: &str = "SELECT \
     (SELECT coalesce(pg_catalog.jsonb_agg(pg_catalog.to_jsonb(m) - 'oid' ORDER BY m.member, m.grantor), '[]'::jsonb)::pg_catalog.text \
      FROM pg_catalog.pg_auth_members m WHERE m.roleid = r.oid \
        AND m.member = (SELECT me.oid FROM pg_catalog.pg_roles me WHERE me.rolname = current_user)), \
     CASE WHEN pg_catalog.current_setting('server_version_num')::int >= 160000 \
          THEN pg_catalog.pg_has_role(current_user, r.oid, 'SET') \
          ELSE pg_catalog.pg_has_role(current_user, r.oid, 'MEMBER') END, \
     EXISTS (SELECT FROM pg_catalog.pg_auth_members m \
             WHERE m.roleid = r.oid \
               AND m.member = (SELECT me.oid FROM pg_catalog.pg_roles me WHERE me.rolname = current_user) \
               AND m.grantor = (SELECT me.oid FROM pg_catalog.pg_roles me WHERE me.rolname = current_user)) \
     FROM pg_catalog.pg_roles r WHERE r.rolname = $1::pg_catalog.text";

/// The session role's memberships in X now (the same text as [`REACH_SQL`]'s first column).
const MEMBERS_OF_SQL: &str = "SELECT \
     (SELECT coalesce(pg_catalog.jsonb_agg(pg_catalog.to_jsonb(m) - 'oid' ORDER BY m.member, m.grantor), '[]'::jsonb)::pg_catalog.text \
      FROM pg_catalog.pg_auth_members m WHERE m.roleid = r.oid \
        AND m.member = (SELECT me.oid FROM pg_catalog.pg_roles me WHERE me.rolname = current_user)) \
     FROM pg_catalog.pg_roles r WHERE r.rolname = $1::pg_catalog.text";

/// The run-as failure of one of our own statements (the sequence, not the caller's).
fn run_as_failure(err: &tokio_postgres::Error, op_index: usize, dry: bool) -> ApiError {
    match err.as_db_error() {
        // Postgres refused the transient grant (a membership changed since the check): the
        // request can't run as that owner.
        Some(db) if matches!(db.code().code(), "42501" | "0LP01" | "XX000") => {
            warn!(
                sqlstate = db.code().code(),
                op_index,
                "schema change: the run-as grant was refused: {}",
                db.message()
            );
            on_dry(
                ApiError::invalid_request(
                    422,
                    format!("operation {op_index}: the database owner can't act as the object's owner now"),
                )
                .with_extra("op_index", json!(op_index)),
                dry,
            )
        }
        _ => read_failure(err, "running as the object's owner", dry),
    }
}

/// P26 steps 1 to 3, before a statement that runs as `role`: SET on `role` for the session
/// role `me` (granted only when needed), then `SET LOCAL ROLE`.
pub(super) async fn enter(
    conn: &mut Connected,
    role: &str,
    me: &str,
    op_index: usize,
    dry: bool,
) -> Result<RunAs, ApiError> {
    let found = rows(conn, REACH_SQL, &[&role])
        .await
        .map_err(|e| read_failure(&e, "reading the owner's memberships", dry))?;
    let Some(row) = found.first() else {
        return Err(on_dry(
            ApiError::invalid_request(422, format!("operation {op_index}: no role {role:?}"))
                .with_extra("op_index", json!(op_index))
                .with_extra("owner", json!(role)),
            dry,
        ));
    };
    let (before, can_set, own_row): (String, bool, bool) = (row.get(0), row.get(1), row.get(2));
    let reach = match (can_set, own_row) {
        (true, _) => Reach::Set,
        (false, true) => Reach::OwnRow,
        (false, false) => Reach::Transient,
    };
    let (x, me_q) = (quote_ident(role), quote_ident(me));
    let grant = match reach {
        Reach::Set => None,
        Reach::OwnRow => Some(format!("GRANT {x} TO {me_q} WITH SET TRUE")),
        Reach::Transient => Some(format!("GRANT {x} TO {me_q} WITH INHERIT FALSE, SET TRUE")),
    };
    if let Some(sql) = grant {
        simple(conn, &sql)
            .await
            .map_err(|e| run_as_failure(&e, op_index, dry))?;
    }
    simple(conn, &format!("SET LOCAL ROLE {x}"))
        .await
        .map_err(|e| run_as_failure(&e, op_index, dry))?;
    Ok(RunAs {
        role: role.to_string(),
        reach,
        before,
    })
}

/// P26 after the statement: `RESET ROLE`, the grant undone, and X's memberships compared with
/// before (any difference fails the request, which rolls back).
pub(super) async fn leave(
    conn: &mut Connected,
    run: RunAs,
    me: &str,
    op_index: usize,
    dry: bool,
) -> Result<(), ApiError> {
    simple(conn, "RESET ROLE")
        .await
        .map_err(|e| read_failure(&e, "resetting the role", dry))?;
    let (x, me_q) = (quote_ident(&run.role), quote_ident(me));
    let undo = match run.reach {
        Reach::Set => None,
        Reach::OwnRow => Some(format!("REVOKE SET OPTION FOR {x} FROM {me_q}")),
        Reach::Transient => Some(format!("REVOKE {x} FROM {me_q}")),
    };
    if let Some(sql) = undo {
        simple(conn, &sql)
            .await
            .map_err(|e| run_as_failure(&e, op_index, dry))?;
    }
    let now: Option<String> = rows(conn, MEMBERS_OF_SQL, &[&run.role])
        .await
        .map_err(|e| read_failure(&e, "reading the owner's memberships", dry))?
        .first()
        .map(|r| r.get(0));
    if now.as_deref() != Some(run.before.as_str()) {
        error!(
            op_index,
            reach = ?run.reach,
            "schema change: running as an object's owner left its memberships changed; rolled back"
        );
        return Err(on_dry(ApiError::internal(), dry));
    }
    Ok(())
}

/// MH-24 / P29: a `no privileges could be revoked` / `were granted` warning (01006, 01007) of
/// one statement of operation `op_index`: 409 `privileges_not_applied`. T5-M1: the re-read runs
/// first, so `mismatches` names what differs from what operations 0 to `op_index` asked (the
/// later ones never ran); empty when the re-read can't tell.
pub(super) async fn warned_not_applied(
    conn: &mut Connected,
    ops: &[Operation],
    prepared: Option<&Prepared>,
    op_index: usize,
    dry: bool,
) -> ApiError {
    let ran = &ops[..=op_index.min(ops.len().saturating_sub(1))];
    let mismatches = match prepared {
        Some(prepared) => match reread_all(conn, ran, prepared, dry).await {
            Ok(pages) => verify_privileges(ran, &pages).err().unwrap_or_default(),
            Err(err) => {
                warn!(
                    op_index,
                    status = err.status,
                    "schema change: the re-read after a `not applied` warning failed"
                );
                Vec::new()
            }
        },
        None => Vec::new(),
    };
    on_dry(
        ApiError::new(
            409,
            ErrorCode::PrivilegesNotApplied,
            format!(
                "operation {op_index}: Postgres granted or revoked less than asked (the role it ran as lacks a grant option); nothing was changed"
            ),
        )
        .with_extra("op_index", json!(op_index))
        .with_extra(
            "mismatches",
            serde_json::to_value(mismatches).unwrap_or_default(),
        ),
        dry,
    )
}

/// The pages the re-read needs: one (grantee, schema) per object op, `schema: None` for schema
/// and default-privilege ops (S3). T4-3: `SetAllRows` is verified too, on its role's page of the
/// table's schema (where the read shows the role's all-rows policy).
fn reread_keys(
    ops: &[Operation],
    function_schemas: &BTreeMap<u32, String>,
) -> BTreeSet<(String, Option<String>)> {
    let name = |g: &Grantee| match g {
        Grantee::Public => "public".to_string(),
        Grantee::Role(r) => r.clone(),
    };
    let mut keys = BTreeSet::new();
    for op in ops {
        let key = match op {
            Operation::Grant { table, grant } | Operation::Revoke { table, grant } => {
                (name(&grant.grantee), Some(table.schema.clone()))
            }
            Operation::GrantSchema { grantee, .. }
            | Operation::RevokeSchema { grantee, .. }
            | Operation::AlterDefaultPrivileges { grantee, .. } => (name(grantee), None),
            Operation::GrantSequence {
                sequence, grantee, ..
            }
            | Operation::RevokeSequence {
                sequence, grantee, ..
            } => (name(grantee), Some(sequence.schema.clone())),
            Operation::GrantFunction { function, grantee }
            | Operation::RevokeFunction {
                function, grantee, ..
            } => match function_schemas.get(&function.oid) {
                Some(schema) => (name(grantee), Some(schema.clone())),
                None => continue,
            },
            Operation::SetAllRows { table, role, .. } => (role.clone(), Some(table.schema.clone())),
            _ => continue,
        };
        keys.insert(key);
    }
    keys
}

/// Every page of one (grantee, schema), read in the transaction.
async fn reread(
    conn: &mut Connected,
    role: &str,
    schema: Option<&str>,
    dry: bool,
) -> Result<Vec<PrivilegesPage>, ApiError> {
    let mut pages = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let params: [&(dyn ToSql + Sync); 4] = [&role, &schema, &after, &REREAD_LIMIT];
        let json = json_value(conn, privileges_sql(), &params)
            .await
            .map_err(|e| read_failure(&e, "reading the privileges again", dry))?;
        let page =
            parse_privileges(&json).map_err(|e| unreadable("the privileges read's JSON", e))?;
        after = page.next_after.clone();
        pages.push(page);
        if after.is_none() {
            return Ok(pages);
        }
        if pages.len() >= REREAD_MAX_PAGES {
            error!("schema change: the privileges re-read didn't end");
            return Err(on_dry(ApiError::internal(), dry));
        }
    }
}

/// Every page the re-read of `ops` needs ([`reread_keys`]).
async fn reread_all(
    conn: &mut Connected,
    ops: &[Operation],
    prepared: &Prepared,
    dry: bool,
) -> Result<Vec<PrivilegesPage>, ApiError> {
    let mut pages = Vec::new();
    for (role, schema) in reread_keys(ops, &prepared.function_schemas) {
        pages.extend(reread(conn, &role, schema.as_deref(), dry).await?);
    }
    Ok(pages)
}

/// MH-24: after the statements, in the transaction, the touched entries as Postgres now has
/// them against what the request asked; any difference is 409 `privileges_not_applied` with
/// every mismatch (object, privilege, grantee, the grantor still holding it, column).
pub(super) async fn verify(
    conn: &mut Connected,
    ops: &[Operation],
    prepared: &Prepared,
    dry: bool,
) -> Result<(), ApiError> {
    let pages = reread_all(conn, ops, prepared, dry).await?;
    match verify_privileges(ops, &pages) {
        Ok(()) => Ok(()),
        Err(mismatches) => Err(on_dry(
            ApiError::new(
                409,
                ErrorCode::PrivilegesNotApplied,
                "Postgres doesn't hold the privileges as the request asked (another grantor's entry, or one the role it ran as couldn't change); nothing was changed",
            )
            .with_extra("mismatches", serde_json::to_value(mismatches).unwrap_or_default()),
            dry,
        )),
    }
}

/// P13 / MH-30: after the commit, one `privileges_changed` event: the Postgres database, the
/// role acted as (`null` for the owner) and each privilege op's name and object; never SQL. A
/// failed write is logged: the change is committed.
#[allow(clippy::too_many_arguments)]
pub(super) async fn record_event<M, J>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    project: Uuid,
    database: Uuid,
    branch: Uuid,
    pg_database: &str,
    acted_as: Option<&str>,
    ops: &[Operation],
) where
    M: MetadataPort,
    J: JobPort,
{
    let changed: Vec<Value> = ops
        .iter()
        .filter_map(|op| {
            let object = ddl::privilege_target(op)?;
            Some(json!({ "op": op_name(op), "object": object }))
        })
        .collect();
    let after = json!({ "pg_database": pg_database, "acted_as": acted_as, "ops": changed });
    let event = AccessEventInput {
        tenant: principal.tenant_id,
        actor: principal.member.as_ref().map(|m| m.id),
        action: AccessAction::PrivilegesChanged,
        target: None,
        project: Some(project),
        database: Some(database),
        branch: Some(branch),
        before: None,
        after: Some(&after),
    };
    if let Err(err) = state.metadata.record_access_event(&event).await {
        error!(
            request_id = principal.request_id.as_str(),
            branch_id = %branch,
            "privileges_changed not recorded: {err}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ops(v: Value) -> Vec<Operation> {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn op_names_are_the_json_tags() {
        let all = ops(json!([
            { "grant": { "table": { "schema": "s", "name": "t" }, "grant": { "privileges": ["select"], "grantee": { "role": "r" } } } },
            { "grant_schema": { "schema": "s", "grantee": { "role": "r" }, "privileges": ["usage"] } },
            { "set_all_rows": { "table": { "schema": "s", "name": "t" }, "role": "r", "enabled": true } },
            { "alter_default_privileges": { "for_role": "o", "schema": "s", "object": "tables", "grant": true, "grantee": { "role": "r" }, "privileges": ["select"] } },
        ]));
        let names: Vec<String> = all.iter().map(op_name).collect();
        assert_eq!(
            names,
            [
                "grant",
                "grant_schema",
                "set_all_rows",
                "alter_default_privileges"
            ]
        );
    }

    // MH-18: every role name a privilege op hands Postgres is checked, `FOR ROLE` included;
    // PUBLIC isn't a name.
    #[test]
    fn named_roles_cover_grantees_all_rows_and_for_role() {
        let all = ops(json!([
            { "revoke": { "table": { "schema": "s", "name": "t" }, "grant": { "privileges": ["select"], "grantee": "public" } } },
            { "revoke_function": { "function": { "oid": 1, "identity": "f()" }, "grantee": { "role": "a" } } },
            { "alter_default_privileges": { "for_role": "o", "schema": "s", "object": "tables", "grant": false, "grantee": { "role": "b" }, "privileges": ["select"] } },
            { "set_all_rows": { "table": { "schema": "s", "name": "t" }, "role": "c", "enabled": false } },
            { "set_table_comment": { "table": { "schema": "s", "name": "t" }, "comment": "x" } },
        ]));
        let named: Vec<Vec<&str>> = all.iter().map(named_roles).collect();
        assert_eq!(
            named,
            vec![vec![], vec!["a"], vec!["b", "o"], vec!["c"], vec![]]
        );
    }

    // P31: grants only (default privileges included), never revokes or PUBLIC.
    #[test]
    fn only_grants_to_a_role_are_flagged() {
        let all = ops(json!([
            { "grant": { "table": { "schema": "s", "name": "t" }, "grant": { "privileges": ["select"], "grantee": { "role": "anon" } } } },
            { "revoke": { "table": { "schema": "s", "name": "t" }, "grant": { "privileges": ["select"], "grantee": { "role": "anon" } } } },
            { "alter_default_privileges": { "for_role": "o", "schema": "s", "object": "tables", "grant": true, "grantee": { "role": "authenticated" }, "privileges": ["select"] } },
            { "alter_default_privileges": { "for_role": "o", "schema": "s", "object": "tables", "grant": false, "grantee": { "role": "anon" }, "privileges": ["select"] } },
            { "grant_function": { "function": { "oid": 1, "identity": "f()" }, "grantee": { "role": "x" } } },
            { "set_all_rows": { "table": { "schema": "s", "name": "t" }, "role": "anon", "enabled": true } },
        ]));
        let flagged: Vec<Option<&str>> = all.iter().map(exposure_grantee).collect();
        assert_eq!(
            flagged,
            vec![
                Some("anon"),
                None,
                Some("authenticated"),
                None,
                Some("x"),
                None
            ]
        );
    }

    // S3: the re-read covers every (grantee, schema) the ops touch; schema and default
    // privileges read the schema list; a function's page is its schema's. Final round T4-3
    // (deliberate change): `SetAllRows` is verified too, so its (role, table's schema) page is
    // read (`z` on `e`).
    #[test]
    fn the_reread_covers_every_touched_grantee_and_schema() {
        let all = ops(json!([
            { "grant": { "table": { "schema": "a", "name": "t" }, "grant": { "privileges": ["select"], "grantee": { "role": "r" } } } },
            { "revoke": { "table": { "schema": "a", "name": "u" }, "grant": { "privileges": ["select"], "grantee": "public" } } },
            { "grant_schema": { "schema": "b", "grantee": { "role": "r" }, "privileges": ["usage"] } },
            { "grant_sequence": { "sequence": { "schema": "c", "name": "q" }, "grantee": { "role": "s" }, "privileges": ["usage"] } },
            { "grant_function": { "function": { "oid": 7, "identity": "d.f()" }, "grantee": { "role": "r" } } },
            { "alter_default_privileges": { "for_role": "o", "schema": "a", "object": "tables", "grant": true, "grantee": { "role": "s" }, "privileges": ["select"] } },
            { "set_all_rows": { "table": { "schema": "e", "name": "t" }, "role": "z", "enabled": true } },
        ]));
        let schemas: BTreeMap<u32, String> = [(7, "d".to_string())].into();
        let keys: Vec<(String, Option<String>)> = reread_keys(&all, &schemas).into_iter().collect();
        let k = |r: &str, s: Option<&str>| (r.to_string(), s.map(String::from));
        assert_eq!(
            keys,
            vec![
                k("public", Some("a")),
                k("r", None),
                k("r", Some("a")),
                k("r", Some("d")),
                k("s", None),
                k("s", Some("c")),
                k("z", Some("e"))
            ]
        );
    }

    #[test]
    fn only_object_revokes_may_run_as_the_database_owner() {
        let all = ops(json!([
            { "revoke": { "table": { "schema": "s", "name": "t" }, "grant": { "privileges": ["select"], "grantee": { "role": "r" } } } },
            { "revoke_schema": { "schema": "s", "grantee": { "role": "r" }, "privileges": ["usage"] } },
            { "grant": { "table": { "schema": "s", "name": "t" }, "grant": { "privileges": ["select"], "grantee": { "role": "r" } } } },
            { "alter_default_privileges": { "for_role": "o", "schema": "s", "object": "tables", "grant": false, "grantee": { "role": "r" }, "privileges": ["select"] } },
            { "set_all_rows": { "table": { "schema": "s", "name": "t" }, "role": "r", "enabled": false } },
        ]));
        let revokes: Vec<bool> = all.iter().map(plain_revoke).collect();
        assert_eq!(revokes, [true, true, false, false, false]);
    }
}
