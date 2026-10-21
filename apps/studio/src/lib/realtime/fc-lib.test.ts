import { describe, expect, it } from "vitest";
import { conflictWhere } from "./conflict";
import { keyConditions, whereByTextKey, whereByTextKeys } from "./keys";

// RT1 final round, batch C: items 3 (parse form) and 8 (a primary-key column isn't "unchecked").

const K = JSON.stringify(["2026-10-16 12:00:00+00"]);

describe("by-key conditions use the server's parse form (review I3)", () => {
  it("one_condition_per_pk_column_carries_parse_and_never_text", () => {
    for (const c of keyConditions(["a", "b"], JSON.stringify(["1", "x"]))) {
      expect(c).toHaveProperty("parse", true);
      expect(c).not.toHaveProperty("text");
    }
  });

  it("a_timestamp_key_text_is_sent_as_the_text_it_is_to_be_parsed", () => {
    expect(whereByTextKey(["created_at"], K)).toEqual({ col: "created_at", eq: "2026-10-16 12:00:00+00", parse: true });
  });

  it("a_page_of_keys_is_one_in_list_with_parse", () => {
    const w = whereByTextKeys(["id"], [JSON.stringify(["1"]), JSON.stringify(["2"])]);
    expect(w).toEqual({ col: "id", in: ["1", "2"], parse: true });
    expect(w).not.toHaveProperty("text");
  });

  it("a_composite_page_is_an_or_of_ands_all_with_parse", () => {
    const w = whereByTextKeys(["a", "b"], [JSON.stringify(["1", "x"]), JSON.stringify(["2", "y"])]) as { or: { and: Record<string, unknown>[] }[] };
    for (const part of w.or) for (const c of part.and) expect(c).toMatchObject({ parse: true });
    expect(JSON.stringify(w)).not.toContain('"text"');
  });

  it("the_update_where_keeps_text_equality_for_originals_and_parse_for_the_key_only", () => {
    const r = conflictWhere({ pk: ["id"], key: JSON.stringify(["81c4-2"]), edited: ["amount_cents", "memo"], originals: { amount_cents: "45000", memo: null } });
    expect(r.where).toEqual({
      and: [{ col: "id", eq: "81c4-2", parse: true }, { col: "amount_cents", eq: "45000", text: true }, { col: "memo", is: null }],
    });
  });
});

describe("editing a primary-key column (review minor)", () => {
  const key = JSON.stringify(["1", "x"]);

  it("a_pk_column_is_never_listed_as_unchecked", () => {
    const r = conflictWhere({ pk: ["a", "b"], key, edited: ["a"], originals: {} });
    expect(r.unchecked).toEqual([]);
    expect(r.where).toEqual({ and: [{ col: "a", eq: "1", parse: true }, { col: "b", eq: "x", parse: true }] });
  });

  it("only_the_non_pk_columns_without_an_original_are_unchecked", () => {
    const r = conflictWhere({ pk: ["a", "b"], key, edited: ["b", "doc", "memo"], originals: { memo: "m" } });
    expect(r.unchecked).toEqual(["doc"]);
    expect(r.where).toEqual({ and: [{ col: "a", eq: "1", parse: true }, { col: "b", eq: "x", parse: true }, { col: "memo", eq: "m", text: true }] });
  });
});
