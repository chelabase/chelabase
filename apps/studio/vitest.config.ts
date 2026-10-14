import path from "node:path";
import { fileURLToPath } from "node:url";
import viteReact from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

const rootDir = path.dirname(fileURLToPath(import.meta.url));

export default defineConfig({
  plugins: [viteReact()],
  resolve: {
    alias: {
      "@": path.resolve(rootDir, "./src"),
      "#": path.resolve(rootDir, "./src"),
    },
  },
  test: {
    environment: "jsdom",
    setupFiles: ["./src/test/setup.ts"],
    include: ["src/**/*.test.{ts,tsx}"],
    // The editor's UI tests (~1–2 s each alone) and property tests ran past the 5 s default under
    // a loaded machine (the gate runs suites in parallel); 20 s is the explicit budget.
    testTimeout: 20_000,
  },
});
