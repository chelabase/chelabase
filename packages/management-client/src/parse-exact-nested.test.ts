import { describe, expect, it } from "vitest";
import { parseExact } from "./fetch";

/**
 * RT1 gf item 3, coordinator ruling: the exact-literal rule is for column values only (the fields of a row, an object
 * in the top-level array). Numbers nested inside a json/jsonb value stay JSON numbers unless a double loses digits.
 */
const rows = (json: string) => parseExact(json) as Record<string, unknown>[];

describe("parseExact: numbers inside a json/jsonb value", () => {
  it("a_nested_number_with_a_trailing_zero_stays_a_number", () => {
    expect(rows('[{"doc":{"n":1.50,"m":[2.50,{"k":0.10}]}}]')[0]).toEqual({ doc: { n: 1.5, m: [2.5, { k: 0.1 }] } });
  });

  it("a_nested_number_a_double_cannot_hold_comes_back_as_its_text", () => {
    expect(rows('[{"doc":{"big":9007199254740993,"long":0.12345678901234567890}}]')[0]).toEqual({
      doc: { big: "9007199254740993", long: "0.12345678901234567890" },
    });
  });

  it("a_jsonb_cell_that_is_a_bare_number_is_a_column_value_and_keeps_its_text", () => {
    expect(rows('[{"doc":1.50,"i":7}]')[0]).toEqual({ doc: "1.50", i: 7 });
  });

  it("a_column_and_a_nested_value_in_one_row_follow_their_own_rules", () => {
    expect(rows('[{"price":2.50,"doc":{"price":2.50}}]')[0]).toEqual({ price: "2.50", doc: { price: 2.5 } });
  });

  it("a_proto_key_in_a_jsonb_value_stays_a_plain_field", () => {
    const out = rows('[{"doc":{"__proto__":1.50,"x":1}},{"__proto__":2.50}]');
    expect(Object.getPrototypeOf(out[0].doc)).toBe(Object.prototype);
    expect(Object.getOwnPropertyDescriptor(out[0].doc, "__proto__")?.value).toBe(1.5);
    expect(Object.getPrototypeOf(out[1])).toBe(Object.prototype);
    expect(Object.getOwnPropertyDescriptor(out[1], "__proto__")?.value).toBe("2.50");
  });

  it("a_top_level_value_that_is_not_rows_uses_the_nested_rule", () => {
    expect(parseExact('{"count":3,"n":1.50}')).toEqual({ count: 3, n: 1.5 });
    expect(parseExact("[1.50, 9007199254740993]")).toEqual([1.5, "9007199254740993"]);
    expect(parseExact("1.50")).toBe(1.5);
  });

  it("rows_without_any_changed_literal_parse_as_plain_json", () => {
    expect(parseExact('[{"a":1,"b":1.5,"c":"2.50","d":null,"e":[1,2]}]')).toEqual([{ a: 1, b: 1.5, c: "2.50", d: null, e: [1, 2] }]);
  });
});
