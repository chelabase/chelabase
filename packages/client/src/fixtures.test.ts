// The contract with the Rust parser: each `fixtures/<name>.json` holds `{ call, tree }`.
// Here the same call is built in TypeScript and its `toTree()` must deep-equal `tree`;
// `services/rest/tests/fixtures.rs` parses every `tree`.
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, test } from "vitest";
import { createClient } from "./index.ts";
import { stringify } from "./json.ts";
import { type Database, types } from "./test-db.ts";

const dir = join(import.meta.dirname, "..", "fixtures");
const opts = { url: "http://localhost:8000", key: "chk_pub_x" };
const db = createClient<Database>({ ...opts, types });
const big = createClient<Database>({ ...opts, bigint: true, types });

const builders: Record<string, () => { toTree(): unknown }> = {
  "count-filtered": () => db.from("billing.invoices").where({ paid: false }).count(),
  "select-basic": () =>
    db
      .from("orders")
      .select("id", "total", { col: "customer_name", as: "customer" })
      .where({ status: "paid", total: { gte: "100" } })
      .orderBy("created_at", "desc")
      .limit(50),
  "select-everything": () => db.from("orders"),
  "select-operators": () =>
    db
      .from("orders")
      .select("id")
      .where({
        and: [
          { status: { eq: "paid" } },
          { region: { neq: "eu" } },
          { qty: { lt: 10 } },
          { qty: { lte: 9 } },
          { total: { gt: "1.00" } },
          { total: { gte: "1.50" } },
          { region: { in: ["eu", "us"] } },
          { vip: { is: true } },
          { customer_name: { like: "A%" } },
          { region: { ilike: "e%" } },
        ],
      }),
  "select-nested-filters": () =>
    db.from("orders").where({
      status: "paid",
      or: [{ region: { in: ["eu"] } }, { vip: true, qty: { gt: 2 } }],
      not: { customer_name: null },
    }),
  "select-several-operators-on-one-column": () =>
    db.from("orders").select("id").where({ qty: { gte: 1, lt: 10 } }),
  "select-alias": () => db.from("orders").select({ col: "customer_name", as: "customer" }, "id"),
  "select-order-nulls": () =>
    db
      .from("orders")
      .orderBy("created_at", "desc", "last")
      .orderBy("id")
      .orderBy("customer_name", "asc", "first")
      .limit(10)
      .offset(20),
  "select-qualified": () => db.from("billing.invoices").select("id", "amount").where({ paid: false }),
  "select-combinator-named-column": () =>
    db.from("orders").where({ and: [{ col: "and", eq: "x" }, { status: "paid" }] }),
  "select-big-values": () =>
    big.from("events").where({
      id: { in: [9007199254740993n, 1n] },
      amount: { gt: "123456789012345678901234567890.12" },
    }),
  "insert-many": () =>
    db
      .from("orders")
      .insert([
        { total: "12.50", region: "eu" },
        { total: "3.00", region: "us" },
      ])
      .returning("id", "total"),
  "insert-big-values": () =>
    big.from("events").insert({
      amount: "123456789012345678901234567890",
      label: "x",
      seq: 9007199254740993n,
      ids: [1n, 9223372036854775807n],
    }),
  "update-where": () =>
    db.from("orders").update({ status: "shipped" }).where({ id: 7 }).returning("*"),
  "update-all": () => db.from("orders").update({ vip: false }).all(),
  "delete-where": () =>
    db
      .from("orders")
      .delete()
      .where({ status: "pending", created_at: { lt: "2026-01-01T00:00:00Z" } }),
  "delete-all": () => db.from("billing.invoices").delete().all().returning("id"),
  call: () => db.call("add_points", { user_id: "7c9e6679-7425-40de-944b-e07fc1f90ae7", n: 5 }),
  "call-qualified-no-args": () => db.call("public.next_id"),
  batch: () =>
    db.batch((tx) => [
      tx.from("orders").insert([{ total: "1.00", region: "eu" }]).returning("id"),
      tx.from("events").update({ label: "x" }).where({ id: 1 }),
      tx.from("billing.invoices").select("id").limit(1),
      tx.call("add_points", { user_id: "u" }),
    ]),
};

/**
 * Parses JSON with every integer literal as a `bigint` and other numbers by their source
 * text, so `1n` and `1`, or `1.50` and `1.5`, compare by their digits on the wire.
 */
function exact(text: string): unknown {
  const reviver = (_key: string, value: unknown, context?: { source?: string }) =>
    typeof value === "number" ? (/^-?\d+$/.test(context!.source!) ? BigInt(context!.source!) : context!.source) : value;
  return JSON.parse(text, reviver as Parameters<typeof JSON.parse>[1]);
}

const files = readdirSync(dir)
  .filter((f) => f.endsWith(".json"))
  .map((f) => f.slice(0, -".json".length))
  .sort();

describe("fixtures", () => {
  test("every fixture has a builder and every builder a fixture", () => {
    expect(Object.keys(builders).sort()).toEqual(files);
  });

  test.each(files)("%s: toTree() equals the fixture's tree", (name) => {
    const text = readFileSync(join(dir, `${name}.json`), "utf8");
    const fixture = JSON.parse(text) as { call: unknown; tree: unknown };
    expect(typeof fixture.call).toBe("string");
    const build = builders[name];
    expect(build, `no builder for ${name}`).toBeDefined();
    const tree = build!().toTree();
    // Structure with JS values, then the exact digits that go on the wire.
    expect(JSON.parse(stringify(tree))).toEqual(fixture.tree);
    expect(exact(stringify(tree))).toEqual((exact(text) as { tree: unknown }).tree);
  });
});
