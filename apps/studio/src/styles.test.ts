import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";

const css = readFileSync(resolve(__dirname, "styles.css"), "utf8");

describe("styles.css", () => {
  it("makes the dark: variant always on (Studio is always dark, not OS-driven)", () => {
    // Tailwind 4's default dark variant is prefers-color-scheme; this must override it.
    expect(css).toMatch(/@custom-variant\s+dark\s*\(\s*&:where\(:root,\s*:root \*\)\s*\)\s*;/);
  });
});
