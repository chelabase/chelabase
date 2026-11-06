// A Postgres dialect whose introspection reads one pool's schema only.
//
// Better Auth introspects twice per pool: its migration (getMigrations) and the schema check
// before an instance's first request. Both call Kysely's PostgresIntrospector.getTables, which
// reads every schema in the database and passes each one's *name* to `has_schema_privilege`
// and `pg_get_serial_sequence`. Those resolve the name against the latest catalog state, not
// the query's snapshot, so a pool schema dropped while the query runs fails it with
// `schema "identity_app_…" does not exist` (get_namespace_oid): one pool's drop broke another
// pool's create or first request. It also made each introspection cost every pool's tables.
//
// This introspector reads only the pool's schema (its name, or the connection's
// current_schema() when Better Auth runs in search_path mode) and uses OIDs throughout, so it
// sees one consistent snapshot of exactly the tables Better Auth compares.
import {
  PostgresDialect,
  PostgresIntrospector,
  sql,
  type DatabaseIntrospector,
  type Kysely,
  type PostgresDialectConfig,
  type SchemaMetadata,
  type TableMetadata,
} from "kysely";

interface ColumnRow {
  column: string;
  not_null: boolean;
  has_default: boolean;
  table: string;
  table_type: string;
  schema: string;
  type: string;
  type_schema: string;
  column_description: string | null;
  auto_incrementing: boolean;
}

class PoolIntrospector implements DatabaseIntrospector {
  readonly #db: Kysely<any>;
  readonly #schema: string | undefined;

  constructor(db: Kysely<any>, schema: string | undefined) {
    this.#db = db;
    this.#schema = schema;
  }

  getSchemas(): Promise<SchemaMetadata[]> {
    // pg_namespace names only: no per-schema function, nothing to race.
    return new PostgresIntrospector(this.#db).getSchemas();
  }

  async getTables(): Promise<TableMetadata[]> {
    const schema = this.#schema === undefined ? sql`pg_catalog.current_schema()` : sql`${this.#schema}`;
    const { rows } = await sql<ColumnRow>`
      SELECT a.attname AS "column", a.attnotnull AS not_null, a.atthasdef AS has_default,
             c.relname AS "table", c.relkind::text AS table_type, ns.nspname AS "schema",
             typ.typname AS "type", dtns.nspname AS type_schema,
             pg_catalog.col_description(a.attrelid, a.attnum) AS column_description,
             EXISTS (
               SELECT 1 FROM pg_catalog.pg_depend d
                 JOIN pg_catalog.pg_class s ON s.oid = d.objid AND s.relkind = 'S'
                WHERE d.classid = 'pg_catalog.pg_class'::pg_catalog.regclass
                  AND d.refclassid = 'pg_catalog.pg_class'::pg_catalog.regclass
                  AND d.refobjid = c.oid AND d.refobjsubid = a.attnum
                  AND d.deptype IN ('a', 'i')
             ) AS auto_incrementing
        FROM pg_catalog.pg_attribute a
        JOIN pg_catalog.pg_class c ON c.oid = a.attrelid
        JOIN pg_catalog.pg_namespace ns ON ns.oid = c.relnamespace
        JOIN pg_catalog.pg_type typ ON typ.oid = a.atttypid
        JOIN pg_catalog.pg_namespace dtns ON dtns.oid = typ.typnamespace
       WHERE ns.nspname = ${schema}
         AND c.relkind IN ('r', 'v', 'p', 'f')
         AND a.attnum >= 0 AND NOT a.attisdropped
       ORDER BY c.relname, a.attnum
    `.execute(this.#db);
    // The same shape as Kysely's PostgresIntrospector.
    const tables = new Map<string, TableMetadata>();
    for (const r of rows) {
      let table = tables.get(r.table);
      if (!table) {
        table = {
          name: r.table,
          schema: r.schema,
          isView: r.table_type === "v",
          isForeign: r.table_type === "f",
          columns: [],
        };
        tables.set(r.table, table);
      }
      table.columns.push({
        name: r.column,
        dataType: r.type,
        dataTypeSchema: r.type_schema,
        isNullable: !r.not_null,
        isAutoIncrementing: r.auto_incrementing,
        hasDefaultValue: r.has_default,
        comment: r.column_description ?? undefined,
      });
    }
    return [...tables.values()];
  }
}

/**
 * PostgresDialect introspecting only `schema`, or the connection's current_schema() when it is
 * undefined (Better Auth's search_path mode, on a connection pinned to the pool's schema).
 */
export class PoolDialect extends PostgresDialect {
  readonly #schema: string | undefined;

  constructor(config: PostgresDialectConfig, schema: string | undefined) {
    super(config);
    this.#schema = schema;
  }

  override createIntrospector(db: Kysely<any>): DatabaseIntrospector {
    return new PoolIntrospector(db, this.#schema);
  }
}
