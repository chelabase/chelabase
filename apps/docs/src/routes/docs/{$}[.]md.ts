import { createFileRoute, notFound } from '@tanstack/react-router';
import { getLlms, getSource } from '@/lib/api-source.server';
import { decodeMarkdownUrl } from '@/lib/shared';

export const Route = createFileRoute('/docs/{$}.md')({
  server: {
    handlers: {
      GET: async ({ params }) => {
        const slugs = decodeMarkdownUrl(params._splat?.split('/') ?? []);
        const page = (await getSource()).getPage(slugs);
        if (!page) throw notFound();

        return new Response(await (await getLlms()).page(page), {
          headers: {
            'Content-Type': 'text/markdown',
          },
        });
      },
    },
  },
});
