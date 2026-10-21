import { describe, expect, it } from "vitest";
import { PRESENCE_COLORS } from "./presence";
import { deltaE, oklchHue, resolve, themes } from "./presence-palette-helpers";

/*
 * RT1 second Playwright pass (2026-10-21), ruling 2: slot 7 (cyan) sat too close to seafoam (slot 0) and to
 * --method-get (slot 1) on screen. Slot 7 must be at least 12 apart (OKLab x100) from both, in both themes.
 * Every pin of presence-palette.test.ts (contrast, the 5.0 floors, the tints) still holds for the new slot 7.
 */
const SLOT_7_FLOOR = 12;

describe.each(themes)("%s", (_name, theme) => {
  it("slot_7_is_at_least_12_from_seafoam_and_from_method_get", () => {
    const cyan = resolve(PRESENCE_COLORS[7]!, theme);
    for (const token of ["--seafoam", "--method-get"]) {
      expect(deltaE(cyan, resolve(`var(${token})`, theme)), `${PRESENCE_COLORS[7]} vs ${token}`).toBeGreaterThanOrEqual(SLOT_7_FLOOR);
    }
  });

  it("slot_7_is_a_blue_oklch_hue_230_to_270", () => {
    // User ruling 2026-10-21: slot 7 is a clear blue, tuned for Basalt (the default theme).
    const hue = oklchHue(resolve(PRESENCE_COLORS[7]!, theme));
    expect(hue, `${PRESENCE_COLORS[7]} hue`).toBeGreaterThanOrEqual(230);
    expect(hue, `${PRESENCE_COLORS[7]} hue`).toBeLessThanOrEqual(270);
  });

  it("slot_7_stays_a_distinct_hex_or_token_in_the_eighth_slot", () => {
    expect(PRESENCE_COLORS).toHaveLength(8);
    expect(PRESENCE_COLORS[7]).toMatch(/^(var\(--[a-z0-9-]+\)|#[0-9a-f]{6})$/i);
    expect(new Set(PRESENCE_COLORS).size).toBe(8);
  });
});
