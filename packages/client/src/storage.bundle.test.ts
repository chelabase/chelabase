// AC4: a query-only app's bundle carries no storage code. `client.storage` loads `storage.ts`
// on first use (a dynamic import), so a bundler splits it into its own chunk.
import { fileURLToPath } from "node:url";
import { build } from "esbuild";
import { expect, test } from "vitest";

const dir = fileURLToPath(new URL(".", import.meta.url));
// Strings only the storage implementation holds.
const MARKERS = ["/storage/v1/upload/", "multipartThreshold", "StorageError"];

async function bundle(contents: string) {
  const out = await build({
    stdin: { contents, resolveDir: dir, loader: "ts", sourcefile: "app.ts" },
    bundle: true,
    splitting: true,
    format: "esm",
    platform: "browser",
    outdir: "out",
    write: false,
    minify: false,
    logLevel: "silent",
  });
  const [entry, ...chunks] = out.outputFiles;
  return { entry: entry!.text, chunks: chunks.map((c) => c.text) };
}

test("AC4: a query-only import bundles without the storage code", async () => {
  const query = await bundle(`
    import { createClient } from "./index.ts";
    const db = createClient({ url: "http://x", key: "chk_pub_x" });
    console.log(await db.from("t").select("id"));
  `);
  for (const m of MARKERS) expect(query.entry, m).not.toContain(m);
  // Storage code is never in the entry, only in the chunk loaded on first use.
  const all = [query.entry, ...query.chunks].join("\n");
  expect(query.chunks.some((c) => c.includes("/storage/v1/upload/"))).toBe(true);
  expect(all).toContain("/storage/v1/upload/");

  // A query-only entry stays small next to the storage chunk it never loads.
  const storageChunk = query.chunks.find((c) => c.includes("/storage/v1/upload/"))!;
  expect(storageChunk.length).toBeGreaterThan(5_000);
  const withStorage = query.entry.length + storageChunk.length;
  expect(query.entry.length).toBeLessThan(withStorage - 5_000);
});
