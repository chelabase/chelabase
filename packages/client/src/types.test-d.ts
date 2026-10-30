// Type-level tests, checked by `tsc --noEmit` (the package's `typecheck`). Every
// `@ts-expect-error` must fire; an unused one is itself an error.
import { expectTypeOf } from "vitest";
import { type Client, createClient, type DatabaseShape, type Int8, type Json } from "./index.ts";
import { createBrowserClient } from "./browser.ts";
import { createServerClient } from "./server.ts";
import { type Database, types } from "./test-db.ts";

const opts = { url: "http://localhost:8000", key: "chk_pub_x" };
const db = createClient<Database>({ ...opts, types });
const big = createClient<Database>({ ...opts, bigint: true, types });

// The generated `Database` satisfies the shape the client accepts.
expectTypeOf<Database>().toExtend<DatabaseShape>();

// --- the bigint option picks the client's mode, even with `Database` given explicitly
expectTypeOf(db).toEqualTypeOf<Client<Database, false>>();
expectTypeOf(big).toEqualTypeOf<Client<Database, true>>();
expectTypeOf(createClient<Database, true>({ ...opts, bigint: true, types })).toEqualTypeOf<Client<Database, true>>();
// @ts-expect-error bigint: true needs the generated types
createClient<Database>({ ...opts, bigint: true });
// @ts-expect-error bigint: true needs the generated types
createClient<Database, true>({ ...opts, bigint: true });
// @ts-expect-error B = true needs bigint: true
createClient<Database, true>(opts);
// @ts-expect-error a boolean that isn't a literal can't pick a mode
createClient<Database>({ ...opts, bigint: Math.random() > 0.5 });

// --- a typed client requires `types`; the untyped one doesn't
// @ts-expect-error a generated Database needs its types (numeric would come back as a number)
createClient<Database>(opts);
// @ts-expect-error the same with an explicit bigint: false
createClient<Database, false>({ ...opts, bigint: false });
createClient<Database>({ ...opts, types });
createClient(opts);
createClient<DatabaseShape>(opts);

// --- Int8 resolves to number or bigint, never both
async function rows() {
  const all = await db.from("orders");
  expectTypeOf(all).toEqualTypeOf<
    {
      id: number;
      total: string;
      status: "paid" | "pending" | "shipped";
      customer_name: string | null;
      region: string;
      vip: boolean;
      qty: number;
      created_at: string;
      tags: string[];
      meta: Json;
      and: string | null;
    }[]
  >();
  const events = await big.from("events");
  expectTypeOf(events).toEqualTypeOf<
    { id: bigint; seq: bigint | null; amount: string; ids: bigint[]; label: string }[]
  >();
  const smallEvents = await db.from("events").select("seq", "ids");
  expectTypeOf(smallEvents).toEqualTypeOf<{ seq: number | null; ids: number[] }[]>();
}
void rows;

// --- select narrows, aliases rename
async function narrowing() {
  const r = await db.from("orders").select("id", "total", { col: "customer_name", as: "customer" });
  expectTypeOf(r).toEqualTypeOf<{ id: number; total: string; customer: string | null }[]>();
  const b = await big.from("orders").select({ col: "id", as: "order_id" });
  expectTypeOf(b).toEqualTypeOf<{ order_id: bigint }[]>();
  // @ts-expect-error unknown column
  db.from("orders").select("nope");
  // @ts-expect-error alias of an unknown column
  db.from("orders").select({ col: "nope", as: "x" });
}
void narrowing;

// --- table names: default schema bare, any schema qualified
async function names() {
  const inv = await db.from("billing.invoices").select("amount", "paid");
  expectTypeOf(inv).toEqualTypeOf<{ amount: string; paid: boolean }[]>();
  const q = await db.from("public.orders").select("id");
  expectTypeOf(q).toEqualTypeOf<{ id: number }[]>();
  // @ts-expect-error not in the default schema
  db.from("invoices");
  // @ts-expect-error unknown table
  db.from("nope");
  // @ts-expect-error unknown schema
  db.from("nope.orders");
}
void names;

