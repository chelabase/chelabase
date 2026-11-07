import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { defineConfig, loadEnv } from "vite";
import { devtools } from "@tanstack/devtools-vite";
import { tanstackRouter } from "@tanstack/router-plugin/vite";
import viteReact from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import { devPort } from "./src/lib/dev-port";

const rootDir = path.dirname(fileURLToPath(import.meta.url));

// The dev server is reached through the gateway by host name (W1): allow that host, and point
// hot-module reload at the gateway's port rather than the dev server's own.

// An entry setting as scripts/lib/entry.sh exports it (recipes source it): the environment wins,
// else the workbench value in infra/compose/.env.example, the one place the defaults live.
function entrySetting(name: string): string {
  const fromEnv = process.env[name];
  if (fromEnv !== undefined) return fromEnv.trim();
  const example = readFileSync(path.resolve(rootDir, "../../infra/compose/.env.example"), "utf8");
  const line = example.split("\n").find((l) => l.startsWith(`${name}=`));
  return line ? line.slice(name.length + 1).trim() : "";
}

// A host setting normalized as Entry does it (lowercase, one trailing dot dropped, IPv6 brackets
// kept); the port is its own, else the scheme's (a blank scheme is https).
function publicHost(raw: string) {
  const host = raw.trim().toLowerCase();
  const m = /^(\[[^\]]*\]|[^:]*)(?::(\d+))?$/.exec(host);
  const name = (m?.[1] ?? host).replace(/\.$/, "");
  const https = entrySetting("CHELABASE_PUBLIC_SCHEME").toLowerCase() !== "http";
  const port = m?.[2] ? Number(m[2]) : https ? 443 : 80;
  return { name, port };
}

export default defineConfig(({ mode }) => {
  // CHELABASE_STUDIO_HOST from the environment wins; else VITE_STUDIO_HOST from .env.local
  // (written by `chelabase dev bootstrap`); else the workbench value.
  const { name, port } = publicHost(
    process.env.CHELABASE_STUDIO_HOST ||
      loadEnv(mode, rootDir, "VITE_").VITE_STUDIO_HOST ||
      entrySetting("CHELABASE_STUDIO_HOST"),
  );
  return {
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
    //
    // The workspace packages are source (`main` is a .ts file): excluded, so Vite serves and
    // watches them as app modules instead of freezing a pre-bundled copy that misses exports a
    // merge adds until the dev server restarts.
    optimizeDeps: {
      exclude: ["@postgres-language-server/wasm", "@chelabase/management-client", "@chelabase/design"],
    },
    server: {
      // Bound to loopback on 3000 (or CHELABASE_STUDIO_PORT): only the gateway reaches it, at
      // Studio's host. Never drift to another port. Inside a workbench stack the gateway is
      // another container, so the stack sets CHELABASE_STUDIO_BIND=0.0.0.0.
      host: entrySetting("CHELABASE_STUDIO_BIND") || "127.0.0.1",
      port: devPort(process.env),
      strictPort: true,
      allowedHosts: [name],
      hmr: { host: name, clientPort: port },
    },
  };
});
