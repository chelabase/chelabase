import { describe, expect, it } from "vitest";
import { ApiError, type UserStepBody } from "@chelabase/management-client";
import { columnSnap as col, mergeOf, planStep as step } from "@/test/merge-fixtures";
import {
  MERGE_STATUS_LABEL,
  applyBlockers,
  baseUnavailable,
  deleteUserStep,
  groupItems,
  insertUserStep,
  appliedSteps,
  describeAfter,
  explainMergeError,
  holdsBranches,
  isFinishPending,
  isFlagged,
  isMergeBusy,
  isMergeOpen,
  leftoverFix,
  leftoverText,
  itemName,
  itemSides,
  mergeErrorText,
  moveUserStep,
  openConflicts,
  readAnalysis,
  readPlan,
  readRehearsal,
  readResult,
  snapshotText,
  stepErrors,
  unacknowledged,
  type MergeItem,
  type PlanStep,
} from "./merges";

describe("merge status", () => {
  it("labels every status", () => {
    expect(MERGE_STATUS_LABEL.applied_with_drift).toBe("Applied with drift");
    expect(Object.keys(MERGE_STATUS_LABEL)).toHaveLength(12);
  });

  it("is busy while a worker holds it", () => {
    for (const s of ["computing", "rehearsing", "applying", "online", "finishing"] as const) expect(isMergeBusy(s)).toBe(true);
    for (const s of ["draft", "rehearsed", "done", "failed", "stale", "cancelled", "applied_with_drift"] as const) {
      expect(isMergeBusy(s)).toBe(false);
    }
  });

  it("is open until it ends, as the API counts it", () => {
    for (const status of ["computing", "draft", "rehearsing", "rehearsed", "applying", "online", "finishing"] as const) {
      expect(isMergeOpen({ status, failed_phase: null })).toBe(true);
    }
    for (const status of ["done", "applied_with_drift", "stale", "cancelled"] as const) {
      expect(isMergeOpen({ status, failed_phase: null })).toBe(false);
    }
  });

  it("counts a merge failed half applied as open, and any other failure as ended (E27)", () => {
    expect(isMergeOpen({ status: "failed", failed_phase: "online" })).toBe(true);
    for (const failed_phase of ["roles", "enum_add", "tx", null]) expect(isMergeOpen({ status: "failed", failed_phase })).toBe(false);
  });

  it("holds its branches while it rehearses, applies, finishes or sits half applied (E27)", () => {
    for (const status of ["rehearsing", "applying", "online", "finishing"] as const) {
      expect(holdsBranches({ status, failed_phase: null })).toBe(true);
    }
    expect(holdsBranches({ status: "failed", failed_phase: "online" })).toBe(true);
    for (const status of ["computing", "draft", "rehearsed", "done", "applied_with_drift", "stale", "cancelled"] as const) {
      expect(holdsBranches({ status, failed_phase: null })).toBe(false);
    }
    expect(holdsBranches({ status: "failed", failed_phase: "tx" })).toBe(false);
  });
});

