//! The protection review (protected branches spec §3): a catalog scan, in one database, of code
//! owned by a non-Admin role that can run in someone else's session; its hash; the SQL of its
//! fixes. Pure: the orchestrator runs the SQL as the owner.
//!
//! Only superusers and the owner role itself are trusted (PB R14: a member of the owner or of
//! `neon_superuser` is listed like anyone, as the DDL guard treats it). Listed:
//!
//! - functions owned by a non-Admin role that are `SECURITY DEFINER`, or reachable through
//!   `pg_depend` (directly, or through an operator to its function) from a policy, a column
//!   default or generated expression, a domain's default, a CHECK constraint (a table's or a
//!   domain's), an index, a view (its `_RETURN` rule), another rule, a trigger (not a
//!   partition's clone) or a range type's support functions (`type`), and transitively from
//!   any reached function or any `SECURITY DEFINER` function (whatever its owner) to the
//!   functions it references (`called_by`);
//! - triggers (not internal, not clones) whose function or relation is owned by a non-Admin
//!   role; when only the relation is, the item has no `function` and shows the relation's
//!   owner;
//! - rules (not a view's `_RETURN`) on a relation owned by a non-Admin role.
//!
//! Skipped: extension members and objects in `pg_catalog`, `information_schema`, `pg_toast`,
//! `pg_temp_*`, `pg_toast_temp_*`, `chelabase_guard` and `chelabase_functions`, and the
//! function triggers (`chelabase_fn_*` running a function in `chelabase_functions`).
//!
//! Every name part of a key, a `via` and a hash line is quoted as an identifier, so no name
//! can make two items share a key or forge a hash line. Each item carries the md5 hex of its
//! definition (`digest`), so a body replaced in place changes the hash.

