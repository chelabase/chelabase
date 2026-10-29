# @chelabase/client

A typed client for the Chelabase data API (`POST /data/v1/query`). No runtime
dependencies: it's built on `fetch`, so it runs in browsers, Node and edge runtimes.

## Install

A workspace package (not published to npm): add `"@chelabase/client": "workspace:*"` to
your app's `package.json`. Fetch the types from the data API (see below).

## Example

```ts
import { createClient } from "@chelabase/client";
import { type Database, types } from "./db"; // fetched from GET /data/v1/types

const db = createClient<Database>({ url: "http://api.chelabase.localhost:8000", key: PUBLIC_KEY, types });

const rows = await db.from("orders").select("id", "total").where({ status: "paid" }).limit(50);
await db.from("orders").insert([{ total: "12.50", vip: false, history: [] }]).returning("id");
await db.from("orders").update({ status: "paid" }).where({ id: 7 });
const [order, points] = await db.batch((tx) => [
  tx.from("orders").insert({ total: "1.00", vip: false, history: [] }).returning("id"),
  tx.call("add_points", { user_id }),
]);
```

Fetch `db.ts` with a secret data key (the same file `chelabase gen types` writes):

```bash
curl -fsS -H "Authorization: Bearer $CHELABASE_DATA_KEY" "$URL/data/v1/types" -o db.ts
```

A server can also fetch the runtime `types` map at startup, so decoding follows the live
schema without a rebuild (compile-time checking still comes from `db.ts` in the build):

```ts
import { createClient, fetchTypes } from "@chelabase/client";

const db = createClient<Database>({ url, key, types: await fetchTypes({ url, key: secretKey }) });
```

## Documentation

The guide covers filters, errors, big numbers (`int8`, `numeric`), generated types (and
`fetchTypes`),
other databases and app users (`db.auth`). Run the docs site (`just docs`, on
http://docs.chelabase.localhost:8000) and open `/docs/using/client`.
