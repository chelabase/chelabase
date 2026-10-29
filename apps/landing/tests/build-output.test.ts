// @vitest-environment node
import { existsSync, readdirSync, readFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { beforeAll, describe, expect, it } from 'vitest';
import { studioUrl } from '../src/studio-url';

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
    expect(html).toContain('Postgres that branches. Host it anywhere.');
    expect(html).toContain('staging merged into main');
    expect(html).toContain('<title>Chelabase — Postgres that branches</title>');
  });

  it('prerendered_html_links_to_studio', () => {
    const base = studioUrl(process.env.VITE_STUDIO_URL);
    expect(html).toContain(`href="${base}/?signup"`);
    expect(html).toContain(`href="${base}/"`);
  });

  it('prerendered_html_has_no_render_error', () => {
    expect(html).not.toContain(RENDER_ERROR_MARKER);
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