describe("analysis documents", () => {
  const altered: MergeItem = {
    id: "column:1:2",
    key: { kind: "column", schema: "public", table: "t", name: "email" },
    source: { alter: { before: col("email"), after: col("email", "varchar(10)") } },
    target: null,
    outcome: "apply",
    data_loss: true,
    security: false,
  };

  it("reads the engine's analysis and ignores what isn't one", () => {
    expect(readAnalysis(null)).toBeNull();
    expect(readAnalysis({ nope: 1 })).toBeNull();
    const a = readAnalysis({ items: [altered], conflicts: [], reported: [] });
    expect(a?.items[0].id).toBe("column:1:2");
  });

  it("derives base, target and source from the two changes", () => {
    const sides = itemSides(altered);
    expect(sides.base).toEqual(col("email"));
    expect(sides.source).toEqual(col("email", "varchar(10)"));
    // The target didn't change it: it still has the base's version.
    expect(sides.target).toEqual(col("email"));
    const created: MergeItem = { ...altered, source: { create: col("x") }, target: { create: col("x", "int4") } };
    expect(itemSides(created)).toEqual({ base: null, source: col("x"), target: col("x", "int4") });
    const dropped: MergeItem = { ...altered, source: { drop: col("y") }, target: null };
    expect(itemSides(dropped)).toEqual({ base: col("y"), source: null, target: col("y") });
  });

  it("shows a snapshot as indented text of its object, and nothing as a dash", () => {
    expect(snapshotText(col("email"))).toBe(JSON.stringify({ table_oid: 1, schema: "public", table: "t", column: { name: "email", type: "text" } }, null, 2));
    expect(snapshotText(null)).toBe("—");
  });

  it("names an item by schema, table and name", () => {
    expect(itemName(altered.key)).toBe("public.t.email");
    expect(itemName({ kind: "role", schema: null, table: null, name: "reporter" })).toBe("reporter");
  });

  it("groups items by object type in the engine's order", () => {
    const role: MergeItem = { ...altered, id: "role:1", key: { kind: "role", schema: null, table: null, name: "r" } };
    const table: MergeItem = { ...altered, id: "table:1", key: { kind: "table", schema: "public", table: null, name: "t" } };
    expect(groupItems([role, altered, table]).map(([kind, items]) => [kind, items.length])).toEqual([
      ["table", 1],
      ["column", 1],
      ["role", 1],
    ]);
  });

  it("reads the plan, the rehearsal and the result loosely", () => {
    expect(readPlan({ steps: [step("a")] })?.steps).toHaveLength(1);
    expect(readPlan(null)).toBeNull();
    const r = readRehearsal({ steps: [{ id: "a", duration_ms: 12, outcome: "ok" }], failed_step: "a", error: "boom", drift: [] });
    expect(r).toMatchObject({ failed_step: "a", error: "boom" });
    expect(r?.steps[0]).toEqual({ id: "a", duration_ms: 12, outcome: "ok" });
    expect(readRehearsal(null)).toBeNull();
    expect(readResult({ created_roles: [{ name: "reporter" }, { name: 7 }] })?.created_roles).toEqual([{ name: "reporter" }]);
  });
});

describe("refusals before apply", () => {
  const flagged = step("drop-col", { data_loss: true, items: ["column:1:2"] });
  const security = step("role:ops", { phase: "roles", security: true, items: [] });

  it("lists conflicts no resolution answers", () => {
    const m = mergeOf({
      analysis: { items: [], conflicts: [{ id: "c1" }, { id: "c2" }], reported: [] },
      resolutions: [{ conflict: "c1", choice: "keep_target" }],
    });
    expect(openConflicts(m)).toEqual(["c2"]);
  });

  it("needs an acknowledgement per flagged item, or per step without items", () => {
    const m = mergeOf({ plan: { steps: [flagged, security, step("plain")] }, acknowledgements: ["column:1:2"] });
    expect(unacknowledged(m)).toEqual(["role:ops"]);
  });

  it("says in plain words what still blocks the apply", () => {
    expect(applyBlockers(mergeOf({ status: "draft" }))).toEqual(["Rehearse the merge first."]);
    expect(applyBlockers(mergeOf({ status: "stale" }))).toEqual([
      "A schema changed since the merge was computed: recompute the merge.",
    ]);
    const rehearsed = mergeOf({ status: "rehearsed", plan: { steps: [flagged] } });
    expect(applyBlockers(rehearsed)).toEqual(["Acknowledge 1 step that loses data, changes security or breaks clients."]);
    expect(applyBlockers({ ...rehearsed, acknowledgements: ["column:1:2"] })).toEqual([]);
  });

  it("flags a breaking step, like a drop, and asks for its acknowledgement (E29a)", () => {
    const rename = step("role:rename:a", { phase: "roles", breaking: { role_rename: { role: "a", new_name: "b" } } });
    expect(isFlagged(rename)).toBe(true);
    const m = mergeOf({ status: "rehearsed", plan: { steps: [rename] } });
    expect(unacknowledged(m)).toEqual(["role:rename:a"]);
    expect(applyBlockers(m)).toEqual(["Acknowledge 1 step that loses data, changes security or breaks clients."]);
  });

  it("knows when the branch point's schema is gone", () => {
    expect(baseUnavailable(mergeOf({ error: "merge_base_unavailable: the branch point is outside the history window" }))).toBe(true);
    expect(baseUnavailable(mergeOf({ error: "something else" }))).toBe(false);
  });
});

