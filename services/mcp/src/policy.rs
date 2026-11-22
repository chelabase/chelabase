//! Operation classes for `execute`: every operation of the management API is `read`, `write`,
//! `destructive` or `excluded`, and four routes take their class from the request body.
//!
//! - `excluded` is never callable from a sandbox: the binary routes, which the CLI bridge
//!   serves instead (and later the MCP's own account routes).
//! - `destructive` needs the caller's explicit flag: anything that removes data, changes who
//!   can do what, creates, replaces or invalidates a credential, or opens an outbound target.
//! - `read` is every `GET` and the `POST` reads; `write` is the rest.
//!
//! The table fails closed: an operation it lacks is `excluded` at run time, the bundle
//! generator refuses to build without a class for every operation, and a test fails when the
//! API and the table disagree.

use Class::{Destructive, Excluded, Read, Write};
use chelabase_query::{Op, ParseMode};
use serde_json::Value;

/// How risky an operation is; decides whether the MCP server runs it. Ordered from least to
/// most strict, so the strictest of several is their `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Class {
    Read,
    Write,
    Destructive,
    Excluded,
}

impl Class {
    /// The name the bundle's `x-class` carries.
    pub fn as_str(self) -> &'static str {
        match self {
            Read => "read",
            Write => "write",
            Destructive => "destructive",
            Excluded => "excluded",
        }
    }
}

/// Classifies a body-dependent route from its JSON body.
pub type Rule = fn(&Value) -> Class;

