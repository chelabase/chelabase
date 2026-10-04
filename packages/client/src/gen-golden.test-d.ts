// The generated golden file (`chelabase gen types` over fixtures/gen/schema.json) against the
// real client types. A Rust test keeps the file equal to the generator's output.
import { expectTypeOf, test } from "vitest";
import { createClient } from "./index.ts";
import type { Database } from "../fixtures/gen/db.ts";
import { types } from "../fixtures/gen/db.ts";

const db = createClient<Database>({ url: "http://x", key: "k", types });
const big = createClient<Database, true>({ url: "http://x", key: "k", types, bigint: true });

test("generated types drive the client", () => {
  expectTypeOf(db.from("orders").select("id", "total", "status", "raw")).toBeObject();
  type Row = Awaited<ReturnType<ReturnType<typeof db.from<"orders">>["select"]>> extends infer R ? R : never;
  expectTypeOf<Row>().not.toBeAny();
  expectTypeOf(db.call("next_id")).not.toBeAny();
  expectTypeOf(db.call("add_points", { user_id: "u" })).not.toBeAny();
  expectTypeOf(db.call("echo", { b: 1 })).not.toBeAny();
  expectTypeOf(db.call("billing.void_invoice", { id: "u" })).not.toBeAny();
  expectTypeOf(db.from("billing.invoices").insert({ amount: "1.5" })).not.toBeAny();
  expectTypeOf(big.from("orders")).not.toBeAny();
  // @ts-expect-error not a column
  db.from("orders").select("nope");
  // @ts-expect-error total is required on insert
  db.from("orders").insert({ vip: true, history: [] });
  // @ts-expect-error overloaded with different return types: not in the Database
  db.call("mixed", { a: "x" });
});

test("results resolve Int8 and keep numeric a string", async () => {
  const rows = await db.from("orders").select("id", "total", "status", "raw", "ids");
  expectTypeOf(rows[0]!.id).toEqualTypeOf<number>();
  expectTypeOf(rows[0]!.total).toEqualTypeOf<string>();
  expectTypeOf(rows[0]!.status).toEqualTypeOf<"pending" | "paid" | 'ship"ped'>();
  expectTypeOf(rows[0]!.ids).toEqualTypeOf<number[]>();
  const rowsBig = await big.from("orders").select("id");
  expectTypeOf(rowsBig[0]!.id).toEqualTypeOf<bigint>();
  expectTypeOf(await db.call("next_id")).toEqualTypeOf<number>();
  expectTypeOf(await db.call("total_spent", { user_id: "u" })).toEqualTypeOf<string>();
});

// Several databases: `Databases` keys, `types` keyed alike, `Database` kept as the main one.
test("generated multi-database types drive bound clients", async () => {
  const m = await import("../fixtures/gen/multi.ts");
  type Dbs = import("../fixtures/gen/multi.ts").Databases;
  expectTypeOf<import("../fixtures/gen/multi.ts").Database>().toEqualTypeOf<Dbs["main"]>();
  const main = createClient<Dbs["main"]>({ url: "http://x", key: "k", types: m.types.main });
  const analytics = main.database<Dbs["analytics"]>("analytics", m.types.analytics);
  expectTypeOf(analytics.from("reports.daily").select("day", "hits")).not.toBeAny();
  expectTypeOf(analytics.from("daily")).not.toBeAny();
  const rows = await analytics.from("daily").select("day", "hits");
  expectTypeOf(rows[0]!.hits).toEqualTypeOf<number>();
  // @ts-expect-error orders is in main, not analytics
  analytics.from("orders");
  // @ts-expect-error daily is in analytics, not main
  main.from("daily");
  const direct = createClient<Dbs["analytics"]>({ url: "http://x", key: "k", types: m.types.analytics });
  expectTypeOf(direct).toEqualTypeOf<typeof analytics>();
});
