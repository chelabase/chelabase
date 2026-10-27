import type {
  BranchRole,
  DefaultObject,
  DefaultPrivileges,
  ExpectedAcl,
  Grant,
  ObjectPrivileges,
  Operation,
  PrivilegesPage,
  PrivilegeTarget,
  ProjectGrant,
  ProjectTeamGrantView,
  SchemaPrivileges,
} from "@chelabase/management-client";
import { closure, actAsReserved } from "@/lib/role-rules";
import { escapeName } from "../hub/escape-name";
import { plural } from "../member-page/logins";

/**
 * The Roles tab's and role page's pure model (task S3): which roles are shown, who holds one
 * (MH-34, Admins only), the staged privilege edits, the operations and `expect_acl` they become
 * (P10, S3) and their words.
 */

// ---- roles ------------------------------------------------------------------------------------

/** A role the hub manages: never the owner, a reserved name or a personal login (R3). */
export const isManaged = (r: Pick<BranchRole, "name" | "origin">): boolean =>
  r.origin !== "owner" && r.origin !== "personal" && !actAsReserved(r.name);

/** The roles the role list shows, in the API's order. */
export const managedRoles = (rows: readonly BranchRole[]): BranchRole[] => rows.filter(isManaged);

/** The roles a role may be made a member of (create and Memberships): managed, plain, never itself. */
export const joinableRoles = (rows: readonly BranchRole[], self?: string): BranchRole[] =>
  rows.filter((r) => isManaged(r) && r.level !== "owner_level" && r.name !== self);

/**
 * Whether `role` is reached by `from` through memberships of any kind (inheritance): used to note a
 * role in `anon`'s or `authenticated`'s chain, whose privileges the data API serves.
 */
export function inheritedBy(rows: readonly BranchRole[], from: string, role: string): boolean {
  const byName = new Map(rows.map((r) => [r.name, r]));
  const seen = new Set<string>();
  const stack = [...(byName.get(from)?.member_of ?? [])];
  while (stack.length > 0) {
    const next = stack.pop()!;
    if (next === role) return true;
    if (seen.has(next)) continue;
    seen.add(next);
    stack.push(...(byName.get(next)?.member_of ?? []));
  }
  return false;
}

/** "anon's chain" / "authenticated's chain" when the data API's roles inherit `role`, else null. */
export function dataApiChainNote(rows: readonly BranchRole[], role: string): string | null {
  const by = ["anon", "authenticated"].filter((d) => rows.some((r) => r.name === d) && inheritedBy(rows, d, role));
  return by.length === 0 ? null : `in ${by.map((d) => `${d}'s`).join(" and ")} chain`;
}

// ---- holders (MH-34, R4) -------------------------------------------------------------------------

export interface Holder {
  memberId: string;
  /** The granted role it comes through; null when the grant names the role itself. */
  via: string | null;
  /** The team whose grant gives it (step 2); absent for the member's own grant. */
  team?: string;
  /** That team's id, to link it. */
  teamId?: string;
}

/** The Postgres roles a grant gives on one database: its override's when that narrows them. `"all"` (Admin) is none here. */
function grantedRoles(grant: ProjectGrant, databaseId: string): string[] {
  const narrowed = grant.overrides.find((o) => o.database_id === databaseId)?.pg_roles;
  const roles = narrowed ?? grant.pg_roles;
  return Array.isArray(roles) ? roles : [];
}

/**
 * Who holds `role` on the database: a member whose grant names it, or names a role that can
 * `SET ROLE` to it (the chain). Admins act as every role and aren't listed as holders.
 */
export function holdersOf(role: string, grants: readonly ProjectGrant[], databaseId: string, rows: readonly BranchRole[]): Holder[] {
  const out: Holder[] = [];
  for (const g of grants) {
    const roles = grantedRoles(g, databaseId);
    if (roles.includes(role)) {
      out.push({ memberId: g.member_id, via: null });
      continue;
    }
    const via = roles.find((r) => closure(rows, r).has(role));
    if (via !== undefined) out.push({ memberId: g.member_id, via });
  }
  return out;
}

/**
 * Holders through teams (P15): each member of a team whose grant names `role`, or a role whose chain
 * reaches it, as "via <team>". Members who are Admin (an own Admin grant, or Admin through any team) already
 * hold everything and are left out (Admins aren't listed as holders); a team with Admin gives every role and
 * isn't listed either. The member's override on `databaseId` narrows what a team gives: its role list
 * intersects the roles the team names (`null` leaves them whole).
 */
