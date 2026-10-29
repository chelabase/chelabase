import { describe, expect, test } from "vitest";
import { customProperties, readCss } from "./css.ts";

const css = readCss("fumadocs.css");
const tokens = new Set(customProperties(readCss("tokens.css")).keys());

// Every colour variable Fumadocs reads (fumadocs-ui/css/lib/default-colors.css), minus the static
// callout and diff colours.
const fdColours = [
  "background",
  "foreground",
  "muted",
  "muted-foreground",
  "popover",
  "popover-foreground",
  "card",
  "card-foreground",
  "border",
  "primary",
  "primary-foreground",
  "secondary",
  "secondary-foreground",
  "accent",
  "accent-foreground",
  "ring",
  "overlay",
].map((name) => `--color-fd-${name}`);

/** The custom properties of every top-level block whose prelude is exactly `selector`. */
function block(selector: string): Map<string, string> {
  const out = new Map<string, string>();
  const stripped = css.replace(/\/\*[\s\S]*?\*\//g, "");
  for (const m of stripped.matchAll(/([^{};]*)\{([^{}]*)\}/g)) {
    if (m[1]!.trim().replace(/\s+/g, " ") !== selector) continue;
    for (const [k, v] of customProperties(`${m[2]};`)) out.set(k, v);
  }
  return out;
}

/** The blocks (selector, properties) that declare `name`. */
function blocksDeclaring(name: string): { selector: string; props: Map<string, string> }[] {
  const stripped = css.replace(/\/\*[\s\S]*?\*\//g, "");
  return [...stripped.matchAll(/([^{};]*)\{([^{}]*)\}/g)]
    .map((m) => ({
      selector: m[1]!.trim().replace(/\s+/g, " "),
      props: customProperties(`${m[2]};`),
    }))
    .filter((b) => b.props.has(name));
}

/** The design tokens a value reads through `var(--…)`. */
function tokenRefs(value: string): string[] {
  return [...value.matchAll(/var\((--[a-z0-9-]+)\)/g)].map((m) => m[1]!);
}

describe("fumadocs skin", () => {
  test("maps_every_fd_colour", () => {
    // `@theme` is the `:root` map: Tailwind emits it on `:root` and generates the `*-fd-*`
    // utilities from it (neutral.css, which used to declare them, is gone).
    const scopes = { ":root (@theme)": block("@theme"), ".dark": block(".dark") };
    for (const [scope, props] of Object.entries(scopes)) {
      for (const name of fdColours) {
        const value = props.get(name);
        expect(value, `${scope} ${name}`).toBeDefined();
        const refs = tokenRefs(value!);
        expect(refs.length, `${scope} ${name}: ${value}`).toBeGreaterThan(0);
        for (const ref of refs) expect(tokens.has(ref), `${scope} ${name}: ${ref}`).toBe(true);
      }
    }
    expect(block("@theme")).toEqual(block(".dark"));
  });

  test("fd_colours_take_the_design_roles", () => {
    const root = block("@theme");
    expect(root.get("--color-fd-background")).toBe("var(--ink)");
    expect(root.get("--color-fd-foreground")).toBe("var(--chalk)");
    expect(root.get("--color-fd-muted")).toBe("var(--deep)");
    expect(root.get("--color-fd-muted-foreground")).toBe("var(--chalk-dim)");
    expect(root.get("--color-fd-popover")).toBe("var(--deep)");
    expect(root.get("--color-fd-card")).toBe("var(--deep)");
    expect(root.get("--color-fd-border")).toBe("var(--line)");
    expect(root.get("--color-fd-primary")).toBe("var(--sand)");
    expect(root.get("--color-fd-primary-foreground")).toBe("var(--ink)");
    expect(root.get("--color-fd-secondary")).toBe("var(--channel)");
    expect(root.get("--color-fd-accent")).toBe("var(--channel)");
    expect(root.get("--color-fd-ring")).toBe("var(--sand)");
    expect(root.get("--color-fd-overlay")).toMatch(/var\(--ink\) 60%/);
  });

  test("badges_use_the_studio_tones", () => {
    const remaps: Record<string, string> = {
      "--color-green-400": "var(--method-get)", // GET
      "--color-blue-400": "var(--seafoam)", // POST
      "--color-yellow-400": "var(--method-put)", // PUT
      "--color-orange-400": "var(--method-patch)", // PATCH
      "--color-red-400": "var(--coral)", // DELETE
    };
    for (const [name, value] of Object.entries(remaps)) {
      const colour = /--color-([a-z]+)-400/.exec(name)![1];
      // (The `.not-prose` Deprecated override has its own test.)
      const found = blocksDeclaring(name).filter((b) => !b.selector.startsWith(".not-prose"));
      // One block, scoped to fumadocs-openapi's Badge class signature (shared-api badge.js:
      // "font-mono font-medium" + "dark:text-<colour>-400"), so every method badge (operation
      // pages and sidebars) takes the tone and nothing else using Tailwind's palette changes.
      // The docs build tests `badge_signature_is_on_operation_and_sidebar_badges` and
      // `badge_signature_is_only_on_badges` check the signature against the real markup.
      expect(found.length, name).toBe(1);
      expect(found[0]!.selector, name).toBe(`.font-mono.font-medium.dark\\:text-${colour}-400`);
      expect([...found[0]!.props], name).toEqual([[name, value]]);
    }
    expect(css).not.toContain(":has(");
  });

  test("deprecated_badge_is_sand_after_the_yellow_remap", () => {
    // D11: the operation heading's "Deprecated" badge (`not-prose`, yellow) is attention, not PUT.
    const selector = ".not-prose.font-mono.font-medium.dark\\:text-yellow-400";
    const found = blocksDeclaring("--color-yellow-400").filter((b) => b.selector === selector);
    expect(found.length).toBe(1);
    expect([...found[0]!.props]).toEqual([["--color-yellow-400", "var(--sand)"]]);
    const stripped = css.replace(/\/\*[\s\S]*?\*\//g, "");
    expect(stripped.indexOf(selector)).toBeGreaterThan(stripped.indexOf("\n.font-mono.font-medium.dark\\:text-yellow-400 {"));
    expect(stripped.indexOf("\n.font-mono.font-medium.dark\\:text-yellow-400 {")).toBeGreaterThan(-1);
  });

  test("callouts_and_diffs_take_the_design_meanings", () => {
    // One meaning: coral = hazard, seafoam = yes, sand = attention, chalk-dim = neutral note.
    const statics = block("@theme static");
    expect(statics.get("--color-fd-error")).toBe("var(--coral)");
    expect(statics.get("--color-fd-success")).toBe("var(--seafoam)");
    expect(statics.get("--color-fd-warning")).toBe("var(--sand)");
    expect(statics.get("--color-fd-info")).toBe("var(--chalk-dim)");
    expect(statics.get("--color-fd-idea")).toBe("var(--chalk-dim)");
    expect(statics.get("--color-fd-diff-remove-symbol")).toBe("var(--coral)");
    expect(statics.get("--color-fd-diff-add-symbol")).toBe("var(--seafoam)");
    expect(statics.get("--color-fd-diff-remove")).toMatch(/^color-mix\(in srgb, var\(--coral\) \d+%, transparent\)$/);
    expect(statics.get("--color-fd-diff-add")).toMatch(/^color-mix\(in srgb, var\(--seafoam\) \d+%, transparent\)$/);
  });

  test("popovers_use_hairlines_not_shadows", () => {
    // Only elements that already draw a full border (`.border`): popovers, selects and the search
    // dialog; not bare popover-coloured surfaces or a sheet with a top border only.
    const stripped = css.replace(/\/\*[\s\S]*?\*\//g, "");
    const rule = /([^{}]*)\{([^}]*border:\s*1px solid var\(--line\)[^}]*)\}/.exec(stripped);
    expect(rule).not.toBeNull();
    expect(rule![2]).toMatch(/box-shadow:\s*none/);
    const parts = rule![1]!.split(",").map((s) => s.trim());
    expect(parts.length).toBeGreaterThan(0);
    for (const p of parts) expect(p, p).toMatch(/^\.bg-fd-popover(\\\/\d+)?\.border$/);
  });

  test("headings_serif_and_code_mono", () => {
    expect(css).toMatch(/#nd-page :is\(h1, h2, h3, h4, h5, h6\):not\(\.not-prose\)\s*\{[^}]*font-family:\s*var\(--font-serif\);\s*font-weight:\s*700/);
    expect(css).toMatch(/\{[^}]*font-family:\s*var\(--font-mono\)/);
    expect(css).toMatch(/figure\.shiki[^{]*\{[^}]*background-color:\s*var\(--deep\)/);
  });
});