describe("refusal texts", () => {
  it("links the merge an error names", () => {
    const open = new ApiError(409, "merge_open_exists", "raw", { merge_id: "m9" });
    expect(mergeErrorText(open)).toEqual({ text: "These branches already have an open merge.", mergeId: "m9" });
    const busy = new ApiError(409, "branch_merge_busy", "raw", { merge_id: "m8" });
    expect(mergeErrorText(busy)).toEqual({
      text: "A merge holds this branch: it is rehearsing or applying, or it failed half applied. Wait for it, or open it to retry or abandon it.",
      mergeId: "m8",
    });
  });

  it("says a rehearsal's throwaway branch is what holds the branch", () => {
    const busy = new ApiError(409, "branch_merge_busy", "raw", { rehearsal_branch_id: "r1" });
    expect(mergeErrorText(busy)).toEqual({
      text: "A merge's rehearsal branch is still being removed from this branch. It goes when the rehearsal ends or the merge is cancelled: try again in a moment.",
      mergeId: null,
    });
  });

  it("says a branch of the merge isn't ready", () => {
    // The apply request doesn't read the target, and renames need acknowledging too (minor 4).
    expect(mergeErrorText(new ApiError(409, "merge_stale", "raw")).text).toBe(
      "A schema changed since the merge was computed. Recompute the merge.",
    );
    expect(mergeErrorText(new ApiError(409, "merge_drops_unacknowledged", "raw")).text).toBe(
      "Acknowledge every step that loses data, changes security or breaks clients first.",
    );
    expect(mergeErrorText(new ApiError(409, "branch_not_ready", "raw"))).toEqual({
      text: "A branch of this merge isn't ready (it may be resetting or restoring). Try again when it is.",
      mergeId: null,
    });
  });

  it("keeps the API's message for anything else", () => {
    expect(mergeErrorText(new ApiError(409, "conflict", "cannot edit a merge that is done"))).toEqual({
      text: "cannot edit a merge that is done",
      mergeId: null,
    });
  });
});

describe("user steps", () => {
  const steps: UserStepBody[] = [
    { id: "backfill", after: "g1", sql: "update t set a = 1" },
    { id: "audit", after: "g2", sql: "insert into log values (1); insert into log values (2)" },
  ];
  // As the planner lays them out: a user step right after its anchor, one row per statement.
  const plan: PlanStep[] = [
    step("g1"),
    step("backfill", { kind: "user" }),
    step("g2"),
    step("audit.1", { kind: "user" }),
    step("audit.2", { kind: "user" }),
    step("g3"),
  ];

  it("moves a step down past the next one", () => {
    expect(moveUserStep(steps, plan, "backfill", "down")).toEqual([
      { id: "backfill", after: "g2", sql: "update t set a = 1" },
      steps[1],
    ]);
  });

  it("moves a step up before the previous one, keeping order at a shared anchor", () => {
    expect(moveUserStep(steps, plan, "audit", "up")).toEqual([
      steps[0],
      { id: "audit", after: "backfill", sql: steps[1].sql },
    ]);
    // Past a user step at the same anchor: it goes first there.
    const shared: UserStepBody[] = [
      { id: "a", after: "g1", sql: "select 1" },
      { id: "b", after: "a", sql: "select 2" },
    ];
    const order = [step("g1"), step("a", { kind: "user" }), step("b", { kind: "user" }), step("g2")];
    expect(moveUserStep(shared, order, "b", "up")).toEqual([
      { id: "b", after: "g1", sql: "select 2" },
      { id: "a", after: "g1", sql: "select 1" },
    ]);
  });

  it("can't move past either end", () => {
    const first = [step("u", { kind: "user" }), step("g1")];
    expect(moveUserStep([{ id: "u", after: null, sql: "x" }], first, "u", "up")).toBeNull();
    expect(moveUserStep(steps, plan, "nope", "down")).toBeNull();
    expect(moveUserStep([{ id: "z", after: "g3", sql: "x" }], [...plan, step("z", { kind: "user" })], "z", "down")).toBeNull();
  });

  it("moves a step up to the very start", () => {
    const order = [step("g1"), step("u", { kind: "user" })];
    expect(moveUserStep([{ id: "u", after: "g1", sql: "x" }], order, "u", "up")).toEqual([{ id: "u", after: null, sql: "x" }]);
  });

  it("inserts a new step right after its anchor with a fresh id", () => {
    const next = insertUserStep(steps, "g1", "select 3");
    expect(next[0]).toEqual({ id: "step-1", after: "g1", sql: "select 3" });
    expect(next.slice(1)).toEqual(steps);
    expect(insertUserStep(next, null, "select 4").find((s) => s.after === null)?.id).toBe("step-2");
  });

  it("deletes a step and re-anchors the ones that followed it", () => {
    const chained: UserStepBody[] = [
      { id: "a", after: "g1", sql: "select 1" },
      { id: "b", after: "a", sql: "select 2" },
    ];
    expect(deleteUserStep(chained, "a")).toEqual([{ id: "b", after: "g1", sql: "select 2" }]);
  });

  it("finds each step's errors in the merge's error text", () => {
    const error =
      "step step-1: this statement runs in phase 1 but follows a phase 2 statement, so it would run before it\nstep step-10: transaction control can't run in a merge";
    expect(stepErrors(error, ["step-1", "step-10", "step-2"])).toEqual({
      "step-1": ["this statement runs in phase 1 but follows a phase 2 statement, so it would run before it"],
      "step-10": ["transaction control can't run in a merge"],
    });
    expect(stepErrors(null, ["step-1"])).toEqual({});
  });

  it("finds them after a refused-resolution line", () => {
    const error = "merge_resolution_refused: c1: not valid\n1 statement(s) can't run in a merge:\nstep step-1: no";
    expect(stepErrors(error, ["step-1"])).toEqual({ "step-1": ["no"] });
  });
});

