/**
 * Schema merges in Studio: status labels, the busy and open predicates, readers for the engine's
 * documents (`analysis`, `plan`, `rehearsal`, `result` are served as the worker stored them), the
 * refusals before apply, refusal texts, and the own-step edits (insert, move, delete).
 */
import { ApiError, type Merge, type MergeLogEntry, type MergeState, type UserStepBody } from "@chelabase/management-client";
import { errorText } from "@/lib/errors";

export const MERGE_STATUS_LABEL: Record<MergeState, string> = {
  computing: "Computing",
  draft: "Draft",
  rehearsing: "Rehearsing",
  rehearsed: "Rehearsed",
  applying: "Applying",
  online: "Applying online steps",
  finishing: "Finishing",
  done: "Done",
  applied_with_drift: "Applied with drift",
  failed: "Failed",
  stale: "Stale",
  cancelled: "Cancelled",
};

const BUSY = new Set<MergeState>(["computing", "rehearsing", "applying", "online", "finishing"]);
const ENDED = new Set<MergeState>(["done", "applied_with_drift", "stale", "cancelled"]);
/** What the API's hold rules look at: the status, and the phase a failed merge failed in. */
type MergeProgress = Pick<Merge, "status" | "failed_phase">;

/** A worker holds the merge: the page polls. */
export const isMergeBusy = (s: MergeState) => BUSY.has(s);
/**
 * Not ended (the API's one-open-merge-per-pair rule counts these). A merge failed in `online` is
 * half applied and stays open until it is retried to the end or abandoned (E27); any other
 * failure has ended.
 */
export const isMergeOpen = (m: MergeProgress) =>
  m.status === "failed" ? m.failed_phase === "online" : !ENDED.has(m.status);
const HOLDING = new Set<MergeState>(["rehearsing", "applying", "online", "finishing"]);
/**
 * The merge refuses reset, restore and delete of its source and target (E27): a worker is on
 * them, or the merge sits half applied. A draft or rehearsed one doesn't hold them.
 */
export const holdsBranches = (m: MergeProgress) =>
  HOLDING.has(m.status) || (m.status === "failed" && m.failed_phase === "online");
/** The API takes edits (resolutions, steps, acknowledgements, options). */
export const isMergeEditable = (s: MergeState) => s === "draft" || s === "rehearsed";
/** The API can cancel it: nothing is changing the target yet. */
export const isMergeCancellable = (s: MergeState) =>
  s === "computing" || s === "draft" || s === "rehearsing" || s === "rehearsed";

/** A timestamp to the minute, in UTC, as the branch tree shows them. */
export const utcMinute = (iso: string) => `${new Date(iso).toISOString().slice(0, 16).replace("T", " ")} UTC`;

/** "Merge dev into main" / "Update dev from main": the branch (not the parent) is the subject. */
export function mergeTitle(direction: Merge["direction"], sourceName: string, targetName: string): string {
  return direction === "into_parent" ? `Merge ${sourceName} into ${targetName}` : `Update ${targetName} from ${sourceName}`;
}

// ---- the engine's documents ------------------------------------------------------------------

export type ObjKind =
  | "schema"
  | "table"
  | "column"
  | "constraint"
  | "index"
  | "policy"
  | "grant"
  | "enum"
  | "sequence"
  | "role"
  | "default_privilege";

/** The engine's `ObjKind` order, which the Changes list follows. */
export const OBJ_KINDS: ObjKind[] = [
  "schema",
  "table",
  "column",
  "constraint",
  "index",
  "policy",
  "grant",
  "enum",
  "sequence",
  "role",
  "default_privilege",
];

export const OBJ_KIND_LABEL: Record<ObjKind, string> = {
  schema: "Schemas",
  table: "Tables",
  column: "Columns",
  constraint: "Constraints",
  index: "Indexes",
  policy: "Policies",
  grant: "Grants",
  enum: "Enums",
  sequence: "Sequences",
  role: "Roles",
  default_privilege: "Default privileges",
};

export interface ObjKey {
  kind: ObjKind;
  schema: string | null;
  table: string | null;
  name: string;
}

/** An externally tagged `ObjSnapshot`: `{ "<kind>": { …the object… } }`. */
export type Snapshot = Record<string, unknown>;

