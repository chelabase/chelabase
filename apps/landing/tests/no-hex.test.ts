// @vitest-environment node
import { readdirSync, readFileSync, statSync } from 'node:fs';
import { join, relative, resolve } from 'node:path';
import { describe, expect, it } from 'vitest';

/** Every colour comes from a design token (D7): the app's own sources hold no colour literal. */

const srcRoot = resolve(import.meta.dirname, '../src');

function sources(dir: string): string[] {
  return readdirSync(dir).flatMap((name) => {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) return sources(path);
    return /\.(tsx|ts|css)$/.test(name) ? [path] : [];
  });
}

/** The colour literals in `text`: hex colours and `rgb(`/`rgba(`/`hsl(`/`hsla(` calls. */
export function colourLiterals(text: string): string[] {
  const hex = [...text.matchAll(/#[0-9a-f]{3,8}\b/gi)].map((m) => m[0]);
  const fns = [...text.matchAll(/\b(?:rgba?|hsla?)\(/gi)].map((m) => m[0]);
  return [...hex, ...fns];
}

describe('no colour literals', () => {
  it('the_check_catches_hex_rgb_and_hsl', () => {
    expect(colourLiterals('fill="#fff6ee"')).toEqual(['#fff6ee']);
    expect(colourLiterals('background: #2b2621;')).toEqual(['#2b2621']);
    expect(colourLiterals('color: rgb(12 11 10 / 0.8)')).toEqual(['rgb(']);
    expect(colourLiterals('color: hsl(0 0% 0%)')).toEqual(['hsl(']);
    expect(colourLiterals('href="#what-title" fill="var(--ink)" bg-ink/8')).toEqual([]);
  });

  it('no_colour_literals', () => {
    const files = sources(srcRoot);
    expect(files.length).toBeGreaterThan(0);
    const hits = files.flatMap((file) =>
      colourLiterals(readFileSync(file, 'utf8')).map((lit) => `${relative(srcRoot, file)}: ${lit}`),
    );
    expect(hits).toEqual([]);
  });
});
