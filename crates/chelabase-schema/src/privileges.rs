//! The members hub's privileges read (one role's ACL entries, a schema page at a time), its
//! checked results (MH-24), the ACL-staleness hashes (P10, S3) and the "all rows" helpers
//! (MH-26).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::LazyLock;

use chelabase_types::table_api::{DefaultObject, Grantee, Operation, PrivilegeTarget, TableRef};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::SchemaError;
use crate::ddl::{privilege_keyword, privilege_name_keyword, schema_privilege, sequence_privilege};
use crate::model::{Grant, RoleRef};

/// The `COMMENT ON POLICY` text that marks the hub's "all rows" policy (MH-26).
pub const ALL_ROWS_MARKER: &str = "chelabase:all_rows";

/// The privilege names the editor offers per object kind (lower snake case, as the ops take
/// them; `maintain` only on Postgres 17 and later).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Offered {
    pub table: Vec<String>,
    pub sequence: Vec<String>,
    pub schema: Vec<String>,
    pub function: Vec<String>,
}

/// One schema's own ACL entries for the read's role.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SchemaPrivileges {
    pub name: String,
    pub owner: String,
    pub owner_settable: bool,
    pub grants: Vec<Grant>,
    pub acl_hash: String,
}

/// One `pg_default_acl` row's entries for the read's role.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DefaultPrivileges {
    pub for_role: String,
    pub schema: String,
    pub object: DefaultObject,
    pub grants: Vec<Grant>,
    pub acl_hash: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum PrivilegeObjectKind {
    Table,
    View,
    MaterializedView,
    PartitionedTable,
    ForeignTable,
    Sequence,
    Function,
    Procedure,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ColumnPrivileges {
    pub name: String,
    pub grants: Vec<Grant>,
}

/// The hub's "all rows" policy on a table, found by shape and marker (MH-26).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct AllRows {
    pub policy: String,
}

/// One object of the page's schema, with the read's role's entries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ObjectPrivileges {
    pub kind: PrivilegeObjectKind,
    pub schema: String,
    pub name: String,
    pub oid: u32,
    /// A function's or procedure's `regprocedure` text; `None` for the other kinds.
    pub identity: Option<String>,
    pub owner: String,
    pub owner_settable: bool,
    pub rls_enabled: bool,
    pub grants: Vec<Grant>,
    pub columns: Vec<ColumnPrivileges>,
    pub all_rows: Option<AllRows>,
    pub acl_hash: String,
}

/// The privileges read's JSON body (one page).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct PrivilegesPage {
    pub role: String,
    pub server_version_num: i32,
    pub offered: Offered,
    pub schemas: Vec<SchemaPrivileges>,
    pub default_privileges: Vec<DefaultPrivileges>,
    pub objects: Vec<ObjectPrivileges>,
    pub next_after: Option<String>,
}

/// A difference between what `ops` asked for and what the re-read shows (MH-24).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct PrivilegeMismatch {
    pub object: PrivilegeTarget,
    /// Upper case, like [`Grant::privilege`].
    pub privilege: String,
    pub grantee: String,
    pub still_granted_by: Option<String>,
    /// R-T4a: the column of a column privilege; `None` (`null`) for the object's own.
    #[serde(default)]
    pub column: Option<String>,
    pub wanted: bool,
}

/// `privileges_sql.sql` and `acl_hashes_sql.sql` with their `@…@` fragments expanded.
static PRIVILEGES_SQL: LazyLock<String> =
    LazyLock::new(|| expand(include_str!("privileges_sql.sql")));
static ACL_HASHES_SQL: LazyLock<String> =
    LazyLock::new(|| expand(include_str!("acl_hashes_sql.sql")));

/// One schema page of `role`'s privileges, every grantor (MH-24), marked roles and `cb_m_*`
/// excluded (MH-13, MH-32). `schema: None` → the schema list and default ACLs only. Every
/// object of the schema is listed (paged), with `role`'s entries; `acl_hash` and
/// `owner_settable` are computed in this SQL (S3, S4). `role` `public` (lower case) reads
/// PUBLIC's entries (T4-1; no real role can have that name), `page.role` staying `"public"`.
/// Params: `$1` role, `$2` schema (nullable), `$3` after (nullable), `$4` limit.
pub fn privileges_sql() -> &'static str {
    &PRIVILEGES_SQL
}

