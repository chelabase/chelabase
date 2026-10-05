import { existsSync, readdirSync, readFileSync, statSync } from 'node:fs';
import { join } from 'node:path';

export type BrokenLink = {
  page: string;
  href: string;
  reason: 'missing-page' | 'missing-fragment';
};

function walk(dir: string): string[] {
  return readdirSync(dir).flatMap((name) => {
    const path = join(dir, name);
    return statSync(path).isDirectory() ? walk(path) : [path];
  });
}

function isFile(path: string): boolean {
  return existsSync(path) && statSync(path).isFile();
}

/** `/x` is the prerendered `x/index.html`, or a file named `x` (api/search has no extension). */
function targetFile(distDir: string, path: string): string | undefined {
  const rel = path.replace(/^\/+/, '').replace(/\/+$/, '');
  const candidates = rel === '' ? ['index.html'] : [`${rel}/index.html`, rel, `${rel}.html`];
  return candidates.map((candidate) => join(distDir, candidate)).find(isFile);
}

function decode(text: string): string {
  try {
    return decodeURIComponent(text);
  } catch {
    return text;
  }
}

const idCache = new Map<string, Set<string>>();

function idsIn(file: string): Set<string> {
  let ids = idCache.get(file);
  if (!ids) {
    ids = new Set();
    const html = readFileSync(file, 'utf8');
    for (const match of html.matchAll(/\sid=(?:"([^"]*)"|'([^']*)')/g)) {
      ids.add(match[1] ?? match[2]);
    }
    idCache.set(file, ids);
  }
  return ids;
}

/**
 * Every internal `href="/..."` (and `href="#..."` on the same page) in the prerendered pages of
 * `distDir` must resolve to a file, and its `#fragment` must be an `id="..."` in that file.
 * External links are ignored.
 */
export function findBrokenLinks(distDir: string): BrokenLink[] {
  idCache.clear();
  const broken: BrokenLink[] = [];
  const pages = walk(distDir).filter((file) => file.endsWith('.html'));
  for (const file of pages) {
    const page = file.slice(distDir.length + 1);
    const html = readFileSync(file, 'utf8');
    const seen = new Set<string>();
    for (const match of html.matchAll(/\shref=(?:"([^"]*)"|'([^']*)')/g)) {
      const href = (match[1] ?? match[2]).replaceAll('&amp;', '&');
      if (seen.has(href)) continue;
      seen.add(href);
      if (!href.startsWith('/') && !href.startsWith('#')) continue;
      if (href.startsWith('//')) continue;

      const hashAt = href.indexOf('#');
      const pathPart = (hashAt === -1 ? href : href.slice(0, hashAt)).split('?')[0];
      const fragment = hashAt === -1 ? '' : decode(href.slice(hashAt + 1));

      const target = pathPart === '' ? file : targetFile(distDir, pathPart);
      if (!target) {
        broken.push({ page, href, reason: 'missing-page' });
        continue;
      }
      // Only HTML targets have ids; skip fragments of other files and the bare "#" / "#top".
      if (fragment === '' || fragment === 'top' || !target.endsWith('.html')) continue;
      if (!idsIn(target).has(fragment)) {
        broken.push({ page, href, reason: 'missing-fragment' });
      }
    }
  }
  return broken.sort(
    (a, b) => a.page.localeCompare(b.page) || a.href.localeCompare(b.href),
  );
}
