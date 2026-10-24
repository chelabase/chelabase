import { describe, expect, it } from "vitest";
import { escapeName } from "./escape-name";

describe("escapeName is injective (MH-38d)", () => {
  it("renders a literal backslash-u escape differently from the real character", () => {
    const literal = "caf\\u00e9"; // c a f \ u 0 0 e 9
    const real = "café";
    expect(escapeName(literal)).not.toBe(escapeName(real));
  });

  it("renders a lone backslash as two backslashes", () => {
    expect(escapeName("\\")).toBe("\\\\");
  });

  it("renders a trailing backslash as two backslashes", () => {
    expect(escapeName("ab\\")).toBe("ab\\\\");
  });

  it("renders one and two backslashes distinctly", () => {
    const outs = new Set(["\\", "\\\\", "\\\\\\"].map(escapeName));
    expect(outs.size).toBe(3);
  });

  it("is injective over every string up to length 6 of a hostile alphabet", () => {
    const alphabet = ["\\", "u", "0", "e", "9", "é", "\u0001", "‮"];
    const seen = new Map<string, string>();
    const walk = (prefix: string, depth: number): void => {
      const out = escapeName(prefix);
      const prior = seen.get(out);
      if (prior !== undefined) expect(prior).toBe(prefix);
      seen.set(out, prefix);
      if (depth === 0) return;
      for (const ch of alphabet) walk(prefix + ch, depth - 1);
    };
    walk("", 6); // length 6 is needed: `é` (6 chars) must not collide with `é` (1 char)
  });
});
