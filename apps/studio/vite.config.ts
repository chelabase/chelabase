import path from "node:path";
import { fileURLToPath } from "node:url";
import { defineConfig } from "vite";
import { devtools } from "@tanstack/devtools-vite";
import { tanstackRouter } from "@tanstack/router-plugin/vite";
import viteReact from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import { devPort } from "./src/lib/dev-port";

const rootDir = path.dirname(fileURLToPath(import.meta.url));

export default defineConfig({
  resolve: {
    alias: {
      "@": path.resolve(rootDir, "./src"),
      "#": path.resolve(rootDir, "./src"),
    },
  },
  plugins: [
    devtools(),
    tailwindcss(),
    tanstackRouter({ target: "react", autoCodeSplitting: true }),
    viteReact(),
  ],
  // The pgls worker (src/lib/pgls) is an ES module worker; its package loads pgls.wasm via
  // `new URL(..., import.meta.url)`, which pre-bundling would break.
  worker: { format: "es" },
  optimizeDeps: { exclude: ["@postgres-language-server/wasm"] },
  server: {
    // 3000 (or CHELADB_STUDIO_PORT). The Zitadel redirect URI is registered for :3000; never
    // drift to another port.
    port: devPort(process.env),
    strictPort: true,
  },
});
