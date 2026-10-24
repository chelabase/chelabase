//! One operation's statements. Every function validates the names it renders and returns
//! [`DdlErrorKind`]; `render` attaches the operation index.

use std::collections::BTreeSet;

use chelabase_types::access::{act_as_reserved, data_api_role};
use chelabase_types::table_api::{
    ColumnChange, ColumnSpec, CommentChange, ConstraintKindSpec, ConstraintSpec, CreateTable,
    DefaultChange, DefaultObject, EnumPosition, FkAction, Grantee, IdentityChange, IdentityKind,
    IndexKey, IndexSpec, LockLevel, NullsOrder, Operation, PolicyCommandSpec, PolicySpec,
    Privilege, PrivilegeName, SchemaPrivilege, SequencePrivilege, TableGrant, TableRef, TypeSpec,
};

use super::{DdlErrorKind, RenderContext};
use crate::model::{Column, Grant, Index, NamedRole, Oid, Policy, PolicyCommand, RoleRef};
use crate::plan::ddl as merge;
use crate::privileges::{ALL_ROWS_MARKER, all_rows_policy_name};
use crate::sql::{ident, literal, qualified};

type R<T> = Result<T, DdlErrorKind>;

/// A statement before `render` gives it its operation index.
pub(super) struct Stmt {
    pub sql: String,
    pub rewrites_table: bool,
    pub lock: LockLevel,
    pub concurrently: bool,
    pub table: Option<TableRef>,
}

impl Stmt {
    fn on(table: &TableRef, lock: LockLevel, sql: String) -> Stmt {
        Stmt {
            sql,
            rewrites_table: false,
            lock,
            concurrently: false,
            table: Some(table.clone()),
        }
    }

    /// A statement whose table the request doesn't name (index by name, enum).
    fn off(lock: LockLevel, sql: String) -> Stmt {
        Stmt {
            sql,
            rewrites_table: false,
            lock,
            concurrently: false,
            table: None,
        }
    }
}

/// Whether the operation runs `CONCURRENTLY` (so it must be the request's only one).
pub(super) fn concurrent(op: &Operation) -> bool {
    match op {
        Operation::CreateIndex(spec) => spec.concurrently,
        Operation::DropIndex { concurrently, .. } => *concurrently,
        _ => false,
    }
}

/// `catalog_current`: no earlier operation of the request changed what `ctx` says about the
/// operation's objects (otherwise a column's current type is unknown). The operation alone in
/// its request (see [`operation_in`]).
#[cfg(test)]
pub(super) fn operation(
    op: &Operation,
    ctx: &RenderContext,
    catalog_current: bool,
) -> R<Vec<Stmt>> {
    operation_in(op, ctx, catalog_current, &mut AllRowsOverlay::default())
}

