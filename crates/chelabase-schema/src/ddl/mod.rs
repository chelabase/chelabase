//! The table editor's DDL renderer: structured [`Operation`]s become SQL statements, each with
//! the lock it takes and whether it may rewrite its table. Pure: no catalog access; what it
//! needs to know about existing tables comes in the [`RenderContext`].
//!
//! Names (tables, columns, constraints, roles, types, collations) are quoted through
//! `sql::ident` / `sql::qualified`; expressions (defaults, checks, `USING`, predicates, policy
//! clauses) are the caller's SQL text, used verbatim once it checks as exactly one expression
//! ([`DdlErrorKind::InvalidExpression`] otherwise). Objects the merge planner also renders
//! (columns, policies, grants, comments) go through its renderers (`plan::ddl`), so a merge
//! and the editor produce the same SQL for the same object.

mod dependents;
mod notices;
mod render;
#[cfg(test)]
mod tests;

pub use dependents::{
    TargetParts, dependents_hash, dependents_sql, drop_target_label, parse_dependents,
    parse_target_parts, target_parts_sql,
};
pub use notices::cascaded_objects;
pub use render::privilege_op_schema;
pub(crate) use render::{
    privilege_keyword, privilege_name_keyword, schema_privilege, sequence_privilege,
};

use std::collections::BTreeMap;

use chelabase_types::access::act_as_reserved;
use chelabase_types::table_api::{
    ColumnSpec, ConstraintKindSpec, ConstraintSpec, DefaultChange, IndexKey, LockLevel, Operation,
    PolicySpec, PrivilegeTarget, SkipReason, TableRef,
};

use crate::model::Table;
use crate::sql::qualified;

/// A table the request touches, as the catalog has it now.
#[derive(Clone, Debug, PartialEq)]
pub struct ContextTable {
    pub table: Table,
    /// `pg_total_relation_size`.
    pub total_bytes: i64,
}

/// What the renderer knows beyond the operations themselves.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RenderContext {
    pub tables: Vec<ContextTable>,
    /// Drops end `CASCADE` instead of `RESTRICT` (the request confirmed its dependents).
    pub cascade: bool,
    /// The `regprocedure` text per function OID, resolved by the orchestrator in the apply
    /// transaction (P11); a function op whose OID is missing is refused.
    pub resolved_functions: BTreeMap<u32, String>,
    /// Object owners, filled in the transaction for an owner run (empty for a role run).
    pub owners: BTreeMap<PrivilegeTarget, String>,
    /// The database owner for an owner run, the acting role for a role run.
    pub session_role: String,
    /// `server_version_num`: `MAINTAIN` is refused below 170000.
    pub server_version_num: i32,
}

impl RenderContext {
    /// The context's model of `table`, if the orchestrator read one.
    pub fn table(&self, table: &TableRef) -> Option<&ContextTable> {
        self.tables
            .iter()
            .find(|t| t.table.schema == table.schema && t.table.name == table.name)
    }
}

/// One SQL statement of the request, in execution order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderedStatement {
    pub sql: String,
    /// The index of the operation it comes from.
    pub op_index: usize,
    /// The statement may rewrite the whole table (unknown counts as rewriting).
    pub rewrites_table: bool,
    pub lock: LockLevel,
    /// Runs outside a transaction (`CREATE`/`DROP INDEX CONCURRENTLY`).
    pub concurrently: bool,
    /// The table the statement touches (`None` for enums).
    pub table: Option<TableRef>,
    /// The role the statement runs as (`SET LOCAL ROLE`), in an owner run only (S4).
    pub run_as: Option<String>,
}

