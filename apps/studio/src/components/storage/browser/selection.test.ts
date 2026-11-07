import { describe, expect, it } from "vitest";
import { EMPTY_SELECTION, deleteReport, selectionReducer, type SelectionState } from "./selection";

const names = ["a.png", "b.png", "c.png", "d.png", "e.png"];
const run = (...actions: Parameters<typeof selectionReducer>[1][]): SelectionState => actions.reduce(selectionReducer, EMPTY_SELECTION);
const picked = (s: SelectionState) => [...s.selected].sort();

describe("selectionReducer", () => {
  it("toggling_adds_then_removes", () => {
    expect(picked(run({ type: "toggle", name: "a.png" }))).toEqual(["a.png"]);
    expect(picked(run({ type: "toggle", name: "a.png" }, { type: "toggle", name: "a.png" }))).toEqual([]);
  });

  it("a_range_selects_from_the_anchor_in_list_order", () => {
    const s = run({ type: "toggle", name: "b.png" }, { type: "range", name: "d.png", order: names });
    expect(picked(s)).toEqual(["b.png", "c.png", "d.png"]);
    // Backwards works too.
    expect(picked(run({ type: "toggle", name: "d.png" }, { type: "range", name: "b.png", order: names }))).toEqual(["b.png", "c.png", "d.png"]);
  });

  it("a_range_without_an_anchor_selects_just_the_one", () => {
    expect(picked(run({ type: "range", name: "c.png", order: names }))).toEqual(["c.png"]);
  });

  it("all_and_clear", () => {
    expect(picked(run({ type: "all", names }))).toEqual(names);
    expect(picked(run({ type: "all", names }, { type: "clear" }))).toEqual([]);
  });

  it("keep_drops_names_no_longer_listed", () => {
    const s = run({ type: "all", names }, { type: "keep", names: ["b.png", "z.png"] });
    expect(picked(s)).toEqual(["b.png"]);
  });

  it("unchanged_actions_keep_the_same_state", () => {
    const s = run({ type: "toggle", name: "a.png" });
    expect(selectionReducer(s, { type: "keep", names })).toBe(s);
    expect(selectionReducer(EMPTY_SELECTION, { type: "clear" })).toBe(EMPTY_SELECTION);
  });
});

describe("deleteReport", () => {
  // #104 AC3: three deleted, or one failing and each failure named with its own reason.
  it("all_removed_says_how_many", () => {
    const r = deleteReport([
      { name: "a.png", ok: true },
      { name: "b.png", ok: true },
      { name: "c.png", ok: true },
    ]);
    expect(r.removed).toEqual(["a.png", "b.png", "c.png"]);
    expect(r.failed).toEqual([]);
    expect(r.summary).toBe("Deleted 3 objects.");
  });

  it("one_failure_is_reported_on_its_own_and_the_rest_counted", () => {
    const r = deleteReport([
      { name: "a.png", ok: true },
      { name: "b.png", ok: false, message: "a row still references it" },
      { name: "c.png", ok: true },
    ]);
    expect(r.removed).toEqual(["a.png", "c.png"]);
    expect(r.failed).toEqual([{ name: "b.png", message: "a row still references it" }]);
    expect(r.summary).toBe("Deleted 2 objects; 1 couldn't be deleted.");
  });

  it("singular_wording", () => {
    expect(deleteReport([{ name: "a.png", ok: true }]).summary).toBe("Deleted 1 object.");
    expect(deleteReport([{ name: "a.png", ok: false, message: "no" }]).summary).toBe("Nothing was deleted; 1 couldn't be deleted.");
  });
});
