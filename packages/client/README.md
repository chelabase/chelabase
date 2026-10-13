# @chelabase/client

A typed client for the Chelabase data API (`POST /data/v1/query`). No runtime
dependencies: it's built on `fetch`, so it runs in browsers, Node and edge runtimes.

## Install

A workspace package (not published to npm): add `"@chelabase/client": "workspace:*"` to
your app's `package.json`. Write the types with `chelabase gen types` (see below).

## Example

```ts
import { createClient } from "@chelabase/client";
import { type Database, types } from "./db"; // written by `chelabase gen types`

const db = createClient<Database>({ url: "http://api.chelabase.localhost:8000", key: PUBLIC_KEY, types });

const rows = await db.from("orders").select("id", "total").where({ status: "paid" }).limit(50);
await db.from("orders").insert([{ total: "12.50", vip: false, history: [] }]).returning("id");
await db.from("orders").update({ status: "paid" }).where({ id: 7 });
const [order, points] = await db.batch((tx) => [
  tx.from("orders").insert({ total: "1.00", vip: false, history: [] }).returning("id"),
  tx.call("add_points", { user_id }),
]);
```

`CHELABASE_DATA_KEY=chk_sec_… chelabase gen types --url <gateway> --out db.ts` writes
`db.ts`.

## Documentation

The guide covers filters, errors, big numbers (`int8`, `numeric`), generated types,
other databases and app users (`db.auth`). Run the docs site (`just docs`, on
http://docs.chelabase.localhost:8000) and open `/docs/using/client`.
