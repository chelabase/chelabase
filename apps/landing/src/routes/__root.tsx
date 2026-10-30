import { createRootRoute, HeadContent, Outlet, Scripts } from '@tanstack/react-router';
import appCss from '@/styles.css?url';

const TITLE = 'Chelabase — Manage, collaborate, iterate on Postgres';
const DESCRIPTION =
  "An open-source Postgres platform you run yourself: branch, merge and restore databases, give your team access Postgres enforces, and query from your app with a typed client. Chelabase's own services are written in Rust.";

export const Route = createRootRoute({
  head: () => ({
    meta: [
      { charSet: 'utf-8' },
      { name: 'viewport', content: 'width=device-width, initial-scale=1' },
      { title: TITLE },
      { name: 'description', content: DESCRIPTION },
      { property: 'og:title', content: TITLE },
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
