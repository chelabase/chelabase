import { describe, expect, test } from "vitest";
import { types as golden } from "../fixtures/gen/db.ts";
import { types as goldenMulti } from "../fixtures/gen/multi.ts";
import { createClient, DataApiError, fetchTypes } from "./index.ts";
import { type Database, types } from "./test-db.ts";
import { fakeFetch } from "./test-fetch.ts";

const url = "http://localhost:8000/";
const secret = "chk_sec_0123456789abcdefghijklmnopqrstuvwxyzABCDEFG";
const UNSAFE = "9007199254740993"; // 2^53 + 1

async function rejection(p: PromiseLike<unknown>): Promise<unknown> {
  return Promise.resolve(p).then(
    () => {
      throw new Error("expected a rejection");
    },
    (e: unknown) => e,
  );
}

describe("fetchTypes", () => {
  test("GETs the runtime map with the secret key and returns it", async () => {
    const fetch = fakeFetch(JSON.stringify(golden));
    const got = await fetchTypes({ url, key: secret, fetch });
    expect(got).toEqual(golden);
    expect(fetch.calls).toHaveLength(1);
    const call = fetch.calls[0]!;
    expect(call.url).toBe("http://localhost:8000/data/v1/types?format=json");
    expect(call.init.method).toBe("GET");
    expect(call.init.headers).toMatchObject({ authorization: `Bearer ${secret}`, accept: "application/json" });
  });

  test("names the database, or asks for every database keyed by name", async () => {
    const one = fakeFetch(JSON.stringify(golden));
    await fetchTypes({ url, key: secret, database: "analytics db", fetch: one });
    expect(one.calls[0]!.url).toBe("http://localhost:8000/data/v1/types?format=json&database=analytics+db");

    const all = fakeFetch(JSON.stringify(goldenMulti));
    const every = await fetchTypes({ url, key: secret, allDatabases: true, fetch: all });
    expect(all.calls[0]!.url).toBe("http://localhost:8000/data/v1/types?format=json&all_databases=true");
    expect(every).toEqual(goldenMulti);
    expect(Object.keys(every)).toEqual(["main", "analytics"]);

    // Only main reachable: the server sends the one-database map; it comes back as `main`.
    const plain = await fetchTypes({ url, key: secret, allDatabases: true, fetch: fakeFetch(JSON.stringify(golden)) });
    expect(plain).toEqual({ main: golden });
  });

  test("refuses a public key, or anything else, before any request, without echoing it", async () => {
    const fetch = fakeFetch("{}");
    const pub = "chk_pub_0123456789abcdefghijklmnopqrstuvwxyzABCDEFG";
    for (const key of [pub, "nonsense", "", ` ${secret}`]) {
      const err = await rejection(fetchTypes({ url, key, fetch }));
      expect(err).toBeInstanceOf(Error);
      expect((err as Error).message).toContain("secret key (chk_sec_");
      if (key !== "") expect((err as Error).message).not.toContain(key.trim());
    }
    const both = await rejection(fetchTypes({ url, key: secret, database: "a", allDatabases: true, fetch }));
    expect((both as Error).message).toContain("database");
    expect(fetch.calls).toHaveLength(0);
  });

  test("a non-2xx answer is a DataApiError with the server's code, without the key", async () => {
    const body = JSON.stringify({ error: { code: "unauthorized", message: "the schema needs a secret key" } });
    const err = await rejection(fetchTypes({ url, key: secret, fetch: fakeFetch(body, 401) }));
    expect(err).toBeInstanceOf(DataApiError);
    expect(err).toMatchObject({ status: 401, code: "unauthorized", message: "the schema needs a secret key" });
    expect(JSON.stringify(err)).not.toContain(secret);
    expect((err as Error).message).not.toContain(secret);

    const proxy = await rejection(fetchTypes({ url, key: secret, fetch: fakeFetch("<html>bad gateway</html>", 502) }));
    expect(proxy).toMatchObject({ status: 502, code: "unavailable" });

    // A 2xx that isn't a types map: `internal`.
    for (const odd of ["not json", "[]", "null", '{"schemas":1}']) {
      const e = await rejection(fetchTypes({ url, key: secret, fetch: fakeFetch(odd) }));
      expect(e).toMatchObject({ code: "internal" });
    }
  });

  test("createClient with the fetched types decodes int8 as the generated module does", async () => {
    const fetched = await fetchTypes({ url, key: secret, fetch: fakeFetch(JSON.stringify(types)) });
    const rows = `[{"id":${UNSAFE},"qty":3}]`;
    const live = createClient<Database>({ url, key: "chk_pub_x", types: fetched, bigint: true, fetch: fakeFetch(rows) });
    const built = createClient<Database>({ url, key: "chk_pub_x", types, bigint: true, fetch: fakeFetch(rows) });
    const a = await live.from("orders").select("id", "qty");
    const b = await built.from("orders").select("id", "qty");
    expect(a).toEqual([{ id: 9007199254740993n, qty: 3 }]);
    expect(a).toEqual(b);
  });
});
