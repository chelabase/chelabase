import { describe, expect, it } from "vitest";
import { member } from "@/test/fake-realtime";
import { MAX_AVATARS, PRESENCE_COLORS, applyPresenceDiff, avatarStack, drawnMembers, editingNotice, initials, presenceColor, presenceColorIndex } from "./presence";

const K = JSON.stringify(["81c4-2"]);
const at = (id: string, row: string | null, column: string | null, editing = false, over = {}) =>
  member({ id, name: id, state: { row, column, editing }, ...over });

describe("colours (§6.2)", () => {
  it("there_are_eight_distinct_hues", () => {
    expect(PRESENCE_COLORS).toHaveLength(8);
    expect(new Set(PRESENCE_COLORS).size).toBe(8);
  });

  it("the_colour_is_chosen_by_color_seed_modulo_8", () => {
    for (const seed of [0, 1, 7, 8, 13, 4_294_967_295]) {
      expect(presenceColorIndex(seed)).toBe(seed % 8);
      expect(presenceColor(seed)).toBe(PRESENCE_COLORS[seed % 8]);
    }
  });

  it("the_same_seed_always_gets_the_same_colour", () => {
    expect(presenceColor(123_456)).toBe(presenceColor(123_456));
  });
});

describe("applyPresenceDiff", () => {
  it("an_update_is_a_leave_and_a_join_of_the_same_id", () => {
    const a = at("a", null, null, false);
    const b = at("b", null, null, false);
    const a2 = at("a", K, "memo", true);
    const next = applyPresenceDiff([a, b], { joins: [a2], leaves: [a] });
    expect(next).toHaveLength(2);
    expect(next.find((m) => m.id === "a")).toEqual(a2);
    expect(next.find((m) => m.id === "b")).toEqual(b);
  });

  it("leaves_remove_and_joins_add", () => {
    const a = at("a", null, null);
    const c = at("c", null, null);
    expect(applyPresenceDiff([a], { joins: [c], leaves: [a] }).map((m) => m.id)).toEqual(["c"]);
  });

  it("a_leave_for_an_unknown_id_changes_nothing", () => {
    const a = at("a", null, null);
    expect(applyPresenceDiff([a], { joins: [], leaves: [at("zz", null, null)] })).toEqual([a]);
  });
});

describe("drawnMembers: own selection is never drawn", () => {
  it("excludes_the_viewers_own_sub_and_members_without_a_row", () => {
    const me = at("me", K, "memo", false, { sub: "u2" });
    const meOtherTab = at("me2", K, "memo", false, { sub: "u2" });
    const ana = at("ana", K, "memo", false, { sub: "u-ana" });
    const idle = at("idle", null, null, false, { sub: "u-idle" });
    expect(drawnMembers([me, meOtherTab, ana, idle], "u2").map((m) => m.id)).toEqual(["ana"]);
  });
});

describe("avatar stack", () => {
  const many = (n: number) => Array.from({ length: n }, (_, i) => at(`m${i}`, null, null));

  it("at_most_five_then_plus_n", () => {
    expect(MAX_AVATARS).toBe(5);
    expect(avatarStack(many(3))).toMatchObject({ extra: 0 });
    expect(avatarStack(many(3)).shown).toHaveLength(3);
    expect(avatarStack(many(5)).extra).toBe(0);
    const r = avatarStack(many(8));
    expect(r.shown.map((m) => m.id)).toEqual(["m0", "m1", "m2", "m3", "m4"]);
    expect(r.extra).toBe(3);
    expect(avatarStack([])).toEqual({ shown: [], extra: 0 });
  });

  it("initials_from_the_name_else_the_email", () => {
    expect(initials({ name: "Ana Lee", email: "a@x.test" })).toBe("AL");
    expect(initials({ name: "Ana", email: "a@x.test" })).toBe("A");
    expect(initials({ name: null, email: "bo@x.test" })).toBe("B");
    expect(initials({ name: "  ", email: "bo@x.test" })).toBe("B");
    expect(initials({ name: "ana maria de lee", email: null })).toBe("AM");
  });
});

describe("editingNotice (§5.1, §6.2)", () => {
  it("names_the_member_editing_this_cell", () => {
    const ana = at("ana", K, "memo", true, { name: "Ana", sub: "u-ana" });
    expect(editingNotice([ana], "u2", K, "memo")).toBe("Ana is editing this cell");
  });

  it("is_null_for_another_cell_another_row_or_a_member_only_selecting", () => {
    const ana = at("ana", K, "memo", true, { name: "Ana", sub: "u-ana" });
    expect(editingNotice([ana], "u2", K, "status")).toBeNull();
    expect(editingNotice([ana], "u2", JSON.stringify(["other"]), "memo")).toBeNull();
    expect(editingNotice([{ ...ana, state: { ...ana.state, editing: false } }], "u2", K, "memo")).toBeNull();
  });

  it("never_names_the_viewer_themselves", () => {
    const self = at("me", K, "memo", true, { name: "Bo", sub: "u2" });
    expect(editingNotice([self], "u2", K, "memo")).toBeNull();
  });

  it("falls_back_to_the_email_then_someone", () => {
    expect(editingNotice([at("a", K, "memo", true, { name: null, email: "ana@x.test", sub: "s1" })], "u2", K, "memo")).toBe("ana@x.test is editing this cell");
    expect(editingNotice([at("a", K, "memo", true, { name: null, email: null, sub: "s1" })], "u2", K, "memo")).toBe("Someone is editing this cell");
  });

  it("two_editors_are_both_named", () => {
    const a = at("a", K, "memo", true, { name: "Ana", sub: "s1" });
    const b = at("b", K, "memo", true, { name: "Cy", sub: "s2" });
    expect(editingNotice([a, b], "u2", K, "memo")).toBe("Ana and Cy are editing this cell");
  });
});
