import { afterEach, describe, expect, test, vi } from "vitest";
import { createClient, UnsafeIntegerError, type TypeMap } from "./index.ts";
import { type Database, types } from "./test-db.ts";
import { fakeFetch } from "./test-fetch.ts";

const opts = { url: "http://localhost:8000", key: "chk_pub_x" };
const UNSAFE = "9007199254740993"; // 2^53 + 1
const NUMERIC30 = "123456789012345678901234567890";

function typed(body: string) {
  return createClient<Database>({ ...opts, types, fetch: fakeFetch(body) });
}
function typedBig(body: string) {
  return createClient<Database>({ ...opts, types, bigint: true, fetch: fakeFetch(body) });
}
async function rejection(p: PromiseLike<unknown>): Promise<unknown> {
  return Promise.resolve(p).then(
    () => {
      throw new Error("expected a rejection");
    },
    (e: unknown) => e,
  );
}
function sentBody(fetch: { calls: { init: RequestInit }[] }): string {
  return fetch.calls[0]!.init.body as string;
}

afterEach(() => {
  vi.restoreAllMocks();
});

describe("a function returning a single row", () => {
  test("default mode throws for an int8 outside the safe range", async () => {
    const err = await rejection(typed(`{"id":${UNSAFE},"amount":1,"label":"a"}`).call("latest_event"));
    expect(err).toBeInstanceOf(UnsafeIntegerError);
    expect(err).toMatchObject({ column: "id", source: UNSAFE });
  });

  test("bigint mode reads the int8 exactly and numeric as an exact string", async () => {
    const row = await typedBig(`{"id":${UNSAFE},"amount":${NUMERIC30},"label":"a"}`).call("latest_event");
    expect(row).toEqual({ id: 9007199254740993n, amount: NUMERIC30, label: "a" });
  });

  test("a 30-digit numeric comes back as an exact string in number mode", async () => {
    const row = await typed(`{"id":1,"amount":${NUMERIC30},"label":"a"}`).call("latest_event");
    expect(row).toEqual({ id: 1, amount: NUMERIC30, label: "a" });
  });

  test("an unmapped key is checked", async () => {
    const err = await rejection(typed(`{"id":1,"amount":1,"label":"a","extra":${UNSAFE}}`).call("latest_event"));
    expect(err).toBeInstanceOf(UnsafeIntegerError);
    expect(err).toMatchObject({ column: "extra" });
  });
});