/// Parses [`privileges_sql`]'s document.
pub fn parse_privileges(json: &str) -> Result<PrivilegesPage, SchemaError> {
    serde_json::from_str(json).map_err(|e| SchemaError::Parse(e.to_string()))
}

/// S3: the current `acl_hash` of each target, same expression as [`privileges_sql`].
/// Params: `$1` jsonb array of [`PrivilegeTarget`] (their serde form); returns one jsonb array
/// `[{object, acl_hash}]`, a missing object left out.
pub fn acl_hashes_sql() -> &'static str {
    &ACL_HASHES_SQL
}

/// Expands the SQL files' fragments, each `@name(args)@`:
/// - `entries(acl)`: the read's role's entries of `acl` as `model::Grant` JSON (`target` CTE);
/// - `owner_settable(owner)`: [`owner_settable_predicate`] for the session role (`me` CTE);
/// - `acl_hash(acl)`, `rel_acl_hash(oid, acl)`: the ACL hash (a relation's covers its columns).
fn expand(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len() * 2);
    let mut rest = sql;
    while let Some(start) = rest.find('@') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let open = after.find('(').expect("a fragment's arguments");
        let end = after.find(")@").expect("a fragment's end");
        let (name, args) = (&after[..open], &after[open + 1..end]);
        out.push_str(&match name {
            "entries" => entries_sql(args),
            "owner_settable" => owner_settable_sql(args),
            "acl_hash" => acl_hash_sql(&acl_rows_sql(args)),
            "rel_acl_hash" => {
                let (oid, acl) = args.split_once(',').expect("rel_acl_hash(oid, acl)");
                acl_hash_sql(&format!(
                    "{} UNION ALL {}",
                    acl_rows_sql(acl.trim()),
                    column_acl_rows_sql(oid.trim())
                ))
            }
            other => panic!("unknown SQL fragment {other}"),
        });
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

fn entries_sql(acl: &str) -> String {
    format!(
        "(SELECT coalesce(pg_catalog.jsonb_agg(pg_catalog.jsonb_build_object(\
           'grantor', pg_catalog.jsonb_build_object('oid', a.grantor::int8, 'name', pg_catalog.pg_get_userbyid(a.grantor)), \
           'grantee', CASE WHEN a.grantee = 0 THEN '\"public\"'::jsonb \
             ELSE pg_catalog.jsonb_build_object('role', pg_catalog.jsonb_build_object('oid', a.grantee::int8, 'name', t.rolname)) END, \
           'privilege', a.privilege_type, 'grantable', a.is_grantable) \
           ORDER BY a.privilege_type COLLATE \"C\", pg_catalog.pg_get_userbyid(a.grantor)::text COLLATE \"C\"), '[]'::jsonb) \
         FROM pg_catalog.aclexplode({acl}) a \
         JOIN target t ON t.oid = a.grantee)"
    )
}

fn owner_settable_sql(owner: &str) -> String {
    owner_settable_predicate(owner, "(SELECT oid FROM me)")
}

/// P27a (T1's ruling, narrowed on PG 17.11): whether the role `me` (an oid expression, the
/// database owner) can run statements as the role `owner` (an oid expression) through the
/// run-as sequence (P26): `owner` is `me`, `me` can SET to it (any path; before Postgres 16
/// every membership can), or `me` holds ADMIN on it directly or through an INHERIT membership.
/// ADMIN reached only through a non-INHERIT membership doesn't count: the transient grant then
/// finds no grantor (`no possible grantors`).
pub fn owner_settable_predicate(owner: &str, me: &str) -> String {
    format!(
        "({owner} = {me} \
         OR CASE WHEN pg_catalog.current_setting('server_version_num')::int >= 160000 \
              THEN pg_catalog.pg_has_role({me}, {owner}, 'SET') \
              ELSE pg_catalog.pg_has_role({me}, {owner}, 'MEMBER') END \
         OR EXISTS (SELECT FROM pg_catalog.pg_auth_members m \
              WHERE m.roleid = {owner} AND m.admin_option \
                AND pg_catalog.pg_has_role({me}, m.member, 'USAGE')))"
    )
}