// --- where: columns, values and operators are checked
db.from("orders").where({ status: "paid", qty: { gte: 1, lt: 10 }, customer_name: null });
db.from("orders").where({ or: [{ region: { in: ["eu"] } }, { vip: true }], not: { status: "pending" } });
db.from("orders").where({ and: [{ col: "and", eq: "x" }, { status: "paid" }] });
db.from("orders").where({ customer_name: { like: "A%" }, vip: { is: true }, total: { is: null } });
big.from("events").where({ id: 1n, seq: { in: [2n] } });
// @ts-expect-error unknown column
db.from("orders").where({ nope: 1 });
// @ts-expect-error wrong value type
db.from("orders").where({ qty: "1" });
// @ts-expect-error not an enum value
db.from("orders").where({ status: "lost" });
// @ts-expect-error in takes a list of the column's values
db.from("orders").where({ qty: { in: ["1"] } });
// @ts-expect-error like only on text columns
db.from("orders").where({ qty: { like: "1%" } });
// @ts-expect-error unknown operator
db.from("orders").where({ qty: { between: [1, 2] } });
// @ts-expect-error int8 is a number without bigint: true
db.from("events").where({ id: 1n });
// @ts-expect-error int8 is a bigint with bigint: true
big.from("events").where({ id: 1 });
// @ts-expect-error a non-nullable column can't be null
db.from("orders").where({ qty: null });
// @ts-expect-error unknown column inside or
db.from("orders").where({ or: [{ nope: 1 }] });
// @ts-expect-error unknown column in a raw condition
db.from("orders").where({ and: [{ col: "nope", eq: 1 }] });

// --- orderBy
db.from("orders").orderBy("created_at", "desc", "last").orderBy("id");
// @ts-expect-error unknown column
db.from("orders").orderBy("nope");
// @ts-expect-error bad direction
db.from("orders").orderBy("id", "down");

// --- writes
async function writes() {
  const none = await db.from("orders").insert([{ total: "1", region: "eu" }]);
  expectTypeOf(none).toEqualTypeOf<[]>();
  const ids = await db.from("orders").insert({ total: "1", region: "eu" }).returning("id");
  expectTypeOf(ids).toEqualTypeOf<{ id: number }[]>();
  const all = await big.from("events").update({ label: "x" }).where({ id: 1n }).returning("*");
  expectTypeOf(all).toEqualTypeOf<{ id: bigint; seq: bigint | null; amount: string; ids: bigint[]; label: string }[]>();
  const del = await db.from("orders").delete().all().returning("id", "total");
  expectTypeOf(del).toEqualTypeOf<{ id: number; total: string }[]>();
  // @ts-expect-error missing a required column
  db.from("orders").insert([{ total: "1" }]);
  // @ts-expect-error unknown column
  db.from("orders").insert([{ total: "1", region: "eu", nope: 1 }]);
  // @ts-expect-error wrong value type
  db.from("orders").update({ qty: "1" });
  // @ts-expect-error int8 is a bigint with bigint: true
  big.from("events").insert({ amount: "1", label: "x", seq: 1 });
  // @ts-expect-error unknown returning column
  db.from("orders").delete().all().returning("nope");
}
void writes;

// --- an update or delete runs only after .where() or .all()
async function unfiltered() {
  // @ts-expect-error an update needs .where() or .all() before it runs
  await db.from("orders").update({ vip: true });
  // @ts-expect-error a delete needs .where() or .all() before it runs
  await db.from("orders").delete().returning("id");
  // @ts-expect-error nor can it go in a batch
  db.batch((tx) => [tx.from("orders").delete()]);
  // @ts-expect-error nor can it go in a batch, with returning
  db.batch((tx) => [tx.from("orders").update({ vip: true }).returning("id")]);
  const ok = await db.from("orders").update({ vip: true }).returning("id").where({ id: 1 });
  expectTypeOf(ok).toEqualTypeOf<{ id: number }[]>();
  const okAll = await db.from("orders").delete().all();
  expectTypeOf(okAll).toEqualTypeOf<[]>();
  // toTree() works before where/all (for inspection)
  db.from("orders").delete().toTree();
}
void unfiltered;

// --- select and returning need at least one column
// @ts-expect-error select needs at least one column
db.from("orders").select();
// @ts-expect-error returning needs at least one column
db.from("orders").insert({ total: "1", region: "eu" }).returning();
// @ts-expect-error returning needs at least one column
db.from("orders").delete().all().returning();

