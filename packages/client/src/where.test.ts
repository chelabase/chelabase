import { describe, expect, test } from "vitest";
import { createClient, op } from "./index.ts";
import { type Database, types } from "./test-db.ts";
import { fakeFetch } from "./test-fetch.ts";

const db = createClient<Database>({ types, url: "http://localhost:8000", key: "chk_pub_x", fetch: fakeFetch("[]") });
const where = (filter: object) => (db.from("orders") as unknown as { where(f: object): { toTree(): { where?: unknown } } }).where(filter).toTree().where;

describe("where", () => {
  test("bare_objects_are_equality_values", () => {
    expect(where({ meta: { eq: 1 } })).toEqual({ col: "meta", eq: { eq: 1 } });
    for (const key of ["eq", "neq", "lt", "lte", "gt", "gte", "in", "is", "like", "ilike"]) {
      const parsed = JSON.parse(`{"${key}": 1}`);
      expect(where({ meta: parsed })).toEqual({ col: "meta", eq: { [key]: 1 } });
    }
    const several = JSON.parse('{"gte": 1, "lt": 2, "in": [3]}');
    expect(where({ meta: several })).toEqual({ col: "meta", eq: several });
    expect(where({ meta: Object.assign(Object.create(null), { eq: 1 }) })).toEqual({ col: "meta", eq: { eq: 1 } });
  });

  test("op_helpers_compile_to_operators", () => {
    expect(where({ total: op.gte("10") })).toEqual({ col: "total", gte: "10" });
    expect(where({ total: op.all(op.gte("10"), op.lt("20")) })).toEqual({
      and: [
        { col: "total", gte: "10" },
        { col: "total", lt: "20" },
      ],
    });
    expect(where({ region: op.in(["eu"]), vip: op.is(true), status: op.is(null) })).toEqual({
      and: [
        { col: "region", in: ["eu"] },
        { col: "vip", is: true },
        { col: "status", is: null },
      ],
    });
  });

  test("lookalike_objects_are_not_operators", () => {
    const hand = { op: "gte", value: "10", ops: [["gte", "10"]], entries: [["gte", "10"]] };
    expect(where({ total: hand })).toEqual({ col: "total", eq: hand });
    const symbols = Object.fromEntries(Object.getOwnPropertySymbols(op.eq(1)).map((s) => [s, true]));
    expect(where({ meta: symbols })).toEqual({ col: "meta", eq: symbols });
    expect(where({ meta: { ...op.gte(1) } })).toEqual({ col: "meta", eq: {} });
    expect(() => op.all({ gte: 1 } as never)).toThrow(TypeError);
  });

  test("op_all_with_no_operators_is_refused", () => {
    expect(() => where({ status: "pending", qty: (op.all as () => never)() })).toThrow(TypeError);
    expect(() => where({ status: "pending", qty: (op.all as (...a: unknown[]) => never)(...([] as unknown[])) })).toThrow(
      "op.all() needs at least one operator",
    );
    expect(() => (op.all as (...a: unknown[]) => unknown)((op.all as () => never)())).toThrow(TypeError);
  });

  test("op_values_in_value_position_are_refused", () => {
    expect(() => where({ and: [{ col: "qty", eq: op.gte(1) }] })).toThrow(TypeError);
    expect(() => where({ qty: op.eq(op.gte(1)) })).toThrow(TypeError);
  });
});
