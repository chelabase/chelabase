import { createGetUrl } from 'fumadocs-core/source';

export const appName = 'Chelabase';
/** The docs pages are served from the site root: `/` is the introduction. */
export const docsRoute = '/';

/** The published docs site. Canonical links, social cards and the sitemap name it, so a copy
 *  served elsewhere (a self-hosted stack's docs host) still points search engines here. */
export const siteUrl = 'https://docs.chelabase.com';

/** A path's canonical URL. The static host serves every page as a directory (`/x/`) and
 *  redirects the bare path there, so the canonical form ends in a slash. */
export function canonicalUrl(path: string): string {
  return `${siteUrl}${path.endsWith('/') ? path : `${path}/`}`;
}

/** Head tags every page shares: favicon links and the social card. */
export const iconLinks = [
  { rel: 'icon', href: '/favicon.svg', type: 'image/svg+xml' },
  { rel: 'icon', href: '/favicon-32.png', type: 'image/png', sizes: '32x32' },
  { rel: 'apple-touch-icon', href: '/apple-touch-icon.png' },
];

/** Open Graph and Twitter tags for one page. */
export function socialMeta(title: string, description: string | undefined, url: string) {
  return [
    { property: 'og:site_name', content: `${appName} docs` },
    { property: 'og:type', content: 'article' },
    { property: 'og:title', content: title },
    ...(description ? [{ property: 'og:description', content: description }] : []),
    { property: 'og:url', content: url },
    { property: 'og:image', content: `${siteUrl}/og.png` },
    { property: 'og:image:width', content: '1200' },
    { property: 'og:image:height', content: '630' },
    { name: 'twitter:card', content: 'summary_large_image' },
  ];
}

const getDocsUrl = createGetUrl(docsRoute);

export function getPageMarkdownUrl(page: { slugs: string[]; locale?: string }) {
  const segments = [...page.slugs];
  if (segments.length === 0) {
    segments.push('index.md');
  } else {
    segments[segments.length - 1] += '.md';
  }

  return { segments, url: getDocsUrl(segments, page.locale) };
}

/** @returns page slugs */
export function decodeMarkdownUrl(segments: string[]) {
  if (segments.length === 0) return [];

  const out = [...segments];
  out[out.length - 1] = out[out.length - 1].replace(/\.md$/, '');
  if (out.length === 1 && out[0] === 'index') out.pop();
  return out;
}
