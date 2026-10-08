import {
  getGetDatabaseQueryKey,
  getListBranchesQueryKey,
  type Branch,
  type DatabaseSettings,
  type Direction,
  type MergeSummary,
} from "@chelabase/management-client";
import type { QueryClient } from "@tanstack/react-query";
import { holdsBranches, isMergeOpen } from "@/lib/merges";
import { TRANSITIONAL } from "@/lib/query";

export const BACKUP_CONNECT_REASON = "restore backup: create a branch from it to connect";
const NAME_RE = /^[a-z0-9][a-z0-9_-]*$/;

export interface TreeRow {
  branch: Branch;
  depth: number;
  /** null for main. */
  parentName: string | null;
}

/** Depth-first: main first, then each branch's children (oldest first) under it. */
export function buildTree(branches: Branch[]): TreeRow[] {
  const live = branches.filter((b) => b.status !== "deleted");
  const byId = new Map(live.map((b) => [b.id as string, b]));
  const children = new Map<string | null, Branch[]>();
  for (const b of live) {
    // A branch whose parent isn't listed is shown as a root rather than lost.
    const key = b.parent_branch_id !== null && byId.has(b.parent_branch_id) ? b.parent_branch_id : null;
    children.set(key, [...(children.get(key) ?? []), b]);
  }
  for (const list of children.values()) list.sort((a, b) => a.created_at.localeCompare(b.created_at));
  const roots = (children.get(null) ?? []).sort(
    (a, b) => Number(b.parent_branch_id === null) - Number(a.parent_branch_id === null),
  );
  const rows: TreeRow[] = [];
  const visit = (b: Branch, depth: number) => {
    const parent = b.parent_branch_id === null ? undefined : byId.get(b.parent_branch_id);
    rows.push({ branch: b, depth, parentName: parent?.name ?? null });
    for (const c of children.get(b.id) ?? []) visit(c, depth + 1);
  };
  for (const r of roots) visit(r, 0);
  return rows;
}

/**
 * After a branch create, delete, reset to parent or restore: that database's branch list and
 * its details (the branch count). Returned by the mutations' `onSuccess`, so they wait for it.
 */
export function invalidateBranches(queryClient: QueryClient, databaseId: string): Promise<unknown> {
  return Promise.all([
    queryClient.invalidateQueries({ queryKey: getListBranchesQueryKey(databaseId) }),
    queryClient.invalidateQueries({ queryKey: getGetDatabaseQueryKey(databaseId) }),
  ]);
}

export const isTransitional = (b: Branch) => TRANSITIONAL.has(b.status);

/** Live `normal` children; backups never count. */
export function liveNormalChildren(branch: Branch, all: Branch[]): Branch[] {
  return all.filter(
    (b) => b.parent_branch_id === branch.id && b.kind === "normal" && b.status !== "deleted",
  );
}

export const PROTECTED_REASON = "Unprotect the branch first";

export type ActionKey = "connect" | "resetPassword" | "resetToParent" | "restore" | "delete" | "mergeInto" | "updateFrom";

/**
 * The open merge of `branch` with its parent in `direction`, if there is one. The API's rule is
 * one open merge per pair and Postgres database, so with `database` (the Postgres name,
 * `DatabaseSummary.postgres_database`, never the display name) only that database's merges count
 * (a merge of another database doesn't block).
 */
export function openMergeFor(
  branch: Branch,
  direction: Direction,
  openMerges: MergeSummary[],
  database?: string,
): MergeSummary | null {
  const parent = branch.parent_branch_id;
  if (parent === null) return null;
  const [source, target] = direction === "into_parent" ? [branch.id, parent] : [parent, branch.id];
  return (
    openMerges.find(
      (m) =>
        m.source_branch_id === source &&
        m.target_branch_id === target &&
        (database === undefined || m.database === database) &&
        isMergeOpen(m),
    ) ?? null
  );
}

/** A merge that holds `branch` (source or target): reset, restore and delete are refused while it does (E27). */
export function holdingMergeFor(branch: Branch, merges: MergeSummary[]): MergeSummary | null {
  return merges.find((m) => holdsBranches(m) && (m.source_branch_id === branch.id || m.target_branch_id === branch.id)) ?? null;
}

