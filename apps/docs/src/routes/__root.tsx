import { createRootRoute, HeadContent, Outlet, Scripts } from '@tanstack/react-router';
import appCss from '@/styles/app.css?url';
import { RootProvider } from 'fumadocs-ui/provider/tanstack';
import SearchDialog from '@/components/search';
import { iconLinks } from '@/lib/shared';

/** Cloudflare Web Analytics (cookieless). Only the published build sets the token (scripts/deploy-site.sh),
 *  so a copy built anywhere else loads no beacon. */
const ANALYTICS_TOKEN = import.meta.env.VITE_CF_ANALYTICS_TOKEN as string | undefined;
const analyticsScripts = ANALYTICS_TOKEN
  ? [{ src: 'https://static.cloudflareinsights.com/beacon.min.js', defer: true, 'data-cf-beacon': JSON.stringify({ token: ANALYTICS_TOKEN }) }]
  : [];

export const Route = createRootRoute({
  head: () => ({
    meta: [
      {
        charSet: 'utf-8',
      },
      {
        name: 'viewport',
        content: 'width=device-width, initial-scale=1',
      },
      {
        title: 'Chelabase',
      },
    ],
    links: [{ rel: 'stylesheet', href: appCss }, ...iconLinks],
    scripts: analyticsScripts,
  }),
  component: RootComponent,
});

function RootComponent() {
  return (
    // Dark only (no light palette yet): `dark` is in the prerendered HTML, so a static host shows
    // no light flash before the scripts run.
    <html lang="en" className="dark" suppressHydrationWarning>
      <head>
        <HeadContent />
      </head>
      <body className="flex flex-col min-h-screen">
        <RootProvider
          search={{ SearchDialog }}
          theme={{ forcedTheme: 'dark', defaultTheme: 'dark', enableSystem: false, hotKey: false }}
        >
          <Outlet />
        </RootProvider>
        <Scripts />
      </body>
    </html>
  );
}
