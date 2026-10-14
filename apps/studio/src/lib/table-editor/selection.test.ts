import { describe, expect, it } from "vitest";
import { NO_SELECTION, reduceSelection, rowKey, viewKey, type SelectionState } from "./selection";

const run = (...actions: Parameters<typeof reduceSelection>[1][]): SelectionState => actions.reduce(reduceSelection, NO_SELECTION);

describe("selection", () => {
  it("row_key_is_the_pk_tuple", () => {
    expect(rowKey(["id"], { id: 7, x: 1 })).toBe("[7]");
    expect(rowKey(["a", "b"], { a: "x", b: null })).toBe('["x",null]');
  });
  it("view_key_changes_with_filter_and_sort", () => {
    expect(viewKey({ changeset: "k", filter: null, sort: null })).not.toBe(viewKey({ changeset: "k", filter: { col: "a", eq: 1 }, sort: null }));
  });
  it("toggle_page_all_matching_clear", () => {
    expect(run({ type: "view", view: "v" }, { type: "toggle", key: "[1]" }, { type: "toggle", key: "[2]" }, { type: "toggle", key: "[1]" }).selection).toEqual({ kind: "rows", keys: ["[2]"] });
    expect(run({ type: "page", keys: ["[1]", "[2]"] }).selection).toEqual({ kind: "rows", keys: ["[1]", "[2]"] });
    expect(run({ type: "page", keys: ["[1]", "[2]"] }, { type: "page", keys: ["[1]", "[2]"] }).selection).toEqual({ kind: "none" });
    expect(run({ type: "page", keys: ["[1]"] }, { type: "all_matching", estimate: 48210 }).selection).toEqual({ kind: "all_matching", estimate: 48210 });
    expect(run({ type: "all_matching", estimate: 5 }, { type: "toggle", key: "[1]" }).selection).toEqual({ kind: "none" });
    expect(run({ type: "toggle", key: "[1]" }, { type: "toggle", key: "[1]" }).selection).toEqual({ kind: "none" });
    expect(run({ type: "page", keys: ["[1]"] }, { type: "clear" }).selection).toEqual({ kind: "none" });
  });
  it("a_new_view_resets", () => {
    const s = run({ type: "view", view: "v1" }, { type: "toggle", key: "[1]" });
    expect(reduceSelection(s, { type: "view", view: "v1" }).selection).toEqual({ kind: "rows", keys: ["[1]"] });
    expect(reduceSelection(s, { type: "view", view: "v2" })).toEqual({ view: "v2", selection: { kind: "none" } });
  });
});