export type Change = { create: Snapshot } | { drop: Snapshot } | { alter: { before: Snapshot; after: Snapshot } };

export type Outcome = "apply" | "target_only" | "overlap" | { conflict: string };

export interface MergeItem {
  id: string;
  key: ObjKey;
  source: Change | null;
  target: Change | null;
  outcome: Outcome;
  data_loss: boolean;
  security: boolean;
}

export type ConflictKind = "edit" | "drop_vs_edit" | "create_collision" | "column_order" | "dependency";

export interface MergeConflict {
  id: string;
  kind: ConflictKind;
  items: string[];
  base: Snapshot | null;
  target: Snapshot | null;
  source: Snapshot | null;
}

export interface Analysis {
  items: MergeItem[];
  conflicts: MergeConflict[];
  reported: unknown[];
}

export type Phase = "roles" | "enum_add" | "tx" | "online";
export const PHASES: Phase[] = ["roles", "enum_add", "tx", "online"];
export const PHASE_LABEL: Record<Phase, string> = {
  roles: "Phase 0: roles",
  enum_add: "Phase 1a: enum values, committed first",
  tx: "Phase 1: one transaction",
  online: "Phase 2: online, one statement at a time",
};

/** What a step breaks outside the database (E29a): the engine's `Breaking`, externally tagged. */
export interface Breaking {
  role_rename?: { role: string; new_name: string };
}

export interface PlanStep {
  id: string;
  kind: "generated" | "user" | "own_sql";
  phase: Phase;
  sql: string;
  items: string[];
  data_loss: boolean;
  rewrite: boolean;
  security: boolean;
  locks: string[];
  role_op: unknown;
  online: unknown;
  /** Set on a role rename: clients connecting as the old name fail to authenticate after the apply. */
  breaking?: Breaking | null;
}

export interface Plan {
  steps: PlanStep[];
}

export interface RehearsalStep {
  id: string;
  duration_ms: number | null;
  outcome: string | null;
}

/** One difference between what the merge should give and what the rehearsal or apply gave. */
export interface Drift {
  key: ObjKey;
  kind: "missing" | "unexpected" | "different";
  expected: Snapshot | null;
  actual: Snapshot | null;
  /** Produced by own SQL: shown for confirmation, not as drift. */
  produced: boolean;
}

export interface Rehearsal {
  steps: RehearsalStep[];
  failed_step: string | null;
  error: string | null;
  drift: Drift[];
}

export interface CreatedRole {
  name: string;
  branch_id?: string;
}

/** What an abandoned merge left on the target (E27): the engine's `Leftover`. */
export interface Leftover {
  kind: string;
  schema: string;
  table: string;
  name: string;
}

/** `result.after`, as the worker writes it at each stage of the after-merge choice. */
export interface AfterDoc {
  choice: string | null;
  /** Why the choice wasn't run (the target doesn't read back as merged). */
  skipped: string | null;
  /** `finishMerge` recorded the request; the worker hasn't started it. */
  requested: boolean;
  /** A reset or delete is running. */
  started: boolean;
  refused: string | null;
  error: string | null;
  /** A keep: `recorded`, `not_advanced` or `not_recorded`. */
  base: string | null;
  /** A reset or delete: `done`, or `started` (the branch is still switching). */
  outcome: string | null;
}

export interface MergeResultDoc {
  created_roles: CreatedRole[];
  drift: Drift[];
  after: AfterDoc | null;
  /** Set only by Abandon; null when the merge wasn't abandoned. */
  leftovers: Leftover[] | null;
}

const isObj = (v: unknown): v is Record<string, unknown> => typeof v === "object" && v !== null && !Array.isArray(v);
const str = (v: unknown): string | null => (typeof v === "string" ? v : null);
const num = (v: unknown): number | null => (typeof v === "number" ? v : null);
const list = (v: unknown): unknown[] => (Array.isArray(v) ? v : []);

/** The stored analysis, or null when there's none (yet) or it isn't one. */
export function readAnalysis(doc: unknown): Analysis | null {
  if (!isObj(doc) || !Array.isArray(doc.items) || !Array.isArray(doc.conflicts)) return null;
  return { items: doc.items as MergeItem[], conflicts: doc.conflicts as MergeConflict[], reported: list(doc.reported) };
}

