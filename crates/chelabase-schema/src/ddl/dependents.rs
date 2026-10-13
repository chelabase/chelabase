//! What `DROP … CASCADE` would remove beyond what the request drops: the catalog query, its
//! parser and the hash a cascading request confirms.

use chelabase_types::table_api::{Dependent, Operation};
use sha2::{Digest, Sha256};

use crate::SchemaError;
use crate::sql::literal;

/// `'pg_catalog.<catalog>'::regclass` as an oid (a `pg_depend` class id).
fn class(catalog: &str) -> String {
    format!("'pg_catalog.{catalog}'::pg_catalog.regclass::pg_catalog.oid")
}

/// `pg_class c` joined to its schema, filtered to `schema.name`.
fn relation(schema: &str, name: &str) -> String {
    format!(
        "pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         AND n.nspname = {} AND c.relname = {}",
        literal(schema),
        literal(name)
    )
}

/// What a missing target is called in the query's error (`table public.a`, …).
fn target_label(op: &Operation) -> String {
    match op {
        Operation::DropTable { table } => format!("table {}.{}", table.schema, table.name),
        Operation::DropColumn { table, column } => {
            format!("column {}.{}.{column}", table.schema, table.name)
        }
        Operation::DropConstraint { table, name } => {
            format!("constraint {name} on {}.{}", table.schema, table.name)
        }
        Operation::DropIndex { schema, name, .. } => format!("index {schema}.{name}"),
        Operation::DropEnum { schema, name } => format!("type {schema}.{name}"),
        _ => String::new(),
    }
}

/// The `(classid, objid, objsubid)` row of one drop target (no row when it doesn't exist);
/// `None` for an operation that drops nothing with dependents.
fn target(op: &Operation) -> Option<String> {
    Some(match op {
        Operation::DropTable { table } => format!(
            "SELECT {}, c.oid, 0 FROM {}",
            class("pg_class"),
            relation(&table.schema, &table.name)
        ),
        Operation::DropColumn { table, column } => format!(
            "SELECT {}, c.oid, a.attnum::pg_catalog.int4 FROM {} \
             JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 \
             AND NOT a.attisdropped AND a.attname = {}",
            class("pg_class"),
            relation(&table.schema, &table.name),
            literal(column)
        ),
        Operation::DropConstraint { table, name } => format!(
            "SELECT {}, k.oid, 0 FROM {} \
             JOIN pg_catalog.pg_constraint k ON k.conrelid = c.oid AND k.conname = {}",
            class("pg_constraint"),
            relation(&table.schema, &table.name),
            literal(name)
        ),
        Operation::DropIndex { schema, name, .. } => format!(
            "SELECT {}, c.oid, 0 FROM {} AND c.relkind IN ('i', 'I')",
            class("pg_class"),
            relation(schema, name)
        ),
        Operation::DropEnum { schema, name } => format!(
            "SELECT {}, t.oid, 0 FROM pg_catalog.pg_type t \
             JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace \
             AND n.nspname = {} AND t.typname = {}",
            class("pg_type"),
            literal(schema),
            literal(name)
        ),
        // A policy has no dependents; the other operations drop nothing.
        _ => return None,
    })
}

