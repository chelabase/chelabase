import { Readable } from "node:stream";
import { expect, test } from "vitest";
import { createServerClient } from "./server.ts";

const url = "http://localhost:8000";
const key = "chk_pub_0123456789abcdefghijklmnopqrstuvwxyzABCDEFG";

test("AC5: the server client uploads a Node Readable as a stream, without reading it all first", async () => {
  const chunks = 64;
  const chunk = 64 * 1024;
  let produced = 0;
  async function* source() {
    for (let i = 0; i < chunks; i++) {
      produced++;
      yield Buffer.alloc(chunk, i);
    }
  }
  let producedWhenSent = -1;
  let received = 0;
  const fetch = (async (_input: string | URL | Request, init?: RequestInit) => {
    producedWhenSent = produced;
    expect(init?.body).toBeInstanceOf(ReadableStream);
    expect((init as { duplex?: string }).duplex).toBe("half");
    // Read it the way a socket would: a chunk at a time.
    for await (const part of init!.body as ReadableStream<Uint8Array>) received += part.byteLength;
    return new Response(JSON.stringify({ bucket: "b", name: "big.bin", version_id: "v", size: received, etag: "e" }), {
      status: 200,
      headers: { "content-type": "application/json" },
    });
  }) as typeof globalThis.fetch;
  const db = createServerClient({ url, key, fetch, cookies: { getAll: () => [] } });
  const seen: number[] = [];
  const res = await db.storage
    .from("b")
    .upload("big.bin", Readable.from(source()), { contentType: "application/octet-stream", onProgress: (p) => seen.push(p.loaded) });
  // At most the stream's first pull had happened when fetch was handed the body.
  expect(producedWhenSent).toBeLessThanOrEqual(2);
  expect(received).toBe(chunks * chunk);
  expect(res.size).toBe(chunks * chunk);
  for (let i = 1; i < seen.length; i++) expect(seen[i]!).toBeGreaterThanOrEqual(seen[i - 1]!);
  expect(seen.at(-1)).toBe(chunks * chunk);
});