export function readPlan(doc: unknown): Plan | null {
  if (!isObj(doc) || !Array.isArray(doc.steps)) return null;
  return { steps: doc.steps as PlanStep[] };
}

const readDrift = (v: unknown): Drift[] => list(v).filter((d): d is Drift => isObj(d) && isObj(d.key));

/** The last rehearsal, read loosely: a missing field reads as empty. */
export function readRehearsal(doc: unknown): Rehearsal | null {
  if (!isObj(doc)) return null;
  const steps = list(doc.steps)
    .filter(isObj)
    .map((s) => ({ id: str(s.id) ?? str(s.step_id) ?? "", duration_ms: num(s.duration_ms), outcome: str(s.outcome) }))
    .filter((s) => s.id !== "");
  return { steps, failed_step: str(doc.failed_step), error: str(doc.error), drift: readDrift(doc.drift) };
}

const readAfter = (v: unknown): AfterDoc | null =>
  isObj(v)
    ? {
        choice: str(v.choice),
        skipped: str(v.skipped),
        requested: v.requested === true,
        started: v.started === true,
        refused: str(v.refused),
        error: str(v.error),
        base: str(v.base),
        outcome: str(v.outcome),
      }
    : null;

const readLeftovers = (v: unknown): Leftover[] | null =>
  Array.isArray(v)
    ? v
        .filter(isObj)
        .filter((l) => [l.kind, l.schema, l.table, l.name].every((x) => typeof x === "string"))
        .map((l) => ({ kind: l.kind as string, schema: l.schema as string, table: l.table as string, name: l.name as string }))
    : null;

export function readResult(doc: unknown): MergeResultDoc | null {
  if (!isObj(doc)) return null;
  const created_roles = list(doc.created_roles)
    .filter(isObj)
    .filter((r) => typeof r.name === "string")
    .map((r) => (typeof r.branch_id === "string" ? { name: r.name as string, branch_id: r.branch_id } : { name: r.name as string }));
  return { created_roles, drift: readDrift(doc.drift), after: readAfter(doc.after), leftovers: readLeftovers(doc.leftovers) };
}

const before = (c: Change | null): Snapshot | undefined =>
  c === null ? undefined : "drop" in c ? c.drop : "alter" in c ? c.alter.before : undefined;
const after = (c: Change): Snapshot | null => ("create" in c ? c.create : "alter" in c ? c.alter.after : null);

/** The object as the base, the target and the source have it (null: it doesn't exist there). */
export function itemSides(item: MergeItem): { base: Snapshot | null; target: Snapshot | null; source: Snapshot | null } {
  const base = before(item.source) ?? before(item.target) ?? null;
  return {
    base,
    target: item.target ? after(item.target) : base,
    source: item.source ? after(item.source) : base,
  };
}

/** A snapshot as read-only text: its object, indented; "—" when the side doesn't have it. */
export function snapshotText(snap: Snapshot | null): string {
  if (snap === null) return "—";
  const [inner] = Object.values(snap);
  return JSON.stringify(inner ?? snap, null, 2);
}

export function itemName(key: ObjKey): string {
  return [key.schema, key.table, key.name].filter((p): p is string => typeof p === "string" && p !== "").join(".");
}

export function groupItems(items: MergeItem[]): [ObjKind, MergeItem[]][] {
  return OBJ_KINDS.map((k) => [k, items.filter((i) => i.key.kind === k)] as [ObjKind, MergeItem[]]).filter(
    ([, xs]) => xs.length > 0,
  );
}

/** How each side changed an item, in words. */
export function changeVerb(c: Change | null): string {
  if (c === null) return "unchanged";
  return "create" in c ? "created" : "drop" in c ? "dropped" : "altered";
}

export function outcomeLabel(o: Outcome): string {
  if (typeof o === "object") return "Conflict";
  return o === "apply" ? "Applies" : o === "overlap" ? "Same on both" : "Target only";
}

export const CONFLICT_KIND_LABEL: Record<ConflictKind, string> = {
  edit: "Changed differently on both sides",
  drop_vs_edit: "Dropped on one side, changed on the other",
  create_collision: "Created on both sides with different definitions",
  column_order: "Both sides added columns to this table",
  dependency: "Depends on something the merge leaves out",
};

// ---- refusals before apply -------------------------------------------------------------------