export function teamHoldersOf(
  role: string,
  teamGrants: readonly ProjectTeamGrantView[],
  rows: readonly BranchRole[],
  ownGrants: readonly ProjectGrant[],
  databaseId: string,
): Holder[] {
  const admins = new Set(ownGrants.filter((g) => g.platform_role === "admin").map((g) => g.member_id));
  for (const t of teamGrants) if (t.platform_role === "admin") for (const id of t.member_ids) admins.add(id);
  const narrowing = new Map(ownGrants.map((g) => [g.member_id, g.overrides.find((o) => o.database_id === databaseId)?.pg_roles ?? null]));
  const out: Holder[] = [];
  for (const t of teamGrants) {
    if (t.platform_role === "admin") continue;
    for (const memberId of t.member_ids) {
      if (admins.has(memberId)) continue;
      const cap = narrowing.get(memberId) ?? null;
      const named = cap === null ? t.pg_roles : t.pg_roles.filter((r) => cap.includes(r));
      if (named.includes(role)) out.push({ memberId, via: null, team: t.name, teamId: t.team_id });
      else {
        const through = named.find((r) => closure(rows, r).has(role));
        if (through !== undefined) out.push({ memberId, via: through, team: t.name, teamId: t.team_id });
      }
    }
  }
  return out;
}

/** One entry per member, in first-seen order, with every source that gives them the role. */
export function holdersByMember(holders: readonly Holder[]): { memberId: string; sources: Holder[] }[] {
  const byId = new Map<string, Holder[]>();
  for (const h of holders) byId.set(h.memberId, [...(byId.get(h.memberId) ?? []), h]);
  return [...byId].map(([memberId, sources]) => ({ memberId, sources }));
}

export const membersText = (n: number) => (n === 0 ? "No members" : plural(n, "member"));

// ---- the privileges read ------------------------------------------------------------------------

export const RELATION_KINDS: ReadonlySet<ObjectPrivileges["kind"]> = new Set(["table", "view", "materialized_view", "partitioned_table", "foreign_table"]);
export const FUNCTION_KINDS: ReadonlySet<ObjectPrivileges["kind"]> = new Set(["function", "procedure"]);
/** Kinds a row-level security policy can be on. */
export const RLS_KINDS: ReadonlySet<ObjectPrivileges["kind"]> = new Set(["table", "partitioned_table"]);
/** The privileges a column can hold (Postgres' column ACL). */
export const COLUMN_PRIVILEGES = ["SELECT", "INSERT", "UPDATE", "REFERENCES"] as const;
export const KIND_WORD: Partial<Record<ObjectPrivileges["kind"], string>> = {
  view: "view",
  materialized_view: "materialized view",
  partitioned_table: "partitioned",
  foreign_table: "foreign",
  procedure: "procedure",
};

/** Every grant, schema-level entry and default ACL the page has read, by object. */
export interface PrivilegeIndex {
  schemas: Map<string, SchemaPrivileges>;
  defaults: Map<string, DefaultPrivileges>;
  /** Relations and sequences, by `[schema, name]`. */
  objects: Map<string, ObjectPrivileges>;
  functions: Map<number, ObjectPrivileges>;
}

const objectKey = (schema: string, name: string) => JSON.stringify([schema, name]);
const defaultKey = (forRole: string, schema: string, object: DefaultObject) => JSON.stringify([forRole, schema, object]);

export function indexPrivileges(head: Pick<PrivilegesPage, "schemas" | "default_privileges"> | undefined, objects: readonly ObjectPrivileges[]): PrivilegeIndex {
  const index: PrivilegeIndex = { schemas: new Map(), defaults: new Map(), objects: new Map(), functions: new Map() };
  for (const s of head?.schemas ?? []) index.schemas.set(s.name, s);
  for (const d of head?.default_privileges ?? []) index.defaults.set(defaultKey(d.for_role, d.schema, d.object), d);
  for (const o of objects) {
    if (FUNCTION_KINDS.has(o.kind)) index.functions.set(o.oid, o);
    else index.objects.set(objectKey(o.schema, o.name), o);
  }
  return index;
}

export const holds = (grants: readonly Grant[] | undefined, privilege: string) => (grants ?? []).some((g) => g.privilege === privilege);
export const holdsOption = (grants: readonly Grant[] | undefined, privilege: string) => (grants ?? []).some((g) => g.privilege === privilege && g.grantable);
const roleName = (r: Grant["grantor"]) => r.name;
/** The grantors of `privilege` other than `owner` (P30: `neon_superuser` included). */
export const otherGrantors = (grants: readonly Grant[], privilege: string, owner: string) =>
  [...new Set(grants.filter((g) => g.privilege === privilege).map((g) => roleName(g.grantor)))].filter((n) => n !== owner);

