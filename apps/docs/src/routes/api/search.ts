import { createFileRoute } from '@tanstack/react-router';
import { createFromSource } from 'fumadocs-core/search/server';
import { getSource } from '@/lib/api-source.server';

export const Route = createFileRoute('/api/search')({
  server: {
    handlers: {
      GET: async () => {
        const server = createFromSource(await getSource(), {
          // https://docs.orama.com/docs/orama-js/supported-languages
          language: 'english',
        });
        return server.staticGET();
      },
    },
  },
});
