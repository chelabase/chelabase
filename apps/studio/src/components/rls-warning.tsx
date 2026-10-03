import type { DataApiSchema, Privilege } from "@cheladb/management-client";
import { CopyButton } from "./copy-button";

const WRITES: Privilege[] = ["insert", "update", "delete"];
const quote = (name: string) => `"${name.replaceAll('"', '""')}"`;

interface Exposed {
  schema: string;
  name: string;
  /** Why this relation bypasses RLS (empty for a plain table). */
  note: string;
  sql: string;
  copyLabel: string;
  /** Whether RLS (once enabled, with policies) governs it. */
  rlsApplies: boolean;
  anonReach: boolean;
  /** Writes by anon, or (for an authenticated-only relation) by authenticated. */
  writes: boolean;
  /** The project's app users table. */
  appUsers?: boolean;
  /** Nobody can reach it yet (the app users table before any GRANT). */
  unreached?: boolean;
}

/** Relations that anon or authenticated can reach without RLS protecting them. */
function exposed(schema: DataApiSchema): Exposed[] {
  const found: Exposed[] = [];
  for (const [schemaName, entry] of Object.entries(schema.schemas)) {
    for (const [name, info] of Object.entries(entry.tables)) {
      // A role missing from the map (or no map at all) has no access.
      const anon = info.privileges?.anon ?? [];
      const authenticated = info.privileges?.authenticated ?? [];
      if (anon.length === 0 && authenticated.length === 0) continue;
      const id = `${quote(schemaName)}.${quote(name)}`;
      const anonReach = anon.length > 0;
      const base = {
        schema: schemaName,
        name,
        anonReach,
        writes: (anonReach ? anon : authenticated).some((p) => WRITES.includes(p)),
      };
      switch (info.kind) {
        case "table":
          if (info.rls) break;
          found.push({ ...base, rlsApplies: true, note: "", sql: `ALTER TABLE ${id} ENABLE ROW LEVEL SECURITY;`, copyLabel: `Copy ALTER TABLE for ${schemaName}.${name}` });
          break;
        case "view":
          if (info.security_invoker) break;
          found.push({
            ...base,
            rlsApplies: true,
            note: "Runs with its owner's rights, so RLS on the tables it reads doesn't apply.",
            sql: `ALTER VIEW ${id} SET (security_invoker = true);`,
            copyLabel: `Copy ALTER VIEW for ${schemaName}.${name}`,
          });
          break;
        case "materialized_view":
        case "foreign_table":
          found.push({
            ...base,
            rlsApplies: false,
            note: `A ${info.kind === "foreign_table" ? "foreign table" : "materialized view"} can't have RLS.`,
            sql: `REVOKE ALL ON ${id} FROM anon, authenticated;`,
            copyLabel: `Copy REVOKE for ${schemaName}.${name}`,
          });
          break;
      }
    }
  }
  // The app users table is named even before any role can reach it: it holds the users' emails.
  const au = schema.app_users;
  const info = au ? schema.schemas[au.schema]?.tables[au.table] : undefined;
  if (au && info?.kind === "table" && !info.rls) {
    const existing = found.find((f) => f.schema === au.schema && f.name === au.table);
    if (existing) existing.appUsers = true;
    else {
      const id = `${quote(au.schema)}.${quote(au.table)}`;
      found.push({
        schema: au.schema, name: au.table, note: "", sql: `ALTER TABLE ${id} ENABLE ROW LEVEL SECURITY;`,
        copyLabel: `Copy ALTER TABLE for ${au.schema}.${au.table}`, rlsApplies: true, anonReach: false, writes: false,
        appUsers: true, unreached: true,
      });
    }
  }
  return found;
}

/** Warns about every reachable relation that bypasses RLS; renders nothing when there are none. */
export function RlsWarning({ schema }: { schema: DataApiSchema }) {
  const items = exposed(schema);
  if (items.length === 0) return null;
  const anonItems = items.filter((i) => i.anonReach);
  const authOnly = items.filter((i) => !i.anonReach && !i.unreached);
  const unreached = items.filter((i) => i.unreached);
  const anonWrites = anonItems.some((i) => i.writes);
  const authWrites = authOnly.some((i) => i.writes);
  const hasPolicies = items.some((i) => i.rlsApplies);
  return (
    <section aria-label="RLS warning" className="space-y-2 rounded-lg border border-destructive/50 p-3 text-sm">
      {anonItems.length > 0 && (
        <p className="font-medium text-destructive">
          {`Anyone with your public key can read these rows${anonWrites ? " and change them" : ""}. Enable RLS before production.`}
        </p>
      )}
      {authOnly.length > 0 && (
        <p className="font-medium text-destructive">
          {`Anyone with a secret key for the authenticated role can read these rows${authWrites ? " and change them" : ""}. Enable RLS before production.`}
        </p>
      )}
      {unreached.length > 0 && (
        <p className="font-medium text-destructive">
          The app users table has RLS off. Enable RLS before you grant any role access to it.
        </p>
      )}
      <ul className="space-y-2">
        {items.map((t) => (
          <li key={`${t.schema}.${t.name}`} data-table={`${t.schema}.${t.name}`} className="space-y-1">
            <div className="font-mono">
              {t.schema}.{t.name}
            </div>
            {t.appUsers && <p className="text-xs text-muted-foreground">This is the app users table.</p>}
            {t.note !== "" && <p className="text-xs text-muted-foreground">{t.note}</p>}
            <div className="flex flex-wrap items-center gap-2">
              <code className="rounded bg-muted px-1.5 py-0.5 text-xs break-all">{t.sql}</code>
              <CopyButton value={t.sql} label={t.copyLabel} />
            </div>
          </li>
        ))}
      </ul>
      {hasPolicies && (
        <p className="text-xs text-muted-foreground">
          With RLS on and no policy, anon and authenticated can&apos;t see any rows.
        </p>
      )}
    </section>
  );
}