/// One row, one text column: the JSON array of every object `DROP … CASCADE` of the request's
/// drop targets would remove beyond the targets and their own parts, each
/// `{kind, schema, name, parent, description}` (`kind` is `pg_identify_object`'s `type`,
/// `description` is `pg_describe_object`'s text, as Postgres's `drop cascades to …` notices
/// print it). `None` when the request drops nothing that can have dependents.
///
/// A target the catalog doesn't have raises an error (`invalid input syntax for type integer:
/// "chelabase: drop target … not found"`) instead of reporting no dependents.
///
/// Only `deptype 'n'` dependents are listed: parts that go automatically with a dropped
/// column (an index on it, a generated column using it) aren't, as Postgres's own notices
/// don't list them.
///
/// `removed` walks `pg_depend` from the targets the way `DROP` does:
/// - *parts* (`deptype` `a`, `i`, `P`, `S` on something removed: columns' indexes and
///   constraints, owned sequences, row and array types, a view's rule) go with what they
///   belong to; parts of a target are the target's (`part`), never reported;
/// - *dependents* (`deptype` `n`, which `RESTRICT` refuses) are what `CASCADE` adds: reported;
/// - an internal part reached as a dependent (a view's rule) takes its owner along (the view),
///   which is what's reported.
///
/// A reference to a whole object (`objsubid` 0) covers its columns. It runs as the acting role,
/// inside the change's transaction: only world-readable catalogs and functions every role may
/// call.
pub fn dependents_sql(ops: &[Operation]) -> Option<String> {
    let targets: Vec<String> = ops.iter().filter_map(target).collect();
    if targets.is_empty() {
        return None;
    }
    // `(label, found)` per target. The error is a cast of the label read from a CTE at run time
    // (a constant would be folded, and raise, at plan time even when every target exists).
    let checks: Vec<String> = ops
        .iter()
        .filter_map(|op| {
            target(op).map(|t| {
                format!(
                    "({}, EXISTS ({t}))",
                    literal(&format!(
                        "chelabase: drop target {} not found",
                        target_label(op)
                    ))
                )
            })
        })
        .collect();
    let covers = |whole: &str, d: &str| {
        format!(
            "{d}.refclassid = {whole}.classid AND {d}.refobjid = {whole}.objid \
             AND ({whole}.objsubid = 0 OR {d}.refobjsubid = {whole}.objsubid)"
        )
    };
    let name_of = |catalog: &str, column: &str, key: &str| {
        format!("(SELECT x.{column}::pg_catalog.text FROM pg_catalog.{catalog} x WHERE x.{key})")
    };
    let relname = name_of("pg_class", "relname", "oid = f.objid");
    let parent_of = |catalog: &str, column: &str| {
        format!(
            "(SELECT c.relname::pg_catalog.text FROM pg_catalog.{catalog} x \
             JOIN pg_catalog.pg_class c ON c.oid = x.{column} WHERE x.oid = f.objid)"
        )
    };
    Some(format!(
        "WITH RECURSIVE targets(classid, objid, objsubid) AS ({targets}), \
         removed(classid, objid, objsubid, part, reported) AS ( \
         SELECT classid, objid, objsubid, true, false FROM targets \
         UNION \
         SELECT s.classid, s.objid, s.objsubid, s.part, s.reported FROM removed r \
         CROSS JOIN LATERAL ( \
         SELECT d.classid, d.objid, d.objsubid, r.part, false FROM pg_catalog.pg_depend d \
         WHERE {parts} AND d.deptype IN ('a', 'i', 'P', 'S') \
         UNION ALL \
         SELECT d.classid, d.objid, d.objsubid, false, true FROM pg_catalog.pg_depend d \
         WHERE {parts} AND d.deptype = 'n' \
         UNION ALL \
         SELECT d.refclassid, d.refobjid, d.refobjsubid, false, true FROM pg_catalog.pg_depend d \
         WHERE NOT r.part AND d.classid = r.classid AND d.objid = r.objid \
         AND d.objsubid = r.objsubid AND d.deptype = 'i' \
         ) s(classid, objid, objsubid, part, reported)), \
         found AS (SELECT DISTINCT r.classid, r.objid, r.objsubid FROM removed r \
         WHERE r.reported \
         AND NOT EXISTS (SELECT FROM removed p WHERE p.part AND p.classid = r.classid \
         AND p.objid = r.objid AND (p.objsubid = 0 OR p.objsubid = r.objsubid)) \
         AND NOT EXISTS (SELECT FROM pg_catalog.pg_depend i WHERE i.classid = r.classid \
         AND i.objid = r.objid AND i.objsubid = r.objsubid AND i.deptype = 'i')), \
         missing AS MATERIALIZED (SELECT v.label FROM (VALUES {checks}) v(label, found) \
         WHERE NOT v.found LIMIT 1) \
         SELECT CASE WHEN m.label IS NULL THEN a.list \
         ELSE m.label::pg_catalog.int4::pg_catalog.text END \
         FROM (SELECT COALESCE(pg_catalog.jsonb_agg(pg_catalog.jsonb_build_object(\
         'kind', o.type, 'schema', o.schema, 'name', COALESCE(x.name, o.identity), \
         'parent', x.parent, \
         'description', pg_catalog.pg_describe_object(f.classid, f.objid, f.objsubid))), \
         '[]'::pg_catalog.jsonb)::pg_catalog.text \
         FROM found f \
         CROSS JOIN LATERAL pg_catalog.pg_identify_object(f.classid, f.objid, f.objsubid) o \
         CROSS JOIN LATERAL (SELECT \
         CASE \
         WHEN f.classid = {pg_class} AND f.objsubid = 0 THEN {relname} \
         WHEN f.classid = {pg_class} THEN {attname} \
         WHEN f.classid = {pg_constraint} THEN {conname} \
         WHEN f.classid = {pg_policy} THEN {polname} \
         WHEN f.classid = {pg_trigger} THEN {tgname} \
         WHEN f.classid = {pg_rewrite} THEN {rulename} \
         WHEN f.classid = {pg_attrdef} THEN {adname} \
         WHEN f.classid = {pg_type} THEN {typname} \
         WHEN f.classid = {pg_proc} THEN {proname} \
         END, \
         CASE \
         WHEN f.classid = {pg_class} AND f.objsubid <> 0 THEN {relname} \
         WHEN f.classid = {pg_constraint} THEN {con_parent} \
         WHEN f.classid = {pg_policy} THEN {pol_parent} \
         WHEN f.classid = {pg_trigger} THEN {tg_parent} \
         WHEN f.classid = {pg_rewrite} THEN {rule_parent} \
         WHEN f.classid = {pg_attrdef} THEN {ad_parent} \
         ELSE NULL::pg_catalog.text \
         END) x(name, parent)) a(list) \
         LEFT JOIN missing m ON true",
        targets = targets.join(" UNION ALL "),
        checks = checks.join(", "),
        parts = covers("r", "d"),
        pg_class = class("pg_class"),
        pg_constraint = class("pg_constraint"),
        pg_policy = class("pg_policy"),
        pg_trigger = class("pg_trigger"),
        pg_rewrite = class("pg_rewrite"),
        pg_attrdef = class("pg_attrdef"),
        pg_type = class("pg_type"),
        pg_proc = class("pg_proc"),
        relname = relname,
        attname = name_of(
            "pg_attribute",
            "attname",
            "attrelid = f.objid AND x.attnum = f.objsubid"
        ),
        conname = name_of("pg_constraint", "conname", "oid = f.objid"),
        polname = name_of("pg_policy", "polname", "oid = f.objid"),
        tgname = name_of("pg_trigger", "tgname", "oid = f.objid"),
        rulename = name_of("pg_rewrite", "rulename", "oid = f.objid"),
        adname = "(SELECT a.attname::pg_catalog.text FROM pg_catalog.pg_attrdef x \
                  JOIN pg_catalog.pg_attribute a ON a.attrelid = x.adrelid AND a.attnum = x.adnum \
                  WHERE x.oid = f.objid)",
        typname = name_of("pg_type", "typname", "oid = f.objid"),
        proname = "(SELECT x.proname::pg_catalog.text || '(' || \
                   pg_catalog.pg_get_function_identity_arguments(x.oid) || ')' \
                   FROM pg_catalog.pg_proc x WHERE x.oid = f.objid)",
        con_parent = "(SELECT COALESCE(c.relname, t.typname)::pg_catalog.text \
                      FROM pg_catalog.pg_constraint x \
                      LEFT JOIN pg_catalog.pg_class c ON c.oid = x.conrelid \
                      LEFT JOIN pg_catalog.pg_type t ON t.oid = x.contypid WHERE x.oid = f.objid)",
        pol_parent = parent_of("pg_policy", "polrelid"),
        tg_parent = parent_of("pg_trigger", "tgrelid"),
        rule_parent = parent_of("pg_rewrite", "ev_class"),
        ad_parent = parent_of("pg_attrdef", "adrelid"),
    ))
}