describe("the worker's error texts", () => {
  it("reads each prefix the worker writes", () => {
    expect(explainMergeError("merge_target_changed: the plan's transaction is no longer on the target (recompute the merge)")).toMatchObject({
      kind: "target_changed",
      message: "the plan's transaction is no longer on the target (recompute the merge)",
    });
    expect(explainMergeError("merge_target_gone: the merge's target no longer exists; nothing more is applied")).toMatchObject({
      kind: "target_gone",
    });
    expect(explainMergeError("merge_half_applied: an earlier apply's transaction is on the target")).toMatchObject({ kind: "half_applied" });
    expect(explainMergeError("merge_stale: the target's schema changed since the merge was computed; nothing was applied")).toMatchObject({
      kind: "stale",
      message: "the target's schema changed since the merge was computed; nothing was applied",
    });
    expect(explainMergeError("merge_base_unavailable: the branch point is outside the history window")).toMatchObject({
      kind: "base_unavailable",
      message: "the branch point is outside the history window",
    });
    expect(explainMergeError("interrupted before the plan's transaction committed; roles and enum values it made before that stay on the target")).toMatchObject({
      kind: "interrupted",
    });
    expect(explainMergeError("canceling statement due to lock timeout")).toEqual({
      kind: "other",
      message: "canceling statement due to lock timeout",
      refused: [],
      rest: "",
    });
    expect(explainMergeError(null)).toBeNull();
  });

  it("reads the rehearsal's texts (I2, D3)", () => {
    expect(explainMergeError("the rehearsal failed at step s2: column \"email\" contains null values")).toMatchObject({
      kind: "rehearsal_failed",
      message: "step s2: column \"email\" contains null values",
    });
    expect(explainMergeError("the rehearsal: reading the result: timed out")).toMatchObject({
      kind: "rehearsal_unread",
      message: "reading the result: timed out",
    });
    for (const [error, message] of [
      ["creating the rehearsal: storage refused the branch", "storage refused the branch"],
      ["creating the rehearsal: branch_not_ready: branch main isn't ready", "branch main isn't ready"],
      ["branch_not_ready: branch main is resetting", "branch main is resetting"],
      ["a previous rehearsal couldn't be removed: compute busy", "a previous rehearsal couldn't be removed: compute busy"],
      ["merge_conflicts_open: 1 conflict(s) have no resolution: c1", "1 conflict(s) have no resolution: c1"],
    ]) {
      expect(explainMergeError(error), error).toMatchObject({ kind: "rehearsal_not_run", message });
    }
  });

  it.each([
    ["branch_not_ready: branch main is resetting", "branch main is resetting"],
    ["merge_conflicts_open: 1 conflict(s) have no resolution: c1", "1 conflict(s) have no resolution: c1"],
    [
      "merge_drops_unacknowledged: acknowledge these data-loss, security or breaking changes first: s1",
      "acknowledge these data-loss, security or breaking changes first: s1",
    ],
  ])("reads a refusal on a rehearsed merge as an apply that didn't start: %s", (error, message) => {
    expect(explainMergeError(error, "rehearsed")).toMatchObject({ kind: "apply_not_started", message });
  });

  it("still reads a real interruption on a rehearsed merge as interrupted", () => {
    expect(explainMergeError("interrupted before the plan's transaction committed; roles stay", "rehearsed")?.kind).toBe("interrupted");
  });

  it("keeps the rehearsal reading of a refusal on a draft", () => {
    expect(explainMergeError("branch_not_ready: branch main is resetting", "draft")?.kind).toBe("rehearsal_not_run");
  });

  it("splits a refused resolution from the planner's lines after it", () => {
    expect(explainMergeError("merge_resolution_refused: c1: not valid here; c2: no own SQL\n1 statement(s) can't run in a merge:\nstep s: no")).toEqual({
      kind: "resolution_refused",
      message: "c1: not valid here; c2: no own SQL",
      refused: ["c1: not valid here", "c2: no own SQL"],
      rest: "1 statement(s) can't run in a merge:\nstep s: no",
    });
  });
});

