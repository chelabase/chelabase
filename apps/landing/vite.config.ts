import react from '@vitejs/plugin-react';
import { tanstackStart } from '@tanstack/react-start/plugin/vite';
import { defineConfig } from 'vite';
import tailwindcss from '@tailwindcss/vite';

// No strictPort here: the prerender step starts a preview server from this config, and a
// strict port breaks the build when :3002 is taken. `pnpm dev` passes --strictPort itself.
export default defineConfig({
  server: { port: 3002 },
  plugins: [
    tailwindcss(),
    tanstackStart({
      prerender: { enabled: true, crawlLinks: true },
      pages: [{ path: '/' }],
    }),
    react(),
  ],
  resolve: {
    tsconfigPaths: true,
  },
});
