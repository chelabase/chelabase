import type { Branch, BranchDatabaseView, ObjectSummary, Project } from "@chelabase/management-client";

/** The table editor's place: one optional URL segment per level, in order. */
export interface EditorParams {
  projectId?: string;
  databaseId?: string;
  branchId?: string;
  pgDatabase?: string;
  schema?: string;
  table?: string;
}

export const LEVELS: readonly (keyof EditorParams)[] = ["projectId", "databaseId", "branchId", "pgDatabase", "schema", "table"];
export const LAST_KEY = "chelabase.studio.tables.last";

/** The URL of a place: each present level up to the first missing one. */
export function editorPath(p: EditorParams): string {
  let path = "/app/tables";
  for (const level of LEVELS) {
    const v = p[level];
    if (!v) break;
    path += `/${encodeURIComponent(v)}`;
  }
  return path;
}

/**
 * The route's params for a place: every level, in `LEVELS` order, with `undefined` for a
 * missing one. The router keeps an optional param it isn't given, so naming each level is
 * what clears the ones below a picked level (I2).
 */
export function editorRouteParams(p: EditorParams): Record<keyof EditorParams, string | undefined> {
  return {
    projectId: p.projectId || undefined,
    databaseId: p.databaseId || undefined,
    branchId: p.branchId || undefined,
    pgDatabase: p.pgDatabase || undefined,
    schema: p.schema || undefined,
    table: p.table || undefined,
  };
}

function pick(p: EditorParams): EditorParams {
  const out: EditorParams = {};
  for (const level of LEVELS) {
    const v = p[level];
    if (typeof v === "string" && v !== "") out[level] = v;
  }
  return out;
}

/** The last place the editor was ready at; null when nothing usable is stored. */
export function readLast(storage: Storage | null): EditorParams | null {
  if (!storage) return null;
  try {
    const raw = storage.getItem(LAST_KEY);
    if (!raw) return null;
    const parsed: unknown = JSON.parse(raw);
    if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) return null;
    return pick(parsed as EditorParams);
  } catch {
    return null;
  }
}

export function writeLast(storage: Storage | null, p: EditorParams): void {
  if (!storage) return;
  try {
    storage.setItem(LAST_KEY, JSON.stringify(pick(p)));
  } catch {
    // Remembering the place is a convenience.
  }
}

/** What each level has loaded so far; an absent list is "not loaded yet". */
export interface Choices {
  projects?: Project[];
  branches?: Branch[];
  pgDatabases?: BranchDatabaseView[];
  objects?: ObjectSummary[];
}

export type Resolution =
  | { kind: "loading" }
  | { kind: "redirect"; params: EditorParams }
  | { kind: "ready"; params: EditorParams }
  | { kind: "not_found"; level: "project" | "database" | "branch" | "pgDatabase" | "schema" | "table" };

/** Normal, ready branches as a tree: depth-first from the roots, siblings by name. */
export function branchTree(branches: Branch[]): { branch: Branch; depth: number }[] {
  const shown = branches.filter((b) => b.kind === "normal" && b.status === "ready");
  const ids = new Set(shown.map((b) => b.id));
  const byName = (a: Branch, b: Branch) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0);
  const children = new Map<string | null, Branch[]>();
  for (const b of shown) {
    // A branch whose parent isn't shown (a backup, say) stands as a root.
    const parent = b.parent_branch_id !== null && ids.has(b.parent_branch_id) ? b.parent_branch_id : null;
    children.set(parent, [...(children.get(parent) ?? []), b]);
  }
  const out: { branch: Branch; depth: number }[] = [];
  const visit = (parent: string | null, depth: number) => {
    for (const b of [...(children.get(parent) ?? [])].sort(byName)) {
      out.push({ branch: b, depth });
      visit(b.id, depth + 1);
    }
  };
  visit(null, 0);
  return out;
}

/** The distinct schemas of a object list, sorted. */
export function schemasOf(objects: ObjectSummary[]): string[] {
  return [...new Set(objects.map((o) => o.schema))].sort();
}