/// `(operationId, METHOD, class)` for every operation of `openapi/management.json`, in the
/// document's order. For a route in [`RULES`] the class is the one a call gets when its body
/// can't be read: the route's strictest.
pub const TABLE: &[(&str, &str, Class)] = &[
    ("listAccessEvents", "GET", Read),
    ("createDatabase", "POST", Write),
    ("getDatabase", "GET", Read),
    ("deleteDatabase", "DELETE", Destructive),
    ("updateDatabase", "PATCH", Destructive),
    ("listBranches", "GET", Read),
    ("createBranch", "POST", Write),
    ("deleteBranch", "DELETE", Destructive),
    ("updateBranch", "PATCH", Destructive),
    ("getBranchConnection", "GET", Read),
    ("consoleQuery", "POST", Destructive),
    ("getConsoleSchema", "GET", Read),
    ("getDataApiSchema", "GET", Read),
    ("getDataApiTypes", "GET", Read),
    ("listBranchDatabases", "GET", Read),
    ("listFunctions", "GET", Read),
    ("putFunction", "PUT", Destructive),
    ("deleteFunction", "DELETE", Destructive),
    ("updateFunction", "PATCH", Destructive),
    ("listFunctionDeployments", "GET", Read),
    ("createFunctionDeployment", "POST", Excluded),
    ("getServiceLogs", "GET", Read),
    ("listFunctionSchedules", "GET", Read),
    ("createFunctionSchedule", "POST", Destructive),
    ("deleteFunctionSchedule", "DELETE", Destructive),
    ("updateFunctionSchedule", "PATCH", Destructive),
    ("listFunctionSecrets", "GET", Read),
    ("putFunctionSecret", "PUT", Destructive),
    ("deleteFunctionSecret", "DELETE", Destructive),
    ("listFunctionTriggers", "GET", Read),
    ("createFunctionTrigger", "POST", Destructive),
    ("deleteFunctionTrigger", "DELETE", Destructive),
    ("updateFunctionTrigger", "PATCH", Destructive),
    ("listGuardExemptions", "GET", Read),
    ("putGuardExemption", "PUT", Destructive),
    ("deleteGuardExemption", "DELETE", Destructive),
    ("listKeys", "GET", Read),
    ("createKey", "POST", Destructive),
    ("revokeKey", "DELETE", Destructive),
    ("updateKey", "PATCH", Destructive),
    ("rotateKey", "POST", Destructive),
    ("createMerge", "POST", Write),
    ("listObjects", "GET", Read),
    ("updatePgDatabase", "PATCH", Destructive),
    ("enableDataApi", "POST", Destructive),
    ("disableDataApi", "DELETE", Destructive),
    ("getPrivileges", "GET", Read),
    ("resetBranchPassword", "POST", Destructive),
    ("resetBranchToParent", "POST", Destructive),
    ("restoreBranch", "POST", Destructive),
    ("getBranchReview", "GET", Read),
    ("acknowledgeBranchReview", "POST", Destructive),
    ("fixBranchReviewItem", "POST", Destructive),
    ("listRoles", "GET", Read),
    ("createRole", "POST", Destructive),
    ("deleteRole", "DELETE", Destructive),
    ("updateRole", "PATCH", Destructive),
    ("previewRoleDelete", "GET", Read),
    ("resetRolePassword", "POST", Destructive),
    ("listRoleSessions", "GET", Read),
    ("applySchemaChanges", "POST", Destructive),
    ("runSql", "POST", Destructive),
    ("cancelSqlRun", "POST", Write),
    ("getSqlSchema", "GET", Read),
    ("storageListBuckets", "GET", Read),
    ("storageCreateBucket", "POST", Destructive),
    ("storageDeleteBucket", "DELETE", Destructive),
    ("storageUpdateBucket", "PATCH", Destructive),
    ("storageCopyObject", "POST", Destructive),
    ("storageListObjects", "POST", Read),
    ("storageMoveObject", "POST", Destructive),
    ("storageResolveObjects", "POST", Read),
    ("storageSignObject", "POST", Write),
    ("storageDeleteObjects", "DELETE", Destructive),
    ("storageDownloadObject", "GET", Read),
    ("storagePutObject", "PUT", Excluded),
    ("storageUploadObject", "POST", Excluded),
    ("storageDeleteObject", "DELETE", Destructive),
    ("storageListParts", "GET", Read),
    ("storageAbortUpload", "DELETE", Destructive),
    ("storageCompleteUpload", "POST", Excluded),
    ("storageUploadPart", "PUT", Excluded),
    ("storageCreateUpload", "POST", Excluded),
    ("getTable", "GET", Read),
    ("getCell", "POST", Read),
    ("insertDefaultRow", "POST", Write),
    ("listTypes", "GET", Read),
    ("getDatabaseConnection", "GET", Read),
    ("listMerges", "GET", Read),
    ("getMerge", "GET", Read),
    ("updateMerge", "PATCH", Write),
    ("abandonMerge", "POST", Destructive),
    ("applyMerge", "POST", Destructive),
    ("cancelMerge", "POST", Destructive),
    ("finishMerge", "POST", Destructive),
    ("exportMergePlan", "GET", Read),
    ("recomputeMerge", "POST", Write),
    ("rehearseMerge", "POST", Read),
    ("retryMerge", "POST", Destructive),
    ("createPersonalLogin", "PUT", Destructive),
    ("resetPersonalLoginPassword", "POST", Destructive),
    ("listDatabasePersonalLogins", "GET", Read),
    ("deletePersonalLogin", "DELETE", Destructive),
    ("resetDatabasePassword", "POST", Destructive),
    ("listSavedQueries", "GET", Read),
    ("createSavedQuery", "POST", Write),
    ("getSavedQuery", "GET", Read),
    ("deleteSavedQuery", "DELETE", Destructive),
    ("updateSavedQuery", "PATCH", Write),
    ("listStorageWebhooks", "GET", Read),
    ("createStorageWebhook", "POST", Destructive),
    ("getStorageWebhook", "GET", Read),
    ("deleteStorageWebhook", "DELETE", Destructive),
    ("updateStorageWebhook", "PATCH", Destructive),
    ("listStorageWebhookAttempts", "GET", Read),
    ("rotateStorageWebhookSecret", "POST", Destructive),
    ("testStorageWebhook", "POST", Destructive),
    ("getInstanceSettings", "GET", Read),
    ("updateInstanceSettings", "PATCH", Destructive),
    ("createInvite", "POST", Destructive),
    ("getInviteDelivery", "GET", Read),
    ("revokeInvite", "DELETE", Destructive),
    ("resendInvite", "POST", Destructive),
    ("getMe", "GET", Read),
    ("listMembers", "GET", Read),
    ("removeMember", "DELETE", Destructive),
    ("updateMember", "PATCH", Destructive),
    ("approveMember", "POST", Destructive),
    ("declineMember", "POST", Destructive),
    ("listMemberPersonalLogins", "GET", Read),
    ("reinstateMember", "POST", Destructive),
    ("revokeMemberPostgresAccess", "POST", Destructive),
    ("suspendMember", "POST", Destructive),
    ("listMemberTeams", "GET", Read),
    ("listProjects", "GET", Read),
    ("createProject", "POST", Write),
    ("getProject", "GET", Read),
    ("deleteProject", "DELETE", Destructive),
    ("updateProject", "PATCH", Write),
    ("listProjectAccessEvents", "GET", Read),
    ("getAppUsers", "GET", Read),
    ("putAppUsers", "PUT", Destructive),
    ("disableAppUsers", "DELETE", Destructive),
    ("listProjectGrants", "GET", Read),
    ("putProjectGrant", "PUT", Destructive),
    ("deleteProjectGrant", "DELETE", Destructive),
    ("putGrantOverride", "PUT", Destructive),
    ("deleteGrantOverride", "DELETE", Destructive),
    ("createProjectInvite", "POST", Destructive),
    ("listProjectTeamGrants", "GET", Read),
    ("listAppUsers", "GET", Read),
    ("createAppUser", "POST", Destructive),
    ("getAppUser", "GET", Read),
    ("deleteAppUser", "DELETE", Destructive),
    ("updateAppUser", "PATCH", Destructive),
    ("deactivateAppUser", "POST", Destructive),
    ("reactivateAppUser", "POST", Destructive),
    ("listAppUserSessions", "GET", Read),
    ("revokeAppUserSessions", "DELETE", Destructive),
    ("listTeams", "GET", Read),
    ("createTeam", "POST", Write),
    ("getTeam", "GET", Read),
    ("deleteTeam", "DELETE", Destructive),
    ("updateTeam", "PATCH", Write),
    ("putTeamGrant", "PUT", Destructive),
    ("deleteTeamGrant", "DELETE", Destructive),
    ("putTeamMember", "PUT", Destructive),
    ("deleteTeamMember", "DELETE", Destructive),
    ("updateTenant", "PATCH", Destructive),
];