// ---- staged edits --------------------------------------------------------------------------------

export type EditTarget =
  | { kind: "relation"; schema: string; name: string }
  | { kind: "schema"; schema: string }
  | { kind: "sequence"; schema: string; name: string }
  | { kind: "function"; schema: string; oid: number; identity: string }
  | { kind: "default"; forRole: string; schema: string; object: DefaultObject };

export type Edit =
  /** Grant (`grant: true`) or revoke one privilege; `column` only on a relation. Privileges are upper case. */
  | { kind: "privilege"; target: EditTarget; privilege: string; column: string | null; grant: boolean }
  /** `REVOKE GRANT OPTION FOR`: the only grant-option change offered (MH-29). */
  | { kind: "option"; target: Exclude<EditTarget, { kind: "default" }>; privilege: string }
  | { kind: "all_rows"; schema: string; name: string; enabled: boolean };

function targetKey(t: EditTarget): string {
  switch (t.kind) {
    case "relation":
    case "sequence":
      return JSON.stringify([t.kind, t.schema, t.name]);
    case "schema":
      return JSON.stringify([t.kind, t.schema]);
    case "function":
      return JSON.stringify([t.kind, t.oid]);
    case "default":
      return JSON.stringify([t.kind, t.forRole, t.schema, t.object]);
  }
}

/** One key per staged cell: its desired value isn't part of it, so a cell has at most one edit. */
export function editKey(e: Edit): string {
  if (e.kind === "all_rows") return JSON.stringify([e.kind, e.schema, e.name]);
  if (e.kind === "option") return JSON.stringify([e.kind, targetKey(e.target), e.privilege]);
  return JSON.stringify([e.kind, targetKey(e.target), e.privilege, e.column]);
}

/** The grants an edit's cell reads; undefined when the object isn't in what was read (gone, or not loaded). */
function grantsOf(index: PrivilegeIndex, t: EditTarget, column: string | null = null): readonly Grant[] | undefined {
  switch (t.kind) {
    case "schema":
      return index.schemas.get(t.schema)?.grants;
    case "relation": {
      const o = index.objects.get(objectKey(t.schema, t.name));
      if (!o) return undefined;
      return column === null ? o.grants : o.columns.find((c) => c.name === column)?.grants;
    }
    case "sequence":
      return index.objects.get(objectKey(t.schema, t.name))?.grants;
    case "function":
      return index.functions.get(t.oid)?.grants;
    case "default":
      // No `pg_default_acl` row yet: nothing is granted, as long as the schema is known.
      return index.defaults.get(defaultKey(t.forRole, t.schema, t.object))?.grants ?? (index.schemas.has(t.schema) ? [] : undefined);
  }
}

/** What the read says for an edit's cell: held, grantable or on; undefined when unknown. */
export function current(index: PrivilegeIndex, e: Edit): boolean | undefined {
  if (e.kind === "all_rows") {
    const o = index.objects.get(objectKey(e.schema, e.name));
    return o ? o.all_rows != null : undefined;
  }
  const grants = grantsOf(index, e.target, e.kind === "privilege" ? e.column : null);
  if (grants === undefined) return undefined;
  return e.kind === "option" ? holdsOption(grants, e.privilege) : holds(grants, e.privilege);
}

/** An edit still changes something against the read (after a reload it may not). */
export function stillChanges(index: PrivilegeIndex, e: Edit): boolean {
  const now = current(index, e);
  if (now === undefined) return false;
  if (e.kind === "option") return now;
  return e.kind === "all_rows" ? now !== e.enabled : now !== e.grant;
}

/** A target as SQL names it in the edit list ("billing.customers", "SCHEMA billing"). */
export function targetText(t: EditTarget): string {
  switch (t.kind) {
    case "schema":
      return `SCHEMA ${escapeName(t.schema)}`;
    case "relation":
      return `${escapeName(t.schema)}.${escapeName(t.name)}`;
    case "sequence":
      return `SEQUENCE ${escapeName(t.schema)}.${escapeName(t.name)}`;
    case "function":
      return escapeName(t.identity);
    case "default":
      return `new ${t.object} in ${escapeName(t.schema)} (created by ${escapeName(t.forRole)})`;
  }
}

/** One edit in the pending list, as the statement it stands for. */
export function editText(e: Edit): string {
  if (e.kind === "all_rows") return `All rows ${e.enabled ? "" : "off "}on ${escapeName(e.schema)}.${escapeName(e.name)}`;
  if (e.kind === "option") return `REVOKE GRANT OPTION FOR ${e.privilege} ON ${targetText(e.target)}`;
  const col = e.column === null ? "" : ` (${escapeName(e.column)})`;
  return `${e.grant ? "GRANT" : "REVOKE"} ${e.privilege}${col} ON ${targetText(e.target)}`;
}

