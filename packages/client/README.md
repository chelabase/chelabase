# @cheladb/client

A typed client for the Cheladb data API (`POST /data/v1/query`). No runtime
dependencies: it's built on `fetch`, so it runs in browsers, Node and edge runtimes.

```ts
import { createClient } from "@cheladb/client";
import { type Database, types } from "./db"; // written by `cheladb gen types`

const db = createClient<Database>({ url: "http://localhost:8000", key: PUBLIC_KEY, types });

const rows = await db
  .from("orders") // the default schema; "billing.invoices" is qualified
  .select("id", "total", { col: "customer_name", as: "customer" })
  .where({ status: "paid", total: { gte: "100" }, or: [{ region: { in: ["eu"] } }, { vip: true }] })
  .orderBy("created_at", "desc", "last")
  .limit(50);

await db.from("orders").insert([{ total: "12.50", region: "eu" }]).returning("id");
await db.from("orders").update({ status: "shipped" }).where({ id });
await db.from("orders").delete().where({ id }).returning("*");
await db.from("logs").delete().all(); // an update or delete needs .where() or .all()
await db.call("add_points", { user_id, n: 5 });
const [order, points] = await db.batch((tx) => [
  tx.from("orders").insert({ total: "1.00", region: "eu" }).returning("id"),
  tx.call("add_points", { user_id }),
]); // one transaction, all or nothing; a typed tuple
```

Builders are immutable and lazy: nothing is sent until you `await` (each `await` sends
the request once); they also have `.catch()` and `.finally()`. `toTree()` returns the
exact JSON tree a builder sends.

An `update` or `delete` runs only after `.where()` or `.all()`: before that, awaiting it
or putting it in a `batch` is a compile error (and the server refuses it anyway).
`.returning()` works before or after. `select()` and `returning()` need at least one
column.

## Filters

`where({...})` takes an object whose keys are combined with `and`:

- a bare value means `eq`: `{ status: "paid" }`;
- `null` means `is null`: `{ deleted_at: null }`;
- an operator object uses the operators: `{ total: { gte: "10", lt: "20" } }`
  (`eq`, `neq`, `lt`, `lte`, `gt`, `gte`, `in`, `is`, `like`, `ilike`; several are
  combined with `and`; `in: []` matches nothing);
- `and` and `or` take lists of filters, and `not` takes one filter.

Calling `where` again adds its conditions with `and`. An `undefined` value throws a
`TypeError` rather than silently dropping the condition; leave the key out instead.

**A column named `and`, `or` or `not`** can't be a key (those keys are the combinators).
Reach it with a condition object `{ col, <operator>: value }` inside an `and`/`or` list
or under `not`:

```ts
db.from("rules").where({ and: [{ col: "and", eq: "x" }, { status: "on" }] });
```

Inside a list (or under `not`), an object whose only keys are `col` (a string) and
operators is read as a condition object; any other object is a filter.

**A json/jsonb value that looks like operators** (`{ meta: { eq: 1 } }`) is read as
operators, not as the object to compare with. Compare such a value with a condition
object: `{ and: [{ col: "meta", eq: { eq: 1 } }] }`.

## Errors

A failed request throws `DataApiError` with `status`, `code` (`invalid_query`,
`unknown_table`, `permission_denied`, `conflict`, …), `message`, and `detail`, `hint`,
`sqlstate` when the server sends them.

A non-2xx answer without the data API's error body (a proxy's error page) gets its
`code` from the status: 401 `unauthorized`, 413 `too_large`, 429 `rate_limited`, 502
and 503 `unavailable`, 504 `timeout`, anything else `internal`. Its message holds the
status and the first 200 characters of the body. A 2xx answer that isn't JSON is a
`DataApiError` with `code: "internal"`. Network failures are rethrown unchanged.

## Big numbers: `int8` and `numeric`

Postgres writes `int8` and `numeric` as JSON numbers with every digit. JavaScript's
`number` holds integers exactly only up to ±(2^53 − 1), so the client reads those values
from their source text. It finds them with the generated `types` map (pass it to
`createClient`), including through `select` aliases, `returning` keys, a function's
scalar return and a set-returning function's columns.

A typed client requires `types`: `createClient<Database>({ url, key })` without them is a
compile error, because without the map the client can't tell an int8 or a numeric from
any other number (a `numeric` typed `string` would come back as a number). The check is
compile-time only, since the generic is erased at run time. `bigint: true` also throws at
run time when `types` is missing. The untyped `createClient({ url, key })` needs no
`types`.

| Postgres | Default | `bigint: true` |
|---|---|---|
| `int2`, `int4` | `number` | `number` |
| `int8` | `number`; **throws `UnsafeIntegerError`** outside the safe range | `bigint`, always |
| `numeric` | `string` with the exact digits | `string` |

- A column's type never depends on its value: an `int8` is `number` or `bigint` for the
  whole client, chosen by `bigint` and followed by the types (`createClient<Database>({
  …, types, bigint: true })` gives `bigint` columns, arguments and returns).
- Forgetting `bigint: true` is loud, not silent: an `int8` outside the safe range throws
  `UnsafeIntegerError` (with `column` and the exact `source` digits) instead of returning
  a rounded number.
- Without `types`, any integer literal outside the safe range throws
  `UnsafeIntegerError`, wherever it is; other numbers stay numbers.
- With `types`, a value the map doesn't cover (a column added after `gen types`, a
  function with no int8/numeric return) still throws `UnsafeIntegerError` when it's a
  top-level integer, or an array of them, outside the safe range. It's never rounded
  silently; run `gen types` again.
- Only top-level row keys (and scalar returns) are mapped; numbers inside `json`/`jsonb`
  objects are parsed as plain JSON numbers.
- Writes are exact too: a `bigint` in `insert`, `update`, `where` or call arguments is
  sent as its digits. Send `numeric` values as strings.

**Runtime requirement:** reading exactly uses `JSON.parse` source text access, available
in **Node 21+, Chrome 114+, Firefox 135+ and Safari 18.4+**. On an older runtime,
`createClient` throws when given `bigint: true` or `types` that include a `numeric`
column; the default mode still works and still throws `UnsafeIntegerError` for an `int8`
outside the safe range (checked after a plain parse). There, an integer is recognised by
its printed form (no exponent or point), so a float between 2^53 and 1e21 (`1.5e20`)
is also refused; floats printed with an exponent (`1e300`) are not.

## Types

`cheladb gen types --url <gateway> --key <secret key> --out db.ts` writes the `Database`
type and the `types` map; pass both (`createClient<Database>({ url, key, types })`). Without
them (`createClient({ url, key })`), the client accepts any table, column and
function and types values as `unknown`.