/// The body-dependent routes: `(operationId, the rule in one sentence, the rule)`.
pub const RULES: &[(&str, &str, Rule)] = &[
    (
        "consoleQuery",
        "Decided by `tree`: a select or count is read, an insert or an update with a `where` is write, and a delete, a call, an update of all rows, a batch holding one of those, or a tree that doesn't parse is destructive.",
        console_query,
    ),
    (
        "runSql",
        "Read only with `read_only: true` (Postgres holds the run read-only); otherwise destructive, whatever the SQL says. Any `copy_out` but false is excluded.",
        run_sql,
    ),
    (
        "storageCreateBucket",
        "Destructive when `public` is anything but false or null; otherwise write.",
        storage_create_bucket,
    ),
    (
        "applySchemaChanges",
        "A dry run (`dry_run: true`) is read, or write when any operation carries a SQL expression (a default, generated column, check, exclusion, index expression or predicate, policy, or `using`). A real run is destructive when any operation carries one, drops something, changes a privilege, owner, schema, policy or row-level security, changes a column's type, default or identity or lifts NOT NULL, or isn't known; otherwise write.",
        schema_changes,
    ),
];

/// The table's class for `operation_id` called with `method` (any case), or `None` when the
/// table has no such operation.
pub fn static_class(operation_id: &str, method: &str) -> Option<Class> {
    TABLE
        .iter()
        .find(|(id, m, _)| *id == operation_id && m.eq_ignore_ascii_case(method))
        .map(|(_, _, class)| *class)
}

/// The one-sentence rule of a body-dependent route.
pub fn rule_of(operation_id: &str) -> Option<&'static str> {
    RULES
        .iter()
        .find(|(id, _, _)| *id == operation_id)
        .map(|(_, sentence, _)| *sentence)
}

/// The class of one call. An operation the table lacks is `excluded`; an excluded operation
/// stays excluded whatever the body says; a body-dependent route without a readable body
/// (`None`: absent or not JSON) gets its strictest class.
pub fn classify(operation_id: &str, method: &str, body: Option<&Value>) -> Class {
    let Some(class) = static_class(operation_id, method) else {
        return Excluded;
    };
    if class == Excluded {
        return Excluded;
    }
    let rule = RULES
        .iter()
        .find(|(id, _, _)| *id == operation_id)
        .map(|(_, _, rule)| rule);
    match (rule, body) {
        (Some(rule), Some(body)) => rule(body),
        _ => class,
    }
}

/// Whether a read-only connection may make a call of `class`: reads only.
pub fn allowed_in_read_only_mode(class: Class) -> bool {
    class == Read
}

/// How the table and an API document disagree.
#[derive(Debug, Default)]
pub struct Coverage {
    /// Operations of the document the table lacks (`operationId METHOD path`).
    pub missing: Vec<String>,
    /// Table entries (and rules) the document lacks (`operationId METHOD`).
    pub unknown: Vec<String>,
    /// Operation ids the table lists more than once.
    pub duplicate: Vec<String>,
}

/// Compares the table with `management` (an OpenAPI document).
pub fn coverage(management: &Value) -> Coverage {
    const METHODS: [&str; 8] = [
        "get", "put", "post", "delete", "options", "head", "patch", "trace",
    ];
    let mut out = Coverage::default();
    let mut seen: Vec<(String, String)> = Vec::new();
    let paths = management.get("paths").and_then(Value::as_object);
    for (path, item) in paths.into_iter().flatten() {
        let Some(item) = item.as_object() else {
            out.missing.push(format!("{path} is not a path item"));
            continue;
        };
        for (method, op) in item.iter().filter(|(m, _)| METHODS.contains(&m.as_str())) {
            let method = method.to_ascii_uppercase();
            match op.get("operationId").and_then(Value::as_str) {
                Some(id) if static_class(id, &method).is_some() => {
                    seen.push((id.to_string(), method));
                }
                Some(id) => out.missing.push(format!("{id} {method} {path}")),
                None => out
                    .missing
                    .push(format!("(no operationId) {method} {path}")),
            }
        }
    }
    for (i, (id, method, _)) in TABLE.iter().enumerate() {
        if !seen.iter().any(|(s, m)| s == id && m == method) {
            out.unknown.push(format!("{id} {method}"));
        }
        if TABLE[..i].iter().any(|(earlier, _, _)| earlier == id) {
            out.duplicate.push((*id).to_string());
        }
    }
    for (id, _, _) in RULES {
        if !TABLE.iter().any(|(t, _, _)| t == id) {
            out.unknown.push(format!("{id} (rule)"));
        }
    }
    out
}