use chelabase_types::protection::{
    FixAction, REVIEW_SCAN_MARK, ReviewItem, ReviewItemKind, ReviewReason,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::SchemaError;
use crate::sql::{ident, literal};

/// One read-only SELECT (starts with [`REVIEW_SCAN_MARK`], one row, one JSON text column: an
/// array of items as [`parse_review`] reads them).
pub fn review_sql(owner: &str) -> String {
    let owner = literal(owner);
    format!(
        "{REVIEW_SCAN_MARK}\n{}",
        SCAN_SQL.replace("{owner}", &owner)
    )
}

/// The scan, with `{owner}` standing for the owner's literal. Every catalog function and
/// operator is schema-qualified.
const SCAN_SQL: &str = r#"WITH RECURSIVE
admin AS (
  SELECT r.oid FROM pg_catalog.pg_roles r
  WHERE r.rolsuper OR r.rolname OPERATOR(pg_catalog.=) {owner}
),
skipped AS (
  SELECT n.oid FROM pg_catalog.pg_namespace n
  WHERE n.nspname OPERATOR(pg_catalog.=) 'pg_catalog'
     OR n.nspname OPERATOR(pg_catalog.=) 'information_schema'
     OR n.nspname OPERATOR(pg_catalog.=) 'pg_toast'
     OR n.nspname OPERATOR(pg_catalog.=) 'chelabase_guard'
     OR n.nspname OPERATOR(pg_catalog.=) 'chelabase_functions'
     OR pg_catalog.starts_with(n.nspname::pg_catalog.text, 'pg_temp_')
     OR pg_catalog.starts_with(n.nspname::pg_catalog.text, 'pg_toast_temp_')
),
edges(classid, objid, fn) AS (
  SELECT d.classid, d.objid, d.refobjid FROM pg_catalog.pg_depend d
  WHERE d.refclassid OPERATOR(pg_catalog.=) 'pg_catalog.pg_proc'::pg_catalog.regclass
  UNION ALL
  SELECT d.classid, d.objid, o.oprcode::pg_catalog.oid FROM pg_catalog.pg_depend d
  JOIN pg_catalog.pg_operator o ON o.oid OPERATOR(pg_catalog.=) d.refobjid
  WHERE d.refclassid OPERATOR(pg_catalog.=) 'pg_catalog.pg_operator'::pg_catalog.regclass
    AND o.oprcode::pg_catalog.oid OPERATOR(pg_catalog.<>) 0::pg_catalog.oid
),
roots(fn, kind, via) AS (
  SELECT d.fn, 'policy'::pg_catalog.text,
         pg_catalog.format('%I.%I.%I', n.nspname, c.relname, pol.polname)
  FROM edges d
  JOIN pg_catalog.pg_policy pol ON pol.oid OPERATOR(pg_catalog.=) d.objid
  JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) pol.polrelid
  JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) c.relnamespace
  WHERE d.classid OPERATOR(pg_catalog.=) 'pg_catalog.pg_policy'::pg_catalog.regclass
  UNION ALL
  SELECT d.fn,
         CASE WHEN a.attgenerated OPERATOR(pg_catalog.=) '' THEN 'default'::pg_catalog.text ELSE 'generated'::pg_catalog.text END,
         pg_catalog.format('%I.%I.%I', n.nspname, c.relname, a.attname)
  FROM edges d
  JOIN pg_catalog.pg_attrdef ad ON ad.oid OPERATOR(pg_catalog.=) d.objid
  JOIN pg_catalog.pg_attribute a ON a.attrelid OPERATOR(pg_catalog.=) ad.adrelid AND a.attnum OPERATOR(pg_catalog.=) ad.adnum
  JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) ad.adrelid
  JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) c.relnamespace
  WHERE d.classid OPERATOR(pg_catalog.=) 'pg_catalog.pg_attrdef'::pg_catalog.regclass
  UNION ALL
  SELECT d.fn, 'default'::pg_catalog.text, pg_catalog.format('%I.%I', n.nspname, t.typname)
  FROM edges d
  JOIN pg_catalog.pg_type t ON t.oid OPERATOR(pg_catalog.=) d.objid
  JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) t.typnamespace
  WHERE d.classid OPERATOR(pg_catalog.=) 'pg_catalog.pg_type'::pg_catalog.regclass
    AND t.typtype OPERATOR(pg_catalog.=) 'd'
  UNION ALL
  SELECT d.fn, 'check'::pg_catalog.text,
         pg_catalog.format('%I.%I.%I', n.nspname, COALESCE(c.relname, t.typname), co.conname)
  FROM edges d
  JOIN pg_catalog.pg_constraint co ON co.oid OPERATOR(pg_catalog.=) d.objid
  JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) co.connamespace
  LEFT JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) co.conrelid
  LEFT JOIN pg_catalog.pg_type t ON t.oid OPERATOR(pg_catalog.=) co.contypid
  WHERE d.classid OPERATOR(pg_catalog.=) 'pg_catalog.pg_constraint'::pg_catalog.regclass
    AND co.contype OPERATOR(pg_catalog.=) 'c'
  UNION ALL
  SELECT d.fn, 'index'::pg_catalog.text, pg_catalog.format('%I.%I', n.nspname, i.relname)
  FROM edges d
  JOIN pg_catalog.pg_class i ON i.oid OPERATOR(pg_catalog.=) d.objid
  JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) i.relnamespace
  WHERE d.classid OPERATOR(pg_catalog.=) 'pg_catalog.pg_class'::pg_catalog.regclass
    AND (i.relkind OPERATOR(pg_catalog.=) 'i' OR i.relkind OPERATOR(pg_catalog.=) 'I')
  UNION ALL
  SELECT d.fn,
         CASE WHEN rw.rulename OPERATOR(pg_catalog.=) '_RETURN' THEN 'view'::pg_catalog.text ELSE 'rule'::pg_catalog.text END,
         CASE WHEN rw.rulename OPERATOR(pg_catalog.=) '_RETURN'
           THEN pg_catalog.format('%I.%I', n.nspname, c.relname)
           ELSE pg_catalog.format('%I.%I.%I', n.nspname, c.relname, rw.rulename) END
  FROM edges d
  JOIN pg_catalog.pg_rewrite rw ON rw.oid OPERATOR(pg_catalog.=) d.objid
  JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) rw.ev_class
  JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) c.relnamespace
  WHERE d.classid OPERATOR(pg_catalog.=) 'pg_catalog.pg_rewrite'::pg_catalog.regclass
  UNION ALL
  SELECT d.fn, 'trigger'::pg_catalog.text,
         pg_catalog.format('%I.%I.%I', n.nspname, c.relname, tg.tgname)
  FROM edges d
  JOIN pg_catalog.pg_trigger tg ON tg.oid OPERATOR(pg_catalog.=) d.objid
  JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) tg.tgrelid
  JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) c.relnamespace
  WHERE d.classid OPERATOR(pg_catalog.=) 'pg_catalog.pg_trigger'::pg_catalog.regclass
    AND NOT tg.tgisinternal AND tg.tgparentid OPERATOR(pg_catalog.=) 0::pg_catalog.oid
  UNION ALL
  SELECT x.fn, 'type'::pg_catalog.text, pg_catalog.format('%I.%I', n.nspname, t.typname)
  FROM pg_catalog.pg_range rg
  JOIN pg_catalog.pg_type t ON t.oid OPERATOR(pg_catalog.=) rg.rngtypid
  JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) t.typnamespace
  CROSS JOIN LATERAL (VALUES (rg.rngcanonical::pg_catalog.oid), (rg.rngsubdiff::pg_catalog.oid)) x(fn)
  WHERE x.fn OPERATOR(pg_catalog.<>) 0::pg_catalog.oid
  UNION ALL
  SELECT p.oid, 'security_definer'::pg_catalog.text, NULL::pg_catalog.text
  FROM pg_catalog.pg_proc p WHERE p.prosecdef
),
reach(fn, kind, via) AS (
  SELECT r.fn, r.kind, r.via FROM roots r
  UNION
  SELECT d.fn, 'called_by'::pg_catalog.text,
         pg_catalog.format('%I.%I(%s)', n.nspname, p.proname, pg_catalog.pg_get_function_identity_arguments(p.oid))
  FROM reach x
  JOIN edges d ON d.objid OPERATOR(pg_catalog.=) x.fn
  JOIN pg_catalog.pg_proc p ON p.oid OPERATOR(pg_catalog.=) x.fn
  JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) p.pronamespace
  WHERE d.classid OPERATOR(pg_catalog.=) 'pg_catalog.pg_proc'::pg_catalog.regclass
    AND d.fn OPERATOR(pg_catalog.<>) x.fn
),
items(doc) AS (
  SELECT pg_catalog.json_build_object(
           'kind', 'function', 'schema', n.nspname, 'name', p.proname,
           'identity', pg_catalog.pg_get_function_identity_arguments(p.oid),
           'function', pg_catalog.format('%I.%I(%s)', n.nspname, p.proname, pg_catalog.pg_get_function_identity_arguments(p.oid)),
           'owner', o.rolname,
           'reasons', pg_catalog.json_agg(pg_catalog.json_build_object('kind', x.kind, 'via', x.via)),
           'opaque_body', p.prosqlbody IS NULL,
           'digest', pg_catalog.md5(CASE WHEN p.prokind OPERATOR(pg_catalog.=) 'a' THEN p.prosrc
                                    ELSE pg_catalog.pg_get_functiondef(p.oid) END))
  FROM reach x
  JOIN pg_catalog.pg_proc p ON p.oid OPERATOR(pg_catalog.=) x.fn
  JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) p.pronamespace
  JOIN pg_catalog.pg_roles o ON o.oid OPERATOR(pg_catalog.=) p.proowner
  WHERE NOT EXISTS (SELECT FROM admin a WHERE a.oid OPERATOR(pg_catalog.=) p.proowner)
    AND NOT EXISTS (SELECT FROM skipped s WHERE s.oid OPERATOR(pg_catalog.=) p.pronamespace)
    AND NOT EXISTS (SELECT FROM pg_catalog.pg_depend e
      WHERE e.classid OPERATOR(pg_catalog.=) 'pg_catalog.pg_proc'::pg_catalog.regclass
        AND e.objid OPERATOR(pg_catalog.=) p.oid AND e.deptype OPERATOR(pg_catalog.=) 'e')
  GROUP BY p.oid, n.nspname, o.rolname
  UNION ALL
  SELECT pg_catalog.json_build_object(
           'kind', 'trigger', 'schema', n.nspname, 'name', tg.tgname,
           'identity', pg_catalog.format('%I.%I', n.nspname, c.relname),
           'function', CASE WHEN fa.admin THEN NULL::pg_catalog.text
                       ELSE pg_catalog.format('%I.%I(%s)', pn.nspname, p.proname, pg_catalog.pg_get_function_identity_arguments(p.oid)) END,
           'owner', CASE WHEN fa.admin THEN co.rolname ELSE po.rolname END,
           'reasons', pg_catalog.json_build_array(pg_catalog.json_build_object(
             'kind', 'trigger', 'via', pg_catalog.format('%I.%I.%I', n.nspname, c.relname, tg.tgname))),
           'opaque_body', false,
           'digest', pg_catalog.md5(pg_catalog.pg_get_triggerdef(tg.oid)))
  FROM pg_catalog.pg_trigger tg
  JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) tg.tgrelid
  JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) c.relnamespace
  JOIN pg_catalog.pg_roles co ON co.oid OPERATOR(pg_catalog.=) c.relowner
  JOIN pg_catalog.pg_proc p ON p.oid OPERATOR(pg_catalog.=) tg.tgfoid
  JOIN pg_catalog.pg_namespace pn ON pn.oid OPERATOR(pg_catalog.=) p.pronamespace
  JOIN pg_catalog.pg_roles po ON po.oid OPERATOR(pg_catalog.=) p.proowner
  CROSS JOIN LATERAL (SELECT EXISTS (SELECT FROM admin a WHERE a.oid OPERATOR(pg_catalog.=) p.proowner) AS admin) fa
  WHERE NOT tg.tgisinternal AND tg.tgparentid OPERATOR(pg_catalog.=) 0::pg_catalog.oid
    AND NOT (pg_catalog.starts_with(tg.tgname::pg_catalog.text, 'chelabase_fn_') AND pn.nspname OPERATOR(pg_catalog.=) 'chelabase_functions')
    AND (NOT fa.admin OR NOT EXISTS (SELECT FROM admin a WHERE a.oid OPERATOR(pg_catalog.=) c.relowner))
    AND NOT EXISTS (SELECT FROM skipped s WHERE s.oid OPERATOR(pg_catalog.=) c.relnamespace)
    AND NOT EXISTS (SELECT FROM pg_catalog.pg_depend e
      WHERE e.classid OPERATOR(pg_catalog.=) 'pg_catalog.pg_class'::pg_catalog.regclass
        AND e.objid OPERATOR(pg_catalog.=) c.oid AND e.deptype OPERATOR(pg_catalog.=) 'e')
  UNION ALL
  SELECT pg_catalog.json_build_object(
           'kind', 'rule', 'schema', n.nspname, 'name', rw.rulename,
           'identity', pg_catalog.format('%I.%I', n.nspname, c.relname),
           'function', NULL::pg_catalog.text,
           'owner', co.rolname,
           'reasons', pg_catalog.json_build_array(pg_catalog.json_build_object(
             'kind', 'rule', 'via', pg_catalog.format('%I.%I.%I', n.nspname, c.relname, rw.rulename))),
           'opaque_body', false,
           'digest', pg_catalog.md5(pg_catalog.pg_get_ruledef(rw.oid)))
  FROM pg_catalog.pg_rewrite rw
  JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) rw.ev_class
  JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) c.relnamespace
  JOIN pg_catalog.pg_roles co ON co.oid OPERATOR(pg_catalog.=) c.relowner
  WHERE rw.rulename OPERATOR(pg_catalog.<>) '_RETURN'
    AND NOT EXISTS (SELECT FROM admin a WHERE a.oid OPERATOR(pg_catalog.=) c.relowner)
    AND NOT EXISTS (SELECT FROM skipped s WHERE s.oid OPERATOR(pg_catalog.=) c.relnamespace)
    AND NOT EXISTS (SELECT FROM pg_catalog.pg_depend e
      WHERE e.classid OPERATOR(pg_catalog.=) 'pg_catalog.pg_class'::pg_catalog.regclass
        AND e.objid OPERATOR(pg_catalog.=) c.oid AND e.deptype OPERATOR(pg_catalog.=) 'e')
)
SELECT COALESCE(pg_catalog.json_agg(i.doc), '[]'::pg_catalog.json)::pg_catalog.text FROM items i;
"#;