// --- builders behave like promises
expectTypeOf(db.from("orders").select("id").catch(() => null)).toEqualTypeOf<Promise<{ id: number }[] | null>>();
expectTypeOf(db.from("orders").select("id").finally(() => {})).toEqualTypeOf<Promise<{ id: number }[]>>();
expectTypeOf(db.call("next_id").catch(() => 0)).toEqualTypeOf<Promise<number>>();

// --- calls
async function calls() {
  expectTypeOf(await db.call("add_points", { user_id: "u", n: 5 })).toEqualTypeOf<number>();
  expectTypeOf(await db.call("add_points", { user_id: "u" })).toEqualTypeOf<number>();
  expectTypeOf(await db.call("next_id")).toEqualTypeOf<number>();
  expectTypeOf(await big.call("next_id")).toEqualTypeOf<bigint>();
  expectTypeOf(await big.call("big_ids", { since: 1n })).toEqualTypeOf<bigint[]>();
  expectTypeOf(await big.call("top_events")).toEqualTypeOf<{ id: bigint; amount: string; label: string }[]>();
  expectTypeOf(await db.call("billing.void_invoice", { id: "x" })).toEqualTypeOf<null>();
  // @ts-expect-error unknown function
  db.call("nope");
  // @ts-expect-error missing a required argument
  db.call("add_points", { n: 1 });
  // @ts-expect-error the function has required arguments
  db.call("add_points");
  // @ts-expect-error wrong argument type
  db.call("add_points", { user_id: "u", n: "5" });
  // @ts-expect-error int8 argument is a number without bigint: true
  db.call("big_ids", { since: 1n });
}
void calls;

// --- batch returns a typed tuple
async function batches() {
  const result = await big.batch((tx) => [
    tx.from("orders").select("id"),
    tx.from("orders").insert([{ total: "1", region: "eu" }]),
    tx.call("total_spent", { user_id: "u" }),
    tx.from("billing.invoices").delete().where({ paid: true }).returning("amount"),
  ]);
  expectTypeOf(result).toEqualTypeOf<[{ id: bigint }[], [], string, { amount: string }[]]>();
  // @ts-expect-error batches don't nest
  big.batch((tx) => [tx.batch(() => [])]);
}
void batches;

// --- untyped: any table and column, unknown values
async function untyped() {
  const anyDb = createClient(opts);
  const r = await anyDb.from("whatever").select("a", { col: "b", as: "c" }).where({ x: 1 });
  expectTypeOf(r).toEqualTypeOf<{ a: unknown; c: unknown }[]>();
  expectTypeOf(await anyDb.call("f", { a: 1 })).toEqualTypeOf<unknown>();
}
void untyped;

// Int8 is opaque: a plain number or bigint isn't one.
// @ts-expect-error
const notInt8: Int8 = 1;
void notInt8;

// --- db.database(name, types?) returns a client typed by the other database
type Databases = { main: Database; analytics: Database };
const multi = createClient<Databases["main"]>({ ...opts, types });
expectTypeOf(multi.database<Databases["analytics"]>("analytics", types)).toEqualTypeOf<
  Client<Databases["analytics"], false>
>();
expectTypeOf(big.database<Database>("analytics", types)).toEqualTypeOf<Client<Database, true>>();
expectTypeOf(multi.database("analytics")).toEqualTypeOf<Client<DatabaseShape, false>>();
// @ts-expect-error a generated database needs its types
multi.database<Database>("analytics");
// @ts-expect-error in bigint mode even an untyped database needs the types (they decode int8)
big.database("analytics");
expectTypeOf(big.database("analytics", types)).toEqualTypeOf<Client<DatabaseShape, true>>();
// @ts-expect-error the name is a string
multi.database(1);