/// `consoleQuery`: parsed with the console's grammar, as the orchestrator parses it.
fn console_query(body: &Value) -> Class {
    let Some(tree) = body.get("tree") else {
        return Destructive;
    };
    let Ok(bytes) = serde_json::to_vec(tree) else {
        return Destructive;
    };
    // A tree this parser refuses is destructive, not write: the orchestrator may be newer
    // than this server and accept an operation this grammar doesn't know.
    match chelabase_query::parse_with(&bytes, ParseMode::Console) {
        Ok(parsed) => op_class(&parsed.op),
        Err(_) => Destructive,
    }
}

fn op_class(op: &Op) -> Class {
    match op {
        Op::Select(_) | Op::Count(_) => Read,
        Op::Insert(_) => Write,
        // An update of every row overwrites the table's values.
        Op::Update(update) if update.all => Destructive,
        Op::Update(_) => Write,
        // A function's effect can't be read from the request.
        Op::Delete(_) | Op::Call(_) => Destructive,
        Op::Batch(ops) => ops.iter().map(op_class).max().unwrap_or(Destructive),
    }
}

/// `runSql`: the body's `read_only` decides, never the SQL text (m5, #349).
fn run_sql(body: &Value) -> Class {
    let Some(body) = body.as_object() else {
        return Destructive;
    };
    // The copy stream is the bridge's (epic `mcp-sealed`).
    if !matches!(
        body.get("copy_out"),
        None | Some(Value::Null | Value::Bool(false))
    ) {
        return Excluded;
    }
    if body.get("read_only") == Some(&Value::Bool(true)) {
        Read
    } else {
        Destructive
    }
}

/// `storageCreateBucket`: a public bucket serves its objects to anyone.
fn storage_create_bucket(body: &Value) -> Class {
    let Some(body) = body.as_object() else {
        return Destructive;
    };
    match body.get("public") {
        None | Some(Value::Null | Value::Bool(false)) => Write,
        Some(_) => Destructive,
    }
}

/// `applySchemaChanges`. A dry run rolls back, but the expressions it evaluates keep their
/// effects outside the transaction (`nextval`, `setval`, dblink), so a dry run is `read` only
/// when no operation runs caller code, and `write` otherwise. A real run is the strictest of
/// its operations; one that runs caller code is destructive.
fn schema_changes(body: &Value) -> Class {
    let Some(body) = body.as_object() else {
        return Destructive;
    };
    let operations = body.get("operations").and_then(Value::as_array);
    if body.get("dry_run") == Some(&Value::Bool(true)) {
        return match operations {
            Some(ops) if !ops.iter().any(runs_code) => Read,
            _ => Write,
        };
    }
    let Some(operations) = operations else {
        return Destructive;
    };
    operations
        .iter()
        .map(|op| {
            if runs_code(op) {
                Destructive
            } else {
                schema_change(op)
            }
        })
        .max()
        .unwrap_or(Write)
}

/// One operation of a schema change: `{"<kind>": {...}}`. Only kinds known to add or rename
/// without changing access are `write`; every other kind, known or not, is destructive.
/// Whether it runs caller code is [`runs_code`]'s question.
fn schema_change(op: &Value) -> Class {
    let Some((kind, fields)) = single_entry(op) else {
        return Destructive;
    };
    match kind.as_str() {
        "rename_table"
        | "set_table_comment"
        | "add_column"
        | "rename_column"
        | "add_constraint"
        | "validate_constraint"
        | "rename_constraint"
        | "create_index"
        | "rename_index"
        | "create_enum"
        | "add_enum_value"
        | "rename_enum_value"
        | "rename_enum" => Write,
        // A new table may carry grants, policies, RLS or another owner.
        "create_table" => {
            let plain = ["grants", "policies", "rls", "owner"]
                .iter()
                .all(|key| empty(fields.get(*key)));
            if fields.is_object() && plain {
                Write
            } else {
                Destructive
            }
        }
        // A comment or a new NOT NULL loses nothing; a new type, a `USING`, an identity, a
        // default, a collation or a lifted NOT NULL may.
        "alter_column" if harmless_column_change(fields) => Write,
        _ => Destructive,
    }
}

