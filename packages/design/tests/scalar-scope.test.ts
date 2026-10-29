import { describe, expect, test } from "vitest";
import { readCss, selectors } from "./css.ts";

const css = readCss("scalar.css");

describe("scalar skin", () => {
  test("every_rule_is_scoped", () => {
    const parts = selectors(css);
    expect(parts.length).toBeGreaterThan(0);
    for (const p of parts) {
      expect(p.startsWith(".design-scalar") || p === "body > .scalar-app", p).toBe(true);
    }
  });

  test("popovers_are_themed", () => {
    expect(selectors(css)).toContain("body > .scalar-app");
  });

  test("maps_onto_design_tokens", () => {
    expect(css).toContain("--scalar-background-1: var(--ink)");
    expect(css).toContain("--scalar-color-accent: var(--sand)");
    expect(css).toContain("--scalar-color-red: var(--coral)");
    expect(css).toContain("--scalar-font: var(--font-sans)");
    expect(css).toContain("--scalar-color-blue: var(--method-get)");
  });

  test("method_tones_live_in_tokens_not_in_the_skin", () => {
    expect(css).not.toMatch(/--method-(get|put|patch)\s*:/);
    expect(css).not.toContain(".studio-api");
  });
});