const lower = <T extends string>(p: string) => p.toLowerCase() as T;

/** The schema-changes operation of one edit; the grantee is always the page's role (never PUBLIC, MH-28). */
export function operationOf(e: Edit, role: string): Operation {
  const grantee = { role };
  if (e.kind === "all_rows") return { set_all_rows: { table: { schema: e.schema, name: e.name }, role, enabled: e.enabled } };
  const t = e.target;
  const p = e.privilege;
  if (e.kind === "option") {
    switch (t.kind) {
      case "relation":
        return { revoke: { table: { schema: t.schema, name: t.name }, grant: { grantee, privileges: [lower(p)], with_grant_option: true } } };
      case "schema":
        return { revoke_schema: { schema: t.schema, grantee, privileges: [lower(p)], grant_option_only: true } };
      case "sequence":
        return { revoke_sequence: { sequence: { schema: t.schema, name: t.name }, grantee, privileges: [lower(p)], grant_option_only: true } };
      case "function":
        return { revoke_function: { function: { oid: t.oid, identity: t.identity }, grantee, grant_option_only: true } };
      case "default":
        // Default privileges have no grant option to revoke (the type rules it out).
        throw new Error("a default ACL has no grant option");
    }
  }
  const grant = e.grant;
  switch (t.kind) {
    case "relation": {
      const g = { grantee, privileges: [lower<"select">(p)], ...(e.column === null ? {} : { columns: [e.column] }) };
      const table = { schema: t.schema, name: t.name };
      return grant ? { grant: { table, grant: g } } : { revoke: { table, grant: g } };
    }
    case "schema":
      return grant
        ? { grant_schema: { schema: t.schema, grantee, privileges: [lower(p)] } }
        : { revoke_schema: { schema: t.schema, grantee, privileges: [lower(p)] } };
    case "sequence": {
      const sequence = { schema: t.schema, name: t.name };
      return grant ? { grant_sequence: { sequence, grantee, privileges: [lower(p)] } } : { revoke_sequence: { sequence, grantee, privileges: [lower(p)] } };
    }
    case "function": {
      const fn = { oid: t.oid, identity: t.identity };
      return grant ? { grant_function: { function: fn, grantee } } : { revoke_function: { function: fn, grantee } };
    }
    case "default":
      return { alter_default_privileges: { for_role: t.forRole, schema: t.schema, object: t.object, grant, grantee, privileges: [lower(p)] } };
  }
}

/** A target in its serde form (`PrivilegeTarget`, externally tagged, S3). */
export function targetOf(t: EditTarget): PrivilegeTarget {
  switch (t.kind) {
    case "relation":
      return { relation: { schema: t.schema, name: t.name } };
    case "schema":
      return { schema: { schema: t.schema } };
    case "sequence":
      return { sequence: { schema: t.schema, name: t.name } };
    case "function":
      return { function: { oid: t.oid } };
    case "default":
      return { default_acl: { for_role: t.forRole, object: t.object, schema: t.schema } };
  }
}

function hashOf(index: PrivilegeIndex, t: EditTarget): string | undefined {
  switch (t.kind) {
    case "schema":
      return index.schemas.get(t.schema)?.acl_hash;
    case "relation":
    case "sequence":
      return index.objects.get(objectKey(t.schema, t.name))?.acl_hash;
    case "function":
      return index.functions.get(t.oid)?.acl_hash;
    case "default":
      // A default ACL with no row has no hash: the server leaves a missing object out.
      return index.defaults.get(defaultKey(t.forRole, t.schema, t.object))?.acl_hash;
  }
}

/** The read's `acl_hash` of every object the edits touch, once each; "all rows" touches no ACL. */
export function expectAclOf(edits: readonly Edit[], index: PrivilegeIndex): ExpectedAcl[] {
  const seen = new Set<string>();
  const out: ExpectedAcl[] = [];
  for (const e of edits) {
    if (e.kind === "all_rows") continue;
    const key = targetKey(e.target);
    if (seen.has(key)) continue;
    seen.add(key);
    const acl_hash = hashOf(index, e.target);
    if (acl_hash !== undefined) out.push({ object: targetOf(e.target), acl_hash });
  }
  return out;
}