describe("reading int8", () => {
  test("default mode reads a safe int8 as a number", async () => {
    const rows = await typed('[{"id":42,"qty":3}]').from("orders").select("id", "qty");
    expect(rows).toEqual([{ id: 42, qty: 3 }]);
    expect(typeof rows[0]!.id).toBe("number");
  });

  test("default mode throws UnsafeIntegerError for an int8 outside the safe range", async () => {
    const err = await rejection(typed(`[{"id":${UNSAFE}}]`).from("orders").select("id"));
    expect(err).toBeInstanceOf(UnsafeIntegerError);
    expect(err).toMatchObject({ name: "UnsafeIntegerError", column: "id", source: UNSAFE });
    expect((err as Error).message).toContain("bigint: true");
    expect((err as Error).message).toContain(UNSAFE);
  });

  test("bigint: true reads every int8 as an exact bigint", async () => {
    const rows = await typedBig(`[{"id":${UNSAFE},"qty":3},{"id":1,"qty":4}]`)
      .from("orders")
      .select("id", "qty");
    expect(rows).toEqual([
      { id: 9007199254740993n, qty: 3 },
      { id: 1n, qty: 4 },
    ]);
  });

  test("null int8 values stay null", async () => {
    const rows = await typedBig('[{"seq":null}]').from("events").select("seq");
    expect(rows).toEqual([{ seq: null }]);
  });

  test("int8 array columns convert each element", async () => {
    const rows = await typedBig(`[{"ids":[1,${UNSAFE}]}]`).from("events").select("ids");
    expect(rows).toEqual([{ ids: [1n, 9007199254740993n] }]);
    const err = await rejection(typed(`[{"ids":[1,${UNSAFE}]}]`).from("events").select("ids"));
    expect(err).toMatchObject({ column: "ids", source: UNSAFE });
  });

  test("aliases are honoured", async () => {
    const rows = await typedBig(`[{"order_id":${UNSAFE},"amount":${NUMERIC30}}]`)
      .from("orders")
      .select({ col: "id", as: "order_id" }, { col: "total", as: "amount" });
    expect(rows).toEqual([{ order_id: 9007199254740993n, amount: NUMERIC30 }]);
    const err = await rejection(
      typed(`[{"order_id":${UNSAFE}}]`).from("orders").select({ col: "id", as: "order_id" }),
    );
    expect(err).toMatchObject({ column: "order_id" });
  });

  test("returning keys are honoured, including returning(\"*\")", async () => {
    const some = await typedBig(`[{"id":${UNSAFE}}]`)
      .from("orders")
      .insert([{ total: "1", region: "eu" }])
      .returning("id");
    expect(some).toEqual([{ id: 9007199254740993n }]);
    const all = await typedBig(`[{"id":${UNSAFE},"total":1.50,"region":"eu"}]`)
      .from("orders")
      .update({ region: "eu" })
      .where({ id: 1n })
      .returning("*");
    expect(all).toEqual([{ id: 9007199254740993n, total: "1.50", region: "eu" }]);
  });

  test("a qualified table uses its own schema's map", async () => {
    const rows = await typed(`[{"amount":${NUMERIC30}.25}]`).from("billing.invoices").select("amount");
    expect(rows).toEqual([{ amount: `${NUMERIC30}.25` }]);
  });

  test("only top-level row keys are exact; jsonb contents are untouched", async () => {
    const rows = await typed(`[{"id":1,"meta":{"id":${UNSAFE},"total":1.50}}]`)
      .from("orders")
      .select("id", "meta");
    expect(rows).toEqual([{ id: 1, meta: { id: 9007199254740992, total: 1.5 } }]);
  });
});

describe("reading numeric", () => {
  test("numeric keeps its exact digits as a string (30 digits)", async () => {
    const rows = await typed(`[{"total":${NUMERIC30}},{"total":12.50},{"total":-0.000001}]`)
      .from("orders")
      .select("total");
    expect(rows).toEqual([{ total: NUMERIC30 }, { total: "12.50" }, { total: "-0.000001" }]);
  });

  test("numeric is a string in bigint mode too", async () => {
    const rows = await typedBig(`[{"total":${NUMERIC30}}]`).from("orders").select("total");
    expect(rows).toEqual([{ total: NUMERIC30 }]);
  });
});

describe("calls", () => {
  test("a scalar numeric return is an exact string", async () => {
    const spent = await typed(`${NUMERIC30}.5`).call("total_spent", { user_id: "u" });
    expect(spent).toBe(`${NUMERIC30}.5`);
  });

  test("a scalar int8 return follows the mode", async () => {
    expect(await typedBig(UNSAFE).call("next_id")).toBe(9007199254740993n);
    expect(await typed("12").call("next_id")).toBe(12);
    const err = await rejection(typed(UNSAFE).call("next_id"));
    expect(err).toMatchObject({ column: "next_id", source: UNSAFE });
  });

  test("a set-returning scalar converts each element", async () => {
    expect(await typedBig(`[1,${UNSAFE}]`).call("big_ids")).toEqual([1n, 9007199254740993n]);
  });

  test("a set-returning function's row columns are exact", async () => {
    const rows = await typedBig(`[{"id":${UNSAFE},"amount":${NUMERIC30},"label":"a"}]`).call("top_events");
    expect(rows).toEqual([{ id: 9007199254740993n, amount: NUMERIC30, label: "a" }]);
  });

  test("a qualified call uses its schema's map", async () => {
    expect(await typedBig(UNSAFE).call("public.next_id")).toBe(9007199254740993n);
  });
});

