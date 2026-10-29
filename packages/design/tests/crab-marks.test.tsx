import { readdirSync, readFileSync, statSync } from "node:fs";
import { dirname, join, relative } from "node:path";
import { fileURLToPath } from "node:url";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, test } from "vitest";
import { ClawMark, CrabFigure, PalmTree, Wordmark } from "../src/crab-marks.tsx";

const src = join(dirname(fileURLToPath(import.meta.url)), "..", "src");

function sources(dir: string): string[] {
  return readdirSync(dir).flatMap((name) => {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) return sources(path);
    return /\.(ts|tsx)$/.test(name) ? [path] : [];
  });
}

/** The colour literals in `text`: hex colours and `rgb(`/`rgba(`/`hsl(`/`hsla(` calls. */
function colourLiterals(text: string): string[] {
  const hex = [...text.matchAll(/#[0-9a-f]{3,8}\b/gi)].map((m) => m[0]);
  const fns = [...text.matchAll(/\b(?:rgba?|hsla?)\(/gi)].map((m) => m[0]);
  return [...hex, ...fns];
}

describe("crab marks", () => {
  test("wordmark_has_an_accessible_name", () => {
    const html = renderToStaticMarkup(<Wordmark />);
    expect(html).toContain('<span class="sr-only">Chelabase</span>');
    expect(html).toMatch(/<span class="[^"]*" aria-hidden="true">/);
    expect(html).toContain("helabase");
  });

  test("wordmark_carries_only_the_shared_rail_classes", () => {
    const html = renderToStaticMarkup(<Wordmark className="x" visualClassName="v" clawClassName="c" />);
    expect(html).toContain('class="font-serif font-bold tracking-[-0.02em] leading-none text-chalk x"');
    expect(html).toContain('class="inline-flex items-end v"');
    expect(html).toContain('class="w-auto -mr-[0.14em] c"');
    expect(renderToStaticMarkup(<Wordmark />)).toContain("h-[1.32em]");
  });

  test("eye_whites_use_the_claw_eye_token", () => {
    const html = renderToStaticMarkup(
      <>
        <ClawMark />
        <svg>
          <CrabFigure />
          <PalmTree />
        </svg>
      </>,
    );
    expect(html).not.toContain("#fff6ee");
    expect(html).toContain("var(--claw-eye)");
  });

  test("the_colour_check_catches_hex_rgb_and_hsl", () => {
    expect(colourLiterals('fill="#fff6ee"')).toEqual(["#fff6ee"]);
    expect(colourLiterals("color: rgb(12 11 10 / 0.8)")).toEqual(["rgb("]);
    expect(colourLiterals("color: hsla(0 0% 0%)")).toEqual(["hsla("]);
    expect(colourLiterals('fill="var(--claw-eye)" href="#a-title"')).toEqual([]);
  });

  test("no_colour_literals_in_components", () => {
    const files = sources(src);
    expect(files.length).toBeGreaterThan(0);
    const hits = files.flatMap((file) => colourLiterals(readFileSync(file, "utf8")).map((lit) => `${relative(src, file)}: ${lit}`));
    expect(hits).toEqual([]);
  });
});