/// [`operation`] as one of a request's operations: `all_rows` carries what the request's
/// earlier `SetAllRows` ops left (T4-3).
pub(super) fn operation_in(
    op: &Operation,
    ctx: &RenderContext,
    catalog_current: bool,
    all_rows: &mut AllRowsOverlay,
) -> R<Vec<Stmt>> {
    use LockLevel::{AccessExclusive, ShareRowExclusive, ShareUpdateExclusive};
    if let Some(schema) = privilege_op_schema(op)
        && super::hidden_schema(schema)
    {
        return Err(DdlErrorKind::HiddenSchema(schema.to_string()));
    }
    let drop_behaviour = if ctx.cascade { "CASCADE" } else { "RESTRICT" };
    let one = |table: &TableRef, lock: LockLevel, sql: String| Ok(vec![Stmt::on(table, lock, sql)]);
    match op {
        Operation::CreateTable(c) => create_table(c, ctx),
        Operation::RenameTable { table, to } => one(
            table,
            AccessExclusive,
            format!("ALTER TABLE {} RENAME TO {}", table_q(table)?, name_q(to)?),
        ),
        Operation::SetTableSchema { table, schema } => one(
            table,
            AccessExclusive,
            format!(
                "ALTER TABLE {} SET SCHEMA {}",
                table_q(table)?,
                name_q(schema)?
            ),
        ),
        Operation::DropTable { table } => one(
            table,
            AccessExclusive,
            format!("DROP TABLE {} {drop_behaviour}", table_q(table)?),
        ),
        Operation::SetTableComment { table, comment } => one(
            table,
            ShareUpdateExclusive,
            merge::comment("TABLE", &table_q(table)?, comment),
        ),
        Operation::SetTableOwner { table, owner } => one(
            table,
            AccessExclusive,
            format!(
                "ALTER TABLE {} OWNER TO {}",
                table_q(table)?,
                ident(role(owner)?)
            ),
        ),
        Operation::AddColumn { table, column } => add_column(table, column),
        Operation::AlterColumn {
            table,
            column,
            change,
        } => alter_column(
            table,
            column,
            change,
            catalog_current
                .then(|| current_type(ctx, table, column))
                .flatten(),
        ),
        Operation::RenameColumn { table, column, to } => one(
            table,
            AccessExclusive,
            format!(
                "ALTER TABLE {} RENAME COLUMN {} TO {}",
                table_q(table)?,
                name_q(column)?,
                name_q(to)?
            ),
        ),
        Operation::DropColumn { table, column } => one(
            table,
            AccessExclusive,
            format!(
                "ALTER TABLE {} DROP COLUMN {} {drop_behaviour}",
                table_q(table)?,
                name_q(column)?
            ),
        ),
        Operation::AddConstraint { table, constraint } => {
            let lock = match constraint.kind {
                ConstraintKindSpec::ForeignKey { .. } => ShareRowExclusive,
                _ => AccessExclusive,
            };
            one(
                table,
                lock,
                format!(
                    "ALTER TABLE {} ADD {}",
                    table_q(table)?,
                    constraint_def(constraint)?
                ),
            )
        }
        Operation::ValidateConstraint { table, name } => one(
            table,
            ShareUpdateExclusive,
            format!(
                "ALTER TABLE {} VALIDATE CONSTRAINT {}",
                table_q(table)?,
                name_q(name)?
            ),
        ),
        Operation::RenameConstraint { table, name, to } => one(
            table,
            AccessExclusive,
            format!(
                "ALTER TABLE {} RENAME CONSTRAINT {} TO {}",
                table_q(table)?,
                name_q(name)?,
                name_q(to)?
            ),
        ),
        Operation::DropConstraint { table, name } => one(
            table,
            AccessExclusive,
            format!(
                "ALTER TABLE {} DROP CONSTRAINT {} {drop_behaviour}",
                table_q(table)?,
                name_q(name)?
            ),
        ),
        Operation::CreateIndex(spec) => {
            let mut s = Stmt::on(
                &spec.table,
                if spec.concurrently {
                    ShareUpdateExclusive
                } else {
                    LockLevel::Share
                },
                create_index(spec)?,
            );
            s.concurrently = spec.concurrently;
            Ok(vec![s])
        }
        Operation::DropIndex {
            schema,
            name: index,
            concurrently,
        } => {
            let iq = qualified(name(schema)?, name(index)?);
            let (sql, lock) = if *concurrently {
                if ctx.cascade {
                    return Err(DdlErrorKind::Unsupported("cascade with concurrently"));
                }
                (
                    format!("DROP INDEX CONCURRENTLY {iq}"),
                    ShareUpdateExclusive,
                )
            } else {
                (format!("DROP INDEX {iq} {drop_behaviour}"), AccessExclusive)
            };
            Ok(vec![Stmt {
                sql,
                rewrites_table: false,
                lock,
                concurrently: *concurrently,
                table: None,
            }])
        }
        Operation::RenameIndex {
            schema,
            name: index,
            to,
        } => Ok(vec![Stmt::off(
            ShareUpdateExclusive,
            format!(
                "ALTER INDEX {} RENAME TO {}",
                qualified(name(schema)?, name(index)?),
                name_q(to)?
            ),
        )]),
        Operation::SetRls {
            table,
            enabled,
            forced,
        } => {
            let tq = table_q(table)?;
            Ok(vec![
                Stmt::on(
                    table,
                    AccessExclusive,
                    format!(
                        "ALTER TABLE {tq} {} ROW LEVEL SECURITY",
                        if *enabled { "ENABLE" } else { "DISABLE" }
                    ),
                ),
                Stmt::on(
                    table,
                    AccessExclusive,
                    format!(
                        "ALTER TABLE {tq} {} ROW LEVEL SECURITY",
                        if *forced { "FORCE" } else { "NO FORCE" }
                    ),
                ),
            ])
        }
        Operation::CreatePolicy { table, policy } => one(
            table,
            AccessExclusive,
            policy_sql(policy, &table_q(table)?)?,
        ),
        Operation::AlterPolicy {
            table,
            name: policy,
            roles,
            using,
            with_check,
        } => {
            let mut sql = format!("ALTER POLICY {} ON {}", name_q(policy)?, table_q(table)?);
            if roles.is_none() && using.is_none() && with_check.is_none() {
                return Err(DdlErrorKind::Empty("change"));
            }
            if let Some(roles) = roles {
                sql.push_str(&format!(" TO {}", role_list(roles)?));
            }
            if let Some(u) = using {
                sql.push_str(&format!(" USING {}", merge::paren(u)));
            }
            if let Some(w) = with_check {
                sql.push_str(&format!(" WITH CHECK {}", merge::paren(w)));
            }
            one(table, AccessExclusive, sql)
        }
        Operation::RenamePolicy {
            table,
            name: policy,
            to,
        } => one(
            table,
            AccessExclusive,
            format!(
                "ALTER POLICY {} ON {} RENAME TO {}",
                name_q(policy)?,
                table_q(table)?,
                name_q(to)?
            ),
        ),
        // `DROP POLICY` has nothing that depends on it: no `RESTRICT`/`CASCADE`.
        Operation::DropPolicy {
            table,
            name: policy,
        } => one(
            table,
            AccessExclusive,
            format!("DROP POLICY {} ON {}", name_q(policy)?, table_q(table)?),
        ),
        Operation::Grant { table, grant } => {
            let tq = table_q(table)?;
            check_table_grant(grant, false, ctx)?;
            Ok(grant_statements(grant, &tq)?
                .into_iter()
                .map(|sql| Stmt::on(table, LockLevel::None, sql))
                .collect())
        }
        Operation::Revoke { table, grant } => {
            let tq = table_q(table)?;
            check_table_grant(grant, true, ctx)?;
            Ok(revoke_statements(grant, &tq)?
                .into_iter()
                .map(|sql| Stmt::on(table, LockLevel::None, sql))
                .collect())
        }
        Operation::CreateEnum {
            schema,
            name: e,
            values,
        } => {
            let labels = values.iter().map(|v| label(v)).collect::<R<Vec<_>>>()?;
            Ok(vec![Stmt::off(
                LockLevel::None,
                format!(
                    "CREATE TYPE {} AS ENUM ({})",
                    qualified(name(schema)?, name(e)?),
                    labels.join(", ")
                ),
            )])
        }
        Operation::AddEnumValue {
            schema,
            name: e,
            value,
            position,
        } => {
            let mut sql = format!(
                "ALTER TYPE {} ADD VALUE {}",
                qualified(name(schema)?, name(e)?),
                label(value)?
            );
            match position {
                Some(EnumPosition::Before(v)) => sql.push_str(&format!(" BEFORE {}", label(v)?)),
                Some(EnumPosition::After(v)) => sql.push_str(&format!(" AFTER {}", label(v)?)),
                None => {}
            }
            Ok(vec![Stmt::off(LockLevel::None, sql)])
        }
        Operation::RenameEnumValue {
            schema,
            name: e,
            from,
            to,
        } => Ok(vec![Stmt::off(
            LockLevel::None,
            format!(
                "ALTER TYPE {} RENAME VALUE {} TO {}",
                qualified(name(schema)?, name(e)?),
                label(from)?,
                label(to)?
            ),
        )]),
        Operation::RenameEnum {
            schema,
            name: e,
            to,
        } => Ok(vec![Stmt::off(
            LockLevel::None,
            format!(
                "ALTER TYPE {} RENAME TO {}",
                qualified(name(schema)?, name(e)?),
                name_q(to)?
            ),
        )]),
        // With `CASCADE`, dropping a type drops every column of that type: their tables are
        // locked `ACCESS EXCLUSIVE`.
        Operation::DropEnum { schema, name: e } => Ok(vec![Stmt::off(
            if ctx.cascade {
                AccessExclusive
            } else {
                LockLevel::None
            },
            format!(
                "DROP TYPE {} {drop_behaviour}",
                qualified(name(schema)?, name(e)?)
            ),
        )]),
        Operation::GrantSchema {
            schema,
            grantee,
            privileges,
        } => {
            let on = format!("SCHEMA {}", name_q(schema)?);
            let names: Vec<&str> = privileges.iter().map(|p| schema_privilege(*p)).collect();
            off_statements(object_grants(&names, grantee, ctx, &on, Revoking::No)?)
        }
        Operation::RevokeSchema {
            schema,
            grantee,
            privileges,
            grant_option_only,
        } => {
            let on = format!("SCHEMA {}", name_q(schema)?);
            let names: Vec<&str> = privileges.iter().map(|p| schema_privilege(*p)).collect();
            off_statements(object_grants(
                &names,
                grantee,
                ctx,
                &on,
                Revoking::Yes(*grant_option_only),
            )?)
        }
        Operation::GrantSequence {
            sequence,
            grantee,
            privileges,
        } => {
            let on = format!("SEQUENCE {}", table_q(sequence)?);
            let names: Vec<&str> = privileges.iter().map(|p| sequence_privilege(*p)).collect();
            off_statements(object_grants(&names, grantee, ctx, &on, Revoking::No)?)
        }
        Operation::RevokeSequence {
            sequence,
            grantee,
            privileges,
            grant_option_only,
        } => {
            let on = format!("SEQUENCE {}", table_q(sequence)?);
            let names: Vec<&str> = privileges.iter().map(|p| sequence_privilege(*p)).collect();
            off_statements(object_grants(
                &names,
                grantee,
                ctx,
                &on,
                Revoking::Yes(*grant_option_only),
            )?)
        }
        Operation::GrantFunction { function, grantee } => {
            let on = format!("ROUTINE {}", resolved_function(ctx, function.oid)?);
            off_statements(object_grants(&["EXECUTE"], grantee, ctx, &on, Revoking::No)?)
        }
        Operation::RevokeFunction {
            function,
            grantee,
            grant_option_only,
        } => {
            let on = format!("ROUTINE {}", resolved_function(ctx, function.oid)?);
            off_statements(object_grants(
                &["EXECUTE"],
                grantee,
                ctx,
                &on,
                Revoking::Yes(*grant_option_only),
            )?)
        }
        Operation::AlterDefaultPrivileges {
            for_role,
            schema,
            object,
            grant,
            grantee,
            privileges,
        } => off_statements(default_privileges(
            for_role, schema, *object, *grant, grantee, privileges, ctx,
        )?),
        Operation::SetAllRows {
            table,
            role: r,
            enabled,
        } => set_all_rows(table, r, *enabled, ctx, all_rows),
    }
}

