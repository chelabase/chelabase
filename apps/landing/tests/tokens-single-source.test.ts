// @vitest-environment node
import { readdirSync, readFileSync, statSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { describe, expect, it } from 'vitest';

const appRoot = resolve(import.meta.dirname, '..');
const tokensPath = resolve(appRoot, '../../packages/design/tokens.css');

const stripComments = (css: string) => css.replace(/\/\*[\s\S]*?\*\//g, '');

/** Every custom property a stylesheet declares (`--name:`), comments ignored. The lookbehind skips
 *  selectors such as `.band--seafoam :focus-visible`. */
function declared(css: string): string[] {
  return [...stripComments(css).matchAll(/(?<![\w-])(--[\w-]+)\s*:/g)].map((m) => m[1]!);
}

/** The declared names of `css` that `tokens` also declares. */
function redeclared(css: string, tokens: Set<string>): string[] {
  return [...new Set(declared(css))].filter((name) => tokens.has(name));
}

/** Tailwind arbitrary properties (`[--name:value]`) in a TSX file that name a token in `tokens`. */
function arbitraryRedeclared(tsx: string, tokens: Set<string>): string[] {
  return [...new Set([...tsx.matchAll(/\[(--[\w-]+):/g)].map((m) => m[1]!))].filter((name) => tokens.has(name));
}

function filesWith(dir: string, ext: string): string[] {
  return readdirSync(dir).flatMap((name) => {
    const path = join(dir, name);
    return statSync(path).isDirectory() ? filesWith(path, ext) : path.endsWith(ext) ? [path] : [];
  });
}
const cssFiles = (dir: string) => filesWith(dir, '.css');

describe('one palette source', () => {
  const tokens = new Set(declared(readFileSync(tokensPath, 'utf8')));

  it('reads_the_design_tokens', () => {
    expect(tokens.has('--ink')).toBe(true);
    expect(tokens.has('--sand')).toBe(true);
  });

  it('the_check_catches_a_redeclared_token', () => {
    expect(redeclared('.landing { --ink: #000; --mine: 1px; }', tokens)).toEqual(['--ink']);
    expect(redeclared('/* --ink: #000; */ .a { --mine: 1px; }', tokens)).toEqual([]);
    expect(redeclared('.band--ink :focus-visible { color: red; }', tokens)).toEqual([]);
  });

  it('the_check_catches_a_tailwind_arbitrary_redeclaration', () => {
    expect(arbitraryRedeclared('className="[--sand:red] [--mine:1px]"', tokens)).toEqual(['--sand']);
    expect(arbitraryRedeclared('className="[--claw-gap:var(--sand)] bg-[var(--sand)]"', tokens)).toEqual([]);
  });

  it('no_app_tsx_redeclares_a_design_token_with_an_arbitrary_property', () => {
    const files = filesWith(join(appRoot, 'src'), '.tsx');
    expect(files.length).toBeGreaterThan(0);
    const hits = files.flatMap((file) =>
      arbitraryRedeclared(readFileSync(file, 'utf8'), tokens).map((name) => `${file}: ${name}`),
    );
    expect(hits).toEqual([]);
  });

  it('no_app_css_redeclares_a_design_token', () => {
    const files = cssFiles(join(appRoot, 'src'));
    expect(files.length).toBeGreaterThan(0);
    const hits = files.flatMap((file) =>
      redeclared(readFileSync(file, 'utf8'), tokens).map((name) => `${file}: ${name}`),
    );
    expect(hits).toEqual([]);
  });
});