/** A `PrivilegeTarget` the server names, in words ("billing.customers", "schema billing"). */
export function serverTargetText(t: PrivilegeTarget, index: PrivilegeIndex): string {
  if ("relation" in t) return `${escapeName(t.relation.schema)}.${escapeName(t.relation.name)}`;
  if ("sequence" in t) return `${escapeName(t.sequence.schema)}.${escapeName(t.sequence.name)}`;
  if ("schema" in t) return `schema ${escapeName(t.schema.schema)}`;
  if ("function" in t) {
    const identity = index.functions.get(t.function.oid)?.identity;
    return identity ? escapeName(identity) : `function ${t.function.oid}`;
  }
  return `future ${t.default_acl.object} in ${escapeName(t.default_acl.schema)} (created by ${escapeName(t.default_acl.for_role)})`;
}

/** "a", "a and b", "a, b and c". */
export function listText(items: readonly string[]): string {
  if (items.length <= 1) return items[0] ?? "";
  return `${items.slice(0, -1).join(", ")} and ${items[items.length - 1]}`;
}

// ---- the role list's summary line -----------------------------------------------------------------

const PRIV_ORDER = ["SELECT", "INSERT", "UPDATE", "DELETE", "TRUNCATE", "REFERENCES", "TRIGGER", "MAINTAIN", "USAGE", "CREATE", "EXECUTE"];
const byOrder = (a: string, b: string) => PRIV_ORDER.indexOf(a) - PRIV_ORDER.indexOf(b);

function relationNoun(kinds: Set<ObjectPrivileges["kind"]>, n: number): string {
  const views = [...kinds].every((k) => k === "view" || k === "materialized_view");
  const tables = [...kinds].every((k) => k !== "view" && k !== "materialized_view");
  return views ? plural(n, "view") : tables ? plural(n, "table") : plural(n, "table and view", "tables and views");
}

/**
 * One line for a role card: what it holds, per kind of object ("SELECT, INSERT on 4 tables in
 * billing · EXECUTE on 2 functions in billing"), what it owns, else its memberships, else
 * "No privileges yet". `pages` is each schema's read (one page; `truncated` when there was more).
 */
export function privilegeSummary(role: Pick<BranchRole, "name" | "member_of">, pages: readonly { objects: readonly ObjectPrivileges[]; truncated: boolean }[]): string {
  const parts: string[] = [];
  const group = (pick: (o: ObjectPrivileges) => boolean) => {
    const privs = new Set<string>();
    const schemas = new Set<string>();
    const kinds = new Set<ObjectPrivileges["kind"]>();
    let n = 0;
    for (const page of pages) {
      for (const o of page.objects) {
        if (!pick(o) || o.grants.length === 0) continue;
        n++;
        kinds.add(o.kind);
        schemas.add(o.schema);
        o.grants.forEach((g) => privs.add(g.privilege));
      }
    }
    return { privs: [...privs].sort(byOrder), schemas: [...schemas].map(escapeName), kinds, n };
  };
  const more = pages.some((p) => p.truncated) ? "+" : "";
  const rel = group((o) => RELATION_KINDS.has(o.kind));
  if (rel.n > 0) parts.push(`${rel.privs.join(", ")} on ${relationNoun(rel.kinds, rel.n)}${more} in ${listText(rel.schemas)}`);
  let columns = 0;
  const colPrivs = new Set<string>();
  for (const page of pages) {
    for (const o of page.objects) {
      if (!RELATION_KINDS.has(o.kind)) continue;
      for (const c of o.columns) {
        if (c.grants.length === 0) continue;
        columns++;
        c.grants.forEach((g) => colPrivs.add(g.privilege));
      }
    }
  }
  if (columns > 0) parts.push(`${[...colPrivs].sort(byOrder).join(", ")} on ${plural(columns, "column")}`);
  const seq = group((o) => o.kind === "sequence");
  if (seq.n > 0) parts.push(`${seq.privs.join(", ")} on ${plural(seq.n, "sequence")}${more} in ${listText(seq.schemas)}`);
  const fn = group((o) => FUNCTION_KINDS.has(o.kind));
  if (fn.n > 0) parts.push(`EXECUTE on ${plural(fn.n, "function")}${more} in ${listText(fn.schemas)}`);
  const owned = pages.flatMap((p) => p.objects).filter((o) => o.owner === role.name);
  if (owned.length > 0) {
    const schemas = [...new Set(owned.map((o) => escapeName(o.schema)))];
    parts.push(`Owns ${plural(owned.length, "object")}${more} in ${listText(schemas)}`);
  }
  if (parts.length > 0) return parts.join(" · ");
  if (role.member_of.length > 0) return `Member of ${role.member_of.map(escapeName).join(", ")}`;
  return "No privileges yet";
}