/// The schema a privilege op names (T4-5); `None` for the other ops and for function ops
/// (named by OID: the orchestrator checks their schema).
pub fn privilege_op_schema(op: &Operation) -> Option<&str> {
    match op {
        Operation::Grant { table, .. }
        | Operation::Revoke { table, .. }
        | Operation::SetAllRows { table, .. } => Some(&table.schema),
        Operation::GrantSequence { sequence, .. } | Operation::RevokeSequence { sequence, .. } => {
            Some(&sequence.schema)
        }
        Operation::GrantSchema { schema, .. }
        | Operation::RevokeSchema { schema, .. }
        | Operation::AlterDefaultPrivileges { schema, .. } => Some(schema),
        _ => None,
    }
}

/// Whether a privilege statement grants or revokes (and then whether only the grant option).
#[derive(Clone, Copy)]
enum Revoking {
    No,
    Yes(bool),
}

/// Statements on no table of the request (schemas, sequences, routines, default privileges).
fn off_statements(sqls: Vec<String>) -> R<Vec<Stmt>> {
    Ok(sqls
        .into_iter()
        .map(|sql| Stmt::off(LockLevel::None, sql))
        .collect())
}

pub(crate) fn schema_privilege(p: SchemaPrivilege) -> &'static str {
    match p {
        SchemaPrivilege::Usage => "USAGE",
        SchemaPrivilege::Create => "CREATE",
    }
}

pub(crate) fn sequence_privilege(p: SequencePrivilege) -> &'static str {
    match p {
        SequencePrivilege::Usage => "USAGE",
        SequencePrivilege::Select => "SELECT",
        SequencePrivilege::Update => "UPDATE",
    }
}

/// The server's `regprocedure` text for `oid` (P11): the client's `identity` never reaches SQL.
fn resolved_function(ctx: &RenderContext, oid: u32) -> R<&str> {
    ctx.resolved_functions
        .get(&oid)
        .map(String::as_str)
        .filter(|s| !s.is_empty())
        .ok_or(DdlErrorKind::UnresolvedFunction(oid))
}

/// A privilege op's grantee (MH-28, S5): `PUBLIC` only in a revoke; a role must be a plain role
/// name ([`role`]) the allow-list ([`super::privilege_grantee`]) takes, with the session role as
/// the owner.
fn privilege_grantee_ref(g: &Grantee, revoking: bool, ctx: &RenderContext) -> R<RoleRef> {
    match g {
        Grantee::Public if revoking => Ok(RoleRef::Public),
        Grantee::Public => Err(DdlErrorKind::GranteeNotAllowed("PUBLIC".to_string())),
        Grantee::Role(r) => {
            let r = role(r)?;
            super::privilege_grantee(r, &ctx.session_role)?;
            Ok(RoleRef::Role {
                oid: Oid(0),
                name: r.to_string(),
            })
        }
    }
}

/// One `GRANT`/`REVOKE` per privilege on `on` (`SCHEMA s`, `SEQUENCE s.q`, `ROUTINE f(…)`), with
/// no grant option (MH-29).
fn object_grants(
    privileges: &[&str],
    grantee: &Grantee,
    ctx: &RenderContext,
    on: &str,
    revoking: Revoking,
) -> R<Vec<String>> {
    if privileges.is_empty() {
        return Err(DdlErrorKind::Empty("privileges"));
    }
    let grantee = privilege_grantee_ref(grantee, matches!(revoking, Revoking::Yes(_)), ctx)?;
    privileges
        .iter()
        .map(|p| {
            let model = Grant {
                grantor: NamedRole {
                    oid: Oid(0),
                    name: String::new(),
                },
                grantee: grantee.clone(),
                privilege: (*p).to_string(),
                grantable: false,
            };
            match revoking {
                Revoking::No => merge::grant_sql(&model, on, None),
                Revoking::Yes(option_only) => merge::revoke_sql(&model, on, None, option_only),
            }
            .ok_or(DdlErrorKind::Unsupported("privilege"))
        })
        .collect()
}

/// `MAINTAIN` exists from Postgres 17 (an unknown version, 0, counts as older).
fn maintain_supported(ctx: &RenderContext) -> R<()> {
    if ctx.server_version_num < 170_000 {
        Err(DdlErrorKind::PrivilegeNotSupported("MAINTAIN"))
    } else {
        Ok(())
    }
}

/// The table `Grant`/`Revoke` (and `CreateTable.grants`) checks: privileges first, then the
/// grantee allow-list (S5), no grant option on a grant (S21), `MAINTAIN` by server version.
pub(super) fn check_table_grant(g: &TableGrant, revoking: bool, ctx: &RenderContext) -> R<()> {
    if g.privileges.is_empty() {
        return Err(DdlErrorKind::Empty("privileges"));
    }
    privilege_grantee_ref(&g.grantee, revoking, ctx)?;
    if !revoking && g.with_grant_option {
        return Err(DdlErrorKind::Unsupported(
            "with_grant_option: a grant never carries the grant option",
        ));
    }
    if g.privileges.contains(&Privilege::Maintain) {
        maintain_supported(ctx)?;
    }
    Ok(())
}

