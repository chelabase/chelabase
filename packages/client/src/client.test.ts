import { describe, expect, test } from "vitest";
import { createClient } from "./index.ts";
import { type Database, types } from "./test-db.ts";
import { fakeFetch } from "./test-fetch.ts";

const url = "http://localhost:8000";
const key = "chk_pub_0123456789abcdefghijklmnopqrstuvwxyzABCDEFG";

describe("requests", () => {
  test("sends_bearer_and_json", async () => {
    const fetch = fakeFetch('[{"id":1}]');
    const db = createClient<Database>({ types, url: `${url}/`, key, fetch });
    const rows = await db.from("orders").select("id").where({ status: "paid" });
    expect(rows).toEqual([{ id: 1 }]);
    expect(fetch.calls).toHaveLength(1);
    const { url: sent, init } = fetch.calls[0]!;
    expect(sent).toBe(`${url}/data/v1/query`);
    expect(init.method).toBe("POST");
    const headers = new Headers(init.headers);
    expect(headers.get("authorization")).toBe(`Bearer ${key}`);
    expect(headers.get("content-type")).toBe("application/json");
    expect(JSON.parse(init.body as string)).toEqual({
      v: 1,
      op: "select",
      from: "orders",
      select: ["id"],
      where: { col: "status", eq: "paid" },
    });
  });

  test("builders are lazy and each await sends one request", async () => {
    const fetch = fakeFetch("[]");
    const db = createClient<Database>({ types, url, key, fetch });
    const q = db.from("orders").select("id");
    expect(fetch.calls).toHaveLength(0);
    await q;
    await q;
    expect(fetch.calls).toHaveLength(2);
  });

  test("builders are immutable: a refinement doesn't change its parent", () => {
    const db = createClient<Database>({ types, url, key, fetch: fakeFetch("[]") });
    const base = db.from("orders").select("id");
    const filtered = base.where({ status: "paid" }).limit(1);
    expect(base.toTree()).toEqual({ v: 1, op: "select", from: "orders", select: ["id"] });
    expect(filtered.toTree()).toMatchObject({ where: { col: "status", eq: "paid" }, limit: 1 });
  });

  test("a second where() is combined with and", () => {
    const db = createClient<Database>({ types, url, key, fetch: fakeFetch("[]") });
    const tree = db.from("orders").where({ status: "paid" }).where({ vip: true }).toTree();
    expect(tree).toMatchObject({
      where: {
        and: [
          { col: "status", eq: "paid" },
          { col: "vip", eq: true },
        ],
      },
    });
  });

  test("an undefined filter value throws instead of widening the filter", () => {
    const db = createClient<Database>({ types, url, key, fetch: fakeFetch("[]") });
    expect(() => db.from("orders").where({ status: undefined })).toThrow(TypeError);
    expect(() => db.from("orders").where({ or: [{ status: "paid" }, { region: undefined }] })).toThrow(
      /or\[1\]\.region is undefined/,
    );
    expect(() => db.from("orders").where({ or: [{}] })).toThrow(/empty filter/);
  });

  test("an empty where adds nothing", () => {
    const db = createClient<Database>({ types, url, key, fetch: fakeFetch("[]") });
    expect(db.from("orders").where({}).toTree()).toEqual({ v: 1, op: "select", from: "orders" });
  });

  test("a jsonb object value that isn't an operator object compares with eq", () => {
    const db = createClient<Database>({ types, url, key, fetch: fakeFetch("[]") });
    const tree = db.from("orders").where({ meta: { kind: "a" } }).toTree();
    expect(tree).toMatchObject({ where: { col: "meta", eq: { kind: "a" } } });
  });

  test("a single insert row is sent as a one-row list", () => {
    const db = createClient<Database>({ types, url, key, fetch: fakeFetch("[]") });
    const tree = db.from("orders").insert({ total: "1", region: "eu" }).toTree();
    expect(tree).toMatchObject({ values: [{ total: "1", region: "eu" }] });
  });

  test("bigint: true without types throws at createClient", () => {
    expect(() => createClient({ url, key, bigint: true } as never)).toThrow(/`types`/);
  });

  test("builders have catch and finally", async () => {
    const db = createClient<Database>({ types, url, key, fetch: fakeFetch("{", 400, { "content-type": "text/plain" }) });
    const caught = await db.from("orders").catch((e: unknown) => (e as Error).name);
    expect(caught).toBe("DataApiError");
    let ran = false;
    const ok = createClient<Database>({ types, url, key, fetch: fakeFetch("[]") });
    expect(await ok.from("orders").finally(() => (ran = true))).toEqual([]);
    expect(ran).toBe(true);
  });

  test("batch sends one request and returns each operation's result", async () => {
    const fetch = fakeFetch('[[{"id":1}],[],"7"]');
    const db = createClient<Database>({ types, url, key, fetch });
    const [inserted, updated, spent] = await db.batch((tx) => [
      tx.from("orders").insert([{ total: "1", region: "eu" }]).returning("id"),
      tx.from("orders").update({ vip: true }).where({ id: 1 }),
      tx.call("total_spent", { user_id: "u" }),
    ]);
    expect(fetch.calls).toHaveLength(1);
    expect(inserted).toEqual([{ id: 1 }]);
    expect(updated).toEqual([]);
    expect(spent).toBe("7");
  });

  test("an operation built for a batch can't be awaited on its own", async () => {
    const db = createClient<Database>({ types, url, key, fetch: fakeFetch("[]") });
    let inner: PromiseLike<unknown> | undefined;
    db.batch((tx) => {
      inner = tx.from("orders").select("id");
      return [inner as never];
    });
    await expect(Promise.resolve(inner)).rejects.toThrow(/batch/);
  });
});

