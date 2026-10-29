import { describe, expect, test } from "vitest";
import { blockProperties, customProperties, readCss } from "./css.ts";

// The Basalt neutrals (user, 2026-10-21) over the unchanged accents, claw and method tones.
const expected: Record<string, string> = {
  "--ink": "#0c0b0a",
  "--deep": "#161412",
  "--channel": "#1e1b18",
  "--shallows": "#25211d",
  "--sand": "#f2c7a3",
  "--sand-lit": "#f7d6b9",
  "--seafoam": "#9fd4c6",
  "--coral": "#eda6aa",
  "--palm": "#a9c89a",
  "--chalk": "#ebe6e0",
  "--chalk-dim": "#b0a89f",
  "--line": "rgb(235 230 224 / 0.15)",
  "--line-strong": "rgb(235 230 224 / 0.34)",
  "--input": "rgb(235 230 224 / 0.4)",
  "--shell": "#f2936e",
  "--shell-dark": "#b85a3e",
  "--shell-light": "#f8b896",
  "--tip": "#fde3cf",
  "--method-get": "#9cc8ea",
  "--method-put": "#c4b6ec",
  "--method-patch": "#e3d58c",
  "--radius": "0.5rem",
};

describe("design tokens", () => {
  const tokens = readCss("tokens.css");
  const props = blockProperties(tokens, ":root");

  test("tokens_match_studio_values", () => {
    for (const [name, value] of Object.entries(expected)) {
      expect(props.get(name), name).toBe(value);
    }
  });

  // The alternate theme, kept for a future picker: the former Lagoon neutrals under a data attribute.
  const lagoon: Record<string, string> = {
    "--ink": "#0b1c1f",
    "--deep": "#0f2a2d",
    "--channel": "#14363a",
    "--shallows": "#163d3e",
    "--chalk": "#e6ede7",
    "--chalk-dim": "#aebfc0",
    "--ink-lit": "#18333a",
    "--line": "rgb(230 237 231 / 0.16)",
    "--line-strong": "rgb(230 237 231 / 0.34)",
    "--input": "rgb(230 237 231 / 0.4)",
  };

  test("lagoon_alternate_block_holds_the_former_neutrals", () => {
    const alt = blockProperties(tokens, ':root[data-theme="lagoon"]');
    expect(Object.fromEntries(alt)).toEqual(lagoon);
  });

  test("every_neutral_basalt_differs_on_has_a_lagoon_value", () => {
    for (const name of Object.keys(lagoon)) expect(props.get(name), name).not.toBe(lagoon[name]);
  });

  test("dark_color_scheme", () => {
    expect(tokens).toMatch(/color-scheme:\s*dark/);
  });

  test("no_font_custom_properties_in_tokens (R5)", () => {
    expect([...props.keys()].filter((k) => /font/.test(k))).toEqual([]);
  });

  test("tailwind_defines_the_font_stacks", () => {
    const tw = customProperties(readCss("tailwind.css"));
    expect(tw.get("--font-sans")).toBe('"Atkinson Hyperlegible Next Variable", "Atkinson Hyperlegible", system-ui, sans-serif');
    expect(tw.get("--font-serif")).toBe('Spectral, "Iowan Old Style", Georgia, serif');
    expect(tw.get("--font-mono")).toBe('"JetBrains Mono Variable", ui-monospace, monospace');
    expect(tw.get("--color-ink")).toBe("var(--ink)");
    expect(tw.get("--color-input")).toBe("var(--input)");
    expect(tw.get("--ease-lagoon")).toBe("cubic-bezier(0.16, 1, 0.3, 1)");
  });

  // D7: the landing's colours are all design tokens, and the preset maps them to utilities.
  test("preset_maps_the_landing_colours", () => {
    const landing: Record<string, string> = {
      "--shell": "#f2936e",
      "--shell-dark": "#b85a3e",
      "--shell-light": "#f8b896",
      "--tip": "#fde3cf",
      "--wet-sand": "#dcae8a",
      "--ink-lit": "#2b2621",
    };
    const tw = customProperties(readCss("tailwind.css"));
    for (const [name, value] of Object.entries(landing)) {
      expect(props.get(name), name).toBe(value);
      expect(tw.get(`--color-${name.slice(2)}`), `--color-${name.slice(2)}`).toBe(`var(${name})`);
    }
  });

  test("tailwind_preset_scans_the_components", () => {
    expect(readCss("tailwind.css")).toContain('@source "./src";');
  });

  test("claw_eye_token", () => {
    expect(props.get("--claw-eye")).toBe("#fff6ee");
  });
});