/// `(col, grantee, privilege, grantor, grantable)` of `acl` (`col` empty, PUBLIC's grantee
/// NULL).
fn acl_rows_sql(acl: &str) -> String {
    format!(
        "SELECT ''::text AS col, \
           CASE WHEN a.grantee = 0 THEN NULL ELSE pg_catalog.pg_get_userbyid(a.grantee)::text END AS grantee, \
           a.privilege_type::text AS privilege, pg_catalog.pg_get_userbyid(a.grantor)::text AS grantor, \
           a.is_grantable AS grantable \
         FROM pg_catalog.aclexplode({acl}) a"
    )
}

/// The same rows for every column ACL of relation `oid`.
fn column_acl_rows_sql(oid: &str) -> String {
    format!(
        "SELECT att.attname::text, \
           CASE WHEN a.grantee = 0 THEN NULL ELSE pg_catalog.pg_get_userbyid(a.grantee)::text END, \
           a.privilege_type::text, pg_catalog.pg_get_userbyid(a.grantor)::text, a.is_grantable \
         FROM pg_catalog.pg_attribute att \
         CROSS JOIN LATERAL pg_catalog.aclexplode(att.attacl) a \
         WHERE att.attrelid = {oid} AND att.attnum > 0 AND NOT att.attisdropped AND att.attacl IS NOT NULL"
    )
}

/// Hex sha256 over `rows` as one sorted JSON array (an empty ACL is `[]`).
fn acl_hash_sql(rows: &str) -> String {
    format!(
        "(SELECT pg_catalog.encode(pg_catalog.sha256(pg_catalog.convert_to(coalesce(\
           pg_catalog.jsonb_agg(pg_catalog.jsonb_build_array(h.col, h.grantee, h.privilege, h.grantor, h.grantable) \
             ORDER BY h.col COLLATE \"C\", h.grantee COLLATE \"C\" NULLS FIRST, h.privilege COLLATE \"C\", \
                      h.grantor COLLATE \"C\", h.grantable)::text, \
           '[]'), 'UTF8')), 'hex') \
         FROM ({rows}) h)"
    )
}

/// What the request asks of one ACL entry, after all its ops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Want {
    Granted,
    Revoked,
    /// `REVOKE GRANT OPTION FOR`: the entry may stay, not grantable.
    NotGrantable,
}

/// One entry an op names: the object, the column (`None`: the object itself), the grantee
/// (`None`: PUBLIC) and the upper-case privilege.
type EntryKey = (
    PrivilegeTarget,
    Option<String>,
    Option<String>,
    &'static str,
);

fn grantee_key(g: &Grantee) -> Option<String> {
    match g {
        Grantee::Public => None,
        Grantee::Role(r) => Some(r.clone()),
    }
}

fn relation(t: &TableRef) -> PrivilegeTarget {
    PrivilegeTarget::Relation {
        schema: t.schema.clone(),
        name: t.name.clone(),
    }
}

