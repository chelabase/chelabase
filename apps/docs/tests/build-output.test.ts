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

const HTTP_METHODS = ['GET', 'PUT', 'POST', 'DELETE', 'OPTIONS', 'HEAD', 'PATCH', 'TRACE'];

function classesOf(tag: string): string[] {
  return /\sclass="([^"]*)"/.exec(tag)?.[1]?.split(/\s+/) ?? [];
}

/** Every rendered element carrying fumadocs-openapi's Badge signature, with its text and colour.
 *  Only real attributes (`class="…"`); the serialized page tree in scripts escapes its quotes. */
function badges(html: string): { text: string; colour: string }[] {
  const out: { text: string; colour: string }[] = [];
  for (const m of html.matchAll(/<[a-z][a-z0-9]*\s[^>]*>([^<]*)/g)) {
    const classes = classesOf(m[0]);
    const colour = classes
      .map((c) => /^dark:text-(green|blue|yellow|orange|red)-400$/.exec(c)?.[1])
      .find(Boolean);
    if (colour && classes.includes('font-mono') && classes.includes('font-medium')) {
      out.push({ text: m[1]!.trim(), colour });
    }
  }
  return out;
}

/** The badges of the serialized page tree (the sidebar the client renders), whose markup is
 *  escaped as `\x3Cspan class=\"…\">GET\x3C/span>`. */
function treeBadges(html: string): { text: string; colour: string }[] {
  const out: { text: string; colour: string }[] = [];
  for (const m of html.matchAll(/\\x3Cspan class=\\"([^"\\]*)\\">([^\\]*)\\x3C\/span>/g)) {
    const tag = `<span class="${m[1]}">`;
    out.push(...badges(`${tag}${m[2]}`));
  }
  return out;
}