/// `ALTER DEFAULT PRIVILEGES FOR ROLE r IN SCHEMA s GRANT|REVOKE p ON <objects> TO|FROM g`, one
/// per privilege. `FOR ROLE` is the session role or a role outside the reserved names (MH-28);
/// each privilege must exist for the object kind.
fn default_privileges(
    for_role: &str,
    schema: &str,
    object: DefaultObject,
    grant: bool,
    grantee: &Grantee,
    privileges: &[PrivilegeName],
    ctx: &RenderContext,
) -> R<Vec<String>> {
    if privileges.is_empty() {
        return Err(DdlErrorKind::Empty("privileges"));
    }
    let schema_q = name_q(schema)?;
    let for_role = name(for_role)?;
    // An empty session role is not known yet (the orchestrator's check before any compute):
    // a reserved name may then still be the session role (the database owner's real name), so
    // only what never is one is refused; the render in the transaction checks the rest.
    let reserved = !ctx.session_role.is_empty() && act_as_reserved(for_role);
    if for_role != ctx.session_role
        && (reserved || data_api_role(for_role) || role(for_role).is_err())
    {
        return Err(DdlErrorKind::ForRoleNotAllowed(for_role.to_string()));
    }
    let grantee = merge::role_ref(&privilege_grantee_ref(grantee, !grant, ctx)?);
    let objects = match object {
        DefaultObject::Tables => "TABLES",
        DefaultObject::Sequences => "SEQUENCES",
        DefaultObject::Functions => "FUNCTIONS",
    };
    privileges
        .iter()
        .map(|p| {
            let keyword = default_privilege(object, *p).ok_or(DdlErrorKind::Unsupported(
                "a privilege the default privileges' object kind doesn't have",
            ))?;
            if *p == PrivilegeName::Maintain {
                maintain_supported(ctx)?;
            }
            let head = format!(
                "ALTER DEFAULT PRIVILEGES FOR ROLE {} IN SCHEMA {schema_q}",
                ident(for_role)
            );
            Ok(if grant {
                format!("{head} GRANT {keyword} ON {objects} TO {grantee}")
            } else {
                format!("{head} REVOKE {keyword} ON {objects} FROM {grantee}")
            })
        })
        .collect()
}

/// The SQL keyword of `p`.
pub(crate) fn privilege_name_keyword(p: PrivilegeName) -> &'static str {
    use PrivilegeName as P;
    match p {
        P::Select => "SELECT",
        P::Insert => "INSERT",
        P::Update => "UPDATE",
        P::Delete => "DELETE",
        P::Truncate => "TRUNCATE",
        P::References => "REFERENCES",
        P::Trigger => "TRIGGER",
        P::Maintain => "MAINTAIN",
        P::Usage => "USAGE",
        P::Execute => "EXECUTE",
    }
}

/// The keyword of `p` when `object` has it.
fn default_privilege(object: DefaultObject, p: PrivilegeName) -> Option<&'static str> {
    use PrivilegeName as P;
    let keyword = privilege_name_keyword(p);
    let has = match object {
        DefaultObject::Tables => !matches!(p, P::Usage | P::Execute),
        DefaultObject::Sequences => matches!(p, P::Usage | P::Select | P::Update),
        DefaultObject::Functions => matches!(p, P::Execute),
    };
    has.then_some(keyword)
}

/// T4-3: what the request's earlier `SetAllRows` ops left on each table they touched.
#[derive(Default)]
pub(super) struct AllRowsOverlay {
    /// (schema, table) → its policy names and the hub's policies per role, after the earlier
    /// ops.
    tables: std::collections::BTreeMap<(String, String), TableAllRows>,
}

struct TableAllRows {
    /// Every policy name on the table.
    taken: BTreeSet<String>,
    /// Role → the names of its hub "all rows" policies.
    ours: std::collections::BTreeMap<String, Vec<String>>,
}

/// The hub's "all rows" policy for `role` (MH-26): a permissive `FOR ALL` policy whose only
/// role is `role`, `USING (true) WITH CHECK (true)`, commented with [`ALL_ROWS_MARKER`].
pub(crate) fn is_all_rows_policy(p: &Policy, role: &str) -> bool {
    p.permissive
        && p.command == PolicyCommand::All
        && matches!(p.roles.as_slice(), [RoleRef::Role { name, .. }] if name == role)
        && p.using.as_deref().map(str::trim) == Some("true")
        && p.with_check.as_deref().map(str::trim) == Some("true")
        && p.comment.as_deref() == Some(ALL_ROWS_MARKER)
}

/// On: `CREATE POLICY` of that shape under a name none of the table's policies has, plus the
/// marker comment; nothing when the table already has one. Off: `DROP POLICY` of each policy of
/// exactly that shape and marker, whatever its name (a user's own policies are never touched).
///
/// T4-3: earlier `SetAllRows` ops of the same request are taken into account through
/// `overlay`, so a request ends as its last toggle on a (table, role) says.
fn set_all_rows(
    table: &TableRef,
    r: &str,
    enabled: bool,
    ctx: &RenderContext,
    overlay: &mut AllRowsOverlay,
) -> R<Vec<Stmt>> {
    let tq = table_q(table)?;
    let r = role(r)?;
    super::privilege_grantee(r, &ctx.session_role)?;
    let policies: &[Policy] = ctx
        .table(table)
        .map_or(&[], |t| t.table.policies.as_slice());
    let state = overlay
        .tables
        .entry((table.schema.clone(), table.name.clone()))
        .or_insert_with(|| TableAllRows {
            taken: policies.iter().map(|p| p.name.clone()).collect(),
            ours: Default::default(),
        });
    let TableAllRows { taken, ours } = state;
    let ours = ours.entry(r.to_string()).or_insert_with(|| {
        policies
            .iter()
            .filter(|p| is_all_rows_policy(p, r))
            .map(|p| p.name.clone())
            .collect()
    });
    let stmt = |sql: String| Stmt::on(table, LockLevel::AccessExclusive, sql);
    if !enabled {
        let drops = ours
            .iter()
            .map(|p| Ok(stmt(format!("DROP POLICY {} ON {tq}", name_q(p)?))))
            .collect::<R<Vec<_>>>()?;
        for p in std::mem::take(ours) {
            taken.remove(&p);
        }
        return Ok(drops);
    }
    if !ours.is_empty() {
        return Ok(Vec::new());
    }
    let policy_name = all_rows_policy_name(r, taken);
    ours.push(policy_name.clone());
    taken.insert(policy_name.clone());
    let model = Policy {
        oid: Oid(0),
        name: name(&policy_name)?.to_string(),
        command: PolicyCommand::All,
        permissive: true,
        roles: vec![RoleRef::Role {
            oid: Oid(0),
            name: r.to_string(),
        }],
        using: Some("true".to_string()),
        with_check: Some("true".to_string()),
        comment: None,
    };
    Ok(vec![
        stmt(merge::create_policy(&model, &tq)),
        stmt(merge::comment(
            "POLICY",
            &format!("{} ON {tq}", ident(&policy_name)),
            &Some(ALL_ROWS_MARKER.to_string()),
        )),
    ])
}

