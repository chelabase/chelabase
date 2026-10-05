import react from '@vitejs/plugin-react';
import { tanstackStart } from '@tanstack/react-start/plugin/vite';
import { defineConfig } from 'vite';
import tailwindcss from '@tailwindcss/vite';
import { fumadocsMdx } from 'fumadocs-mdx/vite';

// No strictPort here: the prerender step starts a preview server from this config, and a
// strict port breaks the build when :3001 is taken. `pnpm dev` passes --strictPort itself.
export default defineConfig({
  server: { port: 3001 },
  plugins: [
    fumadocsMdx(),
    tailwindcss(),
    tanstackStart({
      prerender: { enabled: true, crawlLinks: true },
      pages: [
        { path: '/' },
        { path: '/docs' },
        // The sidebar shows one root folder (tab) at a time, so the crawler reaches only that
        // tab's pages from a page in it. One seed per root; the crawl finds the rest.
        { path: '/docs/using/quickstart' },
        { path: '/docs/reference/errors' },
        { path: '/docs/architecture' },
        { path: '/api/search' },
        { path: '/llms.txt' },
        { path: '/llms-full.txt' },
      ],
    }),
    react(),
  ],
  resolve: {
    tsconfigPaths: true,
    alias: { tslib: 'tslib/tslib.es6.js' },
  },
});
