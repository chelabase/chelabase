import { createRootRoute, HeadContent, Outlet, Scripts } from '@tanstack/react-router';
import appCss from '@/styles.css?url';

const TITLE = 'Chelabase — Manage, collaborate, iterate on Postgres';
const DESCRIPTION =
  "An open-source Postgres platform you can run yourself: branch, merge and restore databases, give your team access Postgres enforces, and query from your app with a typed client. Its core services are written in Rust.";

/** The published landing page. Canonical link, social card and sitemap name it. */
const SITE_URL = 'https://chelabase.com/';

/** Structured data: what search engines show for the product. */
const JSON_LD = {
  '@context': 'https://schema.org',
  '@type': 'SoftwareApplication',
  name: 'Chelabase',
  url: SITE_URL,
  description: DESCRIPTION,
  applicationCategory: 'DeveloperApplication',
  operatingSystem: 'Linux',
  license: 'https://www.apache.org/licenses/LICENSE-2.0',
  offers: { '@type': 'Offer', price: '0', priceCurrency: 'USD' },
  sameAs: ['https://github.com/chelabase/chelabase'],
};

/** Cloudflare Web Analytics (cookieless). Only the published build sets the token (scripts/deploy-site.sh),
 *  so a copy built anywhere else loads no beacon. */
const ANALYTICS_TOKEN = import.meta.env.VITE_CF_ANALYTICS_TOKEN as string | undefined;
const analyticsScripts = ANALYTICS_TOKEN
  ? [{ src: 'https://static.cloudflareinsights.com/beacon.min.js', defer: true, 'data-cf-beacon': JSON.stringify({ token: ANALYTICS_TOKEN }) }]
  : [];

export const Route = createRootRoute({
  head: () => ({
    meta: [
      { charSet: 'utf-8' },
      { name: 'viewport', content: 'width=device-width, initial-scale=1' },
      { title: TITLE },
      { name: 'description', content: DESCRIPTION },
      { property: 'og:site_name', content: 'Chelabase' },
      { property: 'og:title', content: TITLE },
      { property: 'og:description', content: DESCRIPTION },
      { property: 'og:type', content: 'website' },
      { property: 'og:url', content: SITE_URL },
      { property: 'og:image', content: `${SITE_URL}og.png` },
      { property: 'og:image:width', content: '1200' },
      { property: 'og:image:height', content: '630' },
      { property: 'og:image:alt', content: 'Chelabase: manage, collaborate, iterate. On Postgres.' },
      { name: 'twitter:card', content: 'summary_large_image' },
    ],
    links: [
      { rel: 'stylesheet', href: appCss },
      { rel: 'canonical', href: SITE_URL },
      { rel: 'icon', href: '/favicon.svg', type: 'image/svg+xml' },
      { rel: 'icon', href: '/favicon-32.png', type: 'image/png', sizes: '32x32' },
      { rel: 'apple-touch-icon', href: '/apple-touch-icon.png' },
    ],
    scripts: [{ type: 'application/ld+json', children: JSON.stringify(JSON_LD) }, ...analyticsScripts],
  }),
  component: RootComponent,
});

function RootComponent() {
  return (
    // Dark only (no light palette yet): `dark` is in the prerendered HTML, so a static host shows
    // no light flash before the scripts run.
    <html lang="en" className="dark">
      <head>
        <HeadContent />
      </head>
      <body>
        <Outlet />
        <Scripts />
      </body>
    </html>
  );
}