/// An enum label: a name (non-empty, at most 63 bytes, no NUL), rendered as a literal.
fn label(v: &str) -> R<String> {
    Ok(literal(name(v)?))
}

/// A role name as a `RoleSpec`. `public` and `none` parse as `PUBLIC` / an error even when
/// quoted, and the `current_user`-like keywords are refused alongside them (in any case), so a
/// role always reaches Postgres as exactly the role given.
pub(super) fn role(r: &str) -> R<&str> {
    const SPECIAL: [&str; 5] = [
        "public",
        "none",
        "current_user",
        "session_user",
        "current_role",
    ];
    let r = name(r)?;
    if SPECIAL.iter().any(|s| r.eq_ignore_ascii_case(s)) {
        return Err(DdlErrorKind::InvalidName(r.to_string()));
    }
    Ok(r)
}

/// A policy's `TO` list: the quoted roles, or `PUBLIC` when empty.
fn role_list(roles: &[String]) -> R<String> {
    if roles.is_empty() {
        return Ok("PUBLIC".to_string());
    }
    let names = roles
        .iter()
        .map(|r| Ok(ident(role(r)?)))
        .collect::<R<Vec<_>>>()?;
    Ok(names.join(", "))
}

fn nulls(n: &Option<NullsOrder>) -> &'static str {
    match n {
        Some(NullsOrder::First) => " NULLS FIRST",
        Some(NullsOrder::Last) => " NULLS LAST",
        None => "",
    }
}

/// `CREATE INDEX …`, through the merge planner's `create_index`. An unnamed index (Postgres
/// names it) is the same statement without the name.
fn create_index(spec: &IndexSpec) -> R<String> {
    let tq = table_q(&spec.table)?;
    let method = spec.method.as_deref().unwrap_or("btree");
    if !valid_method(method) {
        return Err(DdlErrorKind::InvalidMethod(method.to_string()));
    }
    if spec.keys.is_empty() {
        return Err(DdlErrorKind::Empty("keys"));
    }
    let keys = spec
        .keys
        .iter()
        .map(|k| {
            let (head, descending, order) = match k {
                IndexKey::Column {
                    name: c,
                    descending,
                    nulls: n,
                } => (name_q(c)?, descending, n),
                IndexKey::Expression {
                    sql,
                    descending,
                    nulls: n,
                } => (merge::paren(sql), descending, n),
            };
            Ok(format!(
                "{head}{}{}",
                if *descending { " DESC" } else { "" },
                nulls(order)
            ))
        })
        .collect::<R<Vec<_>>>()?;
    let include = spec
        .include
        .iter()
        .map(|c| name_q(c))
        .collect::<R<Vec<_>>>()?;
    let index_name = match &spec.name {
        Some(n) => name(n)?,
        None => "_",
    };
    let model = Index {
        oid: Oid(0),
        name: index_name.to_string(),
        method: method.to_string(),
        unique: spec.unique,
        nulls_not_distinct: spec.nulls_not_distinct,
        keys,
        include,
        predicate: spec.predicate.clone(),
        options: Vec::new(),
        valid: true,
        comment: None,
    };
    let named = merge::create_index(&model, index_name, &tq, spec.concurrently);
    if spec.name.is_some() {
        return Ok(named);
    }
    // `CREATE [UNIQUE ]INDEX [CONCURRENTLY ]_ ON …` without the placeholder name.
    let head = format!(
        "CREATE {}INDEX {}",
        if spec.unique { "UNIQUE " } else { "" },
        if spec.concurrently {
            "CONCURRENTLY "
        } else {
            ""
        }
    );
    let rest = named
        .strip_prefix(&head)
        .and_then(|r| r.strip_prefix("_ "))
        .ok_or(DdlErrorKind::Unsupported("index layout"))?;
    Ok(format!("{head}{rest}"))
}

/// A name Postgres accepts: non-empty, at most 63 bytes (NAMEDATALEN - 1), no NUL.
pub(super) fn name(n: &str) -> R<&str> {
    if n.is_empty() || n.len() > 63 || n.contains('\0') {
        Err(DdlErrorKind::InvalidName(n.to_string()))
    } else {
        Ok(n)
    }
}

/// A validated, quoted name.
pub(super) fn name_q(n: &str) -> R<String> {
    Ok(ident(name(n)?))
}

/// `schema.table`, validated and quoted.
pub(super) fn table_q(t: &TableRef) -> R<String> {
    Ok(qualified(name(&t.schema)?, name(&t.name)?))
}

/// `schema.name` + `(m1,m2)` when it has modifiers + `[]` when it's an array (P3).
pub(super) fn type_sql(t: &TypeSpec) -> R<String> {
    let mut s = qualified(name(&t.schema)?, name(&t.name)?);
    if !t.modifiers.is_empty() {
        let mods: Vec<String> = t.modifiers.iter().map(i32::to_string).collect();
        s.push_str(&format!("({})", mods.join(",")));
    }
    if t.array {
        s.push_str("[]");
    }
    Ok(s)
}

/// `(a, b)` of validated, quoted column names; empty is `Empty("columns")`.
fn column_list(columns: &[String]) -> R<String> {
    if columns.is_empty() {
        return Err(DdlErrorKind::Empty("columns"));
    }
    let names = columns.iter().map(|c| name_q(c)).collect::<R<Vec<_>>>()?;
    Ok(format!("({})", names.join(", ")))
}

fn generation(kind: &IdentityKind) -> &'static str {
    match kind {
        IdentityKind::Always => "ALWAYS",
        IdentityKind::ByDefault => "BY DEFAULT",
    }
}

