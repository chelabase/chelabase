import { describe, expect, it } from "vitest";
import { initials } from "./members";

/** Fix S (finish review, fix 5): one shared `initials()` for the roster square and the member page. */
describe("initials", () => {
  it("takes the first letters of the first and last word, upper-cased", () => {
    expect(initials("Dev One")).toBe("DO");
    expect(initials("Ana Maria Lopez")).toBe("AL");
    expect(initials("  sofia   marchetti ")).toBe("SM");
  });

  it("keeps a whole letter for a non-BMP first letter and never splits a surrogate pair", () => {
    expect(initials("𝒜lice Bob")).toBe("𝒜B");
  });

  it("is one letter for a one-word name", () => {
    expect(initials("ana")).toBe("A");
  });

  it("never throws or returns a lone surrogate for odd names", () => {
    for (const name of ["", " ", "-", "<b>", "Ünal Çelik", "😀"]) {
      const out = initials(name);
      expect(typeof out).toBe("string");
      expect(out).toBe(out.normalize());
      expect(Array.from(out).length).toBeLessThanOrEqual(2);
    }
  });
});