/// Every entry `ops` name with what the request wants of it; a later op on the same entry
/// overrides an earlier one (a revoke then a grant ends granted).
fn wanted(ops: &[Operation]) -> BTreeMap<EntryKey, Want> {
    let mut out = BTreeMap::new();
    let revoke_want = |option_only: bool| {
        if option_only {
            Want::NotGrantable
        } else {
            Want::Revoked
        }
    };
    for op in ops {
        let (target, columns, grantee, privileges, want): (
            PrivilegeTarget,
            Vec<Option<String>>,
            &Grantee,
            Vec<&'static str>,
            Want,
        ) = match op {
            Operation::Grant { table, grant } | Operation::Revoke { table, grant } => {
                let want = if matches!(op, Operation::Grant { .. }) {
                    Want::Granted
                } else {
                    revoke_want(grant.with_grant_option)
                };
                let columns = if grant.columns.is_empty() {
                    vec![None]
                } else {
                    grant.columns.iter().cloned().map(Some).collect()
                };
                let privileges = grant
                    .privileges
                    .iter()
                    .map(|p| privilege_keyword(*p))
                    .collect();
                (relation(table), columns, &grant.grantee, privileges, want)
            }
            Operation::GrantSchema {
                schema,
                grantee,
                privileges,
            }
            | Operation::RevokeSchema {
                schema,
                grantee,
                privileges,
                ..
            } => {
                let want = match op {
                    Operation::RevokeSchema {
                        grant_option_only, ..
                    } => revoke_want(*grant_option_only),
                    _ => Want::Granted,
                };
                let privileges = privileges.iter().map(|p| schema_privilege(*p)).collect();
                let target = PrivilegeTarget::Schema {
                    schema: schema.clone(),
                };
                (target, vec![None], grantee, privileges, want)
            }
            Operation::GrantSequence {
                sequence,
                grantee,
                privileges,
            }
            | Operation::RevokeSequence {
                sequence,
                grantee,
                privileges,
                ..
            } => {
                let want = match op {
                    Operation::RevokeSequence {
                        grant_option_only, ..
                    } => revoke_want(*grant_option_only),
                    _ => Want::Granted,
                };
                let privileges = privileges.iter().map(|p| sequence_privilege(*p)).collect();
                let target = PrivilegeTarget::Sequence {
                    schema: sequence.schema.clone(),
                    name: sequence.name.clone(),
                };
                (target, vec![None], grantee, privileges, want)
            }
            Operation::GrantFunction { function, grantee } => (
                PrivilegeTarget::Function { oid: function.oid },
                vec![None],
                grantee,
                vec!["EXECUTE"],
                Want::Granted,
            ),
            Operation::RevokeFunction {
                function,
                grantee,
                grant_option_only,
            } => (
                PrivilegeTarget::Function { oid: function.oid },
                vec![None],
                grantee,
                vec!["EXECUTE"],
                revoke_want(*grant_option_only),
            ),
            Operation::AlterDefaultPrivileges {
                for_role,
                schema,
                object,
                grant,
                grantee,
                privileges,
            } => (
                PrivilegeTarget::DefaultAcl {
                    for_role: for_role.clone(),
                    schema: schema.clone(),
                    object: *object,
                },
                vec![None],
                grantee,
                privileges
                    .iter()
                    .map(|p| privilege_name_keyword(*p))
                    .collect(),
                if *grant { Want::Granted } else { Want::Revoked },
            ),
            _ => continue,
        };
        for privilege in privileges {
            for column in &columns {
                out.insert(
                    (
                        target.clone(),
                        column.clone(),
                        grantee_key(grantee),
                        privilege,
                    ),
                    want,
                );
            }
        }
    }
    out
}

/// An object's own entries and its columns' entries.
type ObjectEntries<'a> = (Vec<&'a Grant>, BTreeMap<&'a str, Vec<&'a Grant>>);

/// What the re-read shows, per object.
#[derive(Default)]
struct Seen<'a> {
    objects: BTreeMap<PrivilegeTarget, ObjectEntries<'a>>,
}

impl<'a> Seen<'a> {
    fn of(pages: &'a [PrivilegesPage]) -> Seen<'a> {
        let mut seen = Seen::default();
        for page in pages {
            for s in &page.schemas {
                seen.add(
                    PrivilegeTarget::Schema {
                        schema: s.name.clone(),
                    },
                    &s.grants,
                    &[],
                );
            }
            for d in &page.default_privileges {
                seen.add(
                    PrivilegeTarget::DefaultAcl {
                        for_role: d.for_role.clone(),
                        schema: d.schema.clone(),
                        object: d.object,
                    },
                    &d.grants,
                    &[],
                );
            }
            for o in &page.objects {
                seen.add(object_target(o), &o.grants, &o.columns);
            }
        }
        seen
    }

    fn add(
        &mut self,
        target: PrivilegeTarget,
        grants: &'a [Grant],
        columns: &'a [ColumnPrivileges],
    ) {
        let entry = self.objects.entry(target).or_default();
        entry.0.extend(grants);
        for c in columns {
            entry
                .1
                .entry(c.name.as_str())
                .or_default()
                .extend(&c.grants);
        }
    }

    /// The entries of `column` (`None`: the object's own) of `target`; `None` when no page has
    /// the object.
    fn entries(&self, target: &PrivilegeTarget, column: Option<&str>) -> Option<Vec<&'a Grant>> {
        let (own, columns) = self.objects.get(target)?;
        Some(match column {
            None => own.clone(),
            Some(c) => columns.get(c).cloned().unwrap_or_default(),
        })
    }
}