/// An operation the renderer refuses, with its index in the request.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("operation {op_index}: {kind}")]
pub struct DdlError {
    pub op_index: usize,
    pub kind: DdlErrorKind,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DdlErrorKind {
    /// Empty, longer than 63 bytes, or containing NUL.
    #[error("invalid name {0:?}")]
    InvalidName(String),
    /// A list or change that must not be empty.
    #[error("{0} must not be empty")]
    Empty(&'static str),
    #[error("concurrently is allowed only on a request's single operation")]
    ConcurrentlyNotAlone,
    #[error("invalid access method {0:?}")]
    InvalidMethod(String),
    #[error("invalid operator {0:?}")]
    InvalidOperator(String),
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
    /// A caller expression (a default, a generated column, a `USING` cast, a check, an index
    /// predicate or key expression, a policy clause, an exclusion predicate) that isn't exactly
    /// one SQL expression: empty, a comment, a `;`, unbalanced brackets, a top-level comma, or
    /// text that doesn't parse as one expression. Carries the text as given.
    #[error("not a single SQL expression: {0:?}")]
    InvalidExpression(String),
    /// A drop whose target (for a column: the column or its table) operation `by` of the same
    /// request created, renamed, moved, retyped or dropped: what the catalog says about it (its
    /// dependents, its lock) describes another object. Run the drop in its own request.
    #[error(
        "the drop's target was changed by operation {by} of the same request; drop it in a separate request"
    )]
    ChangedEarlier { by: usize },
    /// A grantee the privilege ops refuse (MH-28).
    #[error("grantee not allowed: {0:?}")]
    GranteeNotAllowed(String),
    /// A `FOR ROLE` the default-privilege op refuses (MH-28).
    #[error("FOR ROLE not allowed: {0:?}")]
    ForRoleNotAllowed(String),
    /// A privilege the server doesn't have (`MAINTAIN` below Postgres 17).
    #[error("privilege not supported by this server: {0}")]
    PrivilegeNotSupported(&'static str),
    /// A function op whose OID the orchestrator didn't resolve (P11).
    #[error("function {0} isn't resolved")]
    UnresolvedFunction(u32),
    /// T4-5: a privilege op on a schema the hub never shows ([`hidden_schema`]).
    #[error("privileges in schema {0:?} can't be changed: it's a system schema")]
    HiddenSchema(String),
}

/// T4-5: the schemas the hub never shows and whose privileges its ops never change: Postgres's
/// own (`pg_…`: `pg_catalog`, `pg_toast`, `pg_temp_<n>`, …; Postgres reserves the prefix),
/// `information_schema`, and Chelabase's (`chelabase…`). Extension schemas need the catalog
/// (the orchestrator checks them).
pub fn hidden_schema(schema: &str) -> bool {
    schema.starts_with("pg_") || schema == "information_schema" || schema.starts_with("chelabase")
}

/// The privilege ops' grantee allow-list (MH-28, S5): never `owner` (the database owner in an
/// owner run, the session role otherwise), an [`act_as_reserved`] name (`pg_…`, `cb_m_…`,
/// `chelabase_…`, `postgres`, `owner`, `authenticator`, `public`, …; `anon` and
/// `authenticated` are allowed), or a `current_user`-like keyword. `PUBLIC` as a grantee is
/// [`Grantee::Public`](chelabase_types::table_api::Grantee::Public), allowed only in revokes by
/// the renderer. Personal logins are refused by the orchestrator (they need the metadata).
pub fn privilege_grantee<'a>(name: &'a str, owner: &str) -> Result<&'a str, DdlErrorKind> {
    const KEYWORDS: [&str; 3] = ["current_user", "session_user", "current_role"];
    if name == owner
        || act_as_reserved(name)
        || name.eq_ignore_ascii_case("authenticator")
        || KEYWORDS.iter().any(|k| name.eq_ignore_ascii_case(k))
    {
        return Err(DdlErrorKind::GranteeNotAllowed(name.to_string()));
    }
    Ok(name)
}

