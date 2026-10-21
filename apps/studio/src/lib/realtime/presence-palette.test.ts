import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { PRESENCE_COLORS, presenceColor } from "./presence";
import { contrast, deltaE, over, resolve, themes } from "./presence-palette-helpers";

/*
 * The presence palette must read as a colour on both themes (Basalt default, Lagoon alternate). The colours need not be
 * theme tokens: an entry is `var(--token)` (resolved from the theme's token block) or a 6-digit hex literal.
 * Thresholds, measured on the six good hues of the old palette (OKLab distance x100, see presence-palette-helpers.ts):
 *  - non-text contrast floor 3:1 (WCAG 1.4.11); the six good hues are 5.7:1 or more on every ground;
 *  - distance floor 5.0: the six good hues are >= 5.6 apart from each other and from chalk-dim, >= 5.8 from sand, >= 7.5 from coral;
 *    the old chalk-dim is 0.0 from chalk-dim and the old shell-light is 3.6 from sand, so both fail;
 *  - tints: the six good hues' 7% row tint moves the ground by >= 3.2 and the 18% editing fill by >= 8.2.
 */
const OLD = [
  "var(--seafoam)",
  "var(--method-get)",
  "var(--palm)",
  "var(--method-put)",
  "var(--method-patch)",
  "var(--shell)",
];
const REPLACED_SLOTS = [6, 7];
const OLD_REPLACED = ["var(--shell-light)", "var(--chalk-dim)"];
const GROUNDS = ["--ink", "--deep", "--channel"];
const TINT_GROUNDS = ["--ink", "--channel"];

const CONTRAST_FLOOR = 3;
const TEXT_CONTRAST_FLOOR = 4.5;
const DISTANCE_FLOOR = 5.0;
const ROW_TINT_FLOOR = 3.0;
const EDIT_FILL_FLOOR = 8.0;
/*
 * The one documented exception. User ruling 2026-10-21: slot 7 becomes a clear blue tuned for Basalt (the default theme);
 * Lagoon is only a kept alternate, and the user accepts that this blue's tints on Lagoon are slightly under the floor.
 * It relaxes only the two tint floors, only for slot 7, only on Lagoon. Every other pin holds for slot 7 on Lagoon,
 * and every other slot keeps the full floors on both themes.
 */
const LAGOON_SLOT_7_ROW_TINT_FLOOR = 2.9;
const LAGOON_SLOT_7_EDIT_FILL_FLOOR = 7.5;

describe("palette shape and stability", () => {
  it("has_eight_entries_and_keeps_the_six_unchanged_hues_in_their_slots", () => {
    expect(PRESENCE_COLORS).toHaveLength(8);
    expect(PRESENCE_COLORS.slice(0, 6)).toEqual(OLD);
  });

  it("the_same_seed_still_gets_the_same_colour_for_the_unchanged_hues", () => {
    for (let slot = 0; slot < 6; slot++) {
      for (const seed of [slot, slot + 8, slot + 8 * 1000, slot + 4_294_967_288]) {
        expect(presenceColor(seed), `seed ${seed}`).toBe(OLD[slot]);
      }
    }
  });

  it("only_the_two_replaced_slots_change", () => {
    for (const slot of REPLACED_SLOTS) {
      expect(PRESENCE_COLORS[slot], `slot ${slot}`).not.toBe(OLD_REPLACED[slot - 6]);
    }
  });

  it("the_new_hues_are_distinct_entries", () => {
    expect(new Set(PRESENCE_COLORS).size).toBe(8);
  });
});