/// The ACL target of a listed object.
fn object_target(o: &ObjectPrivileges) -> PrivilegeTarget {
    match o.kind {
        PrivilegeObjectKind::Function | PrivilegeObjectKind::Procedure => {
            PrivilegeTarget::Function { oid: o.oid }
        }
        PrivilegeObjectKind::Sequence => PrivilegeTarget::Sequence {
            schema: o.schema.clone(),
            name: o.name.clone(),
        },
        PrivilegeObjectKind::Table
        | PrivilegeObjectKind::View
        | PrivilegeObjectKind::MaterializedView
        | PrivilegeObjectKind::PartitionedTable
        | PrivilegeObjectKind::ForeignTable => PrivilegeTarget::Relation {
            schema: o.schema.clone(),
            name: o.name.clone(),
        },
    }
}

fn is_grantee(g: &Grant, grantee: &Option<String>) -> bool {
    match (&g.grantee, grantee) {
        (RoleRef::Public, None) => true,
        (RoleRef::Role { name, .. }, Some(n)) => name == n,
        _ => false,
    }
}

/// MH-24: the end state `ops` ask for, against the re-read pages (one per (grantee, schema)
/// the ops touch, plus the `schema: None` page for schema and default-privilege ops, each read
/// to its last page); every difference.
///
/// Per grantor: a grant is in place when any grantor holds it; a revoke is complete only when
/// no grantor does (one mismatch per grantor still holding it); a grant-option revoke only when
/// no grantor's entry is grantable. A table-wide revoke also covers the table's column
/// entries of that privilege (Postgres revokes those too), unless a later op names the column.
/// An object no page lists holds nothing.
pub fn verify_privileges(
    ops: &[Operation],
    after: &[PrivilegesPage],
) -> Result<(), Vec<PrivilegeMismatch>> {
    let seen = Seen::of(after);
    let wanted = wanted(ops);
    let mut out: BTreeSet<PrivilegeMismatch> = BTreeSet::new();
    let mismatch = |target: &PrivilegeTarget,
                    column: Option<&str>,
                    grantee: &Option<String>,
                    privilege: &str,
                    by: Option<&Grant>| PrivilegeMismatch {
        object: target.clone(),
        privilege: privilege.to_string(),
        grantee: grantee.clone().unwrap_or_else(|| "PUBLIC".to_string()),
        still_granted_by: by.map(|g| g.grantor.name.clone()),
        column: column.map(str::to_string),
        wanted: by.is_none(),
    };
    for ((target, column, grantee, privilege), want) in &wanted {
        fn holding<'g>(
            entries: &[&'g Grant],
            grantee: &Option<String>,
            privilege: &str,
        ) -> Vec<&'g Grant> {
            entries
                .iter()
                .copied()
                .filter(|g| is_grantee(g, grantee) && g.privilege == privilege)
                .collect()
        }
        let entries = seen.entries(target, column.as_deref());
        match want {
            Want::Granted => {
                if entries.is_none_or(|e| holding(&e, grantee, privilege).is_empty()) {
                    out.insert(mismatch(
                        target,
                        column.as_deref(),
                        grantee,
                        privilege,
                        None,
                    ));
                }
            }
            Want::Revoked | Want::NotGrantable => {
                let mut left: Vec<(Option<&str>, &Grant)> = entries
                    .map(|e| holding(&e, grantee, privilege))
                    .unwrap_or_default()
                    .into_iter()
                    .map(|g| (column.as_deref(), g))
                    .collect();
                // T4-2: both kinds of table-wide revoke reach the column entries (a
                // `REVOKE GRANT OPTION FOR` takes the option off them too).
                if column.is_none()
                    && matches!(target, PrivilegeTarget::Relation { .. })
                    && let Some((_, columns)) = seen.objects.get(target)
                {
                    for (c, grants) in columns {
                        let named = wanted.contains_key(&(
                            target.clone(),
                            Some(c.to_string()),
                            grantee.clone(),
                            *privilege,
                        ));
                        if !named {
                            left.extend(
                                holding(grants, grantee, privilege)
                                    .into_iter()
                                    .map(|g| (Some(*c), g)),
                            );
                        }
                    }
                }
                for (c, g) in left {
                    if *want == Want::Revoked || g.grantable {
                        out.insert(mismatch(target, c, grantee, privilege, Some(g)));
                    }
                }
            }
        }
    }
    // T4-3: the last `SetAllRows` on a (table, role) decides; the role's own page shows
    // whether the hub's policy is there (a table no page of the role lists has none).
    let mut all_rows: BTreeMap<(PrivilegeTarget, &str), bool> = BTreeMap::new();
    for op in ops {
        if let Operation::SetAllRows {
            table,
            role,
            enabled,
        } = op
        {
            all_rows.insert((relation(table), role.as_str()), *enabled);
        }
    }
    for ((target, role), enabled) in all_rows {
        let present = after
            .iter()
            .filter(|p| p.role == role)
            .flat_map(|p| &p.objects)
            .any(|o| o.all_rows.is_some() && object_target(o) == target);
        if present != enabled {
            out.insert(PrivilegeMismatch {
                object: target,
                privilege: "ALL_ROWS".to_string(),
                grantee: role.to_string(),
                still_granted_by: None,
                column: None,
                wanted: enabled,
            });
        }
    }
    if out.is_empty() {
        Ok(())
    } else {
        Err(out.into_iter().collect())
    }
}

