/** The hub routes' search params, each parsed against its own shape; anything else is dropped (MH-38b). */

export const MEMBERS_TABS = ["people", "roles", "activity", "instance"] as const;
export type MembersTab = (typeof MEMBERS_TABS)[number];
export type RosterViewParam = "cards" | "list";

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
const MAX_TEXT = 200;

export const isUuid = (v: unknown): v is string => typeof v === "string" && UUID.test(v);

export function parseMembersTab(v: unknown): MembersTab | undefined {
  return typeof v === "string" && (MEMBERS_TABS as readonly string[]).includes(v) ? (v as MembersTab) : undefined;
}

const parseView = (v: unknown): RosterViewParam | undefined => (v === "cards" || v === "list" ? v : undefined);
const parseText = (v: unknown): string | undefined => (typeof v === "string" && v !== "" && v.length <= MAX_TEXT ? v : undefined);

export interface HubSearch {
  tab?: MembersTab;
  project?: string;
  view?: RosterViewParam;
  q?: string;
}

export interface MembersSearch extends HubSearch {
  /** The Roles tab's filter: a database, a branch and a Postgres database name. */
  database?: string;
  branch?: string;
  pg_database?: string;
}

function pick<T extends object>(entries: [keyof T & string, unknown][]): T {
  const out: Record<string, unknown> = {};
  for (const [k, v] of entries) if (v !== undefined) out[k] = v;
  return out as T;
}

/** `/app/members`: the default tab (People) is left out of the URL. */
export function validateMembersSearch(search: Record<string, unknown>): MembersSearch {
  const tab = parseMembersTab(search.tab);
  return pick<MembersSearch>([
    ["tab", tab && tab !== "people" ? tab : undefined],
    ["project", isUuid(search.project) ? search.project : undefined],
    ["view", parseView(search.view)],
    ["q", parseText(search.q)],
    ["database", isUuid(search.database) ? search.database : undefined],
    ["branch", isUuid(search.branch) ? search.branch : undefined],
    ["pg_database", parseText(search.pg_database)],
  ]);
}

/** `/app/members/$memberId`: what the roster link carries so the back link restores the list. */
export function validateMemberPageSearch(search: Record<string, unknown>): HubSearch {
  const tab = parseMembersTab(search.tab);
  return pick<HubSearch>([
    ["tab", tab && tab !== "people" ? tab : undefined],
    ["project", isUuid(search.project) ? search.project : undefined],
    ["view", parseView(search.view)],
    ["q", parseText(search.q)],
  ]);
}

/** `/app/members/roles/$databaseId/$branchId/$role`. */
export function validateRolePageSearch(search: Record<string, unknown>): { pg_database?: string } {
  return pick<{ pg_database?: string }>([["pg_database", parseText(search.pg_database)]]);
}