/// What the dependents query's error calls `op`'s drop target (`table public.a`,
/// `column app.b.x`, …): `None` for an operation that drops nothing with dependents.
pub fn drop_target_label(op: &Operation) -> Option<String> {
    target(op).map(|_| target_label(op))
}

/// One drop target of a request and its own parts, as [`target_parts_sql`] reads them.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetParts {
    /// The operation's index in the slice the query was built from.
    pub op_index: usize,
    /// The target's `pg_describe_object` text (`table public.a` prints as `table a`).
    pub target: String,
    /// The descriptions of what goes with the target whatever the drop behaviour: the target
    /// itself, its parts (`deptype` `a`, `i`, `P`, `S`, walked as [`dependents_sql`] walks
    /// them) and, for a relation, each of its columns.
    pub parts: std::collections::BTreeSet<String>,
}

/// One row, one text column: the JSON array of the request's existing drop targets, each
/// `{op_index, target, parts}` ([`TargetParts`]), in request order. A target the catalog doesn't
/// have is left out ([`dependents_sql`] reports it). `None` when the request drops nothing that
/// can have dependents.
///
/// The orchestrator's cascade check (R4) allows a drop to cascade to the parts of the request's
/// other targets (drops that reach each other), and skips a drop whose target an earlier drop of
/// the request already removed.
pub fn target_parts_sql(ops: &[Operation]) -> Option<String> {
    let targets: Vec<String> = ops
        .iter()
        .enumerate()
        .filter_map(|(i, op)| {
            target(op).map(|t| {
                format!("SELECT {i}, x.classid, x.objid, x.objsubid FROM ({t}) x(classid, objid, objsubid)")
            })
        })
        .collect();
    if targets.is_empty() {
        return None;
    }
    Some(format!(
        "WITH RECURSIVE targets(t, classid, objid, objsubid) AS ({targets}), \
         parts(t, classid, objid, objsubid) AS ( \
         SELECT t, classid, objid, objsubid FROM targets \
         UNION \
         SELECT p.t, d.classid, d.objid, d.objsubid FROM parts p \
         JOIN pg_catalog.pg_depend d ON d.refclassid = p.classid AND d.refobjid = p.objid \
         AND (p.objsubid = 0 OR d.refobjsubid = p.objsubid) AND d.deptype IN ('a', 'i', 'P', 'S')), \
         described(t, description) AS ( \
         SELECT p.t, pg_catalog.pg_describe_object(p.classid, p.objid, p.objsubid) FROM parts p \
         UNION \
         SELECT p.t, pg_catalog.pg_describe_object(p.classid, p.objid, a.attnum::pg_catalog.int4) \
         FROM parts p JOIN pg_catalog.pg_attribute a ON p.classid = {pg_class} AND p.objsubid = 0 \
         AND a.attrelid = p.objid AND a.attnum > 0 AND NOT a.attisdropped) \
         SELECT COALESCE(pg_catalog.jsonb_agg(pg_catalog.jsonb_build_object(\
         'op_index', x.t, \
         'target', pg_catalog.pg_describe_object(x.classid, x.objid, x.objsubid), \
         'parts', (SELECT COALESCE(pg_catalog.jsonb_agg(DISTINCT s.description), '[]'::pg_catalog.jsonb) \
         FROM described s WHERE s.t = x.t AND s.description IS NOT NULL)) ORDER BY x.t), \
         '[]'::pg_catalog.jsonb)::pg_catalog.text FROM targets x",
        targets = targets.join(" UNION ALL "),
        pg_class = class("pg_class"),
    ))
}

