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

## Browser and server

For a server-rendered app (Next.js and others) the app user's session lives in cookies. Create
the browser client in client components, a server client per request, and mount the auth
handler once:

```ts
import { createBrowserClient } from "@chelabase/client/browser";
import { createAuthHandler, createServerClient } from "@chelabase/client/server";

// Browser: data goes to the gateway, sign-in and refresh go through your auth route.
const db = createBrowserClient<Database>({ url, key: PUBLIC_KEY, types });

// Server: one per request. Wrap the framework's cookie store: Next's `cookies()` has no
// `setAll`, so passing it as is gives a read-only client that never refreshes. In Next.js,
// `store.set` works in Server Actions and route handlers, not in Server Components.
const store = await cookies();
const sdb = createServerClient<Database>({
  url,
  key: PUBLIC_KEY,
  // Sign-ins and refreshes are then limited per email and per user, instead of all sharing
  // this server's IP. `clientIp` (optional, needs `serverKey`) adds per-IP limits on top.
  serverKey: SECRET_KEY,
  types,
  cookies: {
    getAll: () => store.getAll(),
    setAll: (list) => list.forEach((c) => store.set(c.name, c.value, c.options)),
  },
});
const user = await sdb.auth.currentUser(); // null when signed out
// In the proxy/middleware: a request only when the access token is about to expire.
await sdb.auth.refreshIfNeeded();

// Once, e.g. app/api/chelabase/auth/[...path]/route.ts
export const POST = createAuthHandler({ url, key: PUBLIC_KEY, serverKey: SECRET_KEY, siteUrl: "https://app.example.com" });
```

When another system owns the session, `createClient({ url, key, accessToken: () => token })`
sends that token and never refreshes it. The full Next.js walkthrough is the guide's Server
rendering page, `/using/client/ssr` on the docs site
(http://docs.chelabase.localhost:8000/using/client/ssr in the workbench).

## Documentation

The guide covers filters, errors, big numbers (`int8`, `numeric`), generated types (and
`fetchTypes`),
other databases and app users (`db.auth`). Read it in `apps/docs/content/docs/using/client/`, or run the docs site in the workbench
(`just workbench up --with docs`, then http://docs.chelabase.localhost:8000/using/client).
