import { describe, expect, test } from "vitest";
import { duplicateCustomProperties, readCss } from "./css.ts";

describe("duplicate custom properties", () => {
  test("helper_catches_a_duplicate_in_one_block", () => {
    const css = ":root { --ink: #000; --deep: #111; --ink: #222; } .x { --ink: #333; }";
    expect(duplicateCustomProperties(css)).toEqual([":root --ink"]);
  });

  test("helper_ignores_the_same_name_in_different_blocks", () => {
    expect(duplicateCustomProperties(":root { --a: 1; } .x { --a: 2; }")).toEqual([]);
  });

  test("no_custom_property_is_declared_twice_in_a_block", () => {
    for (const file of ["tokens.css", "base.css", "tailwind.css", "scalar.css", "fumadocs.css"]) {
      expect(duplicateCustomProperties(readCss(file)), file).toEqual([]);
    }
  });
});