/// Whether an operation makes Postgres run code the caller wrote or picked: a SQL expression
/// (a column's `default` or `generated`, a check, an exclusion's predicate, an index's
/// expression key or predicate, a policy's `using` or `with_check`, an `alter_column`'s
/// `using` or new default) or an exclusion's operators. These are the expression slots of
/// `chelabase_schema::ddl::expressions`, which only checks that each is one expression: a
/// function call with arguments of the caller's choosing passes, and Postgres runs it over
/// the table's rows. A shape this doesn't recognise counts as running code.
fn runs_code(op: &Value) -> bool {
    let Some((kind, fields)) = single_entry(op) else {
        return true;
    };
    match kind.as_str() {
        "add_column" => column_runs_code(fields.get("column")),
        "add_constraint" => constraint_runs_code(fields.get("constraint")),
        "create_table" => {
            let columns = match fields.get("columns") {
                Some(Value::Array(columns)) => columns.iter().any(|c| column_runs_code(Some(c))),
                _ => true,
            };
            let constraints = match fields.get("constraints") {
                None | Some(Value::Null) => false,
                Some(Value::Array(cs)) => cs.iter().any(|c| constraint_runs_code(Some(c))),
                Some(_) => true,
            };
            columns || constraints || !empty(fields.get("policies"))
        }
        "create_index" => {
            let keys = match fields.get("keys") {
                None | Some(Value::Null) => false,
                Some(Value::Array(keys)) => !keys.iter().all(|k| {
                    single_entry(k).is_some_and(|(kind, key)| kind == "column" && key.is_object())
                }),
                Some(_) => true,
            };
            keys || !matches!(fields.get("predicate"), None | Some(Value::Null))
        }
        "alter_column" => !harmless_column_change(fields),
        // These name tables, columns, roles, types and values; none holds an expression.
        "rename_table"
        | "set_table_schema"
        | "drop_table"
        | "set_table_comment"
        | "set_table_owner"
        | "rename_column"
        | "drop_column"
        | "validate_constraint"
        | "rename_constraint"
        | "drop_constraint"
        | "drop_index"
        | "rename_index"
        | "set_rls"
        | "rename_policy"
        | "drop_policy"
        | "grant"
        | "revoke"
        | "create_enum"
        | "add_enum_value"
        | "rename_enum_value"
        | "rename_enum"
        | "drop_enum"
        | "grant_schema"
        | "revoke_schema"
        | "grant_sequence"
        | "revoke_sequence"
        | "grant_function"
        | "revoke_function"
        | "alter_default_privileges"
        | "set_all_rows" => false,
        // `create_policy` and `alter_policy` carry `using` and `with_check`; anything else is
        // a kind this grammar doesn't know.
        _ => true,
    }
}

/// A column spec with a `default` or `generated` expression, or one that isn't an object.
fn column_runs_code(column: Option<&Value>) -> bool {
    let Some(column) = column.and_then(Value::as_object) else {
        return true;
    };
    ["default", "generated"]
        .iter()
        .any(|key| !matches!(column.get(*key), None | Some(Value::Null)))
}

/// A constraint other than a primary key, unique or foreign key: a check runs its expression
/// and an exclusion its operators (and predicate) on the table's rows.
fn constraint_runs_code(constraint: Option<&Value>) -> bool {
    let Some(kind) = constraint.and_then(|c| c.get("kind")) else {
        return true;
    };
    !single_entry(kind)
        .is_some_and(|(kind, _)| matches!(kind.as_str(), "primary_key" | "unique" | "foreign_key"))
}

/// An `alter_column` that only sets a comment or adds NOT NULL.
fn harmless_column_change(fields: &Value) -> bool {
    let Some(change) = fields.get("change").and_then(Value::as_object) else {
        return false;
    };
    change.iter().all(|(key, value)| match key.as_str() {
        _ if value.is_null() => true,
        "comment" => true,
        "not_null" => value == &Value::Bool(true),
        _ => false,
    })
}

/// The one entry of an externally tagged value: `{"<kind>": fields}`.
fn single_entry(value: &Value) -> Option<(&String, &Value)> {
    value
        .as_object()
        .filter(|o| o.len() == 1)
        .and_then(|o| o.iter().next())
}