describe("batch", () => {
  test("each operation's result uses its own keys", async () => {
    const db = typedBig(`[[{"id":${UNSAFE}}],[{"amount":${NUMERIC30}}],${UNSAFE},[{"qty":3}]]`);
    const result = await db.batch((tx) => [
      tx.from("orders").select("id"),
      tx.from("billing.invoices").select("amount"),
      tx.call("next_id"),
      tx.from("orders").select("qty"),
    ]);
    expect(result).toEqual([[{ id: 9007199254740993n }], [{ amount: NUMERIC30 }], 9007199254740993n, [{ qty: 3 }]]);
  });
});

describe("values the map doesn't cover", () => {
  test("an unmapped top-level integer outside the safe range throws (a column added after gen types)", async () => {
    for (const db of [typed, typedBig]) {
      const err = await rejection(db(`[{"id":1,"newcol":${UNSAFE}}]`).from("orders"));
      expect(err).toBeInstanceOf(UnsafeIntegerError);
      expect(err).toMatchObject({ column: "newcol", source: UNSAFE });
    }
  });

  test("an unmapped array of scalars is checked too", async () => {
    const err = await rejection(typed(`[{"id":1,"nums":[1,[2,${UNSAFE}]]}]`).from("orders"));
    expect(err).toMatchObject({ column: "nums", source: UNSAFE });
  });

  test("unmapped safe values and json objects stay as they are", async () => {
    const rows = await typed(`[{"id":1,"newcol":7,"meta":{"x":${UNSAFE}},"tags":["a"]}]`).from("orders");
    expect(rows).toEqual([{ id: 1, newcol: 7, meta: { x: 9007199254740992 }, tags: ["a"] }]);
  });

  test("a mapped function without returns or columns checks its result", async () => {
    const err = await rejection(typed(UNSAFE).call("add_points", { user_id: "u" }));
    expect(err).toMatchObject({ column: "add_points", source: UNSAFE });
    expect(await typed("5").call("add_points", { user_id: "u" })).toBe(5);
  });

  test("an unmapped function checks its scalar result", async () => {
    const err = await rejection(typed(UNSAFE).call("billing.void_invoice", { id: "x" }));
    expect(err).toMatchObject({ column: "billing.void_invoice", source: UNSAFE });
  });

  test("a set-returning function's unmapped columns are checked", async () => {
    const err = await rejection(typed(`[{"id":1,"amount":1,"label":"a","extra":${UNSAFE}}]`).call("top_events"));
    expect(err).toMatchObject({ column: "extra" });
  });
});

describe("without types", () => {
  test("any integer literal outside the safe range throws, naming its key", async () => {
    const db = createClient({ ...opts, fetch: fakeFetch(`[{"n":1,"x":{"deep":${UNSAFE}}}]`) });
    const err = await rejection(db.from("anything"));
    expect(err).toBeInstanceOf(UnsafeIntegerError);
    expect(err).toMatchObject({ column: "deep", source: UNSAFE });
  });

  test("other numbers stay numbers", async () => {
    const db = createClient({ ...opts, fetch: fakeFetch('[{"a":1,"b":12.50,"c":1e300,"d":-7}]') });
    expect(await db.from("anything")).toEqual([{ a: 1, b: 12.5, c: 1e300, d: -7 }]);
  });
});