describe("apply progress", () => {
  const ev = (kind: string, step_id: string | null, outcome: string | null, detail: Record<string, unknown> | null = null) => ({
    at: "2026-10-06T10:00:00Z",
    kind,
    step_id,
    phase: null,
    outcome,
    duration_ms: null,
    detail,
  });
  const steps = [step("g1"), step("g2"), step("ix", { phase: "online" })];

  it("counts the plan's transaction once, as phase1, for every step in it", () => {
    const events = [ev("status", null, null, { to: "applying" }), ev("step", "phase1", "ok")];
    expect([...appliedSteps(events, steps)].sort()).toEqual(["g1", "g2"]);
  });

  it("counts online steps by their ids, and skipped ones", () => {
    const events = [ev("status", null, null, { to: "applying" }), ev("step", "phase1", "ok"), ev("step", "ix", "skipped")];
    expect([...appliedSteps(events, steps)].sort()).toEqual(["g1", "g2", "ix"]);
  });

  it("counts only the current attempt: a failed phase 1 of an earlier one doesn't tick", () => {
    const events = [ev("status", null, null, { to: "applying" }), ev("step", "phase1", "ok"), ev("status", null, null, { to: "applying" })];
    expect([...appliedSteps(events, steps)]).toEqual([]);
  });

  it("doesn't count a failed step", () => {
    expect([...appliedSteps([ev("step", "phase1", "failed")], steps)]).toEqual([]);
  });
});

describe("the result document", () => {
  it("reads what an abandoned merge left", () => {
    const r = readResult({ leftovers: [{ kind: "not_valid_constraint", schema: "public", table: "t", name: "t_fk" }, { kind: 3 }] });
    expect(r?.leftovers).toEqual([{ kind: "not_valid_constraint", schema: "public", table: "t", name: "t_fk" }]);
    expect(readResult({ created_roles: [] })?.leftovers).toBeNull();
    expect(readResult({ leftovers: [] })?.leftovers).toEqual([]);
  });

  it("says what each leftover is and how to put it right", () => {
    const l = (kind: string) => ({ kind, schema: "public", table: "orders", name: "orders_x" });
    expect(leftoverText(l("not_valid_constraint"))).toMatch(/NOT VALID/);
    expect(leftoverFix(l("not_valid_constraint"))).toBe('ALTER TABLE "public"."orders" VALIDATE CONSTRAINT "orders_x";');
    expect(leftoverText(l("not_null_check"))).toMatch(/NOT NULL/);
    expect(leftoverFix(l("not_null_check"))).toBe('ALTER TABLE "public"."orders" DROP CONSTRAINT "orders_x";');
    expect(leftoverText(l("invalid_index"))).toMatch(/INVALID/);
    expect(leftoverFix(l("invalid_index"))).toBe('DROP INDEX CONCURRENTLY IF EXISTS "public"."orders_x";');
    expect(leftoverText(l("unattached_index"))).toMatch(/costs on every write/);
    expect(leftoverFix(l("unattached_index"))).toBe('DROP INDEX CONCURRENTLY IF EXISTS "public"."orders_x";');
    expect(leftoverText(l("something_new"))).toMatch(/orders_x/);
    expect(leftoverFix(l("something_new"))).toBeNull();
  });

  it("quotes names in the suggested SQL", () => {
    expect(leftoverFix({ kind: "invalid_index", schema: "my\"s", table: "t", name: "ix" })).toBe('DROP INDEX CONCURRENTLY IF EXISTS "my""s"."ix";');
  });
});