/** Conflict ids no resolution answers (the API's `merge_conflicts_open`). */
export function openConflicts(merge: Merge): string[] {
  const resolved = new Set(merge.resolutions.map((r) => r.conflict));
  return (readAnalysis(merge.analysis)?.conflicts ?? []).map((c) => c.id).filter((id) => !resolved.has(id));
}

/** What a flagged step's acknowledgement covers: its items, or the step itself when it has none. */
export const ackIds = (s: PlanStep): string[] => (s.items.length > 0 ? s.items : [s.id]);
export const isFlagged = (s: PlanStep) => s.data_loss || s.security || Boolean(s.breaking);

/** Items (or steps) of flagged steps no acknowledgement covers (the API's `merge_drops_unacknowledged`). */
export function unacknowledged(merge: Merge): string[] {
  const acked = new Set(merge.acknowledgements);
  const missing = new Set<string>();
  for (const s of readPlan(merge.plan)?.steps ?? []) {
    if (!isFlagged(s)) continue;
    for (const id of ackIds(s)) if (!acked.has(id)) missing.add(id);
  }
  return [...missing].sort();
}

const plural = (n: number, one: string, many: string) => `${n} ${n === 1 ? one : many}`;

/** Why the merge can't be applied yet, in the API's order; empty when it can. */
export function applyBlockers(merge: Merge): string[] {
  if (merge.status === "stale") return ["A schema changed since the merge was computed: recompute the merge."];
  if (merge.status !== "rehearsed") return ["Rehearse the merge first."];
  const out: string[] = [];
  const open = openConflicts(merge).length;
  if (open > 0) out.push(`Choose how to settle ${plural(open, "conflict", "conflicts")}.`);
  const plan = readPlan(merge.plan)?.steps ?? [];
  const acked = new Set(merge.acknowledgements);
  const steps = plan.filter((s) => isFlagged(s) && ackIds(s).some((id) => !acked.has(id))).length;
  if (steps > 0) {
    out.push(
      steps === 1
        ? "Acknowledge 1 step that loses data, changes security or breaks clients."
        : `Acknowledge ${steps} steps that lose data, change security or break clients.`,
    );
  }
  return out;
}

/** The base (the branch point's schema) is gone: the page offers re-baseline. */
export const baseUnavailable = (merge: Pick<Merge, "error">) => merge.error?.includes("merge_base_unavailable") ?? false;

// ---- refusal texts ---------------------------------------------------------------------------

const mergeIdOf = (err: ApiError): string | null => (typeof err.extra.merge_id === "string" ? err.extra.merge_id : null);

/** An action's refusal in plain words, with the merge it names (to link) when it names one. */
export function mergeErrorText(err: unknown): { text: string; mergeId: string | null } {
  if (!(err instanceof ApiError)) return { text: errorText(err), mergeId: null };
  const id = mergeIdOf(err);
  switch (err.code) {
    case "merge_open_exists":
      return { text: "These branches already have an open merge.", mergeId: id };
    case "merge_busy":
      return id
        ? { text: "Another merge is changing the target branch.", mergeId: id }
        : { text: "A merge of these branches just finished. Try again.", mergeId: null };
    case "branch_merge_busy":
      // A rehearsal's throwaway branch still hangs off it (cancel returns before it is removed).
      if (typeof err.extra.rehearsal_branch_id === "string") {
        return {
          text: "A merge's rehearsal branch is still being removed from this branch. It goes when the rehearsal ends or the merge is cancelled: try again in a moment.",
          mergeId: id,
        };
      }
      return {
        text: "A merge holds this branch: it is rehearsing or applying, or it failed half applied. Wait for it, or open it to retry or abandon it.",
        mergeId: id,
      };
    case "branch_not_ready":
      return { text: "A branch of this merge isn't ready (it may be resetting or restoring). Try again when it is.", mergeId: id };
    case "merge_version_conflict":
      return { text: "This merge changed elsewhere and was reloaded. Check it and save again.", mergeId: null };
    case "merge_conflicts_open":
      return { text: "Choose how to settle every conflict first.", mergeId: null };
    case "merge_drops_unacknowledged":
      return { text: "Acknowledge every step that loses data, changes security or breaks clients first.", mergeId: null };
    case "merge_not_rehearsed":
      return { text: "Rehearse the merge before applying it.", mergeId: null };
    case "merge_stale":
      return { text: "A schema changed since the merge was computed. Recompute the merge.", mergeId: null };
    case "merge_confirm_required":
      return { text: "The target branch is protected: confirm the apply.", mergeId: null };
    default:
      return { text: errorText(err), mergeId: id };
  }
}