describe("writing", () => {
  test("a bigint in insert values is serialised exactly", async () => {
    const fetch = fakeFetch("[]");
    const db = createClient<Database>({ ...opts, types, bigint: true, fetch });
    await db.from("events").insert([{ amount: NUMERIC30, label: "x", seq: 9223372036854775807n }]);
    expect(sentBody(fetch)).toBe(
      `{"v":1,"op":"insert","into":"events","values":[{"amount":"${NUMERIC30}","label":"x","seq":9223372036854775807}]}`,
    );
  });

  test("bigints in where and update are serialised exactly", async () => {
    const fetch = fakeFetch("[]");
    const db = createClient<Database>({ ...opts, types, bigint: true, fetch });
    await db
      .from("events")
      .update({ seq: -9223372036854775808n })
      .where({ id: { in: [9007199254740993n] }, seq: 9007199254740995n });
    expect(sentBody(fetch)).toBe(
      '{"v":1,"op":"update","table":"events","set":{"seq":-9223372036854775808},' +
        '"where":{"and":[{"col":"id","in":[9007199254740993]},{"col":"seq","eq":9007199254740995}]}}',
    );
  });

  test("a bigint call argument is serialised exactly", async () => {
    const fetch = fakeFetch("[]");
    const db = createClient<Database>({ ...opts, types, bigint: true, fetch });
    await db.call("big_ids", { since: 9007199254740993n });
    expect(sentBody(fetch)).toBe('{"v":1,"op":"call","fn":"big_ids","args":{"since":9007199254740993}}');
  });

  test("JSON.stringify semantics are kept for everything else", async () => {
    const fetch = fakeFetch("[]");
    const db = createClient({ ...opts, fetch });
    await db.from("t").insert([
      { s: 'a"\n', n: Number.NaN, d: new Date(0), u: undefined, arr: [undefined, 1], o: { z: null } },
    ]);
    expect(sentBody(fetch)).toBe(
      '{"v":1,"op":"insert","into":"t","values":[{"s":"a\\"\\n","n":null,' +
        '"d":"1970-01-01T00:00:00.000Z","arr":[null,1],"o":{"z":null}}]}',
    );
  });
});

describe("runtimes without JSON.parse source text access", () => {
  function withoutSourceText() {
    const parse = JSON.parse;
    vi.spyOn(JSON, "parse").mockImplementation((text: string, reviver?: (k: string, v: unknown) => unknown) =>
      parse(
        text,
        reviver
          ? function (this: unknown, k: string, v: unknown) {
              return reviver.call(this, k, v);
            }
          : undefined,
      ),
    );
  }

  test("bigint: true throws at createClient with the runtime requirement", () => {
    withoutSourceText();
    expect(() => createClient<Database>({ ...opts, types, bigint: true })).toThrow(/source text.*Node 21/s);
  });

  test("types with a numeric column throw at createClient", () => {
    withoutSourceText();
    expect(() => createClient<Database>({ ...opts, types })).toThrow(/numeric/);
  });

  test("default mode still refuses unsafe int8 values", async () => {
    withoutSourceText();
    const int8Only = {
      schemas: { public: { tables: { orders: { id: "int8" } }, functions: {} } },
    } as const satisfies TypeMap;
    const db = createClient<Database>({ ...opts, types: int8Only, fetch: fakeFetch(`[{"id":${UNSAFE}}]`) });
    const err = await rejection(db.from("orders").select("id"));
    expect(err).toBeInstanceOf(UnsafeIntegerError);
    expect(err).toMatchObject({ column: "id" });
    const safe = createClient<Database>({ ...opts, types: int8Only, fetch: fakeFetch('[{"id":5}]') });
    expect(await safe.from("orders").select("id")).toEqual([{ id: 5 }]);
  });

  test("the fallback leaves floats written with an exponent alone", async () => {
    withoutSourceText();
    const db = createClient({ ...opts, fetch: fakeFetch('[{"a":1e300,"b":-2.5e30,"c":1.5}]') });
    expect(await db.from("t")).toEqual([{ a: 1e300, b: -2.5e30, c: 1.5 }]);
  });

  test("default mode without types still refuses unsafe integers", async () => {
    withoutSourceText();
    const db = createClient({ ...opts, fetch: fakeFetch(`[{"n":${UNSAFE}}]`) });
    expect(await rejection(db.from("t"))).toBeInstanceOf(UnsafeIntegerError);
  });
});
