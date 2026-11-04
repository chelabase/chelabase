import { createFileRoute } from '@tanstack/react-router';
import { getSource } from '@/lib/api-source.server';
import { canonicalUrl } from '@/lib/shared';

// Every page, guides and API reference alike, at its canonical URL. Prerendered to a static file.
export const Route = createFileRoute('/sitemap.xml')({
  server: {
    handlers: {
      GET: async () => {
        const source = await getSource();
        const urls = source
          .getPages()
          .map((page) => `  <url><loc>${canonicalUrl(page.url)}</loc></url>`)
          .sort();
        const body = `<?xml version="1.0" encoding="UTF-8"?>\n<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n${urls.join('\n')}\n</urlset>\n`;
        return new Response(body, { headers: { 'Content-Type': 'application/xml; charset=utf-8' } });
      },
    },
  },
});
