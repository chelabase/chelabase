import { ApiError, type Project } from "@chelabase/management-client";
import { errorText } from "@/lib/errors";
import { refusalExtra, refusalMessage, refusalOrErrorText, whereText, type RefusalContext } from "@/lib/refusal-extra";
import { RefusalText } from "./refusal-text";

/** A refused grant or override: the sentence, the refused role, its chain and the database. */
export interface Failure {
  message: string;
  role?: string;
  chain?: string[];
  databaseId?: string;
  /** The database's name (or "a deleted database"); absent when the caller doesn't know the databases. */
  database?: string;
  /** The branch whose roles broke the rule, and whether it is main (M16). */
  branch?: string;
  branchMain?: boolean;
  /** The server said the project is being deleted, or the member or project is gone. */
  gone?: boolean;
}

/** Reads a 422's `extra` (`role`, `chain`, `database_id`, `branch`); anything else shows the server's message. */
export function failureOf(err: unknown, project: Pick<Project, "databases"> | null, ctx: RefusalContext = {}): Failure {
  if (!(err instanceof ApiError)) return { message: errorText(err) };
  const { role, chain, databaseId, branch, branchMain, why, teamId } = refusalExtra(err);
  // No project given: the databases are unknown, so none is named (never "a deleted database").
  const database = databaseId && project ? (project.databases.find((d) => d.database_id === databaseId)?.name ?? "a deleted database") : undefined;
  return {
    // Studio's own words when the refusal says enough; the server's sentence otherwise.
    message: withTeam(refusalMessage({ role, chain, why }, database, branch, branchMain), teamId, ctx) ?? refusalOrErrorText(err, { ...ctx, databaseName: (id) => project?.databases.find((d) => d.database_id === id)?.name }),
    role,
    chain,
    databaseId,
    database,
    branch,
    branchMain,
    gone: err.code === "project_deleting" || err.code === "not_found",
  };
}

/** The sentence plus the team whose grant is why, when the caller knows its name. */
function withTeam(message: string | null, teamId: string | undefined, ctx: RefusalContext): string | null {
  const team = teamId ? ctx.teamName?.(teamId) : undefined;
  return message && team ? `${message} The ${team} team gives that access.` : message;
}

/** Names the refused role and the role it reaches in backticks, so they are drawn mono (unless the text already has some). */
function withBackticks(failure: Failure): string {
  if (failure.message.includes("`")) return failure.message;
  let text = failure.message;
  for (const name of new Set([failure.role, failure.chain?.[failure.chain.length - 1]])) {
    if (name) text = text.replace(new RegExp(`(?<![\\w\`])${name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}(?![\\w\`])`, "g"), (m) => `\`${m}\``);
  }
  return text;
}

/** The sentence, the chain in mono and "on {database}": shown on the exact row or override. */
export function FailureText({ failure }: { failure: Failure }) {
  const where = whereText(failure.database, failure.branch, failure.branchMain);
  return (
    <div role="alert" className="space-y-1 text-sm text-coral">
      <p>
        <RefusalText text={withBackticks(failure)} />
      </p>
      {failure.chain && (
        <p className="font-mono text-[13px]">
          {failure.chain.join(" → ")}
          {where && <span className="font-sans text-chalk-dim"> {where}</span>}
        </p>
      )}
      {!failure.chain && where && <p className="text-chalk-dim">{where}</p>}
    </div>
  );
}