/// One item of the scan's document.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScanItem {
    kind: ReviewItemKind,
    schema: String,
    name: String,
    identity: String,
    function: Option<String>,
    owner: String,
    reasons: Vec<ReviewReason>,
    opaque_body: bool,
    #[serde(default)]
    digest: String,
}

/// The scan's document for `database`: keys set, reasons sorted and deduplicated, items sorted
/// by key, `new` false.
pub fn parse_review(database: &str, json: &str) -> Result<Vec<ReviewItem>, SchemaError> {
    let scanned: Vec<ScanItem> =
        serde_json::from_str(json).map_err(|e| SchemaError::Parse(format!("review: {e}")))?;
    let mut items: Vec<ReviewItem> = scanned
        .into_iter()
        .map(|s| {
            let mut reasons = s.reasons;
            reasons.sort();
            reasons.dedup();
            ReviewItem {
                // Quoted parts, so `"a.b".c` and `a."b.c"` never share a key.
                key: format!(
                    "{}/{}/{}.{}({})",
                    ident(database),
                    s.kind.as_str(),
                    ident(&s.schema),
                    ident(&s.name),
                    s.identity
                ),
                database: database.to_string(),
                kind: s.kind,
                schema: s.schema,
                name: s.name,
                identity: s.identity,
                function: s.function,
                owner: s.owner,
                reasons,
                opaque_body: s.opaque_body,
                digest: s.digest,
                new: false,
            }
        })
        .collect();
    items.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(items)
}