// ---- the worker's error texts ----------------------------------------------------------------

export type MergeErrorKind =
  | "target_changed"
  | "target_gone"
  | "half_applied"
  | "stale"
  | "resolution_refused"
  | "base_unavailable"
  | "interrupted"
  | "apply_not_started"
  | "rehearsal_failed"
  | "rehearsal_unread"
  | "rehearsal_not_run"
  | "other";

export interface MergeErrorInfo {
  kind: MergeErrorKind;
  /** The first line without its code prefix: a sentence for the reader (or Postgres's own text). */
  message: string;
  /** For `resolution_refused`: one entry per refused choice, `<conflict id>: <reason>`. */
  refused: string[];
  /** The lines after the first (the planner's refused statements). */
  rest: string;
}

const ERROR_PREFIX: [string, MergeErrorKind][] = [
  ["merge_target_changed: ", "target_changed"],
  ["merge_target_gone: ", "target_gone"],
  ["merge_half_applied: ", "half_applied"],
  ["merge_stale: ", "stale"],
  ["merge_resolution_refused: ", "resolution_refused"],
  ["merge_base_unavailable: ", "base_unavailable"],
  // The rehearsal's texts (a rehearsal that ends in `draft`): a step failed, the result couldn't
  // be read, or it couldn't run at all (its branch couldn't be made, a branch wasn't ready, an
  // earlier one couldn't be removed, a conflict was opened meanwhile).
  ["the rehearsal failed at ", "rehearsal_failed"],
  ["the rehearsal: ", "rehearsal_unread"],
  ["creating the rehearsal: ", "rehearsal_not_run"],
  ["branch_not_ready: ", "rehearsal_not_run"],
  ["merge_conflicts_open: ", "rehearsal_not_run"],
];
/** Rehearsal texts kept whole: the sentence is the reason. */
const KEPT_WHOLE = "a previous rehearsal couldn't be removed: ";
/** A reason nested in a rehearsal text (`creating the rehearsal: branch_not_ready: …`). */
const NESTED_PREFIX = "branch_not_ready: ";
/** The refusals the apply makes before it runs anything (the merge stays `rehearsed`). */
const APPLY_REFUSAL_PREFIXES = ["branch_not_ready: ", "merge_conflicts_open: ", "merge_drops_unacknowledged: "];
/** The worker's text for an apply that stopped before its transaction committed. */
const INTERRUPTED_PREFIX = "interrupted before the plan's transaction committed";

/**
 * A merge's `error` read by the code the worker puts first (a prefix, then a sentence), so the
 * page can say what happened in its own words. Anything else is shown as Postgres wrote it.
 */
export function explainMergeError(error: string | null, status?: string): MergeErrorInfo | null {
  if (!error) return null;
  const newline = error.indexOf("\n");
  const first = newline < 0 ? error : error.slice(0, newline);
  const rest = newline < 0 ? "" : error.slice(newline + 1);
  // On a rehearsed merge these refusals came from the apply, which never started.
  if (status === "rehearsed") {
    const prefix = APPLY_REFUSAL_PREFIXES.find((p) => first.startsWith(p));
    if (prefix) return { kind: "apply_not_started", message: first.slice(prefix.length), refused: [], rest };
  }
  for (const [prefix, kind] of ERROR_PREFIX) {
    if (!first.startsWith(prefix)) continue;
    let message = first.slice(prefix.length);
    if (kind === "rehearsal_not_run" && message.startsWith(NESTED_PREFIX)) message = message.slice(NESTED_PREFIX.length);
    return { kind, message, refused: kind === "resolution_refused" ? message.split("; ") : [], rest };
  }
  if (first.startsWith(KEPT_WHOLE)) return { kind: "rehearsal_not_run", message: first, refused: [], rest };
  if (first.startsWith(INTERRUPTED_PREFIX)) return { kind: "interrupted", message: first, refused: [], rest };
  return { kind: "other", message: first, refused: [], rest };
}

