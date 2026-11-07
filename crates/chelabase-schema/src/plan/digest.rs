//! The target's schema digest (E4): phase 1's stale guard and the apply request's stale check.

/// One query (no `;`, so the guard can nest it) returning the `md5` of a catalog projection of
/// the 6g object types: schemas, tables, sequences and indexes (`pg_class` `r`/`S`/`i`), their
/// columns, defaults, constraints, index definitions, enum types, sequence parameters,
/// policies, ACLs, default privileges, comments (policies' included) and sequence ownership.
///
/// - Same exclusions as the catalog reader (P6): `pg_catalog`, `information_schema`,
///   `pg_toast`, temporary schemas, `chelabase`, `chelabase_merge`, `chelabase_guard`,
///   `storage`, `chelabase_functions` and extension members.
/// - Raw catalog columns and node trees, OIDs included, never `pg_get_*def`: the text doesn't
///   depend on `search_path` or the session's role, and any DDL on a 6g object changes it.
///   It's only ever compared on the same database.
/// - Left out on purpose: roles and memberships (phase 0 changes them before phase 1) and
///   enum values (phase 1a adds them before phase 1). A role or enum value change on the
///   target after the rehearsal makes phase 1 fail (rolled back) rather than stale.
///
/// **The admin session (`cloud_admin`, a superuser) must run it through
/// [`admin_digest_sql`]**, which sets [`DIGEST_PRELUDE`] first: the query's operators and
/// casts (`=` between `oid` and `regclass`, …) resolve through `search_path`, and an operator
/// a tenant created in `public` would otherwise run as superuser. Its concatenations are
/// qualified (`OPERATOR(pg_catalog.||)`) as a second guard, and every `"char"` column is cast
/// to `text` (`text || "char"` has no unique operator). The copy inside phase 1's guard
/// runs in the owner's session (E15) and needs no prelude.
pub const TARGET_DIGEST_SQL: &str = "WITH nsp AS (\
SELECT n.oid FROM pg_catalog.pg_namespace n \
WHERE n.nspname NOT IN ('pg_catalog', 'information_schema', 'pg_toast', 'chelabase', 'chelabase_merge', 'chelabase_guard', 'storage', 'chelabase_functions') \
AND n.nspname NOT LIKE 'pg\\_temp\\_%' AND n.nspname NOT LIKE 'pg\\_toast\\_temp\\_%' \
AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_depend d WHERE d.classid = 'pg_catalog.pg_namespace'::pg_catalog.regclass AND d.objid = n.oid AND d.deptype = 'e')\
), rel AS (\
SELECT c.oid FROM pg_catalog.pg_class c \
WHERE c.relnamespace IN (SELECT oid FROM nsp) AND c.relkind IN ('r', 'S', 'i') \
AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_depend d WHERE d.classid = 'pg_catalog.pg_class'::pg_catalog.regclass AND d.objid = c.oid AND d.deptype = 'e')\
), typ AS (\
SELECT t.oid FROM pg_catalog.pg_type t \
WHERE t.typtype = 'e' AND t.typnamespace IN (SELECT oid FROM nsp) \
AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_depend d WHERE d.classid = 'pg_catalog.pg_type'::pg_catalog.regclass AND d.objid = t.oid AND d.deptype = 'e')\
), r AS (\
SELECT 'n|' OPERATOR(pg_catalog.||) n.oid OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) n.nspname OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) n.nspowner OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) coalesce(n.nspacl::text, '') AS x \
FROM pg_catalog.pg_namespace n WHERE n.oid IN (SELECT oid FROM nsp) \
UNION ALL \
SELECT 'c|' OPERATOR(pg_catalog.||) c.oid OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) c.relnamespace OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) c.relname OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) c.relkind::pg_catalog.text OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) c.relowner OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) c.relpersistence::pg_catalog.text OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) c.relrowsecurity OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) c.relforcerowsecurity OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) coalesce(c.relacl::text, '') OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) coalesce(c.reloptions::text, '') \
FROM pg_catalog.pg_class c WHERE c.oid IN (SELECT oid FROM rel) \
UNION ALL \
SELECT 'a|' OPERATOR(pg_catalog.||) a.attrelid OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) a.attnum OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) a.attname OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) a.atttypid OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) a.atttypmod OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) a.attnotnull OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) a.attidentity::pg_catalog.text OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) a.attgenerated::pg_catalog.text OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) a.attisdropped OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) a.attcollation OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) coalesce(a.attacl::text, '') \
FROM pg_catalog.pg_attribute a WHERE a.attrelid IN (SELECT oid FROM rel) AND a.attnum > 0 \
UNION ALL \
SELECT 'd|' OPERATOR(pg_catalog.||) ad.adrelid OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) ad.adnum OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) ad.adbin::text \
FROM pg_catalog.pg_attrdef ad WHERE ad.adrelid IN (SELECT oid FROM rel) \
UNION ALL \
SELECT 'k|' OPERATOR(pg_catalog.||) co.oid OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) co.conrelid OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) co.conname OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) co.contype::pg_catalog.text OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) co.condeferrable OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) co.condeferred OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) co.convalidated OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) coalesce(co.conkey::text, '') OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) co.confrelid OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) coalesce(co.confkey::text, '') OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) co.confupdtype::pg_catalog.text OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) co.confdeltype::pg_catalog.text OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) co.confmatchtype::pg_catalog.text OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) coalesce(co.conbin::text, '') OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) coalesce(co.conexclop::text, '') OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) co.connoinherit OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) co.conindid \
FROM pg_catalog.pg_constraint co WHERE co.conrelid IN (SELECT oid FROM rel) \
UNION ALL \
SELECT 'i|' OPERATOR(pg_catalog.||) i.indexrelid OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) i.indrelid OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) i.indisunique OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) i.indnullsnotdistinct OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) i.indisprimary OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) i.indisvalid OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) i.indkey::text OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) i.indclass::text OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) i.indoption::text OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) i.indcollation::text OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) coalesce(i.indexprs::text, '') OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) coalesce(i.indpred::text, '') \
FROM pg_catalog.pg_index i WHERE i.indrelid IN (SELECT oid FROM rel) \
UNION ALL \
SELECT 't|' OPERATOR(pg_catalog.||) t.oid OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) t.typnamespace OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) t.typname OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) t.typowner OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) coalesce(t.typacl::text, '') \
FROM pg_catalog.pg_type t WHERE t.oid IN (SELECT oid FROM typ) \
UNION ALL \
SELECT 's|' OPERATOR(pg_catalog.||) s.seqrelid OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) s.seqtypid OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) s.seqstart OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) s.seqincrement OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) s.seqmax OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) s.seqmin OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) s.seqcache OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) s.seqcycle \
FROM pg_catalog.pg_sequence s WHERE s.seqrelid IN (SELECT oid FROM rel) \
UNION ALL \
SELECT 'p|' OPERATOR(pg_catalog.||) p.oid OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) p.polrelid OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) p.polname OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) p.polcmd::pg_catalog.text OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) p.polpermissive OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) p.polroles::text OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) coalesce(p.polqual::text, '') OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) coalesce(p.polwithcheck::text, '') \
FROM pg_catalog.pg_policy p WHERE p.polrelid IN (SELECT oid FROM rel) \
UNION ALL \
SELECT 'D|' OPERATOR(pg_catalog.||) da.oid OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) da.defaclrole OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) da.defaclnamespace OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) da.defaclobjtype::pg_catalog.text OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) coalesce(da.defaclacl::text, '') \
FROM pg_catalog.pg_default_acl da \
UNION ALL \
SELECT 'm|' OPERATOR(pg_catalog.||) de.classoid OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) de.objoid OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) de.objsubid OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) de.description \
FROM pg_catalog.pg_description de \
WHERE (de.classoid = 'pg_catalog.pg_class'::pg_catalog.regclass AND de.objoid IN (SELECT oid FROM rel)) \
OR (de.classoid = 'pg_catalog.pg_namespace'::pg_catalog.regclass AND de.objoid IN (SELECT oid FROM nsp)) \
OR (de.classoid = 'pg_catalog.pg_type'::pg_catalog.regclass AND de.objoid IN (SELECT oid FROM typ)) \
OR (de.classoid = 'pg_catalog.pg_policy'::pg_catalog.regclass AND de.objoid IN (SELECT p.oid FROM pg_catalog.pg_policy p WHERE p.polrelid IN (SELECT oid FROM rel))) \
UNION ALL \
SELECT 'o|' OPERATOR(pg_catalog.||) dp.objid OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) dp.refobjid OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) dp.refobjsubid OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) dp.deptype::pg_catalog.text \
FROM pg_catalog.pg_depend dp \
WHERE dp.classid = 'pg_catalog.pg_class'::pg_catalog.regclass AND dp.refclassid = 'pg_catalog.pg_class'::pg_catalog.regclass \
AND dp.deptype IN ('a', 'i') AND dp.objid IN (SELECT oid FROM rel) AND dp.refobjid IN (SELECT oid FROM rel)\
) SELECT pg_catalog.md5(coalesce(pg_catalog.string_agg(r.x, E'\\n' ORDER BY r.x COLLATE \"C\"), '')) FROM r";

/// What the admin session runs before [`TARGET_DIGEST_SQL`]: only `pg_catalog` (and the
/// session's own temporary schema, last) is searched, so no tenant operator, function or cast
/// can be picked up while the query runs as superuser.
pub const DIGEST_PRELUDE: &str = "SET search_path = pg_catalog, pg_temp;\n";

/// The digest as the admin session runs it: [`DIGEST_PRELUDE`], then [`TARGET_DIGEST_SQL`].
/// One psql session per call; the digest is the only row it prints (run psql quiet, `-q`,
/// so the `SET` tag isn't printed, or skip it).
pub fn admin_digest_sql() -> String {
    format!("{DIGEST_PRELUDE}{TARGET_DIGEST_SQL};\n")
}