/// EOS-H1: the roles that have the hub's "all rows" policy on some table of the current
/// database (the read's shape and marker, MH-26: permissive, `FOR ALL`, that one role,
/// `USING (true) WITH CHECK (true)`, commented [`ALL_ROWS_MARKER`]), one `text` per row.
pub const ALL_ROWS_ROLES_SQL: &str = "SELECT DISTINCT r.rolname::pg_catalog.text \
     FROM pg_catalog.pg_policy pol \
     JOIN pg_catalog.pg_roles r ON pol.polroles = ARRAY[r.oid] \
     WHERE pol.polpermissive AND pol.polcmd = '*' \
       AND pg_catalog.pg_get_expr(pol.polqual, pol.polrelid) = 'true' \
       AND pg_catalog.pg_get_expr(pol.polwithcheck, pol.polrelid) = 'true' \
       AND pg_catalog.obj_description(pol.oid, 'pg_policy') = 'chelabase:all_rows'";

/// EOS-H1: every membership as member → the roles it is a member of (any option, MH-26), one
/// `(member, role)` pair of `text` per row.
pub const MEMBERSHIPS_SQL: &str = "SELECT pg_catalog.pg_get_userbyid(m.member)::pg_catalog.text, \
     pg_catalog.pg_get_userbyid(m.roleid)::pg_catalog.text FROM pg_catalog.pg_auth_members m";

/// EOS-H1: the shortest chain by which one of `starting` reaches a role of `all_rows` through
/// `after` (member → roles it is a member of), `[start, …, role]`; with `before`, only a
/// (start, role) pair `before` didn't already connect counts (a change exposes only what it
/// adds). `None` when nothing is exposed. Ties go to the first start and role in name order.
pub fn new_all_rows_chain(
    after: &BTreeMap<String, BTreeSet<String>>,
    before: Option<&BTreeMap<String, BTreeSet<String>>>,
    starting: &BTreeSet<String>,
    all_rows: &BTreeSet<String>,
) -> Option<Vec<String>> {
    let mut best: Option<Vec<String>> = None;
    for start in starting {
        let from: BTreeSet<String> = [start.clone()].into();
        for role in all_rows {
            let Some(chain) = exposure_chain(after, role, &from) else {
                continue;
            };
            if before.is_some_and(|b| exposure_chain(b, role, &from).is_some()) {
                continue;
            }
            if best.as_ref().is_none_or(|b| chain.len() < b.len()) {
                best = Some(chain);
            }
        }
    }
    best
}

/// The roles whose membership makes `role`'s rows reachable through the data API: `anon`,
/// `authenticated`, `authenticator` and the live keys' roles.
const DATA_API_ROLES: [&str; 3] = ["anon", "authenticated", "authenticator"];

/// MH-26: the membership chain by which `role` is, or is inherited by, `anon`,
/// `authenticated`, `authenticator`, one of `key_roles`, or PUBLIC; `None` when it's safe.
/// `edges`: member → roles it is a member of (any option).
///
/// The chain runs from the data API role to `role` (`[anon, mid, role]`), one of the shortest;
/// `[role]` when `role` is itself one of them or PUBLIC (any case).
pub fn all_rows_exposure(
    edges: &BTreeMap<String, BTreeSet<String>>,
    role: &str,
    key_roles: &BTreeSet<String>,
) -> Option<Vec<String>> {
    // PUBLIC is a starting role here only: every role is in it, so it exposes any `role`
    // spelled `public` (it has no membership edges to follow).
    if role.eq_ignore_ascii_case("public") {
        return Some(vec![role.to_string()]);
    }
    let starting: BTreeSet<String> = DATA_API_ROLES
        .iter()
        .map(|r| r.to_string())
        .chain(key_roles.iter().cloned())
        .collect();
    exposure_chain(edges, role, &starting)
}