/** The sidebar of a page (`aside#nd-sidebar`). */
function sidebarOf(html: string): string {
  const start = html.indexOf('<aside id="nd-sidebar"');
  if (start < 0) throw new Error('no #nd-sidebar');
  return html.slice(start, html.indexOf('</aside>', start));
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

  it('html_is_dark_without_javascript', () => {
    // No light flash on a static host: the class is in the prerendered HTML, not set by a script.
    for (const page of ['docs/index.html', 'docs/reference/data/runQuery/index.html']) {
      const html = /<html[^>]*>/.exec(read(page))?.[0];
      expect(html, page).toMatch(/\sclass="([^"]*\s)?dark(\s[^"]*)?"/);
    }
  });

  it('lagoon_tokens_reach_the_css', () => {
    const assets = join(dist, 'assets');
    const sheets = readdirSync(assets)
      .filter((name) => name.endsWith('.css'))
      .map((name) => readFileSync(join(assets, name), 'utf8'));
    expect(sheets.length).toBeGreaterThan(0);
    expect(sheets.some((css) => css.includes('--ink:#0c0b0a'))).toBe(true);
    expect(sheets.some((css) => css.includes('--color-fd-background:var(--ink)'))).toBe(true);
  });

  it('no_theme_toggle', () => {
    // fumadocs-ui's ThemeSwitch (layouts/shared/slots/theme-switch.js) renders a button with
    // aria-label "Toggle Theme" and a data-theme-toggle attribute. Positive control: both strings
    // are still in the installed source, so their absence below is meaningful.
    const source = readFileSync(
      join(docsRoot, 'node_modules/fumadocs-ui/dist/layouts/shared/slots/theme-switch.js'),
      'utf8',
    );
    expect(source).toContain('Toggle Theme');
    expect(source).toContain('data-theme-toggle');
    for (const page of ['docs/index.html', 'docs/reference/data/runQuery/index.html']) {
      const html = read(page);
      expect(html, page).not.toContain('aria-label="Toggle Theme"');
      expect(html, page).not.toContain('data-theme-toggle');
    }
  });

  it('root_redirects_to_docs', () => {
    // A static host has no server redirect: the prerendered page must carry the meta refresh.
    const html = read('index.html');
    expect(html).toMatch(/<meta[^>]*http-equiv="refresh"[^>]*content="0; url=\/docs"/);
    // No canonical link until the docs host is decided (spec D4): a relative one is wrong.
    expect(html).not.toMatch(/rel="canonical"/);
  });

  it('badge_signature_is_on_operation_and_sidebar_badges', () => {
    // packages/lagoon/fumadocs.css remaps the badge tones on `.font-mono.font-medium.dark\:text-
    // <colour>-400`, fumadocs-openapi's Badge. If Fumadocs changes those classes, this fails.
    const operationHtml = read('docs/reference/data/runQuery/index.html');
    // The operation's own badge, and the open folder of its sidebar.
    expect(badges(operationHtml).map((b) => b.text)).toContain('POST');
    expect(badges(sidebarOf(operationHtml)).map((b) => b.text)).toEqual(
      expect.arrayContaining(['POST', 'GET']),
    );
    // A content page in the reference tab: the footer card to the neighbouring operation is
    // rendered; its sidebar folders are collapsed in the HTML and rendered by the client from the
    // serialized page tree, which carries the same Badge markup. Access sits between data and
    // errors in the reference meta.json, so its previous neighbour is the last operation of data.
    const contentHtml = read('docs/reference/access/index.html');
    expect(badges(contentHtml).length).toBeGreaterThan(0);
    const tree = treeBadges(contentHtml);
    for (const method of ['GET', 'POST', 'PATCH', 'PUT', 'DELETE']) {
      expect(tree.map((b) => b.text), method).toContain(method);
    }
    // Each method has its own colour, so each remap reaches its method.
    const colourOf = Object.fromEntries(tree.map((b) => [b.text, b.colour]));
    expect(colourOf).toMatchObject({
      GET: 'green',
      POST: 'blue',
      PUT: 'yellow',
      PATCH: 'orange',
      DELETE: 'red',
    });
  });

  it('badge_signature_is_only_on_badges', () => {
    const pages = walk(dist).filter((file) => file.endsWith('.html'));
    expect(pages.length).toBeGreaterThan(1);
    const offenders = pages.flatMap((file) => {
      const html = readFileSync(file, 'utf8');
      return [...badges(html), ...treeBadges(html)]
        .filter((b) => !HTTP_METHODS.includes(b.text))
        .map((b) => `${relative(dist, file)}: ${JSON.stringify(b.text)}`);
    });
    expect(
      offenders,
      'a non-method Badge (e.g. Deprecated) carries the method signature: decide its colour in packages/lagoon/fumadocs.css',
    ).toEqual([]);
  });

  it('page_headings_are_inside_nd_page_and_not_not_prose', () => {
    // fumadocs.css sets Spectral 700 on `#nd-page :is(h1, …, h6):not(.not-prose)`. The title and
    // section headings of an operation page and a content page must match that selector.
    for (const page of ['docs/reference/data/runQuery/index.html', 'docs/using/quickstart/index.html']) {
      const html = read(page);
      const start = html.indexOf('<article id="nd-page"');
      expect(start, page).toBeGreaterThan(-1);
      const end = html.indexOf('</article>', start);
      const headings = [...html.matchAll(/<h([12])[^>]*>/g)];
      expect(headings.length, page).toBeGreaterThan(1);
      for (const h of headings) {
        expect(h.index! > start && h.index! < end, `${page}: ${h[0]}`).toBe(true);
        expect(classesOf(h[0]), `${page}: ${h[0]}`).not.toContain('not-prose');
      }
    }
    const operation = read('docs/reference/data/runQuery/index.html');
    for (const id of ['authorization', 'request-body', 'response-body']) {
      expect(operation, id).toMatch(new RegExp(`<h2 id="${id}"`));
    }
  });

  it('data_reference_pages_have_flat_urls', () => {
    expect(existsSync(join(dist, 'docs/reference/data/runQuery/index.html'))).toBe(true);
    expect(existsSync(join(dist, 'docs/reference/data/getDataSchema/index.html'))).toBe(true);
  });
});