/// A column definition, through the merge planner's `column_def`. A new identity column
/// renders `GENERATED … AS IDENTITY` without sequence options (Postgres creates and names the
/// sequence), in the place `column_def` puts an identity clause.
fn column_def(c: &ColumnSpec, schema: &str) -> R<String> {
    let model = Column {
        attnum: 0,
        name: name(&c.name)?.to_string(),
        data_type: type_sql(&c.data_type)?,
        type_oid: Oid(0),
        collation: c
            .collation
            .as_ref()
            .map(|coll| Ok::<_, DdlErrorKind>(qualified(name(&coll.schema)?, name(&coll.name)?)))
            .transpose()?,
        default: c.default.clone(),
        not_null: c.not_null,
        identity: None,
        generated: c.generated.clone(),
        comment: None,
        grants: Vec::new(),
    };
    let full = merge::column_def(&model, schema, true);
    let Some(kind) = &c.identity else {
        return Ok(full);
    };
    // `name type [COLLATE c]`, which `full` starts with; the identity clause follows it.
    let head = merge::column_def(
        &Column {
            default: None,
            generated: None,
            ..model.clone()
        },
        schema,
        false,
    );
    debug_assert!(
        full.starts_with(&head),
        "plan::ddl::column_def must render `name type [COLLATE c]` first, whatever follows: \
         {full:?} doesn't start with {head:?}"
    );
    Ok(format!(
        "{head} GENERATED {} AS IDENTITY{}",
        generation(kind),
        &full[head.len()..]
    ))
}

fn fk_action(a: FkAction) -> &'static str {
    match a {
        FkAction::NoAction => "NO ACTION",
        FkAction::Restrict => "RESTRICT",
        FkAction::Cascade => "CASCADE",
        FkAction::SetNull => "SET NULL",
        FkAction::SetDefault => "SET DEFAULT",
    }
}

/// `[a-z_][a-z0-9_]*`.
fn valid_method(m: &str) -> bool {
    let mut bytes = m.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z' | b'_'))
        && bytes.all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_'))
}

/// An operator name: Postgres's operator characters only, at most 63 bytes, and no `--` or
/// `/*` anywhere (they start comments, so no operator can contain them).
fn valid_operator(op: &str) -> bool {
    !op.is_empty()
        && op.len() <= 63
        && op.bytes().all(|b| b"+-*/<>=~!@#%^&|`?".contains(&b))
        && !op.contains("--")
        && !op.contains("/*")
}

/// `[CONSTRAINT name] <kind …> [NOT VALID]`, as in `CREATE TABLE` and `ADD`.
fn constraint_def(k: &ConstraintSpec) -> R<String> {
    let mut s = match &k.name {
        Some(n) => format!("CONSTRAINT {} ", name_q(n)?),
        None => String::new(),
    };
    match &k.kind {
        ConstraintKindSpec::PrimaryKey { columns } => {
            s.push_str(&format!("PRIMARY KEY {}", column_list(columns)?));
        }
        ConstraintKindSpec::Unique {
            columns,
            nulls_not_distinct,
        } => {
            s.push_str("UNIQUE ");
            if *nulls_not_distinct {
                s.push_str("NULLS NOT DISTINCT ");
            }
            s.push_str(&column_list(columns)?);
        }
        ConstraintKindSpec::ForeignKey {
            columns,
            references,
            referenced_columns,
            on_delete,
            on_update,
            deferrable,
            initially_deferred,
        } => {
            s.push_str(&format!(
                "FOREIGN KEY {} REFERENCES {}",
                column_list(columns)?,
                table_q(references)?
            ));
            if !referenced_columns.is_empty() {
                s.push_str(&format!(" {}", column_list(referenced_columns)?));
            }
            if *on_delete != FkAction::NoAction {
                s.push_str(&format!(" ON DELETE {}", fk_action(*on_delete)));
            }
            if *on_update != FkAction::NoAction {
                s.push_str(&format!(" ON UPDATE {}", fk_action(*on_update)));
            }
            if *deferrable {
                s.push_str(" DEFERRABLE");
            }
            if *initially_deferred {
                s.push_str(" INITIALLY DEFERRED");
            }
        }
        ConstraintKindSpec::Check { expression } => {
            s.push_str(&format!("CHECK {}", merge::paren(expression)));
        }
        ConstraintKindSpec::Exclusion {
            method,
            elements,
            predicate,
        } => {
            if !valid_method(method) {
                return Err(DdlErrorKind::InvalidMethod(method.clone()));
            }
            if elements.is_empty() {
                return Err(DdlErrorKind::Empty("columns"));
            }
            let mut parts = Vec::with_capacity(elements.len());
            for e in elements {
                if !valid_operator(&e.operator.name) {
                    return Err(DdlErrorKind::InvalidOperator(e.operator.name.clone()));
                }
                parts.push(format!(
                    "{} WITH {}.{}",
                    name_q(&e.column)?,
                    name_q(&e.operator.schema)?,
                    e.operator.name
                ));
            }
            s.push_str(&format!("EXCLUDE USING {method} ({})", parts.join(", ")));
            if let Some(p) = predicate {
                s.push_str(&format!(" WHERE {}", merge::paren(p)));
            }
        }
    }
    if k.not_valid {
        s.push_str(" NOT VALID");
    }
    Ok(s)
}

/// `CREATE POLICY …`, through the merge planner's `create_policy`.
pub(super) fn policy_sql(p: &PolicySpec, table_q: &str) -> R<String> {
    let roles = p
        .roles
        .iter()
        .map(|r| {
            Ok(RoleRef::Role {
                oid: Oid(0),
                name: role(r)?.to_string(),
            })
        })
        .collect::<R<Vec<_>>>()?;
    let model = Policy {
        oid: Oid(0),
        name: name(&p.name)?.to_string(),
        command: match p.command {
            PolicyCommandSpec::All => PolicyCommand::All,
            PolicyCommandSpec::Select => PolicyCommand::Select,
            PolicyCommandSpec::Insert => PolicyCommand::Insert,
            PolicyCommandSpec::Update => PolicyCommand::Update,
            PolicyCommandSpec::Delete => PolicyCommand::Delete,
        },
        permissive: p.permissive,
        roles,
        using: p.using.clone(),
        with_check: p.with_check.clone(),
        comment: None,
    };
    Ok(merge::create_policy(&model, table_q))
}

pub(crate) fn privilege_keyword(p: Privilege) -> &'static str {
    match p {
        Privilege::Select => "SELECT",
        Privilege::Insert => "INSERT",
        Privilege::Update => "UPDATE",
        Privilege::Delete => "DELETE",
        Privilege::Truncate => "TRUNCATE",
        Privilege::References => "REFERENCES",
        Privilege::Trigger => "TRIGGER",
        Privilege::Maintain => "MAINTAIN",
    }
}

/// One `GRANT` per privilege (and per column, inside each privilege), through the merge
/// planner's `grant_sql` (P4).
pub(super) fn grant_statements(g: &TableGrant, table_q: &str) -> R<Vec<String>> {
    // `grant_sql` refuses only a privilege that isn't upper-case letters.
    per_privilege(g, |model, column| merge::grant_sql(model, table_q, column))
}