// ---- apply progress ---------------------------------------------------------------------------

/**
 * The plan's step ids the current apply attempt finished: the events since the merge last moved
 * to `applying`. The worker logs the plan's transaction once, as `phase1`; when it is ok every
 * `tx` step counts as done.
 */
export function appliedSteps(events: MergeLogEntry[], steps: PlanStep[]): Set<string> {
  const start = events.map((e) => e.kind === "status" && e.detail?.to === "applying").lastIndexOf(true) + 1;
  const done = new Set<string>();
  for (const e of events.slice(start)) {
    if (e.kind !== "step" || e.step_id === null || (e.outcome !== "ok" && e.outcome !== "skipped")) continue;
    if (e.step_id === "phase1") {
      for (const s of steps) if (s.phase === "tx") done.add(s.id);
    } else {
      done.add(e.step_id);
    }
  }
  return done;
}

// ---- what an abandoned merge left ---------------------------------------------------------------

const quote = (ident: string) => `"${ident.replaceAll('"', '""')}"`;
const qualified = (l: Leftover) => `${quote(l.schema)}.${quote(l.table)}`;

/** What a leftover is, in words. */
export function leftoverText(l: Leftover): string {
  switch (l.kind) {
    case "not_valid_constraint":
      return `Constraint ${l.name} was added NOT VALID and never validated. It still checks new rows.`;
    case "not_null_check":
      return `Check ${l.name} stands in for a NOT NULL the merge didn't finish. It still checks new rows.`;
    case "invalid_index":
      return `Index ${l.name} was being built or dropped when the merge stopped. Postgres may have left it INVALID: queries don't use it but writes still update it. It may also be gone.`;
    case "unattached_index":
      return `Index ${l.name} was built by the merge but not put into place. Nothing uses it, and it costs on every write.`;
    default:
      return `${l.kind.replaceAll("_", " ")} ${l.name}`;
  }
}

/** SQL that clears a leftover, or null for a kind this page doesn't know. */
export function leftoverFix(l: Leftover): string | null {
  switch (l.kind) {
    case "not_valid_constraint":
      return `ALTER TABLE ${qualified(l)} VALIDATE CONSTRAINT ${quote(l.name)};`;
    case "not_null_check":
      return `ALTER TABLE ${qualified(l)} DROP CONSTRAINT ${quote(l.name)};`;
    case "invalid_index":
    case "unattached_index":
      return `DROP INDEX CONCURRENTLY IF EXISTS ${quote(l.schema)}.${quote(l.name)};`;
    default:
      return null;
  }
}

// ---- the after-merge choice -------------------------------------------------------------------

/** An after-merge choice that was requested or started and has no outcome yet: the page polls. */
export function isFinishPending(merge: Pick<Merge, "status" | "result">): boolean {
  if (merge.status !== "applied_with_drift") return false;
  const after = readResult(merge.result)?.after;
  return after !== null && after !== undefined && (after.requested || after.started);
}

const VERB: Record<string, [waiting: string, running: string]> = {
  keep: ["keep", "Keeping"],
  reset: ["reset", "Resetting"],
  delete: ["delete", "Deleting"],
};

/** What the after-merge choice did, in words; null when there is no choice recorded yet. */
export function describeAfter(
  after: AfterDoc | null,
  names: { sourceName: string; targetName: string; direction: Merge["direction"] },
): string | null {
  if (after === null) return null;
  const { sourceName: source, targetName: target } = names;
  const choice = after.choice ?? "keep";
  const [verb, running] = VERB[choice] ?? [choice, "Running"];
  if (after.skipped) return `Skipped: ${target} doesn't read back as merged, so ${source} and its base stay as they were.`;
  if (after.started) {
    return choice === "reset" ? `Resetting ${source} to ${target}…` : `${running} ${source}…`;
  }
  if (after.requested) return `Waiting to ${verb} ${source}…`;
  if (after.refused) return `Not done: ${after.refused}. ${source} stays as it is; the merge itself is complete.`;
  if (after.error && after.base === null) return `Couldn't run the choice: ${after.error}.`;
  if (after.base === "recorded") {
    return names.direction === "from_parent"
      ? `Updated: ${source}'s schema is now ${target}'s base.`
      : `Kept ${source}: ${target}'s merged schema is now its base.`;
  }
  if (after.base === "not_advanced") {
    return `Kept ${source}, but it changed after the merge was computed, so its base wasn't advanced: those changes show in its next merge.`;
  }
  if (after.base === "not_recorded") {
    return `Kept ${source}, but recording its new base failed (${after.error ?? "unknown error"}). The base stays as it was.`;
  }
  if (after.outcome === "done") return choice === "delete" ? `${source} was deleted.` : `${source} was reset to ${target}.`;
  if (after.outcome === "started") return `${source} is being reset to ${target}: it is ready when the reset ends.`;
  return null;
}

