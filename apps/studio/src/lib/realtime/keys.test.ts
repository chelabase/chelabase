import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";
import { keyString, originalAliases, pkAliases, textKeyOf, textSelectItems, whereByTextKey, whereByTextKeys } from "./keys";

// W4/S11: the server builds key strings with serde_json over the PK texts; Studio's must be byte-identical.
const FIXTURE = resolve(import.meta.dirname, "../../../../../crates/chelabase-query/tests/fixtures/text_keys.json");
const cases = JSON.parse(readFileSync(FIXTURE, "utf8")) as { texts: string[]; key: string }[];

describe("key strings (S11)", () => {
  it("fixture_has_cases", () => {
    expect(cases.length).toBeGreaterThan(10);
  });

  it("keyString_matches_every_shared_fixture_case_byte_for_byte", () => {
    for (const c of cases) expect(keyString(c.texts)).toBe(c.key);
  });

  it("keyString_has_no_spaces_after_commas", () => {
    expect(keyString(["1", "x"])).toBe('["1","x"]');
  });

  it("textKeyOf_reads_the_pk_aliases_in_pk_order", () => {
    expect(textKeyOf({ id: 1, $pk0: "7", $pk1: "x" }, ["$pk0", "$pk1"])).toBe('["7","x"]');
  });

  it("textKeyOf_is_null_when_an_alias_is_missing_or_null", () => {
    expect(textKeyOf({ id: 1 }, ["$pk0"])).toBeNull();
    expect(textKeyOf({ $pk0: null }, ["$pk0"])).toBeNull();
    expect(textKeyOf({ $pk0: "1" }, [])).toBeNull();
  });

  it("an_unsafe_number_pk_keeps_every_digit", () => {
    // The text form never goes through a JS number (W4).
    expect(textKeyOf({ id: 9007199254740992, $pk0: "9007199254740993" }, ["$pk0"])).toBe('["9007199254740993"]');
  });
});

describe("aliases and the select list (S9)", () => {
  it("pk_aliases_are_dollar_pk_n", () => {
    expect(pkAliases(["a", "b"], ["a", "b", "c"])).toEqual(["$pk0", "$pk1"]);
  });

  it("a_clash_with_a_real_column_renames_the_alias", () => {
    const aliases = pkAliases(["id", "org"], ["id", "org", "$pk0"]);
    expect(aliases[0]).not.toBe("$pk0");
    expect(new Set(aliases).size).toBe(2);
    expect(["id", "org", "$pk0"]).not.toContain(aliases[0]);
    expect(["id", "org", "$pk0"]).not.toContain(aliases[1]);
  });

  it("original_aliases_are_dollar_o_n_by_position", () => {
    expect(originalAliases(["amount", "memo"], ["id", "amount", "memo"])).toEqual({ amount: "$o0", memo: "$o1" });
  });

  it("original_aliases_avoid_taken_names", () => {
    const a = originalAliases(["amount"], ["$o0", "amount"]);
    expect(a.amount).not.toBe("$o0");
    expect(a.amount.startsWith("$o")).toBe(true);
  });

  it("the_select_lists_every_column_then_text_of_items", () => {
    const { items, pkAliases: pa, originalAliases: oa } = textSelectItems({ columns: ["id", "amount", "meta"], pk: ["id"], originals: ["amount"] });
    expect(pa).toEqual(["$pk0"]);
    expect(oa).toEqual({ amount: "$o0" });
    expect(items).toEqual(["id", "amount", "meta", { text_of: "id", as: "$pk0" }, { text_of: "amount", as: "$o0" }]);
  });

  it("a_composite_key_gets_one_text_of_per_pk_column_in_pk_order", () => {
    const { items } = textSelectItems({ columns: ["b", "a"], pk: ["a", "b"], originals: [] });
    expect(items).toEqual(["b", "a", { text_of: "a", as: "$pk0" }, { text_of: "b", as: "$pk1" }]);
  });
});

describe("by-key WHERE (S32)", () => {
  it("one_pk_column_is_a_text_eq", () => {
    expect(whereByTextKey(["id"], '["81c4-2"]')).toEqual({ col: "id", eq: "81c4-2", parse: true });
  });

  it("a_composite_key_is_an_and_in_pk_order", () => {
    expect(whereByTextKey(["a", "b"], '["1","x"]')).toEqual({ and: [{ col: "a", eq: "1", parse: true }, { col: "b", eq: "x", parse: true }] });
  });

  it("an_unsafe_number_is_matched_as_text_not_refused", () => {
    expect(whereByTextKey(["id"], '["9007199254740993"]')).toEqual({ col: "id", eq: "9007199254740993", parse: true });
  });

  it("every_fixture_case_round_trips_through_the_where", () => {
    for (const c of cases) {
      const pk = c.texts.map((_, i) => `c${i}`);
      const w = whereByTextKey(pk, c.key) as { col?: string; eq?: string; and?: { eq: string }[] };
      const got = pk.length === 1 ? [w.eq] : w.and!.map((p) => p.eq);
      expect(got).toEqual(c.texts);
    }
  });

  it("refuses_a_key_with_the_wrong_arity", () => {
    // The message names the key (the stub's "not implemented" does not match).
    expect(() => whereByTextKey(["a", "b"], '["1"]')).toThrow(/key/i);
    expect(() => whereByTextKey([], "[]")).toThrow(/key/i);
    expect(() => whereByTextKey(["id"], "not json")).toThrow(/key/i);
  });

  it("a_page_of_keys_for_one_pk_column_is_in", () => {
    expect(whereByTextKeys(["id"], ['["1"]', '["2"]'])).toEqual({ col: "id", in: ["1", "2"], parse: true });
  });

  it("a_page_of_composite_keys_is_an_or_of_ands", () => {
    expect(whereByTextKeys(["a", "b"], ['["1","x"]', '["2","y"]'])).toEqual({
      or: [
        { and: [{ col: "a", eq: "1", parse: true }, { col: "b", eq: "x", parse: true }] },
        { and: [{ col: "a", eq: "2", parse: true }, { col: "b", eq: "y", parse: true }] },
      ],
    });
  });

  it("no_keys_is_refused_never_an_unfiltered_where", () => {
    expect(() => whereByTextKeys(["id"], [])).toThrow(/key/i);
  });
});
