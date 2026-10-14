import type { ApiError, Dependents, LockLevel, SchemaChangeResponse, SkipReason } from "@chelabase/management-client";
import { editIndexOfOp, type Edit } from "./changeset";

/** Where one edit of the changeset stands after the latest dry run (or apply). */
export type EditStatus =
  | { kind: "pending" }
  | { kind: "ok" }
  | { kind: "rendered_only"; reason: SkipReason }
  | { kind: "error"; message: string; detail: string | null; hint: string | null };

/** One rendered statement, attributed to the edit (not the flattened operation) it came from. */
export interface StatementView {
  sql: string;
  edit: number;
  lock: LockLevel;
  executed: boolean;
  rewrites: boolean;
  skipped: SkipReason | null;
  notices: string[];
}

export interface DryRunView {
  statements: StatementView[];
  perEdit: EditStatus[];
  warnings: string[];
  dependents: Dependents | null;
  /** From `would_succeed`: false → `blocked`; null (not everything was simulated) → `needs_ack`, the user confirms; true → `ready`. */
  apply: "ready" | "needs_ack" | "blocked";
}

export interface ChangeFailure {
  edit: number | null;
  status: EditStatus;
  code: string;
  retryable: boolean;
  dependents: Dependents | null;
}

export function mapDryRun(edits: Edit[], res: SchemaChangeResponse, ctx: { table: string; totalBytes: number | null }): DryRunView {
  const statements: StatementView[] = (res.statements ?? []).map((s) => ({
    sql: s.sql,
    edit: editIndexOfOp(edits, s.op_index),
    lock: s.lock,
    executed: s.executed,
    rewrites: s.rewrites_table ?? false,
    skipped: s.skipped ?? null,
    notices: s.notices ?? [],
  }));
  const perEdit: EditStatus[] = edits.map((_, i) => {
    const skipped = statements.find((s) => s.edit === i && s.skipped !== null);
    return skipped?.skipped ? { kind: "rendered_only", reason: skipped.skipped } : { kind: "ok" };
  });
  const warnings: string[] = [];
  if (statements.some((s) => s.rewrites)) {
    const size = ctx.totalBytes === null ? "" : ` (${formatBytes(ctx.totalBytes)})`;
    warnings.push(`Rewrites ${ctx.table}${size}: reads and writes wait while it runs`);
  }
  const dependents = res.dependents ?? null;
  // Warnings and dependents are information; only an unproven dry run (null) needs the user's acknowledgement.
  const apply = res.would_succeed === false ? "blocked" : res.would_succeed == null ? "needs_ack" : "ready";
  return { statements, perEdit, warnings, dependents, apply };
}

/** Error codes worth retrying as they are (the same request can succeed later). */
const RETRYABLE = new Set(["lock_timeout", "timeout", "unavailable", "worker_unavailable", "rate_limited", "network"]);

/** A `conflict` is retryable when Postgres aborted it as a deadlock (40P01) or a serialization failure (40001). */
const RETRYABLE_SQLSTATES = new Set(["40P01", "40001"]);

function text(v: unknown): string | null {
  return typeof v === "string" ? v : null;
}

export function mapChangeError(edits: Edit[], err: ApiError): ChangeFailure {
  const opIndex = err.extra.op_index;
  const idx = typeof opIndex === "number" ? editIndexOfOp(edits, opIndex) : -1;
  const deps = err.extra.dependents;
  return {
    edit: idx === -1 ? null : idx,
    status: { kind: "error", message: err.message, detail: text(err.extra.detail), hint: text(err.extra.hint) },
    code: err.code,
    retryable: RETRYABLE.has(err.code) || (err.code === "conflict" && RETRYABLE_SQLSTATES.has(text(err.extra.sqlstate) ?? "")),
    dependents: deps && typeof deps === "object" ? (deps as Dependents) : null,
  };
}

export function lockText(l: LockLevel): string {
  switch (l) {
    case "access_exclusive":
      return "Blocks reads and writes on the table while it runs";
    case "share_row_exclusive":
    case "share":
      return "Blocks writes while it runs";
    case "share_update_exclusive":
      return "Allows reads and writes; blocks other schema changes";
    case "none":
      return "No table lock";
  }
}

export function skipText(r: SkipReason): string {
  switch (r) {
    case "large_table_rewrite":
      return "Not simulated: rewrites a large table";
    case "concurrently":
      return "Not simulated: runs concurrently";
    case "after_skipped":
      return "Not simulated: follows a statement that wasn't simulated";
    case "already_dropped":
      return "Not simulated: its object is dropped earlier in this change";
  }
}

const UNITS = ["B", "kB", "MB", "GB", "TB"];

/** Sizes in powers of 1024, `pg_size_pretty` style: "500 B", "8 kB", "1.5 GB". */
export function formatBytes(n: number): string {
  let v = n;
  let u = 0;
  while (Math.abs(v) >= 1024 && u < UNITS.length - 1) {
    v /= 1024;
    u++;
  }
  if (u === 0) return `${Math.round(v)} B`;
  const shown = Math.abs(v) < 10 ? Math.round(v * 10) / 10 : Math.round(v);
  return `${shown} ${UNITS[u]}`;
}