// --- db.auth: signUp's data is typed from `appUsers`, loose without it
type AppDb = {
  defaultSchema: "public";
  appUsers: { schema: "public"; table: "profiles" };
  schemas: {
    public: {
      tables: {
        profiles: {
          Row: { id: string; email: string; email_verified: boolean; created_at: string; deleted_at: string | null; nick: string; age: number | null };
          Insert: { id?: string; email?: string; email_verified?: boolean; created_at?: string; deleted_at?: string | null; nick: string; age?: number | null };
          Update: { nick?: string };
        };
      };
      functions: {};
    };
  };
};
const appDb = createClient<AppDb>({ ...opts, types });
type SignUpArg = Parameters<typeof appDb.auth.signUp>[0];
expectTypeOf<SignUpArg["data"]>().toEqualTypeOf<{ nick: string; age?: number | null } | undefined>();
void appDb.auth.signUp({ email: "a@b.co", password: "pw", data: { nick: "x" } });
// @ts-expect-error unknown column
void appDb.auth.signUp({ email: "a@b.co", password: "pw", data: { nope: 1 } });
// @ts-expect-error a managed column is not insertable by the app
void appDb.auth.signUp({ email: "a@b.co", password: "pw", data: { nick: "x", email: "y" } });
// @ts-expect-error nick is required
void appDb.auth.signUp({ email: "a@b.co", password: "pw", data: {} });

const looseDb = createClient({ ...opts });
expectTypeOf<Parameters<typeof looseDb.auth.signUp>[0]["data"]>().toEqualTypeOf<Record<string, Json> | undefined>();
void looseDb.auth.signUp({ email: "a@b.co", password: "pw", data: { anything: [1, { b: null }] } });
// Without `appUsers` in a generated Database the data is the loose record too.
const plainDb = createClient<Database>({ ...opts, types });
expectTypeOf<Parameters<typeof plainDb.auth.signUp>[0]["data"]>().toEqualTypeOf<Record<string, Json> | undefined>();

// `accessToken` and `auth` are mutually exclusive.
createClient({ ...opts, accessToken: () => "t" });
// @ts-expect-error accessToken replaces the session storage
createClient({ ...opts, accessToken: () => "t", auth: {} });

// --- createServerClient: `cookies` is required; query types match `createClient`
const cookies = { getAll: () => [], setAll: () => {} };
const srv = createServerClient<Database>({ ...opts, cookies, types });
expectTypeOf(srv).toEqualTypeOf<Client<Database, false>>();
expectTypeOf(srv).toEqualTypeOf(db);
expectTypeOf(createServerClient<Database>({ ...opts, cookies, bigint: true, types })).toEqualTypeOf(big);
expectTypeOf(createServerClient({ ...opts, cookies })).toEqualTypeOf<Client<DatabaseShape, false>>();
createServerClient({ ...opts, cookies: { getAll: () => [{ name: "a", value: "b" }] } });
createServerClient({ ...opts, cookies: { getAll: () => [], setAll: async () => {} }, cookieOptions: { sameSite: "strict" } });
// @ts-expect-error the session lives in cookies
createServerClient(opts);
// @ts-expect-error a generated Database needs its types
createServerClient<Database>({ ...opts, cookies });
// @ts-expect-error bigint: true needs the generated types
createServerClient<Database>({ ...opts, cookies, bigint: true });
// @ts-expect-error the server client has no browser session options
createServerClient({ ...opts, cookies, auth: {} });
// @ts-expect-error nor a caller-owned token
createServerClient({ ...opts, cookies, accessToken: () => "t" });

// --- createBrowserClient: query types match `createClient`
const brw = createBrowserClient<Database>({ ...opts, types });
expectTypeOf(brw).toEqualTypeOf<Client<Database, false>>();
expectTypeOf(brw).toEqualTypeOf(db);
expectTypeOf(createBrowserClient<Database>({ ...opts, bigint: true, types })).toEqualTypeOf(big);
expectTypeOf(createBrowserClient({ ...opts, authPath: "/auth", cookieOptions: { secure: false } })).toEqualTypeOf<
  Client<DatabaseShape, false>
>();
// @ts-expect-error a generated Database needs its types
createBrowserClient<Database>(opts);
// @ts-expect-error bigint: true needs the generated types
createBrowserClient<Database>({ ...opts, bigint: true });
// @ts-expect-error the session lives in the app's cookies, not in browser storage
createBrowserClient({ ...opts, auth: {} });
// @ts-expect-error nor a caller-owned token
createBrowserClient({ ...opts, accessToken: () => "t" });
