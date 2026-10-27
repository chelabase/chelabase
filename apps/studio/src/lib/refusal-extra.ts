import { ApiError } from "@chelabase/management-client";
import { errorText } from "@/lib/errors";

/**
 * What the API's 422 for a refused grant, override or invite says about the refusal
 * (`extra`): the role, its chain, the database (and the branch) whose roles broke the rule, and
 * the project of an invite's grant. One reading, shared by the invite dialogs and the Access tab.
 */
export interface RefusalExtra {
  role?: string;
  /** From the refused role itself to the role that is refused; absent when there is none. */
  chain?: string[];
  databaseId?: string;
  projectId?: string;
  /** The team whose grant is why (a team grant, a team member add, an invite's team, a demotion). */
  teamId?: string;
  /** The branch whose roles broke it, and whether that is the database's main (M16). */
  branch?: string;
  branchMain?: boolean;
  /** Why the chain's last role makes it Admin-only. */
  why?: RefusalWhy;
}

export type RefusalWhy = "owner_level" | "login" | "admin" | "reserved";
const WHYS: readonly RefusalWhy[] = ["owner_level", "login", "admin", "reserved"];

const text = (v: unknown) => (typeof v === "string" ? v : undefined);

/** What the reached role is, after "it reaches X, …" (or of the role itself, after "it"). */
const REACHED: Record<RefusalWhy, string> = {
  login: "a LOGIN role",
  owner_level: "which is owner-level",
  admin: "which holds ADMIN on another role",
  reserved: "which is reserved",
};
const ITSELF: Record<RefusalWhy, string> = {
  login: "it's a LOGIN role",
  owner_level: "it's owner-level",
  admin: "it holds ADMIN on another role",
  reserved: "it's reserved",
};

/**
 * A refused grant in Studio's own words: "Only Admin can hold ops on app_main: it reaches
 * writer, a LOGIN role." Null when `extra` doesn't name the role, its chain and why (the
 * caller then shows the server's message).
 */
export function refusalMessage(
  extra: Pick<RefusalExtra, "role" | "chain" | "why">,
  database: string | undefined,
  branch?: string,
  branchMain?: boolean,
): string | null {
  const { role, chain, why } = extra;
  if (!role || !chain || chain.length === 0 || !why) return null;
  const where = whereText(database, branch, branchMain);
  const last = chain[chain.length - 1];
  const reason = chain.length > 1 ? `it reaches ${last}, ${REACHED[why]}` : ITSELF[why];
  return `Only Admin can hold ${role}${where ? ` ${where}` : ""}: ${reason}.`;
}

/** "on posts" or, when the refusal comes from a branch other than main, "on posts, branch dev". */
export function whereText(database: string | undefined, branch: string | undefined, branchMain: boolean | undefined): string | undefined {
  if (!database) return undefined;
  return branch && branchMain === false ? `on ${database}, branch ${branch}` : `on ${database}`;
}

export function refusalExtra(err: ApiError): RefusalExtra {
  const chain = Array.isArray(err.extra.chain) ? err.extra.chain.filter((c): c is string => typeof c === "string") : [];
  return {
    role: text(err.extra.role),
    chain: chain.length > 0 ? chain : undefined,
    databaseId: text(err.extra.database_id),
    projectId: text(err.extra.project_id),
    teamId: text(err.extra.team_id),
    branch: text(err.extra.branch),
    branchMain: typeof err.extra.branch_main === "boolean" ? err.extra.branch_main : undefined,
    why: WHYS.find((w) => w === err.extra.why),
  };
}

const LEVELS = "viewer|developer|admin";

/** "developer" at the start of a level word, as the product writes it. */
const capital = (w: string) => w.charAt(0).toUpperCase() + w.slice(1);

export interface RefusalContext {
  /** A team's name by id; unknown ids name no team. */
  teamName?: (teamId: string) => string | undefined;
  /** A database's name by id, for a sentence that doesn't name it itself. */
  databaseName?: (databaseId: string) => string | undefined;
}

/**
 * The server's 422 sentence for a refused grant in Studio's words: levels capitalised ("the Admin platform
 * role", "would be Developer"), roles and databases left in backticks (drawn mono by `RefusalText`), and
 * the team whose grant is why named. The server's sentence already names the database.
 */
export function refusalSentence(err: ApiError, ctx: RefusalContext = {}): string {
  const extra = refusalExtra(err);
  let text = err.message
    .replace(new RegExp(`\\b(would be) (${LEVELS})\\b`, "g"), (_, a: string, l: string) => `${a} ${capital(l)}`)
    .replace(new RegExp(`\\b(the) (${LEVELS}) (platform role)\\b`, "g"), (_, a: string, l: string, c: string) => `${a} ${capital(l)} ${c}`);
  text = capital(text);
  if (!text.includes("`") && extra.databaseId) {
    const db = ctx.databaseName?.(extra.databaseId);
    if (db) text += ` (on \`${db}\`)`;
  }
  const team = extra.teamId ? ctx.teamName?.(extra.teamId) : undefined;
  if (team) text = `${text.replace(/\.$/, "")}. The ${team} team gives that access.`;
  return text;
}

/** A refusal in Studio's words when the error is one (its `extra` names a role); the plain error text otherwise. */
export function refusalOrErrorText(err: unknown, ctx: RefusalContext = {}): string {
  return err instanceof ApiError && err.extra.role !== undefined ? refusalSentence(err, ctx) : errorText(err);
}