// ---- own steps -------------------------------------------------------------------------------

/** The user step a plan row belongs to (`<id>` or `<id>.<n>` for a step of several statements). */
function ownerOf(row: PlanStep, ids: string[]): string {
  if (row.kind !== "user") return row.id;
  const hits = ids.filter((id) => row.id === id || row.id.startsWith(`${id}.`));
  return hits.sort((a, b) => b.length - a.length)[0] ?? row.id;
}

/** The plan as units: each generated or own-SQL step, and each user step once (all its statements). */
function units(steps: UserStepBody[], plan: PlanStep[]): string[] {
  const ids = steps.map((s) => s.id);
  const out: string[] = [];
  for (const row of plan) {
    const u = ownerOf(row, ids);
    if (out[out.length - 1] !== u) out.push(u);
  }
  return out;
}

/** `step` with `anchor` as its anchor, placed first among the steps already at that anchor. */
function placeAt(steps: UserStepBody[], step: UserStepBody, anchor: string | null): UserStepBody[] {
  const at = steps.findIndex((s) => s.id === step.id);
  const rest = steps.filter((s) => s.id !== step.id);
  const moved = { ...step, after: anchor };
  const first = rest.findIndex((s) => (s.after ?? null) === anchor);
  const i = first >= 0 ? first : Math.min(at, rest.length);
  return [...rest.slice(0, i), moved, ...rest.slice(i)];
}

/**
 * The steps with `id` moved one place up or down in the plan's order (by re-anchoring it), or
 * null when it can't move that way. The API refuses a move out of its phase.
 */
export function moveUserStep(
  steps: UserStepBody[],
  plan: PlanStep[],
  id: string,
  dir: "up" | "down",
): UserStepBody[] | null {
  const step = steps.find((s) => s.id === id);
  if (!step) return null;
  const order = units(steps, plan);
  const i = order.indexOf(id);
  if (i < 0) return null;
  if (dir === "up") {
    if (i === 0) return null;
    return placeAt(steps, step, i >= 2 ? order[i - 2] : null);
  }
  if (i === order.length - 1) return null;
  return placeAt(steps, step, order[i + 1]);
}

/** A new step right after `anchor` (null: the start), with the first free `step-<n>` id. */
export function insertUserStep(steps: UserStepBody[], anchor: string | null, sql: string): UserStepBody[] {
  const taken = new Set(steps.map((s) => s.id));
  let n = 1;
  while (taken.has(`step-${n}`)) n++;
  const fresh: UserStepBody = { id: `step-${n}`, after: anchor, sql };
  const first = steps.findIndex((s) => (s.after ?? null) === anchor);
  if (first < 0) return [...steps, fresh];
  return [...steps.slice(0, first), fresh, ...steps.slice(first)];
}

/** The steps without `id`; steps that followed it now follow its anchor. */
export function deleteUserStep(steps: UserStepBody[], id: string): UserStepBody[] {
  const gone = steps.find((s) => s.id === id);
  if (!gone) return steps;
  return steps.filter((s) => s.id !== id).map((s) => (s.after === id ? { ...s, after: gone.after ?? null } : s));
}

/**
 * The planner's refusals of each user step, found in the merge's error text: one line per
 * refused statement, `step <id>: <reason>`.
 */
export function stepErrors(error: string | null, ids: string[]): Record<string, string[]> {
  const out: Record<string, string[]> = {};
  if (!error) return out;
  for (const line of error.split("\n")) {
    for (const id of ids) {
      const marker = `step ${id}: `;
      const at = line.indexOf(marker);
      if (at >= 0) (out[id] ??= []).push(line.slice(at + marker.length).trim());
    }
  }
  return out;
}