/// The object whose ACL (and owner) a privilege op changes: the table of `Grant`, `Revoke` and
/// `SetAllRows` (a policy needs the table's owner too), the schema, sequence or routine of the
/// new ops, the default ACL of `AlterDefaultPrivileges`. `None` for every other op.
pub fn privilege_target(op: &Operation) -> Option<PrivilegeTarget> {
    let relation = |t: &TableRef| PrivilegeTarget::Relation {
        schema: t.schema.clone(),
        name: t.name.clone(),
    };
    match op {
        Operation::Grant { table, .. }
        | Operation::Revoke { table, .. }
        | Operation::SetAllRows { table, .. } => Some(relation(table)),
        Operation::GrantSchema { schema, .. } | Operation::RevokeSchema { schema, .. } => {
            Some(PrivilegeTarget::Schema {
                schema: schema.clone(),
            })
        }
        Operation::GrantSequence { sequence, .. } | Operation::RevokeSequence { sequence, .. } => {
            Some(PrivilegeTarget::Sequence {
                schema: sequence.schema.clone(),
                name: sequence.name.clone(),
            })
        }
        Operation::GrantFunction { function, .. } | Operation::RevokeFunction { function, .. } => {
            Some(PrivilegeTarget::Function { oid: function.oid })
        }
        Operation::AlterDefaultPrivileges {
            for_role,
            schema,
            object,
            ..
        } => Some(PrivilegeTarget::DefaultAcl {
            for_role: for_role.clone(),
            schema: schema.clone(),
            object: *object,
        }),
        _ => None,
    }
}

/// S4: the role `op`'s statements run as. Only in an owner run (`owners` filled): the object's
/// owner when it isn't the session role; for `AlterDefaultPrivileges`, `for_role` (P28: `FOR
/// ROLE` then names the current role, which needs only SET). A role run never sets it.
fn run_as(op: &Operation, ctx: &RenderContext) -> Option<String> {
    if ctx.owners.is_empty() {
        return None;
    }
    let owner = match op {
        Operation::AlterDefaultPrivileges { for_role, .. } => for_role.clone(),
        _ => ctx.owners.get(&privilege_target(op)?)?.clone(),
    };
    (owner != ctx.session_role).then_some(owner)
}

/// An identity whose catalog knowledge an operation may change.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Key {
    Table(String, String),
    Column(String, String, String),
    Enum(String, String),
}

fn table_key(t: &TableRef) -> Key {
    Key::Table(t.schema.clone(), t.name.clone())
}

fn column_key(t: &TableRef, column: &str) -> Key {
    Key::Column(t.schema.clone(), t.name.clone(), column.to_string())
}

/// The identities `op` changes: the ones it creates, renames (old and new names), moves (old
/// and new), retypes or drops.
fn changes(op: &Operation) -> Vec<Key> {
    match op {
        Operation::CreateTable(c) => vec![table_key(&c.table)],
        Operation::RenameTable { table, to } => vec![
            table_key(table),
            Key::Table(table.schema.clone(), to.clone()),
        ],
        Operation::SetTableSchema { table, schema } => vec![
            table_key(table),
            Key::Table(schema.clone(), table.name.clone()),
        ],
        Operation::DropTable { table } => vec![table_key(table)],
        Operation::AddColumn { table, column } => vec![column_key(table, &column.name)],
        Operation::RenameColumn { table, column, to } => {
            vec![column_key(table, column), column_key(table, to)]
        }
        Operation::AlterColumn {
            table,
            column,
            change,
        } if change.data_type.is_some() => vec![column_key(table, column)],
        Operation::DropColumn { table, column } => vec![column_key(table, column)],
        Operation::CreateEnum { schema, name, .. } | Operation::DropEnum { schema, name } => {
            vec![Key::Enum(schema.clone(), name.clone())]
        }
        Operation::RenameEnum { schema, name, to } => vec![
            Key::Enum(schema.clone(), name.clone()),
            Key::Enum(schema.clone(), to.clone()),
        ],
        _ => vec![],
    }
}

/// The identities whose catalog knowledge `op` relies on: a drop's target (a column's table
/// too), a retyped column and its table.
fn relies_on(op: &Operation) -> Vec<Key> {
    match op {
        Operation::DropTable { table } => vec![table_key(table)],
        Operation::DropColumn { table, column } => {
            vec![table_key(table), column_key(table, column)]
        }
        Operation::AlterColumn {
            table,
            column,
            change,
        } if change.data_type.is_some() => vec![table_key(table), column_key(table, column)],
        Operation::DropEnum { schema, name } => vec![Key::Enum(schema.clone(), name.clone())],
        _ => vec![],
    }
}

