import type { Branch, Merge, MergeDetail, MergeLogEntry, MergeSummary } from "@chelabase/management-client";
import type { PlanStep } from "@/lib/merges";

/** A merge as `getMerge` returns it (`draft`, dev into main, nothing computed beyond an empty plan). */
export function mergeOf(over: Partial<Merge> = {}): Merge {
  return {
    id: "m1",
    source_branch_id: "b-dev",
    target_branch_id: "b-main",
    direction: "into_parent",
    database: "web1",
    status: "draft",
    failed_phase: null,
    version: 3,
    rebaselined: false,
    error: null,
    created_by: "u1",
    created_at: "2026-10-06T10:00:00Z",
    updated_at: "2026-10-06T10:05:00Z",
    base_fingerprint: null,
    source_fingerprint: null,
    target_fingerprint: null,
    analysis: { items: [], conflicts: [], reported: [] },
    resolutions: [],
    steps: [],
    acknowledgements: [],
    options: {},
    plan: { steps: [] },
    rehearsal: null,
    result: null,
    ...over,
  };
}

/** A merge as lists and action answers carry it. */
export function summaryOf(over: Partial<MergeSummary> = {}): MergeSummary {
  const { id, source_branch_id, target_branch_id, direction, database, status, failed_phase, version, rebaselined, error, created_by, created_at, updated_at } =
    mergeOf();
  return {
    id, source_branch_id, target_branch_id, direction, database, status, failed_phase, version, rebaselined, error, created_by, created_at, updated_at,
    ...over,
  };
}

export const detailOf =(merge: Merge, events: MergeLogEntry[] = []): MergeDetail => ({ merge, events });

/** A plan step as the engine writes it. */
export const planStep = (id: string, over: Partial<PlanStep> = {}): PlanStep => ({
  id,
  kind: "generated",
  phase: "tx",
  sql: `-- ${id}`,
  items: [],
  data_loss: false,
  rewrite: false,
  security: false,
  locks: [],
  role_op: null,
  online: null,
  ...over,
});

export const branchOf = (over: Partial<Branch> & { id: string; name: string }): Branch => ({
  parent_branch_id: null,
  kind: "normal",
  status: "ready",
  host: "ep-aaaaaaaaaaaa.localtest.me",
  port: 4432,
  branched_at_time: null,
  branched_at_lsn: null,
  created_at: "2026-09-30T10:00:00Z",
  history_start: null,
  error: null,
  protected: false,
  main_database_missing: false, guard_logins: true, review_acknowledged_hash: null,
  ...over,
});

export const mainBranch = branchOf({ id: "b-main", name: "main" });
export const devBranch = branchOf({ id: "b-dev", name: "dev", parent_branch_id: "b-main" });

/** A column snapshot (the engine's externally tagged `ObjSnapshot`). */
export const columnSnap = (name: string, type = "text") => ({
  column: { table_oid: 1, schema: "public", table: "t", column: { name, type } },
});