/// MH-26, P31 (T4-8): one of the shortest membership chains by which `role` is, or is inherited
/// by, one of `starting` (`[start, mid, role]`); `[role]` when `role` is itself in `starting`;
/// `None` when no starting role reaches it. `edges` as in [`all_rows_exposure`], which is this
/// check started from `anon`, `authenticated`, `authenticator`, `key_roles` and PUBLIC. P31's
/// "exposes to the data API" flag starts from `anon` and `authenticated` only.
pub fn exposure_chain(
    edges: &BTreeMap<String, BTreeSet<String>>,
    role: &str,
    starting: &BTreeSet<String>,
) -> Option<Vec<String>> {
    if starting.contains(role) {
        return Some(vec![role.to_string()]);
    }
    // Breadth first from every starting role at once: each role's first discoverer is its
    // parent, and a role seen once is never queued again, so cycles end.
    let mut parent: BTreeMap<&str, Option<&str>> =
        starting.iter().map(|s| (s.as_str(), None)).collect();
    let mut queue: VecDeque<&str> = starting.iter().map(String::as_str).collect();
    while let Some(member) = queue.pop_front() {
        for of in edges.get(member).into_iter().flatten() {
            if parent.contains_key(of.as_str()) {
                continue;
            }
            parent.insert(of.as_str(), Some(member));
            if of == role {
                let mut chain = vec![role.to_string()];
                let mut at = member;
                loop {
                    chain.push(at.to_string());
                    match parent.get(at).copied().flatten() {
                        Some(p) => at = p,
                        None => break,
                    }
                }
                chain.reverse();
                return Some(chain);
            }
            queue.push_back(of.as_str());
        }
    }
    None
}

