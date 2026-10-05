import { createFileRoute } from '@tanstack/react-router';
import { getLlms } from '@/lib/api-source.server';

export const Route = createFileRoute('/llms-full.txt')({
  server: {
    handlers: {
      GET: async () => new Response(await (await getLlms()).full()),
    },
  },
});
