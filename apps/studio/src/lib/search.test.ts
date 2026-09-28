import { describe, expect, it } from "vitest";
import { parseSearch, stringifySearch } from "./search";

describe("search params", () => {
  // Zitadel user ids and codes can be all digits; JSON-parsing them turns them into
  // (imprecise) numbers and the reset/verify pages ignore them.
  it("keeps every value a string, digits included", () => {
    expect(parseSearch("?userId=392719759931605005&code=123456")).toEqual({
      userId: "392719759931605005",
      code: "123456",
    });
  });

  it("round-trips through stringify", () => {
    // Empty values are left out, so routes with optional params don't show `?code=`.
    expect(stringifySearch({ authRequest: "V2_1", empty: "" })).toBe("?authRequest=V2_1");
    expect(stringifySearch({})).toBe("");
    expect(parseSearch(stringifySearch({ a: "x y&z" }))).toEqual({ a: "x y&z" });
  });
});
