import { createRootRoute, HeadContent, Outlet, Scripts } from '@tanstack/react-router';
import appCss from '@/styles.css?url';

const DESCRIPTION =
  "Postgres on Neon's storage engine, with copy-on-write branches, point-in-time restore, a typed data API and auth for your app's users, managed from one console.";

export const Route = createRootRoute({
  head: () => ({
    meta: [
      { charSet: 'utf-8' },
      { name: 'viewport', content: 'width=device-width, initial-scale=1' },
      { title: 'Chelabase — Postgres that branches' },
      { name: 'description', content: DESCRIPTION },
      { property: 'og:title', content: 'Chelabase — Postgres that branches' },
      { property: 'og:description', content: DESCRIPTION },
      { property: 'og:type', content: 'website' },
    ],
    links: [{ rel: 'stylesheet', href: appCss }],
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