/// [`target_parts_sql`]'s JSON.
pub fn parse_target_parts(json: &str) -> Result<Vec<TargetParts>, SchemaError> {
    serde_json::from_str(json).map_err(|e| SchemaError::Parse(e.to_string()))
}

/// The dependents query's JSON, sorted.
pub fn parse_dependents(json: &str) -> Result<Vec<Dependent>, SchemaError> {
    let mut objects: Vec<Dependent> =
        serde_json::from_str(json).map_err(|e| SchemaError::Parse(e.to_string()))?;
    objects.sort();
    Ok(objects)
}

/// SHA-256 (lowercase hex) of the sorted list's JSON: what a cascading request confirms.
pub fn dependents_hash(objects: &[Dependent]) -> String {
    let mut sorted = objects.to_vec();
    sorted.sort();
    let json = serde_json::to_string(&sorted).expect("a list of strings serializes");
    hex::encode(Sha256::digest(json.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chelabase_types::table_api::TableRef;

    fn t(schema: &str, name: &str) -> TableRef {
        TableRef {
            schema: schema.into(),
            name: name.into(),
        }
    }

    #[test]
    fn the_dependents_query_parses_as_one_select() {
        let ops = [
            Operation::DropTable {
                table: t("app", "Old"),
            },
            Operation::DropColumn {
                table: t("public", "notes"),
                column: "y".into(),
            },
            Operation::DropConstraint {
                table: t("public", "notes"),
                name: "fk".into(),
            },
            Operation::DropIndex {
                schema: "public".into(),
                name: "i".into(),
                concurrently: false,
            },
            Operation::DropEnum {
                schema: "public".into(),
                name: "mood".into(),
            },
        ];
        let sql = dependents_sql(&ops).unwrap();
        let parsed = pg_query::parse(&sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert_eq!(parsed.protobuf.stmts.len(), 1);
        assert!(matches!(
            parsed.protobuf.stmts[0].stmt.as_ref().unwrap().node,
            Some(pg_query::NodeEnum::SelectStmt(_))
        ));
        let targets = sql
            .split_once("targets(classid, objid, objsubid) AS (")
            .and_then(|(_, rest)| rest.split_once("), removed"))
            .unwrap()
            .0;
        assert_eq!(
            targets.matches("SELECT ").count(),
            5,
            "five targets: {targets}"
        );
    }

    #[test]
    fn target_names_are_literals() {
        let sql = dependents_sql(&[Operation::DropTable {
            table: t("we'ird", "a\\b'; DROP TABLE x; --"),
        }])
        .unwrap();
        assert!(sql.contains("n.nspname = 'we''ird'"), "{sql}");
        assert!(
            sql.contains(r"c.relname = E'a\\b''; DROP TABLE x; --'"),
            "{sql}"
        );
        let parsed = pg_query::parse(&sql).unwrap();
        assert_eq!(parsed.protobuf.stmts.len(), 1);
    }

    #[test]
    fn only_drops_with_dependents_query() {
        let none = [
            Operation::DropPolicy {
                table: t("public", "notes"),
                name: "p".into(),
            },
            Operation::RenameTable {
                table: t("public", "notes"),
                to: "n".into(),
            },
        ];
        assert_eq!(dependents_sql(&none), None);
    }

    #[test]
    fn the_walk_follows_normal_dependencies_only_for_reporting() {
        let sql = dependents_sql(&[Operation::DropEnum {
            schema: "public".into(),
            name: "mood".into(),
        }])
        .unwrap();
        assert!(sql.contains("d.deptype = 'n'"));
        assert!(sql.contains("d.deptype IN ('a', 'i', 'P', 'S')"));
        assert!(!sql.contains("pg_authid"), "world-readable catalogs only");
    }

    #[test]
    fn a_missing_target_raises_at_run_time() {
        let sql = dependents_sql(&[
            Operation::DropTable {
                table: t("public", "a"),
            },
            Operation::DropColumn {
                table: t("app", "b"),
                column: "x".into(),
            },
        ])
        .unwrap();
        assert!(
            sql.contains("'chelabase: drop target table public.a not found'"),
            "{sql}"
        );
        assert!(
            sql.contains("'chelabase: drop target column app.b.x not found'"),
            "{sql}"
        );
        // Materialized, so the failing cast never sees a constant the planner could fold.
        assert!(sql.contains("missing AS MATERIALIZED"), "{sql}");
        assert!(sql.contains("m.label::pg_catalog.int4"), "{sql}");
        assert_eq!(pg_query::parse(&sql).unwrap().protobuf.stmts.len(), 1);
    }

    // T6 review, Important 2: each target's own parts, by the operation's index.
    #[test]
    fn the_parts_query_numbers_targets_by_operation() {
        let ops = [
            Operation::RenameTable {
                table: t("public", "notes"),
                to: "n".into(),
            },
            Operation::DropTable {
                table: t("public", "parent"),
            },
            Operation::DropPolicy {
                table: t("public", "notes"),
                name: "p".into(),
            },
            Operation::DropEnum {
                schema: "public".into(),
                name: "mood".into(),
            },
        ];
        let sql = target_parts_sql(&ops).unwrap();
        let parsed = pg_query::parse(&sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert_eq!(parsed.protobuf.stmts.len(), 1);
        assert!(sql.contains("SELECT 1, x.classid"), "{sql}");
        assert!(sql.contains("SELECT 3, x.classid"), "{sql}");
        assert!(!sql.contains("SELECT 0, x.classid") && !sql.contains("SELECT 2, x.classid"));
        // Parts only: never the `n` dependents a cascade adds.
        assert!(!sql.contains("deptype = 'n'"), "{sql}");
        assert!(sql.contains("pg_describe_object"), "{sql}");
        assert_eq!(target_parts_sql(&ops[..1]), None);
    }

    #[test]
    fn target_parts_parse() {
        let parsed = parse_target_parts(
            r#"[{"op_index":1,"target":"table child","parts":["table child","column pid of table child","constraint child_pid_fkey on table child"]}]"#,
        )
        .unwrap();
        assert_eq!(parsed[0].op_index, 1);
        assert_eq!(parsed[0].target, "table child");
        assert!(parsed[0].parts.contains("constraint child_pid_fkey on table child"));
        assert_eq!(parse_target_parts("[]").unwrap(), vec![]);
        assert!(parse_target_parts(r#"[{"op_index":0,"target":"t","parts":[],"x":1}]"#).is_err());
    }

    #[test]
    fn drop_target_labels() {
        assert_eq!(
            drop_target_label(&Operation::DropColumn {
                table: t("app", "b"),
                column: "x".into()
            })
            .as_deref(),
            Some("column app.b.x")
        );
        assert_eq!(
            drop_target_label(&Operation::RenameTable {
                table: t("public", "a"),
                to: "b".into()
            }),
            None
        );
    }

    #[test]
    fn parse_refuses_unknown_fields() {
        assert!(
            parse_dependents(r#"[{"kind":"view","schema":null,"name":"v","parent":null,"x":1}]"#)
                .is_err()
        );
        assert_eq!(parse_dependents("[]").unwrap(), vec![]);
    }
}
