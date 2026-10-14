import { describe, expect, it } from "vitest";
import { ApiError, type SchemaChangeResponse } from "@chelabase/management-client";
import { newEdit } from "./changeset";
import { formatBytes, lockText, mapChangeError, mapDryRun, skipText } from "./dry-run";

const T = { schema: "public", name: "invoices" };
const edits = [
  newEdit([{ add_column: { table: T, column: { name: "d", data_type: { schema: "pg_catalog", name: "int8" } } } }, { add_constraint: { table: T, constraint: { kind: { unique: { columns: ["d"] } } } } }]),
  newEdit([{ alter_column: { table: T, column: "memo", change: { data_type: { schema: "pg_catalog", name: "int8" } } } }]),
];
const stmt = (op_index: number, over: Record<string, unknown> = {}) => ({ sql: `SQL ${op_index}`, op_index, executed: true, lock: "access_exclusive", notices: [], rewrites_table: false, skipped: null, ...over });

describe("dry run mapping", () => {
  it("statements_map_to_edits_and_all_ok_is_ready", () => {
    const res = { dry_run: true, committed: false, would_succeed: true, statements: [stmt(0), stmt(1), stmt(2)] } as SchemaChangeResponse;
    const v = mapDryRun(edits, res, { table: "public.invoices", totalBytes: 12582912 });
    expect(v.statements.map((s) => [s.sql, s.edit])).toEqual([["SQL 0", 0], ["SQL 1", 0], ["SQL 2", 1]]);
    expect(v.perEdit).toEqual([{ kind: "ok" }, { kind: "ok" }]);
    expect(v.apply).toBe("ready");
    expect(v.warnings).toEqual([]);
  });
  it("rewrites_and_rendered_only_need_an_acknowledgement", () => {
    const res = { dry_run: true, committed: false, would_succeed: null, statements: [stmt(0), stmt(1), stmt(2, { executed: false, rewrites_table: true, skipped: "large_table_rewrite" })] } as SchemaChangeResponse;
    const v = mapDryRun(edits, res, { table: "public.invoices", totalBytes: 12582912 });
    expect(v.perEdit[1]).toEqual({ kind: "rendered_only", reason: "large_table_rewrite" });
    expect(v.warnings).toEqual(["Rewrites public.invoices (12 MB): reads and writes wait while it runs"]);
    expect(v.apply).toBe("needs_ack");
  });
  it("rewrite_warning_without_a_size", () => {
    const res = { dry_run: true, committed: false, would_succeed: true, statements: [stmt(2, { rewrites_table: true }), stmt(2, { rewrites_table: true })] } as SchemaChangeResponse;
    expect(mapDryRun(edits, res, { table: "public.invoices", totalBytes: null }).warnings).toEqual(["Rewrites public.invoices: reads and writes wait while it runs"]);
  });
  it("dependents_pass_through", () => {
    const res = { dry_run: true, committed: false, would_succeed: true, statements: [], dependents: { hash: "h1", objects: [{ kind: "view", name: "v_open", schema: "public", description: "view public.v_open" }] } } as SchemaChangeResponse;
    expect(mapDryRun(edits, res, { table: "t", totalBytes: null }).dependents?.hash).toBe("h1");
  });
  it("errors_land_on_their_edit", () => {
    const f = mapChangeError(edits, new ApiError(400, "schema_change_failed", "column \"memo\" cannot be cast", { op_index: 2, sqlstate: "42804", detail: "d", hint: "h" }));
    expect(f).toEqual({ edit: 1, status: { kind: "error", message: "column \"memo\" cannot be cast", detail: "d", hint: "h" }, code: "schema_change_failed", retryable: false, dependents: null });
    expect(mapChangeError(edits, new ApiError(409, "lock_timeout", "lock timeout", { op_index: 0 })).retryable).toBe(true);
    const moved = mapChangeError(edits, new ApiError(409, "dependents_changed", "changed", { dependents: { hash: "h2", objects: [] } }));
    expect(moved).toMatchObject({ edit: null, dependents: { hash: "h2" } });
    expect(mapChangeError(edits, new ApiError(403, "branch_protected", "branch is protected")).edit).toBeNull();
  });
  it("texts", () => {
    expect(lockText("access_exclusive")).toBe("Blocks reads and writes on the table while it runs");
    expect(lockText("share_row_exclusive")).toBe("Blocks writes while it runs");
    expect(lockText("share")).toBe("Blocks writes while it runs");
    expect(lockText("share_update_exclusive")).toBe("Allows reads and writes; blocks other schema changes");
    expect(lockText("none")).toBe("No table lock");
    expect(skipText("large_table_rewrite")).toBe("Not simulated: rewrites a large table");
    expect(skipText("concurrently")).toBe("Not simulated: runs concurrently");
    expect(skipText("after_skipped")).toBe("Not simulated: follows a statement that wasn't simulated");
    expect(skipText("already_dropped")).toBe("Not simulated: its object is dropped earlier in this change");
    expect([formatBytes(500), formatBytes(8192), formatBytes(12582912), formatBytes(1610612736)]).toEqual(["500 B", "8 kB", "12 MB", "1.5 GB"]);
  });
});