describe("the after-merge choice, in words", () => {
  const read = (after: unknown) => readResult({ after })?.after ?? null;
  const say = (after: unknown) => describeAfter(read(after), { sourceName: "dev", targetName: "main", direction: "into_parent" });

  it("says nothing when there is no choice yet", () => {
    expect(say(undefined)).toBeNull();
  });

  it("says a skipped choice was skipped, and why", () => {
    expect(say({ choice: "reset", skipped: "the target doesn't read back as merged" })).toBe(
      "Skipped: main doesn't read back as merged, so dev and its base stay as they were.",
    );
  });

  it("says a requested or started choice is on its way", () => {
    expect(say({ choice: "reset", requested: true })).toBe("Waiting to reset dev…");
    expect(say({ choice: "delete", started: true, requested: true })).toBe("Deleting dev…");
    expect(say({ choice: "reset", started: true })).toBe("Resetting dev to main…");
    expect(say({ choice: "keep", requested: true })).toBe("Waiting to keep dev…");
  });

  it("says what a keep did to the base", () => {
    expect(say({ choice: "keep", base: "recorded" })).toBe("Kept dev: main's merged schema is now its base.");
    expect(
      describeAfter(read({ choice: "keep", base: "recorded" }), { sourceName: "main", targetName: "dev", direction: "from_parent" }),
    ).toBe("Updated: main's schema is now dev's base.");
    expect(say({ choice: "keep", base: "not_advanced" })).toBe(
      "Kept dev, but it changed after the merge was computed, so its base wasn't advanced: those changes show in its next merge.",
    );
    expect(say({ choice: "keep", base: "not_recorded", error: "boom" })).toBe(
      "Kept dev, but recording its new base failed (boom). The base stays as it was.",
    );
  });

  it("says what a reset or delete did, or why it was refused", () => {
    expect(say({ choice: "reset", outcome: "done" })).toBe("dev was reset to main.");
    expect(say({ choice: "reset", outcome: "started" })).toBe("dev is being reset to main: it is ready when the reset ends.");
    expect(say({ choice: "delete", outcome: "done" })).toBe("dev was deleted.");
    expect(say({ choice: "delete", refused: "dev has child branches: x" })).toBe(
      "Not done: dev has child branches: x. dev stays as it is; the merge itself is complete.",
    );
    expect(say({ choice: "reset", error: "the merge moved on" })).toBe("Couldn't run the choice: the merge moved on.");
  });

  it("knows a request is still pending", () => {
    const base = { status: "applied_with_drift" as const };
    expect(isFinishPending(mergeOf({ ...base, result: { after: { choice: "keep", requested: true } } }))).toBe(true);
    expect(isFinishPending(mergeOf({ ...base, result: { after: { choice: "reset", started: true } } }))).toBe(true);
    expect(isFinishPending(mergeOf({ ...base, result: { after: { choice: "keep", skipped: "x" } } }))).toBe(false);
    expect(isFinishPending(mergeOf({ ...base, result: { after: { choice: "keep", base: "recorded" } } }))).toBe(false);
    expect(isFinishPending(mergeOf({ status: "done", result: { after: { choice: "keep", requested: true } } }))).toBe(false);
  });
});