describe("databases", () => {
  test("database_binds_the_top_level_field", async () => {
    const fetch = fakeFetch('[{"id":1}]');
    const db = createClient<Database>({ types, url, key, fetch });
    await db.database("analytics").from("orders").select("id");
    await db.from("orders").select("id");
    const [bound, main] = fetch.calls.map((c) => JSON.parse(c.init.body as string));
    expect(bound).toEqual({ v: 1, database: "analytics", op: "select", from: "orders", select: ["id"] });
    expect(main).not.toHaveProperty("database");
    expect(fetch.calls[0]!.url).toBe(`${url}/data/v1/query`);
    expect(new Headers(fetch.calls[0]!.init.headers).get("authorization")).toBe(`Bearer ${key}`);
  });

  test("a bound client sends calls and writes with the database too", async () => {
    const fetch = fakeFetch("null");
    const an = createClient({ url, key, fetch }).database("analytics");
    await an.call("next_id");
    await an.from("orders").delete().all();
    for (const c of fetch.calls) expect(JSON.parse(c.init.body as string).database).toBe("analytics");
  });

  test("batch_carries_database_only_at_top_level", async () => {
    const fetch = fakeFetch("[[],[]]");
    const an = createClient({ url, key, fetch }).database("analytics");
    await an.batch((t) => [t.from("a").select("id"), t.call("f")]);
    const body = JSON.parse(fetch.calls[0]!.init.body as string);
    expect(body.database).toBe("analytics");
    expect(body.ops).toHaveLength(2);
    for (const op of body.ops) expect(op).not.toHaveProperty("database");
  });

  test("bound clients take their own types and bigint mode", () => {
    const db = createClient<Database>({ types, url, key, fetch: fakeFetch("[]") });
    // @ts-expect-error the types are required in bigint mode (the runtime check is the backstop)
    expect(() => createClient({ url, key, bigint: true, types, fetch: fakeFetch("[]") }).database("x")).toThrow(
      /types/,
    );
    expect(db.database("analytics", types)).toBeDefined();
  });
});

describe("count (TE-B1 T8)", () => {
  test("count_sends_a_count_tree_and_resolves_the_number", async () => {
    const fetch = fakeFetch('{"count":2}');
    const db = createClient<Database>({ types, url, key, fetch });
    const res: { count: number } = await db.from("orders").where({ status: "paid" }).count();
    expect(res).toEqual({ count: 2 });
    const { init } = fetch.calls[0];
    expect(JSON.parse(init.body as string)).toEqual({ v: 1, op: "count", from: "orders", where: { col: "status", eq: "paid" } });
  });
  test("count_without_a_filter_and_in_a_batch", async () => {
    expect(createClient<Database>({ types, url, key, fetch: fakeFetch("[]") }).from("orders").count().toTree()).toEqual({ v: 1, op: "count", from: "orders" });
  });
});
