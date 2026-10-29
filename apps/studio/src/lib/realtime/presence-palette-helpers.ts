// Test-only colour maths for the presence palette tests (not app code): WCAG contrast and OKLab distance.
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

export type Rgb = [number, number, number];
export type Theme = Map<string, string>;

const tokensPath = join(dirname(fileURLToPath(import.meta.url)), "../../../../../packages/design/tokens.css");

function block(css: string, selector: string): Map<string, string> {
  const stripped = css.replace(/\/\*[\s\S]*?\*\//g, "");
  for (const m of stripped.matchAll(/([^{};]*)\{([^{}]*)\}/g)) {
    if (m[1]!.trim().replace(/\s+/g, " ") !== selector) continue;
    const out = new Map<string, string>();
    for (const d of m[2]!.matchAll(/(--[a-z0-9-]+)\s*:\s*([^;{}]+);/gi)) out.set(d[1]!, d[2]!.trim().replace(/\s+/g, " "));
    return out;
  }
  throw new Error(`no rule for ${selector}`);
}

const css = readFileSync(tokensPath, "utf8");
/** Basalt is the bare `:root` block; Lagoon overrides only the neutrals under `[data-theme="lagoon"]`. */
export const basalt: Theme = block(css, ":root");
export const lagoon: Theme = new Map([...basalt, ...block(css, ':root[data-theme="lagoon"]')]);
export const themes = [["basalt", basalt], ["lagoon", lagoon]] as const;

export function parseHex(value: string): Rgb {
  const hex = /^#([0-9a-f]{6})$/i.exec(value.trim());
  if (!hex) throw new Error(`not a 6-digit hex colour: ${value}`);
  const n = parseInt(hex[1]!, 16);
  return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
}

/** `var(--token)` is resolved from the theme's token block (through chains of vars); anything else must be a hex literal. */
export function resolve(value: string, theme: Theme): Rgb {
  let v = value.trim();
  for (let i = 0; i < 5; i++) {
    const m = /^var\((--[a-z0-9-]+)\)$/i.exec(v);
    if (!m) break;
    const next = theme.get(m[1]!);
    if (next === undefined) throw new Error(`unknown token ${m[1]}`);
    v = next;
  }
  return parseHex(v);
}

const lin = (c: number) => {
  const s = c / 255;
  return s <= 0.04045 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4;
};

export function luminance([r, g, b]: Rgb): number {
  return 0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b);
}

export function contrast(a: Rgb, b: Rgb): number {
  const [hi, lo] = [luminance(a), luminance(b)].sort((x, y) => y - x);
  return (hi! + 0.05) / (lo! + 0.05);
}

/** OKLab (Ottosson) from sRGB bytes. */
export function oklab([r, g, b]: Rgb): [number, number, number] {
  const [lr, lg, lb] = [lin(r), lin(g), lin(b)];
  const l = Math.cbrt(0.4122214708 * lr + 0.5363325363 * lg + 0.0514459929 * lb);
  const m = Math.cbrt(0.2119034982 * lr + 0.6806995451 * lg + 0.1073969566 * lb);
  const s = Math.cbrt(0.0883024619 * lr + 0.2817188376 * lg + 0.6299787005 * lb);
  return [
    0.2104542553 * l + 0.793617785 * m - 0.0040720468 * s,
    1.9779984951 * l - 2.428592205 * m + 0.4505937099 * s,
    0.0259040371 * l + 0.7827717662 * m - 0.808675766 * s,
  ];
}

/** OKLCH hue angle in degrees, 0..360 (atan2 of OKLab b over a). */
export function oklchHue(rgb: Rgb): number {
  const [, a, b] = oklab(rgb);
  return ((Math.atan2(b, a) * 180) / Math.PI + 360) % 360;
}

/** OKLab Euclidean distance, scaled by 100 (so 1 is about a just-noticeable difference). */
export function deltaE(a: Rgb, b: Rgb): number {
  const [x, y] = [oklab(a), oklab(b)];
  return 100 * Math.hypot(x[0] - y[0], x[1] - y[1], x[2] - y[2]);
}

/** `color` at `alpha` (0..1) composited over an opaque `ground` in sRGB, as the browser paints it. */
export function over(color: Rgb, alpha: number, ground: Rgb): Rgb {
  return color.map((v, i) => Math.round(v * alpha + ground[i]! * (1 - alpha))) as Rgb;
}