const HELD_REASON = "a merge is rehearsing or applying on this branch";
const HALF_APPLIED_REASON = "a merge failed half applied on this branch: retry it or abandon it";

/** Why a held branch can't be reset, restored or deleted; null when no merge holds it. */
export function heldReason(branch: Branch, merges: MergeSummary[]): string | null {
  const held = holdingMergeFor(branch, merges);
  if (held === null) return null;
  return held.status === "failed" ? HALF_APPLIED_REASON : HELD_REASON;
}

/**
 * Why each action is unavailable (null when it is available). `openMerges` are the database's
 * merges: an open one between the branch and its parent (in `database`, when given) blocks
 * starting another that way, and one that holds the branch blocks reset, restore and delete.
 */
export function actionReasons(
  branch: Branch,
  all: Branch[],
  openMerges: MergeSummary[] = [],
  database?: string,
): Record<ActionKey, string | null> {
  const isMain = branch.kind === "normal" && branch.parent_branch_id === null;
  const kids = liveNormalChildren(branch, all);
  const childReason = kids.length > 0 ? `has child branches: ${kids.map((k) => k.name).join(", ")}` : null;
  const notReady = branch.status !== "ready" ? `branch is not ready (it is ${branch.status})` : null;
  const backup = branch.kind === "backup";
  const unprotect = branch.protected ? PROTECTED_REASON : null;

  const connect = backup ? BACKUP_CONNECT_REASON : notReady;
  const held = heldReason(branch, openMerges);
  const rewrite = backup ? "a restore backup can't be rewritten; create a branch from it" : (notReady ?? childReason ?? held);
  const parent = all.find((b) => b.id === branch.parent_branch_id) ?? null;
  const parentName = parent?.name ?? "the parent";
  const parentNotReady = parent && parent.status !== "ready" ? `parent ${parent.name} is not ready (it is ${parent.status})` : null;
  const mergeBase =
    branch.kind !== "normal"
      ? "a restore backup can't be merged; create a branch from it"
      : branch.parent_branch_id === null
        ? "main has no parent to merge with"
        : (notReady ?? parentNotReady);
  const mergeReason = (direction: Direction) =>
    mergeBase ??
    (openMergeFor(branch, direction, openMerges, database)
      ? direction === "into_parent"
        ? `a merge into ${parentName} is already open`
        : `an update from ${parentName} is already open`
      : null);
  return {
    mergeInto: mergeReason("into_parent"),
    updateFrom: mergeReason("from_parent"),
    connect,
    resetPassword: connect,
    resetToParent: isMain ? "main can't be reset to a parent" : (unprotect ?? rewrite),
    restore: rewrite,
    delete: isMain
      ? "main can't be deleted; delete the database instead"
      : branch.status === "deleting"
        ? "branch is already being deleted"
        : (unprotect ?? childReason ?? held),
  };
}

/** The message under the name field, or null when the name is acceptable. */
export function validateBranchName(name: string): string | null {
  if (name === "main") return "The name main is reserved for the database's first branch.";
  if (name.length < 1 || name.length > 63 || !NAME_RE.test(name)) {
    return "Use 1-63 characters from a-z, 0-9, _ and -, starting with a letter or digit.";
  }
  return null;
}

