import { describe, expect, test } from "vitest";
import { customProperties, readCss } from "./css.ts";

// Values as Studio had them at 34ec766 (styles.css and the Scalar skin).
const expected: Record<string, string> = {
  "--ink": "#0b1c1f",
  "--deep": "#0f2a2d",
  "--channel": "#14363a",
  "--shallows": "#163d3e",
  "--sand": "#f2c7a3",
  "--sand-lit": "#f7d6b9",
  "--seafoam": "#9fd4c6",
  "--coral": "#eda6aa",
  "--palm": "#a9c89a",
  "--chalk": "#e6ede7",
  "--chalk-dim": "#aebfc0",
  "--line": "rgb(230 237 231 / 0.16)",
  "--line-strong": "rgb(230 237 231 / 0.34)",
  "--input": "rgb(230 237 231 / 0.4)",
  "--shell": "#f2936e",
  "--shell-dark": "#b85a3e",
  "--shell-light": "#f8b896",
  "--tip": "#fde3cf",
  "--method-get": "#9cc8ea",
  "--method-put": "#c4b6ec",
  "--method-patch": "#e3d58c",
  "--radius": "0.5rem",
  "--lagoon-font-sans": '"Atkinson Hyperlegible Next Variable", "Atkinson Hyperlegible", system-ui, sans-serif',
  "--lagoon-font-serif": 'Spectral, "Iowan Old Style", Georgia, serif',
  "--lagoon-font-mono": '"JetBrains Mono Variable", ui-monospace, monospace',
};

describe("lagoon tokens", () => {
  const tokens = readCss("tokens.css");
  const props = customProperties(tokens);

  test("tokens_match_studio_values", () => {
    for (const [name, value] of Object.entries(expected)) {
      expect(props.get(name), name).toBe(value);
    }
  });

  test("dark_color_scheme", () => {
    expect(tokens).toMatch(/color-scheme:\s*dark/);
  });

  test("no_font_custom_properties_in_tokens (R5)", () => {
    expect([...props.keys()].filter((k) => /^--font-/.test(k))).toEqual([]);
  });

  test("tailwind_maps_font_utilities_onto_lagoon_stacks", () => {
    const tw = customProperties(readCss("tailwind.css"));
    expect(tw.get("--font-sans")).toBe("var(--lagoon-font-sans)");
    expect(tw.get("--font-serif")).toBe("var(--lagoon-font-serif)");
    expect(tw.get("--font-mono")).toBe("var(--lagoon-font-mono)");
    expect(tw.get("--color-ink")).toBe("var(--ink)");
    expect(tw.get("--color-input")).toBe("var(--input)");
    expect(tw.get("--ease-lagoon")).toBe("cubic-bezier(0.16, 1, 0.3, 1)");
  });

  // D7: the landing's colours are all lagoon tokens, and the preset maps them to utilities.
  test("preset_maps_the_landing_colours", () => {
    const landing: Record<string, string> = {
      "--shell": "#f2936e",
      "--shell-dark": "#b85a3e",
      "--shell-light": "#f8b896",
      "--tip": "#fde3cf",
      "--wet-sand": "#dcae8a",
      "--ink-lit": "#18333a",
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
