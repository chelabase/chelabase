import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");

/** Reads a CSS file of the package. */
export function readCss(name: string): string {
  return readFileSync(join(root, name), "utf8");
}

function stripComments(css: string): string {
  return css.replace(/\/\*[\s\S]*?\*\//g, "");
}

/** Every `--x: value;` declaration (comments stripped). A later declaration wins. */
export function customProperties(css: string): Map<string, string> {
  const out = new Map<string, string>();
  for (const m of stripComments(css).matchAll(/(--[a-z0-9-]+)\s*:\s*([^;{}]+);/gi)) {
    out.set(m[1]!, m[2]!.trim().replace(/\s+/g, " "));
  }
  return out;
}

/** `"<selector> <name>"` for each custom property declared more than once in one block. */
export function duplicateCustomProperties(css: string): string[] {
  const out: string[] = [];
  for (const m of stripComments(css).matchAll(/([^{};]*)\{([^{}]*)\}/g)) {
    const selector = m[1]!.trim().replace(/\s+/g, " ");
    const seen = new Set<string>();
    for (const d of m[2]!.matchAll(/(--[a-z0-9-]+)\s*:/gi)) {
      const name = d[1]!;
      if (seen.has(name)) out.push(`${selector} ${name}`);
      seen.add(name);
    }
  }
  return out;
}

/** Every selector part (split on commas) of every rule, comments stripped. */
export function selectors(css: string): string[] {
  const prelude = stripComments(css)
    .split("{")
    .slice(0, -1)
    .map((chunk) => chunk.split("}").pop()!.trim());
  return prelude.flatMap((selector) => selector.split(",").map((part) => part.trim()));
}

/** The custom properties of the one rule whose selector is exactly `selector` (comments stripped). */
export function blockProperties(css: string, selector: string): Map<string, string> {
  for (const m of stripComments(css).matchAll(/([^{};]*)\{([^{}]*)\}/g)) {
    if (m[1]!.trim().replace(/\s+/g, " ") === selector) return customProperties(`${m[2]};`);
  }
  throw new Error(`no rule for ${selector}`);
}
