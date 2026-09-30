import type { BranchSummary, DatabaseSettings } from "@cheladb/types";

export const BACKUP_CONNECT_REASON = "restore backup: create a branch from it to connect";
const NAME_RE = /^[a-z0-9][a-z0-9_-]*$/;

export interface TreeRow {
  branch: BranchSummary;
  depth: number;
  /** null for main. */
  parentName: string | null;
}

/** Depth-first: main first, then each branch's children (oldest first) under it. */
export function buildTree(branches: BranchSummary[]): TreeRow[] {
  const live = branches.filter((b) => b.status !== "deleted");
  const byId = new Map(live.map((b) => [b.id as string, b]));
  const children = new Map<string | null, BranchSummary[]>();
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
  const visit = (b: BranchSummary, depth: number) => {
    const parent = b.parent_branch_id === null ? undefined : byId.get(b.parent_branch_id);
    rows.push({ branch: b, depth, parentName: parent?.name ?? null });
    for (const c of children.get(b.id) ?? []) visit(c, depth + 1);
  };
  for (const r of roots) visit(r, 0);
  return rows;
}

const TRANSITIONAL = new Set(["pending", "provisioning", "resetting", "deleting"]);
export const isTransitional = (b: BranchSummary) => TRANSITIONAL.has(b.status);

/** Live `normal` children; backups never count. */
export function liveNormalChildren(branch: BranchSummary, all: BranchSummary[]): BranchSummary[] {
  return all.filter(
    (b) => b.parent_branch_id === branch.id && b.kind === "normal" && b.status !== "deleted",
  );
}

export type ActionKey = "connect" | "resetPassword" | "resetToParent" | "restore" | "delete";

/** Why each action is unavailable (null when it is available). */
export function actionReasons(branch: BranchSummary, all: BranchSummary[]): Record<ActionKey, string | null> {
  const isMain = branch.kind === "normal" && branch.parent_branch_id === null;
  const kids = liveNormalChildren(branch, all);
  const childReason = kids.length > 0 ? `has child branches: ${kids.map((k) => k.name).join(", ")}` : null;
  const notReady = branch.status !== "ready" ? `branch is not ready (it is ${branch.status})` : null;
  const backup = branch.kind === "backup";

  const connect = backup ? BACKUP_CONNECT_REASON : notReady;
  const rewrite = backup ? "a restore backup can't be rewritten; create a branch from it" : (notReady ?? childReason);
  return {
    connect,
    resetPassword: connect,
    resetToParent: isMain ? "main can't be reset to a parent" : rewrite,
    restore: rewrite,
    delete: isMain
      ? "main can't be deleted; delete the database instead"
      : branch.status === "deleting"
        ? "branch is already being deleted"
        : childReason,
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

export function parseSettings(input: {
  window: string;
  unit: WindowUnit;
  maxBranches: string;
}): { ok: true; settings: DatabaseSettings } | { ok: false; window?: string; maxBranches?: string } {
  const errors: { window?: string; maxBranches?: string } = {};
  const w = /^\d+$/.test(input.window.trim()) ? Number(input.window.trim()) : NaN;
  const secs = w * (input.unit === "days" ? 86400 : 3600);
  if (Number.isNaN(w) || secs < MIN_WINDOW_SECS || secs > MAX_WINDOW_SECS) {
    errors.window = "The history window must be a whole number between 1 hour and 30 days.";
  }
  const m = /^\d+$/.test(input.maxBranches.trim()) ? Number(input.maxBranches.trim()) : NaN;
  if (Number.isNaN(m) || m > MAX_BRANCH_LIMIT) {
    errors.maxBranches = "The branch limit must be a whole number between 0 and 20.";
  }
  if (errors.window || errors.maxBranches) return { ok: false, ...errors };
  return { ok: true, settings: { history_window: secs, max_branches: m } };
}
