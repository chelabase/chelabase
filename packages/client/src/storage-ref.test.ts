// #102: `client.storage.resolve` turns `StorageRef`s (a `storage.object_ref` column's values)
// into URLs in one batched request.
import { expect, test } from "vitest";
import { createClient, type StorageRef, type StorageRefLike } from "./index.ts";

const url = "http://localhost:8000";
const key = "chk_pub_0123456789abcdefghijklmnopqrstuvwxyzABCDEFG";
const B1 = "00000000-0000-0000-0000-0000000000b1";
const B2 = "00000000-0000-0000-0000-0000000000b2";

interface Call {
  method: string;
  url: URL;
  body: unknown;
}

/** A fake resolve endpoint: answers each ref with `answer`, recording every call. */
function server(answer: (r: StorageRef & { id?: string }) => string | null) {
  const calls: Call[] = [];
  const fetch = (async (input: string | URL | Request, init?: RequestInit) => {
    const body = init?.body === undefined ? undefined : JSON.parse(String(init.body));
    calls.push({ method: init?.method ?? "GET", url: new URL(String(input)), body });
    const refs = (body as { refs: StorageRef[] }).refs;
    return new Response(JSON.stringify({ urls: refs.map(answer), expires_at: 99 }), {
      status: 200,
      headers: { "content-type": "application/json" },
    });
  }) as typeof globalThis.fetch;
  return { fetch, calls };
}

const signed = (r: StorageRef) => `/storage/v1/object/sign/b/${r.name}?token=t.${r.name}`;

test("AC2: one request resolves every ref, in input order", async () => {
  const s = server(signed);
  const storage = createClient({ url, key, fetch: s.fetch }).storage;
  const refs = [
    { bucket_id: B1, name: "c.png" },
    { bucket_id: B2, name: "a.png" },
    { bucket_id: B1, name: "b.png" },
  ];
  const urls = await storage.resolve(refs, { expiresIn: 60 });
  expect(urls).toEqual([
    `${url}/storage/v1/object/sign/b/c.png?token=t.c.png`,
    `${url}/storage/v1/object/sign/b/a.png?token=t.a.png`,
    `${url}/storage/v1/object/sign/b/b.png?token=t.b.png`,
  ]);
  expect(s.calls).toHaveLength(1);
  expect(s.calls[0]!.method).toBe("POST");
  expect(s.calls[0]!.url.pathname).toBe("/storage/v1/object/resolve");
  expect(s.calls[0]!.body).toEqual({ refs, expires_in: 60 });
});

test("AC2: a lone ref resolves to its URL; download names the file", async () => {
  const s = server(signed);
  const storage = createClient({ url, key, fetch: s.fetch }).database("other").storage;
  const one = await storage.resolve({ bucket_id: B1, name: "a.png" }, { download: "me.png" });
  expect(one).toBe(`${url}/storage/v1/object/sign/b/a.png?token=t.a.png&download=me.png`);
  expect(s.calls[0]!.url.searchParams.get("database")).toBe("other");
  expect(s.calls[0]!.body).toEqual({ refs: [{ bucket_id: B1, name: "a.png" }] });
});

test("AC3: a ref the caller can't read is null in its slot, not an error", async () => {
  const s = server((r) => (r.name === "hidden.png" ? null : signed(r)));
  const storage = createClient({ url, key, fetch: s.fetch }).storage;
  const urls = await storage.resolve([
    { bucket_id: B1, name: "a.png" },
    { bucket_id: B1, name: "hidden.png" },
    { bucket_id: B1, name: "b.png" },
  ]);
  expect(urls).toEqual([`${url}${signed({ bucket_id: B1, name: "a.png" })}`, null, `${url}${signed({ bucket_id: B1, name: "b.png" })}`]);
  expect(await storage.resolve({ bucket_id: B1, name: "hidden.png" })).toBeNull();
});

test("AC4: an empty array (or only null refs) sends no request", async () => {
  const s = server(signed);
  const storage = createClient({ url, key, fetch: s.fetch }).storage;
  expect(await storage.resolve([])).toEqual([]);
  expect(await storage.resolve([null, undefined])).toEqual([null, null]);
  expect(await storage.resolve(null)).toBeNull();
  expect(s.calls).toHaveLength(0);
});

test("null refs (a nullable column) keep their slots; only the others are sent", async () => {
  const s = server(signed);
  const storage = createClient({ url, key, fetch: s.fetch }).storage;
  const urls = await storage.resolve([null, { bucket_id: B1, name: "a.png" }, undefined]);
  expect(urls).toEqual([null, `${url}${signed({ bucket_id: B1, name: "a.png" })}`, null]);
  expect(s.calls[0]!.body).toEqual({ refs: [{ bucket_id: B1, name: "a.png" }] });
});

test("a public bucket's URL comes back as the server gives it, made absolute", async () => {
  const s = server((r) => `/storage/v1/object/public/br/pub/${r.name}`);
  const storage = createClient({ url, key, fetch: s.fetch }).storage;
  expect(await storage.resolve({ bucket_id: B1, name: "logo.png" })).toBe(`${url}/storage/v1/object/public/br/pub/logo.png`);
});

test("#102: an object id (a foreign key) and a bucket's name with a path resolve in the same batch", async () => {
  const s = server((r) => `/storage/v1/object/sign/b/${r.name ?? r.id}?token=t`);
  const storage = createClient({ url, key, fetch: s.fetch }).storage;
  const refs: StorageRefLike[] = [
    { id: B2 },
    { bucket: "avatars", name: "u1/me.png" },
    { bucket_id: B1, name: "a.png" },
  ];
  const urls = await storage.resolve(refs);
  expect(urls).toEqual([
    `${url}/storage/v1/object/sign/b/${B2}?token=t`,
    `${url}/storage/v1/object/sign/b/u1/me.png?token=t`,
    `${url}/storage/v1/object/sign/b/a.png?token=t`,
  ]);
  expect(s.calls).toHaveLength(1);
  // Each ref is sent as its own shape, nothing more.
  expect(s.calls[0]!.body).toEqual({ refs: [{ id: B2 }, { bucket: "avatars", name: "u1/me.png" }, { bucket_id: B1, name: "a.png" }] });
  expect(await storage.resolve({ id: B1 })).toBe(`${url}/storage/v1/object/sign/b/${B1}?token=t`);
});

test("#102: extra fields on a ref (a whole row) are not sent", async () => {
  const s = server(signed);
  const storage = createClient({ url, key, fetch: s.fetch }).storage;
  const row = { bucket_id: B1, name: "a.png", owner: "u1" } as StorageRef;
  await storage.resolve([row, { id: B2, other: 1 } as StorageRefLike]);
  expect(s.calls[0]!.body).toEqual({ refs: [{ bucket_id: B1, name: "a.png" }, { id: B2 }] });
});
