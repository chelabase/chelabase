export const GITHUB_URL = 'https://github.com/chelabase/chelabase';

/**
 * A docs page's address. `page` is its path on the docs site, e.g. `using/quickstart`. `raw` is the
 * build-time `VITE_DOCS_URL` (the docs site's origin); without one, the page is read on GitHub.
 */
export function docsHref(page: string, raw?: string): string {
  const base = raw?.trim().replace(/\/+$/, '');
  return base ? `${base}/${page}` : `${GITHUB_URL}/blob/main/apps/docs/content/docs/${page}.mdx`;
}
