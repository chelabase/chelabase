import { readFileSync } from 'node:fs';
import react from '@vitejs/plugin-react';
import { tanstackStart } from '@tanstack/react-start/plugin/vite';
import { defineConfig } from 'vite';
import tailwindcss from '@tailwindcss/vite';
import { fumadocsMdx } from 'fumadocs-mdx/vite';

// No strictPort here: the prerender step starts a preview server from this config, and a
// strict port breaks the build when :3001 is taken. `pnpm dev` passes --strictPort itself.

// The dev server is reached through the gateway by host name (W1): allow that host, and point
// hot-module reload at the gateway's port rather than the dev server's own.

// An entry setting as scripts/lib/entry.sh exports it (recipes source it): the environment wins,
// else the workbench value in infra/compose/.env.example, the one place the defaults live.
function entrySetting(name: string): string {
  const fromEnv = process.env[name];
  if (fromEnv !== undefined) return fromEnv.trim();
  const example = readFileSync(new URL('../../infra/compose/.env.example', import.meta.url), 'utf8');
  const line = example.split('\n').find((l) => l.startsWith(`${name}=`));
  return line ? line.slice(name.length + 1).trim() : '';
}

// A host setting normalized as Entry does it (lowercase, one trailing dot dropped, IPv6 brackets
// kept); the port is its own, else the scheme's (a blank scheme is https).
function publicHost(raw: string) {
  const host = raw.trim().toLowerCase();
  const m = /^(\[[^\]]*\]|[^:]*)(?::(\d+))?$/.exec(host);
  const name = (m?.[1] ?? host).replace(/\.$/, '');
  const https = entrySetting('CHELABASE_PUBLIC_SCHEME').toLowerCase() !== 'http';
  const port = m?.[2] ? Number(m[2]) : https ? 443 : 80;
  return { name, port };
}

const { name, port } = publicHost(entrySetting('CHELABASE_DOCS_HOST'));

export default defineConfig({
  // Bound to loopback: only the gateway reaches it, at this app's host. Inside a workbench stack
  // the gateway is another container, so the stack sets CHELABASE_DOCS_BIND=0.0.0.0.
  server: { host: entrySetting('CHELABASE_DOCS_BIND') || '127.0.0.1', port: 3001, allowedHosts: [name], hmr: { host: name, clientPort: port } },
  plugins: [
    fumadocsMdx(),
    tailwindcss(),
    tanstackStart({
      prerender: { enabled: true, crawlLinks: true },
      pages: [
        { path: '/' },
        // The sidebar shows one root folder (tab) at a time, so the crawler reaches only that
        // tab's pages from a page in it. One seed per root; the crawl finds the rest.
        { path: '/using/quickstart' },
        { path: '/reference/errors' },
        { path: '/architecture' },
        { path: '/api/search' },
        { path: '/llms.txt' },
        { path: '/llms-full.txt' },
        { path: '/sitemap.xml' },
      ],
    }),
    react(),
  ],
  resolve: {
    tsconfigPaths: true,
    alias: { tslib: 'tslib/tslib.es6.js' },
  },
});