/// The first operation of the request that changed each identity, in request order.
#[derive(Default)]
struct Changed(Vec<(Key, usize)>);

impl Changed {
    /// The first earlier operation that changed something `op` relies on.
    fn by(&self, op: &Operation) -> Option<usize> {
        let keys = relies_on(op);
        self.0
            .iter()
            .filter(|(k, _)| keys.contains(k))
            .map(|(_, i)| *i)
            .min()
    }

    fn record(&mut self, op: &Operation, op_index: usize) {
        for k in changes(op) {
            if !self.0.iter().any(|(seen, _)| *seen == k) {
                self.0.push((k, op_index));
            }
        }
    }
}

/// Renders `ops` in order. Fails on the first operation the renderer refuses.
///
/// The context (and the dependents query) describe the catalog before the request. So a drop
/// (`DropTable`, `DropColumn`, `DropEnum`) whose target an earlier operation created, renamed,
/// moved, retyped or dropped is refused ([`DdlErrorKind::ChangedEarlier`]), and a type change
/// of a column whose table or column an earlier operation changed counts as rewriting.
pub fn render(ops: &[Operation], ctx: &RenderContext) -> Result<Vec<RenderedStatement>, DdlError> {
    if ops.len() > 1
        && let Some(op_index) = ops.iter().position(render::concurrent)
    {
        return Err(DdlError {
            op_index,
            kind: DdlErrorKind::ConcurrentlyNotAlone,
        });
    }
    let mut out = Vec::new();
    let mut changed = Changed::default();
    let mut all_rows = render::AllRowsOverlay::default();
    for (op_index, op) in ops.iter().enumerate() {
        if let Some(bad) = expressions(op).into_iter().find(|e| !single_expression(e)) {
            return Err(DdlError {
                op_index,
                kind: DdlErrorKind::InvalidExpression(bad.to_string()),
            });
        }
        let changed_by = changed.by(op);
        let drop = matches!(
            op,
            Operation::DropTable { .. } | Operation::DropColumn { .. } | Operation::DropEnum { .. }
        );
        if let (Some(by), true) = (changed_by, drop) {
            return Err(DdlError {
                op_index,
                kind: DdlErrorKind::ChangedEarlier { by },
            });
        }
        let stmts = render::operation_in(op, ctx, changed_by.is_none(), &mut all_rows)
            .map_err(|kind| DdlError { op_index, kind })?;
        changed.record(op, op_index);
        let run_as = run_as(op, ctx);
        out.extend(stmts.into_iter().map(|s| RenderedStatement {
            sql: s.sql,
            op_index,
            rewrites_table: s.rewrites_table,
            lock: s.lock,
            concurrently: s.concurrently,
            table: s.table,
            run_as: run_as.clone(),
        }));
    }
    Ok(out)
}

/// Every caller expression `op` carries, in no particular order.
fn expressions(op: &Operation) -> Vec<&str> {
    fn column(c: &ColumnSpec) -> impl Iterator<Item = &str> {
        c.default.iter().chain(&c.generated).map(String::as_str)
    }
    fn constraint(c: &ConstraintSpec) -> Option<&str> {
        match &c.kind {
            ConstraintKindSpec::Check { expression } => Some(expression),
            ConstraintKindSpec::Exclusion { predicate, .. } => predicate.as_deref(),
            _ => None,
        }
    }
    fn policy(p: &PolicySpec) -> impl Iterator<Item = &str> {
        p.using.iter().chain(&p.with_check).map(String::as_str)
    }
    match op {
        Operation::CreateTable(c) => c
            .columns
            .iter()
            .flat_map(column)
            .chain(c.constraints.iter().filter_map(constraint))
            .chain(c.policies.iter().flat_map(policy))
            .collect(),
        Operation::AddColumn { column: c, .. } => column(c).collect(),
        Operation::AlterColumn { change, .. } => {
            let default = match &change.default {
                Some(DefaultChange::Set(e)) => Some(e.as_str()),
                _ => None,
            };
            change.using.as_deref().into_iter().chain(default).collect()
        }
        Operation::AddConstraint { constraint: c, .. } => constraint(c).into_iter().collect(),
        Operation::CreateIndex(spec) => spec
            .keys
            .iter()
            .filter_map(|k| match k {
                IndexKey::Expression { sql, .. } => Some(sql.as_str()),
                IndexKey::Column { .. } => None,
            })
            .chain(spec.predicate.as_deref())
            .collect(),
        Operation::CreatePolicy { policy: p, .. } => policy(p).collect(),
        Operation::AlterPolicy {
            using, with_check, ..
        } => using.iter().chain(with_check).map(String::as_str).collect(),
        _ => vec![],
    }
}