/// One `REVOKE` per privilege and column, through `revoke_sql`; `with_grant_option` revokes
/// only the grant option (`REVOKE GRANT OPTION FOR …`).
fn revoke_statements(g: &TableGrant, table_q: &str) -> R<Vec<String>> {
    per_privilege(g, |model, column| {
        merge::revoke_sql(model, table_q, column, g.with_grant_option)
    })
}

/// `sql` for each privilege, and inside each privilege for each column (or the whole table).
fn per_privilege(
    g: &TableGrant,
    sql: impl Fn(&Grant, Option<&str>) -> Option<String>,
) -> R<Vec<String>> {
    if g.privileges.is_empty() {
        return Err(DdlErrorKind::Empty("privileges"));
    }
    let grantee = match &g.grantee {
        Grantee::Public => RoleRef::Public,
        Grantee::Role(r) => RoleRef::Role {
            oid: Oid(0),
            name: role(r)?.to_string(),
        },
    };
    for c in &g.columns {
        name(c)?;
    }
    let mut out = Vec::new();
    for p in &g.privileges {
        let model = Grant {
            grantor: NamedRole {
                oid: Oid(0),
                name: String::new(),
            },
            grantee: grantee.clone(),
            privilege: privilege_keyword(*p).to_string(),
            grantable: g.with_grant_option,
        };
        let columns: Vec<Option<&str>> = if g.columns.is_empty() {
            vec![None]
        } else {
            g.columns.iter().map(|c| Some(c.as_str())).collect()
        };
        for column in columns {
            out.push(sql(&model, column).ok_or(DdlErrorKind::Unsupported("privilege"))?);
        }
    }
    Ok(out)
}

fn column_comment(table_q: &str, column: &str, text: &Option<String>) -> R<String> {
    Ok(merge::comment(
        "COLUMN",
        &format!("{table_q}.{}", name_q(column)?),
        text,
    ))
}

fn create_table(c: &CreateTable, ctx: &RenderContext) -> R<Vec<Stmt>> {
    let tq = table_q(&c.table)?;
    if c.columns.is_empty() {
        return Err(DdlErrorKind::Empty("columns"));
    }
    let mut elements = c
        .columns
        .iter()
        .map(|col| column_def(col, &c.table.schema))
        .collect::<R<Vec<_>>>()?;
    if !c.primary_key.is_empty() {
        elements.push(format!("PRIMARY KEY {}", column_list(&c.primary_key)?));
    }
    for k in &c.constraints {
        elements.push(constraint_def(k)?);
    }
    let mut sqls = vec![format!("CREATE TABLE {tq} ({})", elements.join(", "))];
    if c.comment.is_some() {
        sqls.push(merge::comment("TABLE", &tq, &c.comment));
    }
    for col in &c.columns {
        if col.comment.is_some() {
            sqls.push(column_comment(&tq, &col.name, &col.comment)?);
        }
    }
    if let Some(rls) = &c.rls {
        if rls.enabled {
            sqls.push(format!("ALTER TABLE {tq} ENABLE ROW LEVEL SECURITY"));
        }
        if rls.forced {
            sqls.push(format!("ALTER TABLE {tq} FORCE ROW LEVEL SECURITY"));
        }
    }
    for p in &c.policies {
        sqls.push(policy_sql(p, &tq)?);
    }
    for g in &c.grants {
        check_table_grant(g, false, ctx)?;
        sqls.extend(grant_statements(g, &tq)?);
    }
    if let Some(owner) = &c.owner {
        sqls.push(format!("ALTER TABLE {tq} OWNER TO {}", ident(role(owner)?)));
    }
    Ok(sqls
        .into_iter()
        .map(|sql| Stmt::on(&c.table, LockLevel::None, sql))
        .collect())
}

/// A default `ADD COLUMN` stores without rewriting the table: a number, a single-quoted string
/// without a cast, `true`, `false` or `null` (any case). Anything else may be volatile.
fn plain_literal(default: &str) -> bool {
    let d = default.trim();
    if ["true", "false", "null"]
        .iter()
        .any(|k| d.eq_ignore_ascii_case(k))
    {
        return true;
    }
    if let Some(inner) = d
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
    {
        return !inner.replace("''", "").contains('\'');
    }
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let n = d.strip_prefix('-').unwrap_or(d);
    match n.split_once('.') {
        Some((int, frac)) => digits(int) && digits(frac),
        None => digits(n),
    }
}

fn add_column(table: &TableRef, column: &ColumnSpec) -> R<Vec<Stmt>> {
    let tq = table_q(table)?;
    let mut add = Stmt::on(
        table,
        LockLevel::AccessExclusive,
        format!(
            "ALTER TABLE {tq} ADD COLUMN {}",
            column_def(column, &table.schema)?
        ),
    );
    // A type outside `pg_catalog` may be a domain with constraints, which Postgres checks by
    // rewriting the table even without a default: unknown counts as rewriting.
    add.rewrites_table = column.data_type.schema != "pg_catalog"
        || column.identity.is_some()
        || column.generated.is_some()
        || column.default.as_deref().is_some_and(|d| !plain_literal(d));
    let mut out = vec![add];
    if column.comment.is_some() {
        out.push(Stmt::on(
            table,
            LockLevel::ShareUpdateExclusive,
            column_comment(&tq, &column.name, &column.comment)?,
        ));
    }
    Ok(out)
}

/// The column's current type (`format_type` text) from the context, if the context has it.
fn current_type<'a>(ctx: &'a RenderContext, table: &TableRef, column: &str) -> Option<&'a str> {
    ctx.table(table)?
        .table
        .columns
        .iter()
        .find(|c| c.name == column)
        .map(|c| c.data_type.as_str())
}

/// `n` of `prefix(n)`, or `None` (no modifier) of exactly `prefix`; `Err` for anything else.
fn modifiers_of<'a>(text: &'a str, prefix: &str) -> Result<Option<&'a str>, ()> {
    let rest = text.strip_prefix(prefix).ok_or(())?;
    if rest.is_empty() {
        return Ok(None);
    }
    rest.strip_prefix('(')
        .and_then(|r| r.strip_suffix(')'))
        .map(Some)
        .ok_or(())
}

