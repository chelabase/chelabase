import { createFileRoute, Link } from '@tanstack/react-router';
import { canonicalUrl } from '@/lib/shared';

// No home page: `/` sends readers to the docs. The prerendered page carries a meta refresh, which
// works on a static host with no server redirect. No `beforeLoad` redirect: the prerender follows
// redirects and would write the /docs page into index.html without the meta refresh.
export const Route = createFileRoute('/')({
  head: () => ({
    meta: [{ httpEquiv: 'refresh', content: '0; url=/docs' }],
    links: [{ rel: 'canonical', href: canonicalUrl('/docs') }],
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
