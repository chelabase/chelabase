/**
 * The project page's search params: the tab, and a branch's Connection panel to open on load
 * (the merge page links created roles there: `?database=<id>&branch=<id>&panel=connection&role=<name>`).
 */
export interface ProjectSearch {
  tab?: "settings" | "users" | "access";
  /** The database whose branches open. */
  database?: string;
  branch?: string;
  /** The branch panel to open; only the connection details today. */
  panel?: "connection";
  /** The login role the connection details show (the owner when absent). */
  role?: string;
}

/** Which branch's connection details open, for which role (null: the owner). */
export interface ConnectionFocus {
  databaseId: string;
  branchId: string;
  role: string | null;
}

const text = (v: unknown): string | undefined => (typeof v === "string" && v !== "" ? v : undefined);

export function parseProjectSearch(search: Record<string, unknown>): ProjectSearch {
  const out: ProjectSearch = {};
  if (search.tab === "settings" || search.tab === "users" || search.tab === "access") out.tab = search.tab;
  const database = text(search.database);
  const branch = text(search.branch);
  const role = text(search.role);
  if (database) out.database = database;
  if (branch) out.branch = branch;
  if (search.panel === "connection") out.panel = "connection";
  if (role) out.role = role;
  return out;
}

export function connectionFocus(search: ProjectSearch): ConnectionFocus | null {
  if (!search.database || !search.branch || search.panel !== "connection") return null;
  return { databaseId: search.database, branchId: search.branch, role: search.role ?? null };
}