/// `e` is exactly one SQL expression, so it can't add clauses to the statement it goes into:
/// it scans with no comment, no `;`, no top-level `,`, and brackets that balance without ever
/// closing one opened outside it; and `SELECT (e)` parses as one plain `SELECT` with one
/// target. String literals are single tokens, so a `;` or `--` inside one is fine.
fn single_expression(e: &str) -> bool {
    use pg_query::NodeEnum as N;
    use pg_query::protobuf::Token;
    if e.trim().is_empty() {
        return false;
    }
    let Ok(scan) = pg_query::scan(e) else {
        return false;
    };
    let mut depth = 0i64;
    for t in &scan.tokens {
        match Token::try_from(t.token) {
            Ok(Token::Ascii40 | Token::Ascii91) => depth += 1,
            Ok(Token::Ascii41 | Token::Ascii93) => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            Ok(Token::Ascii44) if depth == 0 => return false,
            Ok(Token::Ascii59 | Token::SqlComment | Token::CComment) | Err(_) => return false,
            _ => {}
        }
    }
    if depth != 0 {
        return false;
    }
    let Ok(parsed) = pg_query::parse(&format!("SELECT ({e})")) else {
        return false;
    };
    let [raw] = parsed.protobuf.stmts.as_slice() else {
        return false;
    };
    let Some(N::SelectStmt(s)) = raw.stmt.as_ref().and_then(|n| n.node.as_ref()) else {
        return false;
    };
    s.target_list.len() == 1
        && s.from_clause.is_empty()
        && s.where_clause.is_none()
        && s.group_clause.is_empty()
        && s.having_clause.is_none()
        && s.window_clause.is_empty()
        && s.values_lists.is_empty()
        && s.sort_clause.is_empty()
        && s.limit_offset.is_none()
        && s.limit_count.is_none()
        && s.locking_clause.is_empty()
        && s.with_clause.is_none()
        && s.into_clause.is_none()
        && s.distinct_clause.is_empty()
        && s.larg.is_none()
        && s.rarg.is_none()
}

/// The existing table an operation works on (`None` for a table it creates, an index named
/// on its own, and enums).
fn existing_table(op: &Operation) -> Option<&TableRef> {
    match op {
        Operation::CreateTable(_)
        | Operation::DropIndex { .. }
        | Operation::RenameIndex { .. }
        | Operation::CreateEnum { .. }
        | Operation::AddEnumValue { .. }
        | Operation::RenameEnumValue { .. }
        | Operation::RenameEnum { .. }
        | Operation::DropEnum { .. } => None,
        // Schemas, sequences, routines and default ACLs aren't tables.
        Operation::GrantSchema { .. }
        | Operation::RevokeSchema { .. }
        | Operation::GrantSequence { .. }
        | Operation::RevokeSequence { .. }
        | Operation::GrantFunction { .. }
        | Operation::RevokeFunction { .. }
        | Operation::AlterDefaultPrivileges { .. } => None,
        Operation::CreateIndex(spec) => Some(&spec.table),
        // The table's policies name the hub's policy (on) or find it (off).
        Operation::SetAllRows { table, .. } => Some(table),
        Operation::RenameTable { table, .. }
        | Operation::SetTableSchema { table, .. }
        | Operation::DropTable { table }
        | Operation::SetTableComment { table, .. }
        | Operation::SetTableOwner { table, .. }
        | Operation::AddColumn { table, .. }
        | Operation::AlterColumn { table, .. }
        | Operation::RenameColumn { table, .. }
        | Operation::DropColumn { table, .. }
        | Operation::AddConstraint { table, .. }
        | Operation::ValidateConstraint { table, .. }
        | Operation::RenameConstraint { table, .. }
        | Operation::DropConstraint { table, .. }
        | Operation::SetRls { table, .. }
        | Operation::CreatePolicy { table, .. }
        | Operation::AlterPolicy { table, .. }
        | Operation::RenamePolicy { table, .. }
        | Operation::DropPolicy { table, .. }
        | Operation::Grant { table, .. }
        | Operation::Revoke { table, .. } => Some(table),
    }
}

