// @vitest-environment node
import { existsSync, readdirSync, readFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { beforeAll, describe, expect, it } from 'vitest';
import { docsHref, GITHUB_URL } from '../src/site-urls';

// These tests read the static site that `vite build` writes to dist/client.
const dist = join(resolve(import.meta.dirname, '..'), 'dist/client');

// React writes this marker (a Suspense boundary that fell back to client rendering) when a
// component throws while the page prerenders. The build still exits 0.
const RENDER_ERROR_MARKER = '<!--$!-->';

describe('the built landing page', () => {
  let html = '';

  beforeAll(() => {
    const index = join(dist, 'index.html');
    if (!existsSync(index)) {
      throw new Error(`${index} does not exist: run \`pnpm --filter @chelabase/landing build\` first`);
    }
    html = readFileSync(index, 'utf8');
  });

  it('prerendered_html_is_dark_without_javascript', () => {
    const tag = /<html[^>]*>/.exec(html)?.[0];
    expect(tag).toMatch(/\sclass="([^"]*\s)?dark(\s[^"]*)?"/);
  });

  it('prerendered_html_carries_the_copy', () => {
    expect(html).toContain('Manage, collaborate, iterate. On Postgres.');
    expect(html).toContain('staging&#x27;s schema merged into main');
    expect(html).toContain('<title>Chelabase — Manage, collaborate, iterate on Postgres</title>');
  });

  it('prerendered_html_links_to_the_docs_and_github', () => {
    expect(html).toContain(`href="${docsHref('using/quickstart', process.env.VITE_DOCS_URL)}"`);
    expect(html).toContain(`href="${GITHUB_URL}"`);
  });

  it('prerendered_html_has_no_render_error', () => {
    expect(html).not.toContain(RENDER_ERROR_MARKER);
  });

  // The hero's fonts are fetched with the HTML rather than after the CSS: the latin subsets of
  // Spectral 600 and 700 (tagline, wordmark) and Atkinson (body, buttons). Nothing else.
  it('preloads_the_hero_fonts', () => {
    const links = [...html.matchAll(/<link[^>]*rel="preload"[^>]*>/g)].map((m) => m[0]);
    const fonts = links.filter((l) => /as="font"/.test(l));
    const files = fonts.map((l) => /href="\/assets\/([^"]+)"/.exec(l)?.[1]);
    expect(files.map((f) => f?.replace(/-[\w-]{8}\.woff2$/, '')).sort()).toEqual(
      ['atkinson-hyperlegible-next-latin-wght-normal', 'spectral-latin-600-normal', 'spectral-latin-700-normal'].sort(),
    );
    for (const [i, l] of fonts.entries()) {
      expect(l).toContain('type="font/woff2"');
      expect(l).toContain('crossorigin="anonymous"');
      expect(existsSync(join(dist, 'assets', files[i]!)), files[i]).toBe(true);
    }
    // The stylesheet references the same hashed files, so the preload is the one the page uses.
    const css = readdirSync(join(dist, 'assets'))
      .filter((f) => f.endsWith('.css'))
      .map((f) => readFileSync(join(dist, 'assets', f), 'utf8'))
      .join('\n');
    for (const f of files) expect(css).toContain(`/assets/${f}`);
  });

  it('short_screen_rules_are_built', () => {
    const assets = join(dist, 'assets');
    const css = readdirSync(assets)
      .filter((f) => f.endsWith('.css'))
      .map((f) => readFileSync(join(assets, f), 'utf8'))
      .join('\n');
    const block = /@media\s*\((?:max-height:\s*|height\s*<=\s*)780px\)\s*\{(?:[^{}]|\{[^{}]*\})*\}/g;
    const rules = (css.match(block) ?? []).join('\n');
    expect(rules).toContain('3.25rem');
    expect(rules).toContain('1.5rem');
  });
});
