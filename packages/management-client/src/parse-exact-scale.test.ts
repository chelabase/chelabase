import { describe, expect, it } from "vitest";
import { parseExact } from "./fetch";

/**
 * RT1 gf, item 3 (user, 2026-10-21): Studio shows a numeric as Postgres's own text. The console route already writes
 * Postgres's literal (`2.50`, a 20-digit value in full, NaN and the infinities as quoted strings); the browser's parse
 * is where `2.50` became `2.5`. Rule for `parseExact` (used by `consoleQueryExact`, so Studio only; the data API's JSON
 * is not touched): a number comes back as a JS number only when `String(number)` is exactly its source text; any other
 * source text (a kept trailing zero, an exponent form, digits a double cannot hold, `-0`) comes back as that source text.
 */
const one = (json: string) => (parseExact(`[{"v":${json}}]`) as { v: unknown }[])[0].v;

describe("parseExact keeps the literal Postgres wrote", () => {
  it.each([
    ["2.50", "2.50"],
    ["5.00", "5.00"],
    ["0.10", "0.10"],
    ["-3.40", "-3.40"],
    ["100.0", "100.0"],
    ["0.00", "0.00"],
    ["12345678901234567890.12", "12345678901234567890.12"],
    ["1e+20", "1e+20"],
    ["-0", "-0"],
  ])("%s_comes_back_as_its_source_text", (source, expected) => {
    expect(one(source)).toBe(expected);
  });

  it.each([
    ["42", 42],
    ["-7", -7],
    ["1.5", 1.5],
    ["0.1", 0.1],
    ["0", 0],
    ["123456.789", 123456.789],
    ["9007199254740991", 9007199254740991],
  ])("%s_that_a_double_prints_identically_stays_a_number", (source, expected) => {
    expect(one(source)).toBe(expected);
  });

  it("the_big_integer_rule_is_unchanged", () => {
    expect(one("9007199254740993")).toBe("9007199254740993");
  });

  it("quoted_nan_and_infinities_pass_through_as_the_strings_postgres_prints", () => {
    const rows = parseExact('[{"a":"NaN","b":"Infinity","c":"-Infinity","d":null}]') as Record<string, unknown>[];
    expect(rows[0]).toEqual({ a: "NaN", b: "Infinity", c: "-Infinity", d: null });
  });
});