/// The longest prefix of `s` that fits `max` bytes, on a character boundary.
fn truncated(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// `<role>_all_rows`, at most 63 bytes, with an 8-hex suffix on a clash with `taken`.
///
/// The role's name is cut (on a character boundary) to fit; a clash adds `_<8 hex>` from
/// sha256 over the role and an attempt counter, so the name is deterministic for a given
/// `taken`.
pub fn all_rows_policy_name(role: &str, taken: &BTreeSet<String>) -> String {
    const SUFFIX: &str = "_all_rows";
    let plain = format!("{}{SUFFIX}", truncated(role, 63 - SUFFIX.len()));
    if !taken.contains(&plain) {
        return plain;
    }
    let head = truncated(role, 63 - SUFFIX.len() - 9);
    (0u64..)
        .map(|attempt| {
            let mut h = Sha256::new();
            h.update(role.as_bytes());
            h.update([0]);
            h.update(attempt.to_be_bytes());
            format!("{head}{SUFFIX}_{}", hex::encode(&h.finalize()[..4]))
        })
        .find(|n| !taken.contains(n))
        .expect("a free name")
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod fragment_tests {
    use super::*;

    /// Every fragment is expanded, and both reads hash with the same expression (S3), a NULL
    /// ACL as its default (T4-4: `coalesce(acl, acldefault(kind, owner))`; final round,
    /// deliberately updated for that expression).
    #[test]
    fn the_reads_share_one_hash_expression_and_leave_no_fragment() {
        for sql in [privileges_sql(), acl_hashes_sql()] {
            assert!(!sql.contains('@'), "{sql}");
        }
        let plain = |acl: &str| acl_hash_sql(&acl_rows_sql(acl));
        let dflt = |acl: &str, kind: &str, owner: &str| {
            format!("coalesce({acl}, pg_catalog.acldefault({kind}, {owner}))")
        };
        assert!(acl_hashes_sql().contains(&plain(&dflt("n.nspacl", "'n'", "n.nspowner"))));
        assert!(privileges_sql().contains(&plain(&dflt("s.nspacl", "'n'", "s.nspowner"))));
        assert!(acl_hashes_sql().contains(&plain(&dflt("c.relacl", "'s'", "c.relowner"))));
        assert!(acl_hashes_sql().contains(&plain(&dflt("p.proacl", "'f'", "p.proowner"))));
        assert!(privileges_sql().contains(&plain(&dflt("o.acl", "o.default_kind", "o.owner"))));
        let rel = |oid: &str, acl: &str| {
            acl_hash_sql(&format!(
                "{} UNION ALL {}",
                acl_rows_sql(acl),
                column_acl_rows_sql(oid)
            ))
        };
        assert!(acl_hashes_sql().contains(&rel("c.oid", &dflt("c.relacl", "'r'", "c.relowner"))));
        assert!(
            privileges_sql().contains(&rel("o.oid", &dflt("o.acl", "o.default_kind", "o.owner")))
        );
    }

    /// EOS-H1: the all-rows roles read finds the hub's policy by its marker.
    #[test]
    fn the_all_rows_roles_read_uses_the_marker() {
        assert!(ALL_ROWS_ROLES_SQL.contains(&format!("'{ALL_ROWS_MARKER}'")));
    }

    /// EOS-H1: a change exposes only the (start, role) pairs it connects; the shortest chain
    /// wins; nothing is exposed without an all-rows role.
    #[test]
    fn a_new_chain_is_one_the_change_adds() {
        let graph = |pairs: &[(&str, &str)]| {
            let mut edges: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
            for (m, of) in pairs {
                edges
                    .entry(m.to_string())
                    .or_default()
                    .insert(of.to_string());
            }
            edges
        };
        let set =
            |names: &[&str]| -> BTreeSet<String> { names.iter().map(|s| s.to_string()).collect() };
        let before = graph(&[("anon", "r"), ("k", "m")]);
        let after = graph(&[("anon", "r"), ("k", "m"), ("m", "r")]);
        let all = set(&["r"]);
        // anon already reached r: only k's new path counts.
        assert_eq!(
            new_all_rows_chain(&after, Some(&before), &set(&["anon", "k"]), &all),
            Some(vec!["k".into(), "m".into(), "r".into()])
        );
        assert_eq!(
            new_all_rows_chain(&before, Some(&before), &set(&["anon", "k"]), &all),
            None
        );
        // Without `before` every reach counts, the shortest first.
        assert_eq!(
            new_all_rows_chain(&after, None, &set(&["anon", "k"]), &all),
            Some(vec!["anon".into(), "r".into()])
        );
        assert_eq!(
            new_all_rows_chain(&after, None, &set(&["r"]), &all),
            Some(vec!["r".into()])
        );
        assert_eq!(
            new_all_rows_chain(&after, None, &set(&["k"]), &set(&[])),
            None
        );
    }

    /// MH-26: the chain is one of the shortest, from the data API side to the role.
    #[test]
    fn exposure_returns_a_shortest_chain() {
        let mut edges: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (m, of) in [
            ("anon", "a"),
            ("a", "b"),
            ("b", "target"),
            ("authenticated", "target"),
        ] {
            edges.entry(m.into()).or_default().insert(of.into());
        }
        assert_eq!(
            all_rows_exposure(&edges, "target", &BTreeSet::new()),
            Some(vec!["authenticated".to_string(), "target".to_string()])
        );
    }

    /// T4-8: PUBLIC starts only the all-rows check; `exposure_chain` takes `public` as a name
    /// like any other, and the all-rows check is `exposure_chain` from its full set otherwise.
    #[test]
    fn public_starts_only_the_all_rows_check() {
        let mut edges: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (m, of) in [("anon", "a"), ("k", "b"), ("x", "c"), ("c", "x")] {
            edges.entry(m.into()).or_default().insert(of.into());
        }
        let keys: BTreeSet<String> = ["k".to_string()].into();
        let p31: BTreeSet<String> = ["anon".to_string(), "authenticated".to_string()].into();
        for public in ["public", "PUBLIC", "Public"] {
            assert_eq!(exposure_chain(&edges, public, &p31), None, "{public}");
            assert_eq!(
                all_rows_exposure(&edges, public, &keys),
                Some(vec![public.to_string()])
            );
        }
        let full: BTreeSet<String> = ["anon", "authenticated", "authenticator", "k"]
            .map(String::from)
            .into();
        for role in ["a", "b", "c", "x", "anon", "k", "authenticator", "none", ""] {
            assert_eq!(
                all_rows_exposure(&edges, role, &keys),
                exposure_chain(&edges, role, &full),
                "{role}"
            );
        }
    }
}
