import { describe, expect, it } from "vitest";
import type { Branch, BranchDatabaseView, ObjectSummary, Project } from "@chelabase/management-client";
import { BRANCHES, OBJECTS, PG_DATABASES, PROJECTS, branch, obj } from "@/test/table-editor-fixtures";
import { LAST_KEY, branchTree, editorPath, readLast, resolveParams, schemasOf, writeLast } from "./context";

const projects = PROJECTS as unknown as Project[];
const branches = BRANCHES as unknown as Branch[];
const pgDatabases = PG_DATABASES as unknown as BranchDatabaseView[];
const objects = OBJECTS as unknown as ObjectSummary[];
const all = { projects, branches, pgDatabases, objects };
const throwing = { getItem: () => { throw new Error("no"); }, setItem: () => { throw new Error("no"); } } as unknown as Storage;

describe("editor context", () => {
  it("editor_path_stops_at_the_first_missing_level", () => {
    expect(editorPath({})).toBe("/app/tables");
    expect(editorPath({ projectId: "p1", databaseId: "d1", table: "x" })).toBe("/app/tables/p1/d1");
    expect(editorPath({ projectId: "p1", databaseId: "d1", branchId: "b1", pgDatabase: "my db", schema: "public", table: "a/b" })).toBe("/app/tables/p1/d1/b1/my%20db/public/a%2Fb");
  });
  it("last_context_round_trips_and_survives_broken_storage", () => {
    localStorage.clear();
    writeLast(localStorage, { projectId: "p2", databaseId: "d9" });
    expect(JSON.parse(localStorage.getItem(LAST_KEY)!)).toEqual({ projectId: "p2", databaseId: "d9" });
    expect(readLast(localStorage)).toEqual({ projectId: "p2", databaseId: "d9" });
    localStorage.setItem(LAST_KEY, "{nope");
    expect(readLast(localStorage)).toBeNull();
    expect(readLast(throwing)).toBeNull();
    expect(() => writeLast(throwing, {})).not.toThrow();
    expect(readLast(null)).toBeNull();
  });
  it("fills_levels_in_order_while_data_allows", () => {
    expect(resolveParams({}, null, {})).toEqual({ kind: "loading" });
    expect(resolveParams({}, null, { projects })).toEqual({ kind: "redirect", params: { projectId: "p1", databaseId: "d1" } });
    expect(resolveParams({}, { projectId: "p2" }, { projects })).toEqual({ kind: "redirect", params: { projectId: "p2", databaseId: "d9" } });
    expect(resolveParams({}, { projectId: "gone" }, { projects })).toEqual({ kind: "redirect", params: { projectId: "p1", databaseId: "d1" } });
    expect(resolveParams({ projectId: "p1", databaseId: "d1" }, null, { projects, branches })).toEqual({ kind: "redirect", params: { projectId: "p1", databaseId: "d1", branchId: "b1" } });
    expect(resolveParams({ projectId: "p1", databaseId: "d1", branchId: "b1" }, null, all)).toEqual({ kind: "redirect", params: { projectId: "p1", databaseId: "d1", branchId: "b1", pgDatabase: "app", schema: "public" } });
  });
  it("schema_falls_back_to_the_first", () => {
    const only = [obj("billing", "ledger"), obj("audit", "log")] as unknown as ObjectSummary[];
    expect(resolveParams({ projectId: "p1", databaseId: "d1", branchId: "b1", pgDatabase: "app" }, null, { ...all, objects: only })).toEqual({
      kind: "redirect", params: { projectId: "p1", databaseId: "d1", branchId: "b1", pgDatabase: "app", schema: "audit" },
    });
    expect(schemasOf(objects)).toEqual(["billing", "public"]);
  });
  it("ready_and_not_found", () => {
    const base = { projectId: "p1", databaseId: "d1", branchId: "b1", pgDatabase: "app", schema: "public" };
    expect(resolveParams(base, null, all)).toEqual({ kind: "ready", params: base });
    expect(resolveParams({ ...base, table: "invoices" }, null, all)).toEqual({ kind: "ready", params: { ...base, table: "invoices" } });
    expect(resolveParams({ ...base, table: "nope" }, null, all)).toEqual({ kind: "not_found", level: "table" });
    expect(resolveParams({ ...base, branchId: "zz" }, null, all)).toEqual({ kind: "not_found", level: "branch" });
    expect(resolveParams({ projectId: "zz" }, null, all)).toEqual({ kind: "not_found", level: "project" });
    expect(resolveParams({ ...base, pgDatabase: "zz" }, null, all)).toEqual({ kind: "not_found", level: "pgDatabase" });
  });
  it("branch_tree_keeps_ready_normal_branches_depth_first", () => {
    const extra = [...branches, branch("b6", "alpha", "b1") as unknown as Branch];
    expect(branchTree(extra).map((r) => [r.branch.name, r.depth])).toEqual([["main", 0], ["alpha", 1], ["staging", 1], ["feat-discounts", 2]]);
  });
});