/// The existing tables the request works on, deduplicated, in request order: the orchestrator
/// reads their models (and sizes) into the [`RenderContext`] before rendering.
pub fn context_tables(ops: &[Operation]) -> Vec<TableRef> {
    let mut out: Vec<TableRef> = Vec::new();
    for t in ops.iter().filter_map(existing_table) {
        if !out.contains(t) {
            out.push(t.clone());
        }
    }
    out
}

/// The first operation naming an existing table that is in `absent` (the catalog has no
/// relation of that name before the request) and that no earlier operation of the request
/// creates, renames or moves to that name: its index and the table.
pub fn first_missing_table<'a>(
    ops: &'a [Operation],
    absent: &[TableRef],
) -> Option<(usize, &'a TableRef)> {
    let mut made: Vec<TableRef> = Vec::new();
    for (op_index, op) in ops.iter().enumerate() {
        if let Some(t) = existing_table(op)
            && absent.contains(t)
            && !made.contains(t)
        {
            return Some((op_index, t));
        }
        match op {
            Operation::CreateTable(c) => made.push(c.table.clone()),
            Operation::RenameTable { table, to } => made.push(TableRef {
                schema: table.schema.clone(),
                name: to.clone(),
            }),
            Operation::SetTableSchema { table, schema } => made.push(TableRef {
                schema: schema.clone(),
                name: table.name.clone(),
            }),
            _ => {}
        }
    }
    None
}

/// What a dry run executes (`None`) and skips: a `CONCURRENTLY` statement (it can't run in the
/// dry run's transaction); a statement that may rewrite a table over `max_bytes` (from `ctx`;
/// a table it doesn't have counts as 0 bytes); every statement after the first such skip (they
/// may depend on it).
pub fn dry_run_plan(
    stmts: &[RenderedStatement],
    ctx: &RenderContext,
    max_bytes: i64,
) -> Vec<Option<SkipReason>> {
    let mut skipped_rewrite = false;
    stmts
        .iter()
        .map(|s| {
            if skipped_rewrite {
                return Some(SkipReason::AfterSkipped);
            }
            if s.concurrently {
                return Some(SkipReason::Concurrently);
            }
            let bytes = s
                .table
                .as_ref()
                .and_then(|t| ctx.table(t))
                .map_or(0, |t| t.total_bytes);
            if s.rewrites_table && bytes > max_bytes {
                skipped_rewrite = true;
                return Some(SkipReason::LargeTableRewrite);
            }
            None
        })
        .collect()
}

/// The tables the request drops from (`DROP TABLE`, `DROP COLUMN`, `DROP CONSTRAINT`),
/// deduplicated, in request order.
fn drop_tables(ops: &[Operation]) -> Vec<&TableRef> {
    let mut out: Vec<&TableRef> = Vec::new();
    for op in ops {
        if let Operation::DropTable { table }
        | Operation::DropColumn { table, .. }
        | Operation::DropConstraint { table, .. } = op
            && !out.contains(&table)
        {
            out.push(table);
        }
    }
    out
}

/// `LOCK TABLE … IN ACCESS EXCLUSIVE MODE` for each table the request drops from, so the
/// dependents the request confirmed can't change before its drops run.
pub fn lock_sql(ops: &[Operation]) -> Vec<String> {
    drop_tables(ops)
        .into_iter()
        .map(|t| {
            format!(
                "LOCK TABLE {} IN ACCESS EXCLUSIVE MODE",
                qualified(&t.schema, &t.name)
            )
        })
        .collect()
}
