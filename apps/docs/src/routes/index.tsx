import { createFileRoute, Link } from '@tanstack/react-router';

// No home page: `/` sends readers to the docs. The prerendered page carries a meta refresh, which
// works on a static host with no server redirect. No `beforeLoad` redirect: the prerender follows
// redirects and would write the /docs page into index.html without the meta refresh.
export const Route = createFileRoute('/')({
  head: () => ({
    // No canonical link until the docs host is decided (spec D4).
    meta: [{ httpEquiv: 'refresh', content: '0; url=/docs' }],
  }),
  component: Home,
});

function Home() {
  return (
    <main className="flex flex-1 items-center justify-center p-8">
      <Link to="/docs/$" params={{ _splat: '' }} className="text-fd-primary underline">
        Go to the documentation
      </Link>
    </main>
  );
}
