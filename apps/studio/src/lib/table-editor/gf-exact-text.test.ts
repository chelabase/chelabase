import { describe, expect, it } from "vitest";
import { buildFilterTree, whereByKey } from "@/lib/console";
import { lookupKey, typedKeyString } from "@/lib/realtime/keys";
import { avatarLabels } from "@/lib/realtime/presence";
import { member } from "@/test/fake-realtime";
import { cellText, editText, toJson, toTsv } from "./rows";

/** RT1 gf, coder's edge tests: numeric values read as their exact text (item 3) and avatar labels (item 6). */

describe("toJson: Copy (JSON) of exact numeric text", () => {
  const numeric = new Set(["price", "n"]);

  it("writes_number_column_texts_as_the_json_numbers_postgres_wrote", () => {
    const out = toJson([{ id: "r1", price: "2.50", n: "12345678901234567890.12" }], numeric);
    expect(out).toBe('[\n  {\n    "id": "r1",\n    "price": 2.50,\n    "n": 12345678901234567890.12\n  }\n]');
  });

  it("keeps_nan_infinities_and_non_number_columns_as_strings", () => {
    const out = JSON.parse(toJson([{ id: "2.50", price: "NaN", n: "-Infinity" }], numeric));
    expect(out).toEqual([{ id: "2.50", price: "NaN", n: "-Infinity" }]);
  });

  it("leaves_numbers_nulls_and_json_values_alone", () => {
    const out = JSON.parse(toJson([{ price: 1.5, n: null, doc: { n: 1.5 } }], numeric));
    expect(out).toEqual([{ price: 1.5, n: null, doc: { n: 1.5 } }]);
  });

  it("an_exponent_text_is_a_json_number_too", () => {
    expect(toJson([{ n: "1e+20" }], numeric)).toContain('"n": 1e+20');
  });
});

describe("exact text in the other paths", () => {
  it("cells_editors_and_tsv_show_the_text", () => {
    expect(cellText("2.50")).toBe("2.50");
    expect(editText("2.50")).toBe("2.50");
    expect(toTsv(["price"], [{ price: "2.50" }])).toBe("price\n2.50");
  });

  it("a_numeric_key_read_as_text_is_matched_by_its_text", () => {
    expect(whereByKey(["k"], { k: "2.50" })).toEqual({ col: "k", eq: "2.50" });
    expect(whereByKey(["k"], { k: "9007199254740993" })).toEqual({ col: "k", eq: "9007199254740993" });
  });

  it("a_numeric_key_text_takes_the_lookup_path_and_keeps_its_text", () => {
    const types = { k: { type: "numeric(12,2)", array: false } };
    expect(typedKeyString(["k"], { k: "2.50" }, types)).toBeNull();
    expect(lookupKey(["k"], { k: "2.50" }, types)).toBe('["2.50"]');
    // an integer key stays a number and keeps its exact key
    expect(typedKeyString(["k"], { k: 42 }, { k: { type: "bigint" } })).toBe('["42"]');
  });

  it("a_filter_on_a_numeric_sends_the_text_when_a_double_would_change_it", () => {
    const columns = { price: { type: "numeric(12,2)", nullable: true, has_default: false, generated: false, array: false, enum: null } };
    expect(buildFilterTree({ mode: "and", rows: [{ col: "price", op: "eq", value: "2.50" }] }, columns)).toEqual({ ok: true, where: { col: "price", eq: "2.50" } });
    expect(buildFilterTree({ mode: "and", rows: [{ col: "price", op: "gt", value: "2.5" }] }, columns)).toEqual({ ok: true, where: { col: "price", gt: 2.5 } });
  });
});

describe("avatarLabels: more edges", () => {
  const m = (id: string, name: string | null, email: string | null = `${id}@x.test`) => member({ id, name, email, sub: `s-${id}` });

  it("members_without_a_name_who_share_an_email_initial_get_two_letters_of_the_email", () => {
    expect(avatarLabels([m("a", null, "rosa@x.test"), m("b", null, "ravi@x.test")])).toEqual(["RO", "RA"]);
  });

  it("a_named_and_an_unnamed_member_with_the_same_initial_are_told_apart", () => {
    expect(avatarLabels([m("a", "Rosa"), m("b", null, "ravi@x.test")])).toEqual(["RO", "RA"]);
  });

  it("members_identical_every_way_keep_a_two_letter_email_form", () => {
    expect(avatarLabels([m("a", "Rosa", "ro@x.test"), m("b", "Rosa", "ro@x.test")])).toEqual(["RO", "RO"]);
  });

  it("letters_beyond_the_basic_plane_are_kept_whole", () => {
    const labels = avatarLabels([m("a", "𝒜na"), m("b", "𝒜bo")]);
    expect(labels).toEqual(["𝒜N", "𝒜B"]);
  });
});
