import { describe, expect, test } from "vitest";
import { UnsafeIntegerError } from "./error.ts";
import { createClient } from "./index.ts";
import { type Database, types } from "./test-db.ts";
import { fakeFetch } from "./test-fetch.ts";

const url = "http://localhost:8000";
const key = "chk_pub_0123456789abcdefghijklmnopqrstuvwxyzABCDEFG";

describe("count", () => {
  test("a count is a number in bigint mode too", async () => {
    const db = createClient<Database>({ types, url, key, bigint: true, fetch: fakeFetch('{"count":9007199254740991}') });
    const res: { count: number } = await db.from("orders").count();
    expect(res).toEqual({ count: 9007199254740991 });
  });

  test("a count beyond the safe range throws", async () => {
    const db = createClient<Database>({ types, url, key, fetch: fakeFetch('{"count":9007199254740993}') });
    await expect(db.from("orders").count()).rejects.toBeInstanceOf(UnsafeIntegerError);
  });

  test("select and orderBy are left out; limit and offset are refused", () => {
    const db = createClient<Database>({ types, url, key, fetch: fakeFetch("[]") });
    expect(db.from("orders").select("id").orderBy("id").where({ vip: true }).count().toTree()).toEqual({
      v: 1,
      op: "count",
      from: "orders",
      where: { col: "vip", eq: true },
    });
    expect(() => db.from("orders").limit(1).count()).toThrow(TypeError);
    expect(() => db.from("orders").offset(1).count()).toThrow(TypeError);
  });

  test("a count in a batch resolves to its own result", async () => {
    const fetch = fakeFetch('[{"count":3},[{"id":1}]]');
    const db = createClient<Database>({ types, url, key, fetch });
    const [n, rows] = await db.batch((tx) => [tx.from("orders").where({ vip: true }).count(), tx.from("orders").select("id")]);
    const typed: { count: number } = n;
    expect(typed).toEqual({ count: 3 });
    expect(rows).toEqual([{ id: 1 }]);
    expect(JSON.parse(fetch.calls[0]!.init.body as string)).toEqual({
      v: 1,
      op: "batch",
      ops: [
        { op: "count", from: "orders", where: { col: "vip", eq: true } },
        { op: "select", from: "orders", select: ["id"] },
      ],
    });
  });
});
