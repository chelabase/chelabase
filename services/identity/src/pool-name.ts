// Pool names are the only caller input that reaches DDL (as a schema name), so they are
// checked against one exact pattern before anything else happens.

export interface PoolRef {
  /** The name callers use: `members` or `app-<lowercase uuid>`. */
  name: string;
  /** The Postgres schema holding the pool's tables. Always matches SCHEMA_PATTERN. */
  schema: string;
}

const APP_POOL = /^app-([0-9a-f]{8})-([0-9a-f]{4})-([0-9a-f]{4})-([0-9a-f]{4})-([0-9a-f]{12})$/;
/** The same pattern m5's `identity_admin` functions enforce. */
export const SCHEMA_PATTERN = /^identity_(members|app_[0-9a-f]{32})$/;

/** The pool for `name`, or null when it is not exactly `members` or `app-<lowercase uuid>`. */
export function parsePoolName(name: string): PoolRef | null {
  if (name === "members") return { name, schema: "identity_members" };
  const m = APP_POOL.exec(name);
  if (!m) return null;
  const schema = `identity_app_${m.slice(1).join("")}`;
  // Belt and braces: the derived schema must pass the DDL functions' own check.
  return SCHEMA_PATTERN.test(schema) ? { name, schema } : null;
}
