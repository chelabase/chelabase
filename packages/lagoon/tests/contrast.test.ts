import { describe, expect, test } from "vitest";
import { customProperties, readCss } from "./css.ts";

const props = customProperties(readCss("tokens.css"));

function rgb(value: string | undefined): [number, number, number] {
  const hex = /^#([0-9a-f]{6})$/i.exec(value ?? "");
  if (!hex) throw new Error(`not a hex colour: ${value}`);
  const n = parseInt(hex[1]!, 16);
  return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
}

function luminance([r, g, b]: [number, number, number]): number {
  const lin = (c: number) => {
    const s = c / 255;
    return s <= 0.03928 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4;
  };
  return 0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b);
}

function ratio(a: [number, number, number], b: [number, number, number]): number {
  const [hi, lo] = [luminance(a), luminance(b)].sort((x, y) => y - x);
  return (hi! + 0.05) / (lo! + 0.05);
}

const grounds = ["--ink", "--deep", "--channel"];

describe("lagoon contrast", () => {
  test("method_tones_and_text_reach_4_5_to_1", () => {
    const fg = ["--method-get", "--method-put", "--method-patch", "--seafoam", "--coral", "--chalk", "--chalk-dim", "--sand"];
    for (const f of fg) {
      for (const g of grounds) {
        expect(ratio(rgb(props.get(f)), rgb(props.get(g))), `${f} on ${g}`).toBeGreaterThanOrEqual(4.5);
      }
    }
  });

  test("method_tones_on_their_badge_tint", () => {
    // A method badge is its tone at 10% over the ground, with the tone as text.
    for (const tone of ["--method-get", "--method-put", "--method-patch", "--seafoam", "--coral", "--chalk-dim"]) {
      const c = rgb(props.get(tone));
      for (const g of grounds) {
        const ground = rgb(props.get(g));
        const tint = c.map((v, i) => Math.round(v * 0.1 + ground[i]! * 0.9)) as [number, number, number];
        expect(ratio(c, tint), `${tone} on its 10% tint over ${g}`).toBeGreaterThanOrEqual(5.4);
      }
    }
  });

  test("fumadocs_callout_and_diff_colours_reach_4_5_to_1", () => {
    // Callout icons and diff +/- symbols are drawn on ink, deep (cards, code blocks) and channel.
    const statics = customProperties(readCss("fumadocs.css"));
    const names = ["error", "success", "warning", "info", "idea", "diff-remove-symbol", "diff-add-symbol"];
    for (const name of names) {
      const token = /^var\((--[a-z-]+)\)$/.exec(statics.get(`--color-fd-${name}`) ?? "")?.[1];
      expect(token, name).toBeDefined();
      for (const g of grounds) {
        expect(ratio(rgb(props.get(token!)), rgb(props.get(g))), `${name} (${token}) on ${g}`).toBeGreaterThanOrEqual(4.5);
      }
    }
    // The diff symbols also sit on their own tint over deep (the code block).
    for (const [tint, symbol] of [["diff-remove", "diff-remove-symbol"], ["diff-add", "diff-add-symbol"]] as const) {
      const m = /^color-mix\(in srgb, var\((--[a-z-]+)\) (\d+)%, transparent\)$/.exec(statics.get(`--color-fd-${tint}`) ?? "");
      expect(m, tint).not.toBeNull();
      const a = Number(m![2]) / 100;
      const c = rgb(props.get(m![1]!));
      const deep = rgb(props.get("--deep"));
      const over = c.map((v, i) => Math.round(v * a + deep[i]! * (1 - a))) as [number, number, number];
      const fg = rgb(props.get(/^var\((--[a-z-]+)\)$/.exec(statics.get(`--color-fd-${symbol}`)!)![1]!));
      expect(ratio(fg, over), `${symbol} on ${tint} over deep`).toBeGreaterThanOrEqual(4.5);
    }
  });

  test("input_border_reaches_3_to_1", () => {
    const m = /^rgb\((\d+) (\d+) (\d+) \/ ([\d.]+)\)$/.exec(props.get("--input") ?? "");
    expect(m, "--input is rgb(r g b / a)").not.toBeNull();
    const [r, g, b] = [Number(m![1]), Number(m![2]), Number(m![3])];
    const a = Number(m![4]);
    for (const ground of grounds) {
      const bg = rgb(props.get(ground));
      const over = [r, g, b].map((c, i) => Math.round(c * a + bg[i]! * (1 - a))) as [number, number, number];
      expect(ratio(over, bg), `--input on ${ground}`).toBeGreaterThanOrEqual(3);
    }
  });
});
