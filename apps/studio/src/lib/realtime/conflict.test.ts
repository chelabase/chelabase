import { describe, expect, it } from "vitest";
import { CHANGED_TITLE, GONE_TEXT, NO_CHECK_NOTICE, captureOriginals, conflictWhere, isComparable } from "./conflict";

const K = JSON.stringify(["81c4-2"]);

describe("constants", () => {
  it("texts_follow_the_spec", () => {
    expect(NO_CHECK_NOTICE).toBe("No conflict check for this column type");
    expect(GONE_TEXT).toBe("This row was deleted or is hidden from this role.");
    expect(CHANGED_TITLE).toBe("Changed by someone else");
  });
});

describe("isComparable (R18)", () => {
  it.each(["json", "xml", "point", "line", "lseg", "box", "path", "polygon", "circle", "json[]", "point[]"])("%s_is_not_comparable", (type) => {
    expect(isComparable({ type })).toBe(false);
  });

  it.each(["text", "integer", "bigint", "numeric(12,2)", "double precision", "jsonb", "uuid", "bytea", "timestamp with time zone", "boolean", "character varying(20)", "text[]", "integer[]", "public.invoice_status", "citext"])(
    "%s_is_comparable",
    (type) => {
      expect(isComparable({ type })).toBe(true);
    },
  );
});

describe("captureOriginals (W1)", () => {
  const aliases = { amount_cents: "$o0", memo: "$o1" };

  it("reads_the_text_aliases_not_the_typed_values", () => {
    expect(captureOriginals({ id: "x", amount_cents: 45000, memo: "Q3 plan", $o0: "45000", $o1: "Q3 plan" }, aliases)).toEqual({ amount_cents: "45000", memo: "Q3 plan" });
  });

  it("a_null_text_is_null", () => {
    expect(captureOriginals({ $o0: "1", $o1: null }, aliases)).toEqual({ amount_cents: "1", memo: null });
  });

  it("a_column_whose_alias_the_row_lacks_is_absent_not_null", () => {
    const got = captureOriginals({ $o0: "1" }, aliases);
    expect(got).toEqual({ amount_cents: "1" });
    expect("memo" in got).toBe(false);
  });

  it("is_a_snapshot_a_later_row_change_does_not_alter", () => {
    const row: Record<string, unknown> = { $o0: "1" };
    const snap = captureOriginals(row, aliases);
    row.$o0 = "2";
    expect(snap.amount_cents).toBe("1");
  });
});

describe("conflictWhere (§6.3, R18)", () => {
  it("pk_only_when_nothing_is_edited", () => {
    expect(conflictWhere({ pk: ["id"], key: K, edited: [], originals: {} })).toEqual({ where: { col: "id", eq: "81c4-2", parse: true }, unchecked: [] });
  });

  it("adds_each_edited_columns_original_as_text_eq", () => {
    const r = conflictWhere({ pk: ["id"], key: K, edited: ["amount_cents", "status"], originals: { amount_cents: "45000", status: "open", memo: "x" } });
    expect(r.where).toEqual({
      and: [{ col: "id", eq: "81c4-2", parse: true }, { col: "amount_cents", eq: "45000", text: true }, { col: "status", eq: "open", text: true }],
    });
    expect(r.unchecked).toEqual([]);
  });

  it("a_null_original_compares_with_is_null", () => {
    const r = conflictWhere({ pk: ["id"], key: K, edited: ["memo"], originals: { memo: null } });
    expect(r.where).toEqual({ and: [{ col: "id", eq: "81c4-2", parse: true }, { col: "memo", is: null }] });
  });

  it("an_edited_column_without_an_original_is_left_out_and_reported", () => {
    const r = conflictWhere({ pk: ["id"], key: K, edited: ["meta", "memo"], originals: { memo: "m" } });
    expect(r.unchecked).toEqual(["meta"]);
    expect(r.where).toEqual({ and: [{ col: "id", eq: "81c4-2", parse: true }, { col: "memo", eq: "m", text: true }] });
  });

  it("an_only_unchecked_edit_still_keys_by_pk_alone", () => {
    const r = conflictWhere({ pk: ["id"], key: K, edited: ["meta"], originals: {} });
    expect(r.where).toEqual({ col: "id", eq: "81c4-2", parse: true });
    expect(r.unchecked).toEqual(["meta"]);
  });

  it("a_composite_key_comes_first_in_pk_order", () => {
    const r = conflictWhere({ pk: ["a", "b"], key: JSON.stringify(["1", "x"]), edited: ["memo"], originals: { memo: "m" } });
    expect(r.where).toEqual({ and: [{ col: "a", eq: "1", parse: true }, { col: "b", eq: "x", parse: true }, { col: "memo", eq: "m", text: true }] });
  });

  it("an_original_is_never_sent_as_a_typed_number", () => {
    const r = conflictWhere({ pk: ["id"], key: K, edited: ["amount_cents"], originals: { amount_cents: "9007199254740993" } });
    expect(JSON.stringify(r.where)).toContain('"eq":"9007199254740993"');
  });
});