describe("no neutral, text or shared-meaning token is a presence colour", () => {
  const forbidden = [
    "--chalk-dim", "--shell-light", "--chalk", "--ink", "--deep", "--channel", "--shallows", "--ink-lit", "--line", "--line-strong",
    "--input", "--tip", "--claw-eye", "--sand", "--sand-lit", "--wet-sand", "--coral",
  ];

  it("no_entry_is_a_forbidden_token", () => {
    for (const color of PRESENCE_COLORS) {
      for (const token of forbidden) expect(color, `${color} vs ${token}`).not.toBe(`var(${token})`);
    }
  });

  it("every_entry_is_a_var_token_or_hex_that_resolves_in_both_themes", () => {
    for (const color of PRESENCE_COLORS) {
      expect(color, color).toMatch(/^(var\(--[a-z0-9-]+\)|#[0-9a-f]{6})$/i);
      for (const [name, theme] of themes) expect(() => resolve(color, theme), `${color} in ${name}`).not.toThrow();
    }
  });

  it("no_entry_resolves_to_the_value_of_a_forbidden_token_in_either_theme", () => {
    for (const [name, theme] of themes) {
      for (const color of PRESENCE_COLORS) {
        const rgb = resolve(color, theme).join(",");
        for (const token of forbidden) {
          const v = theme.get(token);
          if (!v || !/^#[0-9a-f]{6}$/i.test(v)) continue;
          expect(rgb, `${color} equals ${token} in ${name}`).not.toBe(resolve(v, theme).join(","));
        }
      }
    }
  });
});

describe.each(themes)("%s", (themeName, theme) => {
  const rgbs = PRESENCE_COLORS.map((c) => resolve(c, theme));
  const ground = (g: string) => resolve(`var(${g})`, theme);

  it("every_presence_colour_reaches_3_to_1_on_ink_deep_and_channel", () => {
    PRESENCE_COLORS.forEach((c, i) => {
      for (const g of GROUNDS) expect(contrast(rgbs[i]!, ground(g)), `${c} on ${g}`).toBeGreaterThanOrEqual(CONTRAST_FLOOR);
    });
  });

  it("avatar_initials_are_ink_and_reach_4_5_to_1_on_every_presence_colour", () => {
    // presence-layer.tsx draws the initials in `text-ink` on the presence colour.
    const layer = readFileSync(
      join(dirname(fileURLToPath(import.meta.url)), "../../components/table-editor/presence-layer.tsx"),
      "utf8",
    );
    const avatar = layer.slice(layer.indexOf('data-testid="avatar"'));
    expect(avatar.slice(0, avatar.indexOf("{initials(m)}"))).toMatch(/\btext-ink\b/);
    PRESENCE_COLORS.forEach((c, i) => {
      expect(contrast(rgbs[i]!, ground("--ink")), `ink on ${c}`).toBeGreaterThanOrEqual(TEXT_CONTRAST_FLOOR);
    });
  });

  it("every_pair_of_presence_colours_is_at_least_5_apart", () => {
    for (let i = 0; i < 8; i++) {
      for (let j = i + 1; j < 8; j++) {
        expect(deltaE(rgbs[i]!, rgbs[j]!), `${PRESENCE_COLORS[i]} vs ${PRESENCE_COLORS[j]}`).toBeGreaterThanOrEqual(DISTANCE_FLOOR);
      }
    }
  });

  it("every_presence_colour_is_at_least_5_from_sand_coral_and_chalk_dim", () => {
    for (const token of ["--sand", "--coral", "--chalk-dim"]) {
      PRESENCE_COLORS.forEach((c, i) => {
        expect(deltaE(rgbs[i]!, ground(token)), `${c} vs ${token}`).toBeGreaterThanOrEqual(DISTANCE_FLOOR);
      });
    }
  });

  it("the_7_percent_row_tint_and_18_percent_editing_fill_are_visible_on_ink_and_channel", () => {
    PRESENCE_COLORS.forEach((c, i) => {
      const exception = themeName === "lagoon" && i === 7;
      const rowFloor = exception ? LAGOON_SLOT_7_ROW_TINT_FLOOR : ROW_TINT_FLOOR;
      const fillFloor = exception ? LAGOON_SLOT_7_EDIT_FILL_FLOOR : EDIT_FILL_FLOOR;
      for (const g of TINT_GROUNDS) {
        const base = ground(g);
        expect(deltaE(over(rgbs[i]!, 0.07, base), base), `${c} 7% on ${g}`).toBeGreaterThanOrEqual(rowFloor);
        expect(deltaE(over(rgbs[i]!, 0.18, base), base), `${c} 18% on ${g}`).toBeGreaterThanOrEqual(fillFloor);
      }
    });
  });
});