/// SHA-256, lowercase hex, of the items sorted by key, one line each:
/// `key \t ident(owner) \t reasons \t digest`, reasons sorted, each `kind` or `kind:via`,
/// joined by `,`; lines joined by `\n`. `new` and `opaque_body` aren't hashed.
pub fn review_hash(items: &[ReviewItem]) -> String {
    let mut sorted: Vec<&ReviewItem> = items.iter().collect();
    sorted.sort_by(|a, b| a.key.cmp(&b.key));
    let lines: Vec<String> = sorted
        .into_iter()
        .map(|item| {
            let mut reasons: Vec<&ReviewReason> = item.reasons.iter().collect();
            reasons.sort();
            let reasons: Vec<String> = reasons
                .into_iter()
                .map(|r| match &r.via {
                    Some(via) => format!("{}:{via}", r.kind.as_str()),
                    None => r.kind.as_str().to_string(),
                })
                .collect();
            // The owner quoted: no name can forge a tab or a line. The digest is the scan's
            // md5 hex (an older document's empty one hashes as empty).
            format!(
                "{}\t{}\t{}\t{}",
                item.key,
                ident(&item.owner),
                reasons.join(","),
                item.digest
            )
        })
        .collect();
    hex::encode(Sha256::digest(lines.join("\n").as_bytes()))
}

