import { existsSync, readdirSync, readFileSync, statSync } from 'node:fs';
import { join, relative, resolve } from 'node:path';
import { beforeAll, describe, expect, it } from 'vitest';
import { findBrokenLinks } from './links';

// These tests read the static site that `vite build` writes to dist/client.
const docsRoot = resolve(import.meta.dirname, '..');
const repoRoot = resolve(docsRoot, '../..');
const dist = join(docsRoot, 'dist/client');
const content = join(docsRoot, 'content/docs');

// React writes this marker (a Suspense boundary that fell back to client rendering) when an MDX
// component is missing or throws while a page prerenders. The build still exits 0; its log only
// says "Error in renderToReadableStream".
const RENDER_ERROR_MARKER = '<!--$!-->';

function walk(dir: string): string[] {
  return readdirSync(dir).flatMap((name) => {
    const path = join(dir, name);
    return statSync(path).isDirectory() ? walk(path) : [path];
  });
}

function read(path: string): string {
  return readFileSync(join(dist, path), 'utf8');
}

function countOperations(file: string): number {
  const doc = JSON.parse(readFileSync(join(repoRoot, 'openapi', file), 'utf8'));
  const methods = ['get', 'put', 'post', 'delete', 'options', 'head', 'patch', 'trace'];
  return Object.values<Record<string, unknown>>(doc.paths).flatMap((item) =>
    Object.keys(item).filter((key) => methods.includes(key)),
  ).length;
}

function indexPages(dir: string): string[] {
  const root = join(dist, dir);
  if (!existsSync(root)) return [];
  return walk(root).filter((file) => file.endsWith('/index.html'));
}

describe('the built docs site', () => {
  beforeAll(() => {
    if (!existsSync(dist)) {
      throw new Error(`${dist} does not exist: run \`pnpm --filter @chelabase/docs build\` first`);
    }
  });

  it('prerenders_the_docs_index', () => {
    expect(existsSync(join(dist, 'docs/index.html'))).toBe(true);
    expect(read('docs/index.html')).toContain('Introduction');
  });

  it('prerenders_every_management_operation', () => {
    const doc = JSON.parse(readFileSync(join(repoRoot, 'openapi/management.json'), 'utf8'));
    const methods = ['get', 'put', 'post', 'delete', 'options', 'head', 'patch', 'trace'];
    const operations = Object.values<Record<string, unknown>>(doc.paths).flatMap((item) =>
      Object.keys(item).filter((key) => methods.includes(key)),
    ).length;
    expect(operations).toBeGreaterThan(0);

    const pages = walk(join(dist, 'docs/reference/management')).filter((file) =>
      file.endsWith('/index.html'),
    );
    expect(pages.length).toBe(operations);

    const create = pages.find((file) => file.includes('createDatabase'));
    expect(create, 'a createDatabase page').toBeDefined();
    expect(readFileSync(create!, 'utf8')).toContain('/v1/databases');
  });

  it('prerenders_every_data_operation', () => {
    const operations = countOperations('data.json');
    expect(operations).toBe(2);
    const pages = indexPages('docs/reference/data');
    expect(pages.length).toBe(operations);

    const run = pages.find((file) => file.includes('runQuery'));
    expect(run, 'a runQuery page').toBeDefined();
    expect(pages.some((file) => file.includes('getDataSchema')), 'a getDataSchema page').toBe(true);
    const html = readFileSync(run!, 'utf8');
    expect(html).toContain('/data/v1/query');
    // QueryRequest is a oneOf; every variant must be listed.
    for (const variant of ['Select', 'Insert', 'Update', 'Delete', 'Call', 'Batch']) {
      expect(html, `${variant}Request`).toContain(`${variant}Request`);
    }
  });

  it('prerenders_every_login_operation', () => {
    const operations = countOperations('login.json');
    expect(operations).toBeGreaterThan(0);
    const pages = indexPages('docs/architecture/login-api');
    expect(pages.length).toBe(operations);
  });

  it('emits_llms_and_search', () => {
    expect(existsSync(join(dist, 'llms.txt'))).toBe(true);
    expect(existsSync(join(dist, 'llms-full.txt'))).toBe(true);
    expect(existsSync(join(dist, 'api/search'))).toBe(true);
    expect(() => JSON.parse(read('api/search'))).not.toThrow();
  });

  it('client_bundle_has_no_node_builtins', () => {
    const assets = join(dist, 'assets');
    const scripts = readdirSync(assets).filter((name) => name.endsWith('.js'));
    expect(scripts.length).toBeGreaterThan(0);
    const offenders = scripts.filter((name) => {
      const text = readFileSync(join(assets, name), 'utf8');
      // Imports of node built-ins, with or without the `node:` prefix; a bare "path" string in a
      // syntax-highlighting grammar is not an import.
      return /(from|import\(|require\()\s*["'](node:)?(fs|path)(\/promises)?["']/.test(text);
    });
    expect(offenders).toEqual([]);
  });

  it('no_page_rendered_an_error', () => {
    const pages = walk(dist).filter((file) => file.endsWith('.html'));
    expect(pages.length).toBeGreaterThan(1);
    const offenders = pages
      .filter((file) => readFileSync(file, 'utf8').includes(RENDER_ERROR_MARKER))
      .map((file) => relative(dist, file));
    expect(offenders).toEqual([]);
  });

  it('no_internal_link_is_broken', () => {
    expect(findBrokenLinks(dist)).toEqual([]);
  });

  it('no_page_is_a_stub', () => {
    const stubs = walk(content)
      .filter((file) => file.endsWith('.mdx') && readFileSync(file, 'utf8').includes('stub:'))
      .map((file) => relative(content, file))
      .sort();
    expect(stubs).toEqual([]);
  });

  it('every_content_page_is_prerendered', () => {
    const pages = walk(content).filter((file) => file.endsWith('.mdx'));
    expect(pages.length).toBeGreaterThan(0);
    const missing = pages
      .map((file) => relative(content, file).replace(/\.mdx$/, '').replace(/(^|\/)index$/, ''))
      .filter((slug) => !existsSync(join(dist, 'docs', slug, 'index.html')))
      .sort();
    expect(missing).toEqual([]);
  });

  it('every_root_folder_has_prerendered_pages', () => {
    const metas = walk(content).filter((file) => file.endsWith('/meta.json'));
    const roots = metas.filter((file) => JSON.parse(readFileSync(file, 'utf8')).root === true);
    expect(roots.length).toBeGreaterThan(0);
    const empty = roots
      .map((file) => relative(content, join(file, '..')))
      .filter((folder) => indexPages(join('docs', folder)).length === 0);
    expect(empty).toEqual([]);
  });

  it('every_page_has_its_own_title', () => {
    const title = (path: string) => /<title>([^<]*)<\/title>/.exec(read(path))?.[1];
    expect(title('docs/index.html')).toBe('Introduction · Chelabase');
    expect(title('docs/using/quickstart/index.html')).toBe('Quickstart · Chelabase');
    const api = indexPages('docs/reference/data').find((file) => file.includes('runQuery'));
    expect(api, 'a runQuery page').toBeDefined();
    expect(title(relative(dist, api!))).toMatch(/^.+ · Chelabase$/);
    expect(title(relative(dist, api!))).not.toBe('Chelabase');
  });

  it('data_reference_pages_have_flat_urls', () => {
    expect(existsSync(join(dist, 'docs/reference/data/runQuery/index.html'))).toBe(true);
    expect(existsSync(join(dist, 'docs/reference/data/getDataSchema/index.html'))).toBe(true);
  });
});