/// Whether `ALTER COLUMN … TYPE new` (no `USING`, no collation) keeps the table's storage: only
/// the binary-coercible widenings below. Anything else, or an unknown current type, rewrites.
///
/// | current                  | new (`pg_catalog`, not an array)            |
/// |--------------------------|---------------------------------------------|
/// | `character varying(n)`   | `varchar(m ≥ n)`, `varchar`, `text`         |
/// | `text`                   | `varchar`                                   |
/// | `numeric(p,s)`           | `numeric(p2 ≥ p, s)`, `numeric`             |
fn in_place_type_change(current: Option<&str>, new: &TypeSpec) -> bool {
    let Some(current) = current else {
        return false;
    };
    if new.schema != "pg_catalog" || new.array {
        return false;
    }
    let int = |s: &str| s.trim().parse::<i32>().ok();
    match (new.name.as_str(), new.modifiers.as_slice()) {
        ("varchar", mods) => {
            if current == "text" {
                return mods.is_empty();
            }
            let Ok(Some(n)) = modifiers_of(current, "character varying") else {
                return false;
            };
            let Some(n) = int(n) else {
                return false;
            };
            match mods {
                [] => true,
                [m] => *m >= n,
                _ => false,
            }
        }
        ("text", []) => matches!(modifiers_of(current, "character varying"), Ok(Some(_))),
        ("numeric", mods) => {
            let Ok(Some(ps)) = modifiers_of(current, "numeric") else {
                return false;
            };
            let Some((p, s)) = ps
                .split_once(',')
                .and_then(|(p, s)| Some((int(p)?, int(s)?)))
            else {
                return false;
            };
            match mods {
                [] => true,
                [p2] => *p2 >= p && s == 0,
                [p2, s2] => *p2 >= p && *s2 == s,
                _ => false,
            }
        }
        _ => false,
    }
}

/// In the order Postgres accepts: `[DROP DEFAULT]` (when the type and the default both change:
/// `USING` isn't applied to the old default), `[DROP IDENTITY]` (an identity column refuses
/// `SET DEFAULT` and `DROP NOT NULL`), `TYPE …` (with `COLLATE` and `USING`), the default (unless
/// already dropped), not-null (`ADD … AS IDENTITY` needs NOT NULL first), `ADD`/`SET GENERATED`,
/// comment. Only `TYPE` rewrites.
fn alter_column(
    table: &TableRef,
    column: &str,
    change: &ColumnChange,
    current_type: Option<&str>,
) -> R<Vec<Stmt>> {
    if change.data_type.is_none() && (change.using.is_some() || change.collation.is_some()) {
        return Err(DdlErrorKind::Unsupported(
            "using or collation without a type",
        ));
    }
    let tq = table_q(table)?;
    let alter = format!("ALTER TABLE {tq} ALTER COLUMN {}", name_q(column)?);
    let mut out = Vec::new();
    let mut push = |sql: String, rewrites_table: bool| {
        let mut s = Stmt::on(table, LockLevel::AccessExclusive, sql);
        s.rewrites_table = rewrites_table;
        out.push(s);
    };
    let drop_default_first = change.data_type.is_some() && change.default.is_some();
    if drop_default_first {
        push(format!("{alter} DROP DEFAULT"), false);
    }
    if let Some(IdentityChange::Drop) = &change.identity {
        push(format!("{alter} DROP IDENTITY"), false);
    }
    if let Some(t) = &change.data_type {
        let mut sql = format!("{alter} TYPE {}", type_sql(t)?);
        if let Some(coll) = &change.collation {
            sql.push_str(&format!(
                " COLLATE {}",
                qualified(name(&coll.schema)?, name(&coll.name)?)
            ));
        }
        if let Some(using) = &change.using {
            sql.push_str(&format!(" USING {using}"));
        }
        let rewrites = change.using.is_some()
            || change.collation.is_some()
            || !in_place_type_change(current_type, t);
        push(sql, rewrites);
    }
    match &change.default {
        Some(DefaultChange::Set(expr)) => push(format!("{alter} SET DEFAULT {expr}"), false),
        Some(DefaultChange::Drop) if !drop_default_first => {
            push(format!("{alter} DROP DEFAULT"), false)
        }
        Some(DefaultChange::Drop) | None => {}
    }
    match change.not_null {
        Some(true) => push(format!("{alter} SET NOT NULL"), false),
        Some(false) => push(format!("{alter} DROP NOT NULL"), false),
        None => {}
    }
    match &change.identity {
        Some(IdentityChange::Add(kind)) => push(
            format!("{alter} ADD GENERATED {} AS IDENTITY", generation(kind)),
            false,
        ),
        Some(IdentityChange::Set(kind)) => {
            push(format!("{alter} SET GENERATED {}", generation(kind)), false)
        }
        Some(IdentityChange::Drop) | None => {}
    }
    if let Some(c) = &change.comment {
        let text = match c {
            CommentChange::Set(t) => Some(t.clone()),
            CommentChange::Clear => None,
        };
        out.push(Stmt::on(
            table,
            LockLevel::ShareUpdateExclusive,
            column_comment(&tq, column, &text)?,
        ));
    }
    if out.is_empty() {
        return Err(DdlErrorKind::Empty("change"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_specs_that_are_not_names_are_refused() {
        for r in [
            "public",
            "PUBLIC",
            "none",
            "Current_User",
            "session_user",
            "current_role",
        ] {
            assert_eq!(role(r), Err(DdlErrorKind::InvalidName(r.to_string())));
        }
        assert_eq!(role("publicist"), Ok("publicist"));
        let notes = TableRef {
            schema: "public".into(),
            name: "notes".into(),
        };
        let op = Operation::SetTableOwner {
            table: notes.clone(),
            owner: "public".into(),
        };
        assert!(operation(&op, &RenderContext::default(), true).is_err());
        let op = Operation::Grant {
            table: notes,
            grant: TableGrant {
                columns: vec![],
                privileges: vec![Privilege::Select],
                grantee: Grantee::Role("none".into()),
                with_grant_option: false,
            },
        };
        assert!(operation(&op, &RenderContext::default(), true).is_err());
    }

    #[test]
    fn in_place_type_changes_need_a_known_current_type() {
        let ty = |name: &str, modifiers: &[i32]| TypeSpec {
            schema: "pg_catalog".into(),
            name: name.into(),
            modifiers: modifiers.to_vec(),
            array: false,
        };
        assert!(in_place_type_change(
            Some("numeric(10,0)"),
            &ty("numeric", &[12])
        ));
        assert!(!in_place_type_change(
            Some("numeric(10,2)"),
            &ty("numeric", &[12])
        ));
        assert!(!in_place_type_change(
            Some("character varying"),
            &ty("varchar", &[10])
        ));
        assert!(!in_place_type_change(
            Some("character varying(20)[]"),
            &ty("text", &[])
        ));
        assert!(!in_place_type_change(None, &ty("text", &[])));
        let mut arr = ty("varchar", &[]);
        arr.array = true;
        assert!(!in_place_type_change(Some("text"), &arr));
    }
}