/**
 * Where the editor stands for these URL params: levels given are checked against the loaded
 * data, level by level; missing ones are filled in order while the data for them is there.
 * A level being filled takes the last place's value when every level above equals the last
 * place's and that value is valid for the loaded data; otherwise the default (project the
 * first; database the first; branch the root; Postgres database the main one; schema `public`
 * or the first), and below a level that differs from the last place, the defaults too.
 * A table is never filled.
 */
export function resolveParams(p: EditorParams, last: EditorParams | null, c: Choices): Resolution {
  const cur: EditorParams = { ...p };
  let filled = false;
  // Whether every level so far equals the last place's: only then may a level below take its value.
  let onLast = last !== null;
  const finish = (): Resolution => (filled ? { kind: "redirect", params: cur } : { kind: "ready", params: cur });
  // Data a level needs hasn't arrived: what's filled so far is still worth a redirect.
  const wait = (): Resolution => (filled ? { kind: "redirect", params: cur } : { kind: "loading" });
  /** Fill `level` from the last place when allowed and valid, else from the default. */
  const fill = (level: keyof EditorParams, valid: (v: string) => boolean, fallback: string | undefined): boolean => {
    const fromLast = onLast ? last?.[level] : undefined;
    const value = fromLast !== undefined && valid(fromLast) ? fromLast : fallback;
    if (value === undefined) return false;
    if (value !== fromLast) onLast = false;
    cur[level] = value;
    filled = true;
    return true;
  };
  const given = (level: keyof EditorParams) => {
    if (onLast && cur[level] !== last?.[level]) onLast = false;
  };

  // Project
  if (!c.projects) return wait();
  const projects = c.projects;
  if (cur.projectId) {
    if (!projects.some((x) => x.id === cur.projectId)) return { kind: "not_found", level: "project" };
    given("projectId");
  } else if (!fill("projectId", (v) => projects.some((x) => x.id === v), projects[0]?.id)) {
    return finish();
  }
  const project = projects.find((x) => x.id === cur.projectId)!;

  // Database
  if (cur.databaseId) {
    if (!project.databases.some((d) => d.database_id === cur.databaseId)) return { kind: "not_found", level: "database" };
    given("databaseId");
  } else if (!fill("databaseId", (v) => project.databases.some((d) => d.database_id === v), project.databases[0]?.database_id)) {
    return finish();
  }

  // Branch
  if (!c.branches) return wait();
  const tree = branchTree(c.branches);
  if (cur.branchId) {
    if (!tree.some((r) => r.branch.id === cur.branchId)) return { kind: "not_found", level: "branch" };
    given("branchId");
  } else {
    const root = c.branches.find((b) => b.parent_branch_id === null && b.kind === "normal" && b.status === "ready");
    if (!fill("branchId", (v) => tree.some((r) => r.branch.id === v), root?.id)) return finish();
  }

  // Postgres database
  if (!c.pgDatabases) return wait();
  const pgDatabases = c.pgDatabases;
  if (cur.pgDatabase) {
    if (!pgDatabases.some((d) => d.name === cur.pgDatabase)) return { kind: "not_found", level: "pgDatabase" };
    given("pgDatabase");
  } else if (!fill("pgDatabase", (v) => pgDatabases.some((d) => d.name === v), pgDatabases.find((d) => d.main)?.name)) {
    return finish();
  }

  // Schema
  if (!c.objects) return wait();
  const schemas = schemasOf(c.objects);
  if (cur.schema) {
    // An empty database lists no schemas: the given one stands.
    if (schemas.length > 0 && !schemas.includes(cur.schema)) return { kind: "not_found", level: "schema" };
  } else {
    fill("schema", (v) => schemas.includes(v), schemas.includes("public") || schemas.length === 0 ? "public" : schemas[0]);
  }

  // Table
  if (cur.table && !c.objects.some((o) => o.schema === cur.schema && o.name === cur.table)) {
    return { kind: "not_found", level: "table" };
  }
  return finish();
}
