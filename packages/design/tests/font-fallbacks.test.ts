import { describe, expect, it } from "vitest";
import { customProperties, readCss } from "./css";

// The web fonts load with `font-display: swap`, so text first paints in a local font. Each stack
// names a metric-matched fallback face right after its web font, so the swap moves nothing.

const stripComments = (s: string) => s.replace(/\/\*[\s\S]*?\*\//g, "");

/** The descriptors of every `@font-face` in tokens.css, by family. */
function fallbackFaces(): Map<string, Array<Map<string, string>>> {
  const out = new Map<string, Array<Map<string, string>>>();
  for (const m of stripComments(readCss("tokens.css")).matchAll(/@font-face\s*\{([^}]*)\}/g)) {
    const d = new Map<string, string>();
    for (const decl of m[1]!.split(";")) {
      const i = decl.indexOf(":");
      if (i > 0) d.set(decl.slice(0, i).trim(), decl.slice(i + 1).trim().replace(/\s+/g, " "));
    }
    const family = d.get("font-family")!.replace(/^"|"$/g, "");
    out.set(family, [...(out.get(family) ?? []), d]);
  }
  return out;
}

const FALLBACKS = {
  "--font-sans": ["Atkinson Hyperlegible Next Variable", "Atkinson Hyperlegible Next Fallback"],
  "--font-serif": ["Spectral", "Spectral Fallback"],
  "--font-mono": ["JetBrains Mono Variable", "JetBrains Mono Fallback"],
} as const;

describe("metric-matched font fallbacks", () => {
  it.each(Object.entries(FALLBACKS))("%s names its fallback right after the web font", (prop, [web, fallback]) => {
    const stack = customProperties(readCss("tailwind.css"))
      .get(prop)!
      .split(",")
      .map((f) => f.trim().replace(/^"|"$/g, ""));
    expect(stack.slice(0, 2)).toEqual([web, fallback]);
  });

  it.each(Object.values(FALLBACKS).map(([, f]) => f))("%s is a local face with every metric override", (family) => {
    const faces = fallbackFaces().get(family) ?? [];
    expect(faces.length).toBeGreaterThan(0);
    for (const face of faces) {
      expect(face.get("src")).toMatch(/^local\(/);
      expect(face.get("src")).not.toMatch(/url\(/);
      for (const d of ["size-adjust", "ascent-override", "descent-override", "line-gap-override"]) {
        expect(face.get(d), `${family} ${d}`).toMatch(/^\d+(\.\d+)?%$/);
      }
    }
  });

  it("serif and sans have a bold fallback, so bold text is not synthesised over the regular face", () => {
    for (const family of ["Spectral Fallback", "Atkinson Hyperlegible Next Fallback"]) {
      const weights = (fallbackFaces().get(family) ?? []).map((f) => f.get("font-weight"));
      expect(weights.some((w) => w && /^[1-5]\d\d /.test(w)), family).toBe(true);
      expect(weights.some((w) => w && / 900$/.test(w)), family).toBe(true);
    }
  });
});