const pad = (n: number) => String(n).padStart(2, "0");
/** A `datetime-local` value (local time, minute precision). */
export function toLocalInput(d: Date): string {
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

const MINUTE = 60_000;
/** Minute-rounded epoch bounds; `since` (where the branch's history begins) raises the minimum. */
function bounds(now: Date, windowSecs: number, since?: string | null) {
  const max = Math.floor(now.getTime() / MINUTE) * MINUTE;
  const window = Math.ceil((now.getTime() - windowSecs * 1000) / MINUTE) * MINUTE;
  const start = since == null ? NaN : Math.ceil(new Date(since).getTime() / MINUTE) * MINUTE;
  const min = Number.isNaN(start) ? window : Math.max(window, start);
  return { min, window, max };
}

/**
 * `max(now - history window, since)` .. `now` as `datetime-local` values (min rounded up, max
 * down). `since` is the branch's `history_start`: nothing older exists on it. When it's null
 * (unknown), only the window bounds the minimum.
 */
export function restoreBounds(now: Date, windowSecs: number, since?: string | null): { min: string; max: string } {
  const b = bounds(now, windowSecs, since);
  return { min: toLocalInput(new Date(b.min)), max: toLocalInput(new Date(b.max)) };
}

/** A `datetime-local` value as a UTC RFC 3339 timestamp, if it lies inside the bounds. */
export function pointToUtc(
  value: string,
  now: Date,
  windowSecs: number,
  since?: string | null,
): { ok: true; iso: string } | { ok: false; error: string } {
  const t = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}$/.test(value) ? new Date(value).getTime() : NaN;
  if (Number.isNaN(t)) return { ok: false, error: "Pick a date and time." };
  const b = bounds(now, windowSecs, since);
  if (t > b.max) return { ok: false, error: "The time can't be in the future." };
  if (t < b.window) return { ok: false, error: "That time is outside the history window." };
  if (t < b.min) return { ok: false, error: "That time is before the branch's history begins." };
  return { ok: true, iso: new Date(t).toISOString() };
}

export const MIN_WINDOW_SECS = 3600;
export const MAX_WINDOW_SECS = 30 * 24 * 3600;
export const MAX_BRANCH_LIMIT = 20;

export type WindowUnit = "hours" | "days";

/** The window in the largest whole unit (days when it is a whole number of days). */
export function windowParts(secs: number): { value: number; unit: WindowUnit } {
  return secs % 86400 === 0 ? { value: secs / 86400, unit: "days" } : { value: Math.round(secs / 3600), unit: "hours" };
}

export const MAX_ROWS_RANGE = [1, 100_000] as const;
export const STATEMENT_TIMEOUT_RANGE = [100, 120_000] as const;
export const MAX_CONNECTIONS_RANGE = [1, 50] as const;

export interface SettingsInput {
  window: string;
  unit: WindowUnit;
  maxBranches: string;
  maxRows: string;
  statementTimeoutMs: string;
  maxConnections: string;
}
export type SettingsErrors = Partial<Record<Exclude<keyof SettingsInput, "unit">, string>>;

const whole = (text: string) => (/^\d+$/.test(text.trim()) ? Number(text.trim()) : NaN);
const inRange = (n: number, [min, max]: readonly [number, number]) => !Number.isNaN(n) && n >= min && n <= max;

export function parseSettings(
  input: SettingsInput,
): { ok: true; settings: DatabaseSettings } | ({ ok: false } & SettingsErrors) {
  const errors: SettingsErrors = {};
  const w = whole(input.window);
  const secs = w * (input.unit === "days" ? 86400 : 3600);
  if (Number.isNaN(w) || secs < MIN_WINDOW_SECS || secs > MAX_WINDOW_SECS) {
    errors.window = "The history window must be a whole number between 1 hour and 30 days.";
  }
  const m = whole(input.maxBranches);
  if (Number.isNaN(m) || m > MAX_BRANCH_LIMIT) {
    errors.maxBranches = "The branch limit must be a whole number between 0 and 20.";
  }
  const rows = whole(input.maxRows);
  if (!inRange(rows, MAX_ROWS_RANGE)) errors.maxRows = "Max rows must be a whole number between 1 and 100000.";
  const timeout = whole(input.statementTimeoutMs);
  if (!inRange(timeout, STATEMENT_TIMEOUT_RANGE)) {
    errors.statementTimeoutMs = "Statement timeout must be a whole number between 100 and 120000 milliseconds.";
  }
  const connections = whole(input.maxConnections);
  if (!inRange(connections, MAX_CONNECTIONS_RANGE)) {
    errors.maxConnections = "Max connections must be a whole number between 1 and 50.";
  }
  if (Object.keys(errors).length > 0) return { ok: false, ...errors };
  return {
    ok: true,
    settings: {
      history_window: secs,
      max_branches: m,
      max_rows: rows,
      statement_timeout_ms: timeout,
      max_connections: connections,
    },
  };
}