/// The SQL of one fix (P7), `owner` quoted as an identifier. A function's item without its
/// `function` is a [`SchemaError::Parse`]; a trigger's without one reassigns its table.
pub fn fix_sql(item: &ReviewItem, action: FixAction, owner: &str) -> Result<String, SchemaError> {
    let owner = ident(owner);
    let function = || {
        item.function
            .as_deref()
            .ok_or_else(|| SchemaError::Parse(format!("review item {} has no function", item.key)))
    };
    Ok(match (item.kind, action) {
        (ReviewItemKind::Function, FixAction::Reassign) => {
            format!("ALTER ROUTINE {} OWNER TO {owner};", function()?)
        }
        (ReviewItemKind::Function, FixAction::Drop) => {
            format!("DROP ROUTINE {} RESTRICT;", function()?)
        }
        // A trigger whose function is Admin-owned (no `function`) is listed for its table.
        (ReviewItemKind::Trigger, FixAction::Reassign) => match item.function.as_deref() {
            Some(function) => format!("ALTER FUNCTION {function} OWNER TO {owner};"),
            None => format!("ALTER TABLE {} OWNER TO {owner};", item.identity),
        },
        (ReviewItemKind::Trigger, FixAction::Drop) => {
            format!(
                "DROP TRIGGER {} ON {} RESTRICT;",
                ident(&item.name),
                item.identity
            )
        }
        (ReviewItemKind::Rule, FixAction::Reassign) => {
            format!("ALTER TABLE {} OWNER TO {owner};", item.identity)
        }
        (ReviewItemKind::Rule, FixAction::Drop) => {
            format!(
                "DROP RULE {} ON {} RESTRICT;",
                ident(&item.name),
                item.identity
            )
        }
    })
}
