import { describe, expect, it } from "vitest";
import type { Branch, BranchDatabaseView, ObjectSummary, Project } from "@chelabase/management-client";
import { BRANCHES, OBJECTS, PG_DATABASES, PROJECTS, pgDb } from "@/test/table-editor-fixtures";
import { formatCount } from "@/components/table-editor/table-list";
import { resolveParams, type EditorParams } from "./context";
import { DRAFTS_KEY, createTableEditorStore } from "./store";

// Final fix round (FB), model-level findings.
const projects = PROJECTS as unknown as Project[];
const branches = BRANCHES as unknown as Branch[];
const pgDatabases = [...PG_DATABASES, pgDb("analytics", false)] as unknown as BranchDatabaseView[];
const objects = OBJECTS as unknown as ObjectSummary[];
const all = { projects, branches, pgDatabases, objects };

describe("T2 I1: the rail reopens the whole last context", () => {
  const last: EditorParams = { projectId: "p1", databaseId: "d2", branchId: "b2", pgDatabase: "reporting", schema: "billing", table: "ledger" };

  it("each_level_takes_the_last_value_while_the_levels_above_match_it", () => {
    expect(resolveParams({}, last, { projects })).toEqual({ kind: "redirect", params: { projectId: "p1", databaseId: "d2" } });
    expect(resolveParams({ projectId: "p1", databaseId: "d2" }, last, { projects, branches })).toEqual({ kind: "redirect", params: { projectId: "p1", databaseId: "d2", branchId: "b2" } });
    expect(resolveParams({ projectId: "p1", databaseId: "d2", branchId: "b2" }, last, all)).toEqual({
      kind: "redirect", params: { projectId: "p1", databaseId: "d2", branchId: "b2", pgDatabase: "reporting", schema: "billing" },
    });
  });

  it("a_stale_level_falls_back_to_the_default_and_so_do_the_levels_below", () => {
    const stale = { ...last, branchId: "gone" };
    expect(resolveParams({ projectId: "p1", databaseId: "d2" }, stale, { projects, branches })).toEqual({ kind: "redirect", params: { projectId: "p1", databaseId: "d2", branchId: "b1" } });
    expect(resolveParams({ projectId: "p1", databaseId: "d2", branchId: "b1" }, stale, all)).toEqual({
      kind: "redirect", params: { projectId: "p1", databaseId: "d2", branchId: "b1", pgDatabase: "app", schema: "public" },
    });
  });

  it("a_different_place_above_ignores_the_last_values_below", () => {
    expect(resolveParams({ projectId: "p1", databaseId: "d1" }, last, { projects, branches })).toEqual({ kind: "redirect", params: { projectId: "p1", databaseId: "d1", branchId: "b1" } });
    expect(resolveParams({ projectId: "p2" }, last, { projects })).toEqual({ kind: "redirect", params: { projectId: "p2", databaseId: "d9" } });
  });

  it("an_unknown_last_schema_falls_back_to_public", () => {
    expect(resolveParams({ projectId: "p1", databaseId: "d2", branchId: "b2" }, { ...last, schema: "nope" }, all)).toEqual({
      kind: "redirect", params: { projectId: "p1", databaseId: "d2", branchId: "b2", pgDatabase: "reporting", schema: "public" },
    });
  });
});

describe("T2 M4: drafts from sessionStorage are validated edit by edit", () => {
  it("keeps_only_well_formed_edits_and_drops_empty_changesets", () => {
    const op = { alter_column: { table: { schema: "public", name: "invoices" }, column: "memo", change: { not_null: true } } };
    const good = { id: "e1", ops: [op], label: "Set memo not null" };
    sessionStorage.clear();
    sessionStorage.setItem(DRAFTS_KEY, JSON.stringify({
      a: { key: "a", base: "fp", edits: [good, { id: "e2" }, { id: 3, ops: [op], label: "x" }, { id: "e4", ops: "no", label: "x" }] },
      b: { key: "b", base: "fp", edits: [{ ops: [op] }, null] },
      c: { key: "c", base: null, edits: [good] },
    }));
    const s = createTableEditorStore(sessionStorage).getState();
    expect(Object.keys(s.changesets).sort()).toEqual(["a", "c"]);
    expect(s.changesets.a.edits).toEqual([good]);
  });
});

describe("T2 M8: compact counts", () => {
  it("rounds_up_into_the_next_unit", () => {
    expect(formatCount(999)).toBe("999");
    expect(formatCount(999_500)).toBe("1M");
    expect(formatCount(999_949)).toBe("1M");
    expect(formatCount(99_960)).toBe("100k");
    expect(formatCount(48_210)).toBe("48.2k");
  });
});