/// Absent, null or an empty list.
fn empty(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => true,
        Some(Value::Array(items)) => items.is_empty(),
        Some(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CONSOLE: (&str, &str) = ("consoleQuery", "POST");
    const RUN_SQL: (&str, &str) = ("runSql", "POST");
    const BUCKET: (&str, &str) = ("storageCreateBucket", "POST");
    const CHANGES: (&str, &str) = ("applySchemaChanges", "POST");

    fn class((id, method): (&str, &str), body: Value) -> Class {
        classify(id, method, Some(&body))
    }

    fn console(tree: Value) -> Class {
        class(CONSOLE, json!({ "tree": tree }))
    }

    fn changes(operations: Value) -> Class {
        class(CHANGES, json!({ "operations": operations }))
    }

    #[test]
    fn console_query_classes() {
        let select = json!({"v": 1, "op": "select", "from": "t"});
        assert_eq!(console(select.clone()), Class::Read);
        assert_eq!(
            console(json!({"v": 1, "op": "count", "from": "t"})),
            Class::Read
        );
        assert_eq!(
            console(json!({"v": 1, "op": "insert", "into": "t", "values": [{"a": 1}]})),
            Class::Write
        );
        assert_eq!(
            console(
                json!({"v": 1, "op": "update", "table": "t", "set": {"a": 1},
                "where": {"col": "a", "eq": 2}})
            ),
            Class::Write
        );
        // An update of every row overwrites the table's values.
        assert_eq!(
            console(json!({"v": 1, "op": "update", "table": "t", "set": {"a": 1}, "all": true})),
            Class::Destructive
        );
        assert_eq!(
            console(json!({"v": 1, "op": "delete", "from": "t", "where": {"col": "a", "eq": 2}})),
            Class::Destructive
        );
        // The route's other fields don't change the class.
        assert_eq!(
            class(
                CONSOLE,
                json!({"role": "app", "database": "db", "tree": select})
            ),
            Class::Read
        );
        // A tree this parser can't read (a newer grammar, or junk) is never a read or write.
        assert_eq!(
            console(json!({"v": 1, "op": "truncate", "table": "t"})),
            Class::Destructive
        );
        assert_eq!(console(json!("select * from t")), Class::Destructive);
    }

    #[test]
    fn console_call_is_destructive() {
        assert_eq!(
            console(json!({"v": 1, "op": "call", "fn": "f", "args": {}})),
            Class::Destructive
        );
    }

    #[test]
    fn console_batch_with_a_delete_is_destructive() {
        let select = json!({"op": "select", "from": "t"});
        let insert = json!({"op": "insert", "into": "t", "values": [{"a": 1}]});
        let delete = json!({"op": "delete", "from": "t", "all": true});
        let call = json!({"op": "call", "fn": "f"});
        let batch = |ops: Value| console(json!({"v": 1, "op": "batch", "ops": ops}));
        assert_eq!(batch(json!([select, select])), Class::Read);
        assert_eq!(batch(json!([select, insert])), Class::Write);
        assert_eq!(batch(json!([select, insert, delete])), Class::Destructive);
        assert_eq!(batch(json!([call, select])), Class::Destructive);
    }

    #[test]
    fn a_missing_body_takes_the_strictest_class() {
        for (id, method) in [CONSOLE, RUN_SQL, BUCKET, CHANGES] {
            assert_eq!(classify(id, method, None), Class::Destructive, "{id}");
            for body in [json!(null), json!("x"), json!([1]), json!(7), json!(true)] {
                assert_eq!(
                    classify(id, method, Some(&body)),
                    Class::Destructive,
                    "{id} {body}"
                );
            }
        }
        assert_eq!(class(CONSOLE, json!({})), Class::Destructive);
        assert_eq!(class(CHANGES, json!({})), Class::Destructive);
        assert_eq!(
            class(CHANGES, json!({"operations": "drop"})),
            Class::Destructive
        );
        // A route without a rule keeps its static class whatever the body says.
        assert_eq!(classify("createTeam", "POST", None), Class::Write);
        assert_eq!(
            classify("createTeam", "POST", Some(&json!({"dry_run": true}))),
            Class::Write
        );
        assert_eq!(classify("getMe", "get", None), Class::Read);
    }

    #[test]
    fn run_sql_class_follows_read_only_and_never_the_sql_text() {
        let run = |extra: Value| {
            let mut body = json!({"run_id": "00000000-0000-0000-0000-000000000000"});
            for sql in ["SELECT 1", "DROP TABLE t", "DELETE FROM t", "garbage ;;"] {
                body["sql"] = sql.into();
                for (k, v) in extra.as_object().unwrap() {
                    body[k] = v.clone();
                }
                let got = class(RUN_SQL, body.clone());
                let first = class(RUN_SQL, {
                    let mut b = body.clone();
                    b["sql"] = "SELECT 1".into();
                    b
                });
                assert_eq!(got, first, "{body}");
            }
            class(RUN_SQL, body)
        };
        assert_eq!(run(json!({"read_only": true})), Class::Read);
        assert_eq!(run(json!({"read_only": false})), Class::Destructive);
        assert_eq!(run(json!({})), Class::Destructive);
        assert_eq!(run(json!({"read_only": null})), Class::Destructive);
        assert_eq!(run(json!({"read_only": "true"})), Class::Destructive);
        assert_eq!(run(json!({"read_only": 1})), Class::Destructive);
        assert_eq!(run(json!({"read_only": [true]})), Class::Destructive);
        assert_eq!(class(RUN_SQL, json!("SELECT 1")), Class::Destructive);
    }

    #[test]
    fn copy_out_is_excluded() {
        for copy_out in [json!(true), json!("true"), json!(1), json!({})] {
            let body = json!({"sql": "SELECT 1", "read_only": true, "copy_out": copy_out});
            assert_eq!(class(RUN_SQL, body), Class::Excluded, "{copy_out}");
        }
        for copy_out in [json!(false), json!(null)] {
            let body = json!({"sql": "SELECT 1", "read_only": true, "copy_out": copy_out});
            assert_eq!(class(RUN_SQL, body), Class::Read, "{copy_out}");
        }
    }

    #[test]
    fn a_public_bucket_is_destructive() {
        assert_eq!(
            class(BUCKET, json!({"name": "b", "public": true})),
            Class::Destructive
        );
        assert_eq!(
            class(BUCKET, json!({"name": "b", "public": "true"})),
            Class::Destructive
        );
        assert_eq!(
            class(BUCKET, json!({"name": "b", "public": 1})),
            Class::Destructive
        );
        assert_eq!(
            class(BUCKET, json!({"name": "b", "public": false})),
            Class::Write
        );
        assert_eq!(
            class(BUCKET, json!({"name": "b", "public": null})),
            Class::Write
        );
        assert_eq!(class(BUCKET, json!({"name": "b"})), Class::Write);
    }

    #[test]
    fn schema_changes_classes() {
        let table = json!({"name": "t"});
        let column = json!({"name": "c", "data_type": {"name": "text"}});
        // A dry run rolls back, whatever it holds.
        assert_eq!(
            class(
                CHANGES,
                json!({"dry_run": true,
                "operations": [{"drop_table": {"table": table}}]})
            ),
            Class::Read
        );
        assert_eq!(
            class(
                CHANGES,
                json!({"dry_run": "true",
                "operations": [{"add_column": {"table": table, "column": column}}]})
            ),
            Class::Write
        );
        assert_eq!(
            changes(json!([{"add_column": {"table": table, "column": column}}])),
            Class::Write
        );
        assert_eq!(
            changes(
                json!([{"create_table": {"table": table, "columns": [column],
                "grants": [], "policies": [], "rls": null, "owner": null}}])
            ),
            Class::Write
        );
        assert_eq!(
            changes(json!([{"create_index": {"table": table, "keys": []}},
                {"rename_column": {"table": table, "column": "c", "to": "d"}}])),
            Class::Write
        );
        assert_eq!(
            changes(json!([{"alter_column": {"table": table, "column": "c",
                "change": {"not_null": true, "comment": {"set": "x"}}}}])),
            Class::Write
        );
        for removal in [
            json!({"drop_table": {"table": table}}),
            json!({"drop_column": {"table": table, "column": "c"}}),
            json!({"drop_index": {"name": "i"}}),
            json!({"drop_constraint": {"table": table, "name": "k"}}),
            json!({"drop_enum": {"name": "e"}}),
            // A new type can lose values; a lifted NOT NULL removes a constraint.
            json!({"alter_column": {"table": table, "column": "c",
                "change": {"data_type": {"name": "int4"}, "using": "c::int4"}}}),
            json!({"alter_column": {"table": table, "column": "c",
                "change": {"not_null": false}}}),
            json!({"alter_column": {"table": table, "column": "c", "change": "x"}}),
            // Not in this grammar: never a write.
            json!({"truncate_table": {"table": table}}),
            json!({"add_column": {}, "drop_table": {}}),
            json!("drop_table"),
        ] {
            let ops = json!([{"add_column": {"table": table, "column": column}}, removal]);
            assert_eq!(changes(ops), Class::Destructive, "{removal}");
        }
    }

    #[test]
    fn schema_change_with_a_grant_or_policy_is_destructive() {
        let table = json!({"name": "t"});
        let column = json!({"name": "c", "data_type": {"name": "text"}});
        for op in [
            "grant",
            "revoke",
            "grant_schema",
            "revoke_schema",
            "grant_sequence",
            "revoke_sequence",
            "grant_function",
            "revoke_function",
            "alter_default_privileges",
            "set_rls",
            "create_policy",
            "alter_policy",
            "rename_policy",
            "drop_policy",
            "set_all_rows",
            "set_table_owner",
            "set_table_schema",
        ] {
            assert_eq!(changes(json!([{ op: {} }])), Class::Destructive, "{op}");
        }
        for (key, value) in [
            (
                "grants",
                json!([{"role": "anon", "privileges": ["select"]}]),
            ),
            ("policies", json!([{"name": "p"}])),
            ("rls", json!({"enabled": false})),
            ("owner", json!("someone")),
            ("grants", json!("all")),
        ] {
            let mut create = json!({"table": table, "columns": [column]});
            create[key] = value;
            assert_eq!(
                changes(json!([{ "create_table": create }])),
                Class::Destructive,
                "{key}"
            );
        }
    }

    /// Every operation that carries a caller-written SQL expression, or makes Postgres run an
    /// operator the caller names, over the table's rows.
    fn code_carrying_operations() -> Vec<Value> {
        let t = json!({"name": "orders"});
        let int4 = json!({"name": "int4"});
        let col = |extra: Value| {
            let mut c = json!({"name": "x", "data_type": int4});
            for (k, v) in extra.as_object().unwrap() {
                c[k] = v.clone();
            }
            c
        };
        vec![
            // The audit's four cases (B1).
            json!({"add_column": {"table": t, "column": col(json!({
                "default": "public.delete_account('00000000-0000-0000-0000-000000000001')"}))}}),
            json!({"add_column": {"table": t, "column": col(json!({"generated": "public.purge(id)"}))}}),
            json!({"add_constraint": {"table": t, "constraint": {"name": "k",
                "kind": {"check": {"expression": "public.purge(id) IS NOT NULL"}}}}}),
            json!({"create_index": {"table": t,
                "keys": [{"expression": {"sql": "public.purge(id)"}}]}}),
            // The other expression slots.
            json!({"add_column": {"table": t, "column": col(json!({"default": "0"}))}}),
            json!({"create_index": {"table": t, "keys": [{"column": {"name": "id"}}],
                "predicate": "public.purge(id)"}}),
            json!({"add_constraint": {"table": t, "constraint": {"kind": {"exclusion": {
                "method": "gist", "elements": [{"column": "c", "operator": {"name": "&&"}}],
                "predicate": "public.purge(id)"}}}}}),
            // An exclusion runs the operators it names on every pair of rows.
            json!({"add_constraint": {"table": t, "constraint": {"kind": {"exclusion": {
                "method": "gist", "elements": [{"column": "c",
                    "operator": {"schema": "public", "name": "===="}}]}}}}}),
            json!({"create_table": {"table": t, "columns": [col(json!({"default": "now()"}))]}}),
            json!({"create_table": {"table": t, "columns": [col(json!({"generated": "id * 2"}))]}}),
            json!({"create_table": {"table": t, "columns": [col(json!({}))],
                "constraints": [{"kind": {"check": {"expression": "x > 0"}}}]}}),
            json!({"alter_column": {"table": t, "column": "x",
                "change": {"default": {"set": "public.purge(1)"}}}}),
            json!({"alter_column": {"table": t, "column": "x",
                "change": {"data_type": int4, "using": "public.purge(x)"}}}),
            json!({"create_policy": {"table": t, "policy": {"name": "p", "using": "true"}}}),
            json!({"alter_policy": {"table": t, "name": "p", "with_check": "true"}}),
            // A shape this grammar doesn't know may hold an expression.
            json!({"add_column": {"table": t, "column": "x"}}),
            json!({"add_constraint": {"table": t, "constraint": {"kind": "check"}}}),
            json!({"create_index": {"table": t, "keys": [{"other": {"sql": "f()"}}]}}),
            json!({"create_index": {"table": t, "keys": "public.purge(id)"}}),
            json!({"create_table": {"table": t, "columns": "x"}}),
            json!({"future_kind": {"table": t}}),
        ]
    }

    #[test]
    fn schema_change_with_a_caller_expression_is_destructive() {
        for op in code_carrying_operations() {
            assert_eq!(changes(json!([op])), Class::Destructive, "{op}");
            let body = json!({"dry_run": false, "operations": [op]});
            assert_eq!(class(CHANGES, body), Class::Destructive, "{op}");
        }
        // Without an expression the same kinds stay writes.
        let t = json!({"name": "orders"});
        let column = json!({"name": "x", "data_type": {"name": "int4"},
            "default": null, "generated": null, "not_null": true});
        for op in [
            json!({"add_column": {"table": t, "column": column}}),
            json!({"create_table": {"table": t, "columns": [column], "constraints": [
                {"kind": {"primary_key": {"columns": ["x"]}}},
                {"kind": {"unique": {"columns": ["x"]}}},
                {"kind": {"foreign_key": {"columns": ["x"], "references": {"name": "o"},
                    "referenced_columns": ["id"]}}}]}}),
            json!({"add_constraint": {"table": t, "constraint": {"kind": {"unique": {
                "columns": ["x"]}}}}}),
            json!({"create_index": {"table": t, "keys": [{"column": {"name": "x"}}],
                "predicate": null}}),
        ] {
            assert_eq!(changes(json!([op])), Class::Write, "{op}");
        }
    }

    #[test]
    fn a_dry_run_with_an_expression_is_write() {
        let dry = |ops: Value| class(CHANGES, json!({"dry_run": true, "operations": ops}));
        for op in code_carrying_operations() {
            assert_eq!(dry(json!([op])), Class::Write, "{op}");
        }
        let setval = json!({"add_column": {"table": {"name": "orders"}, "column": {
            "name": "x", "data_type": {"name": "int8"}, "default": "setval('orders_id_seq', 1)"}}});
        let drop = json!({"drop_table": {"table": {"name": "orders"}}});
        assert_eq!(dry(json!([drop, setval])), Class::Write);
        // With no expression a dry run is still a read, whatever it would change.
        assert_eq!(dry(json!([drop])), Class::Read);
        assert_eq!(dry(json!([])), Class::Read);
        // A dry run whose operations can't be read is never a read.
        assert_eq!(dry(json!("drop")), Class::Write);
        assert_eq!(class(CHANGES, json!({"dry_run": true})), Class::Write);
        assert_eq!(dry(json!(["add_column"])), Class::Write);
    }

    #[test]
    fn only_read_is_allowed_in_read_only_mode() {
        assert!(allowed_in_read_only_mode(Class::Read));
        assert!(!allowed_in_read_only_mode(Class::Write));
        assert!(!allowed_in_read_only_mode(Class::Destructive));
        assert!(!allowed_in_read_only_mode(Class::Excluded));
    }

    #[test]
    fn excluded_wins_and_classes_have_names() {
        assert_eq!(
            classify("storagePutObject", "PUT", Some(&json!({"read_only": true}))),
            Class::Excluded
        );
        let names: Vec<&str> = [
            Class::Read,
            Class::Write,
            Class::Destructive,
            Class::Excluded,
        ]
        .into_iter()
        .map(Class::as_str)
        .collect();
        assert_eq!(names, ["read", "write", "destructive", "excluded"]);
    }
}
