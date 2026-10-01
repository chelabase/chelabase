# Data API (roadmap step 6)

**Date:** 2026-10-01
**Status:** Draft, for the user's review.
**Roadmap:** `docs/roadmap/2026-09-28-neon-and-data-api.md`, step 6 ("The data API (`cheladb-rest`)").
**Branch:** `data-api` (worktree `.worktrees/data-api`).

## Goal

Apps read and write a branch's tables over HTTP, from browsers and from servers, with a typed TypeScript SDK. The design is our own: a structured JSON query, not a URL grammar, and nothing copied from PostgREST or Supabase.

**Step 6 is the engine.** App users (one Zitadel organization per project, the app-owned users table, sync, `db.auth`) are step **6b**. Until then, requests authenticate with per-branch keys. 6b plugs a user's JWT into the same gateway step, so the engine doesn't change.

**Done when:**
- `just data-e2e` passes through the gateway:
  1. provision a database, then create a table with psql;
  2. read and write it with the SDK, using the public key (`anon`) and a secret key (`authenticated`);
  3. `cheladb gen types`, then typecheck a sample file against the output;
  4. add an RLS policy and see it enforced;
  5. revoke a key and see it refused within 30 s.
- The live tests (see Testing) pass against a real compute.
- Studio shows the branch's endpoint, its keys and the RLS warning.

## Decisions

Made with the user on 2026-10-01; the roadmap's "Agreed direction" and "First subset of operations" still apply.

| Topic | Decision | Why |
|---|---|---|
| Scope | Engine, SDK and type generation now; app users in 6b | Each is about the size of step 5; they meet at one interface (verified identity → role and claims). |
| Credentials before 6b | Per-branch **public** key (always `anon`, safe in browsers) and **secret** keys (server only; a chosen role, default `authenticated`) | Usable from apps right away; revocable and rotatable; nothing for developers to sign. |
| Addressing | **The key names the branch.** One URL: `/data/v1/query` | In 6b the public key stays as the client id and the user's JWT rides alongside it; a JWT names a project, not a branch. |
| Connection | **Direct to the compute** (Postgres published on loopback), as `authenticator` with a per-branch password set by the worker through `admin_sql` | Lowest latency, and `authenticator` stays `NOINHERIT` and outside `neon_superuser` (spec roles are created `IN ROLE neon_superuser`). |
| Fit in the stack | **The gateway checks keys** (`Auth::DataKey`) and mints the internal identity token; `cheladb-rest` trusts only that token | The model every service already uses; revocation in one place; 6b extends only the gateway. |
| Schema changes | A DDL event trigger regenerates a **version uuid**; each request reads it in its first round trip | Exact, with no extra round trip and no listener connections. A uuid, not a counter, so a restore that rewinds the schema can't reuse a version number for a different schema. |
| Schemas | **One reserved schema, `cheladb`, holding only machinery.** No platform data in user databases. **Exposure is Postgres privileges:** a schema is reachable when the role has `USAGE` on it. There's no "exposed schemas" list. | One place to reason about access. Identity lives in Zitadel and the users table belongs to the app (6b), so nothing platform-owned branches, resets or restores with the user's data. |
| RLS | **Off by default.** Studio warns about every reachable table with RLS off. | Orgs iterate quickly; the warning says to enable it before production. |
| Default privileges (`public`) | `anon`: `SELECT`. `authenticated`: `SELECT, INSERT, UPDATE, DELETE`. Functions: `EXECUTE` revoked from `PUBLIC`, granted to `authenticated`. | Read-only for the browser key by default; each org grants more or less. A function can write, so `anon` gets none by default. |

## Architecture

```
client ──POST /data/v1/query, Authorization: Bearer <key>──► gateway
  route: Scope::Data, Auth::DataKey, Cors::Any (no credentials)
  sha256(key) → GET :8083 /internal/data/keys/{sha256}   (cached 30 s, unknown keys too)
     → { key_id, branch_id, kind, role }                 (unknown or revoked → 401)
  mint internal identity token { aud: "rest", branch_id, role, key_id }
  ──► cheladb-rest
     verify the identity token
     branch → GET :8083 /internal/data/branches/{id}/connection   (cached 60 s)
        → { host, port, database, user: "authenticator", password, status, settings }
     pool for the branch (sqlx)
     BEGIN
       SELECT set_config('role', $role, true), set_config('request.jwt.claims', $claims, true),
              set_config('statement_timeout', $ms, true), (SELECT version FROM cheladb.schema_version)
       [version unknown → introspect]
       parse → shape cache → SQL → run
     COMMIT
  ◄── JSON bytes from Postgres, unparsed
```

- **`cheladb-rest`** (`services/rest`) is one multi-tenant service for every branch, run by `just dev` like the others. It holds no metadata-database credentials; everything comes from the internal listener.
- **`Service::Rest`** joins `cheladb-routes`, and the data endpoints are declared there like every other endpoint.
- **CORS:** the data endpoints answer any origin without credentials (`Cors::Any`), since the public key is used from browsers. `Authorization` is an allowed header.
- `request.jwt.claims` is `{}` for keys in step 6. In 6b it carries the verified user's claims.

## Keys

New metadata table `data_api_keys`:

| Column | Notes |
|---|---|
| `id` | uuid |
| `branch_id` | references `branches`; deleted with the branch |
| `kind` | `public` or `secret` |
| `role` | `anon` for public keys; for secret keys, the chosen role |
| `name` | shown in Studio; the public key is named `public` |
| `prefix` | the first 12 characters (`chk_pub_` or `chk_sec_` + 4), to recognize a key in Studio and logs |
| `sha256` | of the whole key; unique |
| `sealed` | public keys only: the key sealed (AAD = key id) so Studio can show it again |
| `created_at`, `last_used_at`, `revoked_at` | `last_used_at` is updated at most once a minute per key |

- **Format:** `chk_pub_` or `chk_sec_` followed by 32 random bytes in base62. Keys are high-entropy, so a plain SHA-256 is enough.
- **Public key:** each branch gets exactly one live public key, created when the branch first becomes ready. Rotating it creates a new one and revokes the old.
- **Secret keys:** created on demand and returned **once**. The role defaults to `authenticated`. Any role in the branch's database may be chosen except a superuser or a member of `neon_superuser`; the create call checks this through the worker (`admin_sql`) and refuses otherwise. On success the worker also runs `GRANT <role> TO authenticator`, because `set_config('role', …)` needs membership.
- **Lifecycle:** keys can be revoked and rotated. Keys belong to the branch, not its timeline, so reset and restore keep them. A new branch starts with its own new public key and no secret keys. Deleting a branch deletes its keys.
- **Revocation** takes effect within 30 s (the gateway cache).
- **Abuse:** unknown keys are cached for 30 s as well, and cache misses (lookups that reach the internal listener) are limited to 20 per second per client address; over the limit is 429.

## Data model (migration)

```sql
CREATE TABLE data_api_keys (…);   -- as above; index on branch_id; unique sha256
ALTER TABLE branches ADD COLUMN authenticator_password bytea;            -- sealed, AAD = branch id + "authenticator"
ALTER TABLE databases
    ADD COLUMN max_rows integer NOT NULL DEFAULT 1000,                   -- 1 .. 100000
    ADD COLUMN statement_timeout_ms integer NOT NULL DEFAULT 8000,       -- 100 .. 120000
    ADD COLUMN max_connections integer NOT NULL DEFAULT 5,               -- 1 .. 50, per branch
    ADD COLUMN default_schema text NOT NULL DEFAULT 'public';
```

- Existing branches get an `authenticator_password` and a public key the next time the bootstrap runs (reconcile triggers it; see Worker).

## The query language

`POST /data/v1/query`. The body is one JSON object. `"v": 1` is required, unknown fields are rejected anywhere in the tree, and the body is capped at 1 MiB.

```jsonc
{ "v":1, "op":"select", "from":"orders",                 // or "billing.invoices"
  "select":["id","total",{"col":"customer_name","as":"customer"}],   // omitted = all columns
  "where":{"and":[{"col":"status","eq":"paid"},{"col":"total","gte":100},
                  {"or":[{"col":"region","in":["eu","us"]},{"col":"vip","is":true}]}]},
  "orderBy":[{"col":"created_at","dir":"desc","nulls":"last"}],
  "limit":50, "offset":0 }
{ "v":1, "op":"insert", "into":"orders", "values":[{…},{…}], "returning":["id"] }
{ "v":1, "op":"update", "table":"orders", "set":{"status":"shipped"}, "where":{…}, "returning":"*" }
{ "v":1, "op":"delete", "from":"orders", "where":{…} }
{ "v":1, "op":"call", "fn":"add_points", "args":{"user_id":"…","n":5} }
{ "v":1, "op":"batch", "ops":[ {…}, {…} ] }
```

- **Names:** `"t"` means `<default_schema>.t`; `"s.t"` is qualified. Names are matched exactly against the schema cache (no case folding) and always quoted in SQL.
- **Filters:** `eq`, `neq`, `lt`, `lte`, `gt`, `gte`, `in` (a JSON array), `is` (`null`, `true`, `false`), `like` and `ilike`, combined with `and`, `or` and `not`. A filter object has exactly one operator.
- **Writes:** `returning` is a column list or `"*"`. Without it, writes return `[]`. An `update` or `delete` without `where` is refused unless it has `"all": true`. All rows in one `insert` must have the same keys.
- **`call`:** named arguments only. The function must be reachable by the role (part of the visibility rules).
- **`batch`:** the operations run in order in one transaction, all or nothing. It holds at most 50 operations, and batches don't nest.
- **Responses:**
  - select and writes return a JSON array of rows;
  - `call` returns the function's result as JSON, or an array for set-returning functions;
  - `batch` returns an array of each operation's result.

## SQL generation

**The SQL text depends only on the query's shape. Values are always bind parameters.**

- **Reads:** `SELECT coalesce(json_agg(t), '[]')::text FROM (SELECT "id", "customer_name" AS "customer" FROM "public"."orders" WHERE … ORDER BY … LIMIT $n OFFSET $m) t`.
- **Scalar filter values:** bound as text and cast to the column's type from the schema cache, e.g. `"total" >= CAST($2 AS numeric)`. Enums, domains, uuid and timestamptz all work, and indexes stay usable.
- **`in`:** bound as one jsonb array: `"region" = ANY (SELECT CAST(x AS <type>) FROM jsonb_array_elements_text($3) x)`.
- **`like` and `ilike`:** only on text-like columns (refused otherwise, `invalid_query`).
- **Insert:** `INSERT INTO "s"."t" ("a","b") SELECT "a","b" FROM jsonb_populate_recordset(NULL::"s"."t", $1)`.
  - The column list is the rows' key set, which is part of the shape, so columns left out get their defaults.
  - One row or a thousand produce the same SQL.
- **Update:** `UPDATE "s"."t" SET ("a","b") = (SELECT "a","b" FROM jsonb_populate_record(NULL::"s"."t", $1)) WHERE …`.
- **Writes are wrapped:** `WITH r AS (… RETURNING …) SELECT coalesce(json_agg(r), '[]')::text FROM r`.
- **Call:** `SELECT to_json("s"."f"("a" => CAST($1 AS <type>), …))::text`. A set-returning function becomes `SELECT coalesce(json_agg(f), '[]')::text FROM "s"."f"(…) f`. Arguments are cast to the declared parameter types, and a call is resolved to exactly one function by name and argument names.
- **Row cap:** `limit` is clamped to `max_rows`. Without `limit`, `max_rows` applies. The cap applies to `returning` too.
- **Shape cache:**
  - The key is `(branch_id, schema version, hash of the tree with its values removed)`, and the value is the SQL text plus the binding plan.
  - `in` lists and insert rows hash by structure, not length.
  - Identical SQL text lets sqlx reuse its prepared statement on each connection.
  - LRU-bounded per branch (1,000 shapes) and globally (50,000).

## The schema cache and visibility

- **Introspection:** one query, as `authenticator`, reads tables, views, columns (type, nullability, whether a default or generated value exists), enums and functions. It covers every schema except the reserved ones: `cheladb`, `pg_catalog`, `information_schema`, `pg_toast` and Neon's internal schemas (`neon`, plus any other schema the compute image creates for itself; the list lives in one constant with a test).
- **Keying:** by `(branch_id, schema version)`, LRU-bounded.
- **Visibility per role, computed lazily and cached with the schema:**
  - a table or view needs `has_schema_privilege(role, schema, 'USAGE')` and some table privilege;
  - a function needs `USAGE` on its schema and `EXECUTE`.
- **Hiding names:** a table, view or function the role can't reach gets the same error as one that doesn't exist (`unknown_table`, `unknown_function`), so a public key can't be used to discover table names.
- **Column privileges:** Postgres still enforces them (42501 → 403).

## The `cheladb` schema (machinery only)

Installed and healed by the worker's bootstrap. It's owned by a platform role (created by the bootstrap, `NOLOGIN`, not granted to app roles).

- `cheladb.schema_version (version uuid NOT NULL)`, holding one row. `SELECT` is granted to `authenticator`.
- An event trigger on `ddl_command_end` and `sql_drop` that sets `version = gen_random_uuid()`. Its function is `SECURITY DEFINER`, owned by the platform role, so DDL by any role can bump it.
- Nothing else in step 6. In 6b the claim helpers (`cheladb.user_id()` and so on) go here, with `EXECUTE` granted to app roles.
- If a user drops or changes it, the next bootstrap reinstalls it. Until then, requests to that branch fail with `unavailable` ("the data API's schema is missing; it will be reinstalled").

## Worker

**The data API bootstrap** is one idempotent script run through `admin_sql` as `cloud_admin`.

- **When it runs:**
  - after every compute (re)creation: provision, branch create, reset, restore, and reconcile's `RecreateBranchCompute`;
  - when reconcile finds `cheladb.schema_version` missing or the branch without a public key.
- **What it does:**
  1. Creates the `cheladb` schema and its owner role, the version table and the event trigger.
  2. Makes `authenticator` `LOGIN` with the branch's password, generating and sealing one if the branch has none. A restore can rewind the role, so this runs every time.
  3. Sets the default privileges in `public` for the owner role, as in Decisions.
  4. On a database's first bootstrap only (tracked in `cheladb`), applies the same grants to the tables and functions that already exist in `public`.
  5. Creates the branch's public key if it has none.
- **The secret-key role check** is a worker RPC (`data_api.check_role`): it answers whether the role exists, isn't a superuser and isn't a `neon_superuser` member, and on success grants the role to `authenticator`.
- **Compute:** Postgres (5432) is also published on loopback (host port 0), and `ComputeBackend` reports the published port next to the container address.

## API

**Internal listener (`:8083`), service identity tokens:**

| Endpoint | Caller | Returns |
|---|---|---|
| `GET /internal/data/keys/{sha256}` | gateway | `{ key_id, branch_id, kind, role }`, 404 when unknown or revoked; updates `last_used_at` (at most once a minute) |
| `GET /internal/data/branches/{id}/connection` | rest | `{ host, port, database, user, password, status, settings: { max_rows, statement_timeout_ms, max_connections, default_schema } }` |

**Platform API (gateway → orchestrator, platform auth):**

| Endpoint | Result |
|---|---|
| `GET /v1/databases/{id}/branches/{branch_id}/keys` | the keys (prefix, kind, role, name, dates); the public key in full |
| `POST /v1/databases/{id}/branches/{branch_id}/keys` `{ name, role? }` | 201 with the secret key, **once**; 422 for a refused role |
| `POST /v1/databases/{id}/branches/{branch_id}/keys/{key_id}/rotate` | 201 with the new key; the old one is revoked |
| `DELETE /v1/databases/{id}/branches/{branch_id}/keys/{key_id}` | 204; revokes. The live public key can only be rotated, not revoked. |
| `PATCH /v1/databases/{id}` | gains `max_rows`, `statement_timeout_ms`, `max_connections`, `default_schema` |

**Data API (gateway → rest):**

| Endpoint | Auth | Result |
|---|---|---|
| `POST /data/v1/query` | key | the query result |
| `GET /data/v1/schema` | secret key only | what the key's role can see: tables, views, columns, enums and functions (for `gen types`) |
| `GET /v1/databases/{id}/branches/{branch_id}/data-api/schema` | platform | everything `anon` or `authenticated` can reach, with each table's RLS flag (for Studio's warning). Served by the orchestrator, which checks the caller owns the database, then asks rest's internal `GET /internal/schema/{branch_id}` (service token; rest has no metadata access). |

## Errors

Body: `{ "error": { "code", "message", "detail"?, "hint"?, "sqlstate"? } }`.

| Case | HTTP | `code` |
|---|---|---|
| Malformed tree, unknown field, bad operator or value type | 400 | `invalid_query` |
| Unknown or unreachable table/view; function | 404 | `unknown_table`; `unknown_function` |
| Unknown column | 400 | `unknown_column` |
| Missing, unknown or revoked key; a public key on `/data/v1/schema` | 401 | `unauthorized` |
| Too many unknown-key lookups | 429 | `rate_limited` |
| 42501 | 403 | `permission_denied` |
| 23505, 23503, 23P01 | 409 | `conflict` |
| 23502, 23514, class 22 | 400 | `invalid_value` |
| P0001 (`RAISE EXCEPTION`) | 400 | `raised`, with the function's message |
| 40001, 40P01 | 409 | `retry` |
| 57014 | 504 | `timeout` |
| Branch not ready, compute unreachable, `cheladb` missing | 503 with `Retry-After` | `unavailable` |
| Body over 1 MiB | 413 | `too_large` |
| Anything else | 500 | `internal`: a generic message with the request id; details only in logs |

## Pools and caches

- **Pools:**
  - one sqlx pool per branch: at most `max_connections`, minimum 0, idle connections closed after 60 s, and an idle pool dropped after 5 min;
  - a global cap (`CHELADB_REST_MAX_CONNECTIONS`, default 200) bounds the total across pools.
- **Moved computes:** if a connection fails while connecting (before any statement is sent), the engine drops the branch's connection info, fetches it again and retries once. A failure after a statement was sent is never retried.

| Cache | Where | Key | Lifetime |
|---|---|---|---|
| Key → branch, role | gateway | sha256 | 30 s (unknown keys too) |
| Connection info | rest | branch | 60 s, or until a connect failure |
| Schema and visibility | rest | branch, schema version | LRU |
| Shapes | rest | branch, schema version, shape hash | LRU |

## SDK: `@cheladb/client` (`packages/client`)

No dependencies, built on `fetch`, so it runs in browsers, Node and edge runtimes.

```ts
const db = createClient<Database>({ url: "http://localhost:8000", key: PUBLIC_KEY });
const rows = await db.from("orders")
  .select("id", "total", { col: "customer_name", as: "customer" })
  .where({ status: "paid", total: { gte: 100 }, or: [{ region: { in: ["eu"] } }, { vip: true }] })
  .orderBy("created_at", "desc").limit(50);
await db.from("orders").insert([{ total: "12.50" }]).returning("id");
await db.from("orders").update({ status: "shipped" }).where({ id });
await db.from("orders").delete().where({ id });
await db.call("add_points", { user_id, n: 5 });
await db.batch((tx) => [tx.from("a").insert([…]), tx.from("b").update({…}).where({…})]);
```

- **`where`:** a bare value means `eq`, and keys combine with `and`. It compiles to exactly the tree above.
- **Typing:** builders are thenable, and table names, column names, filter values, `select` result shapes (with aliases) and function arguments are all typed from `Database`.
- **Errors:** failures throw `DataApiError { status, code, message, detail, hint, sqlstate }`.
- **Values:** `numeric` and `int8` are strings, and timestamps are ISO strings, as Postgres's JSON gives them.

## Type generation

`cheladb gen types --url <gateway> --key <secret key> [--out db.ts]` reads `GET /data/v1/schema` and writes a `Database` type, grouped by schema.

- **Type mapping:**
  - text-like, uuid, `numeric`, `int8`, `date` and the time types → `string`;
  - `int2`, `int4`, `float4` and `float8` → `number`;
  - `bool` → `boolean`;
  - `json` and `jsonb` → `Json`;
  - arrays → `T[]`;
  - enums → string unions;
  - domains → their base type;
  - anything else → `string`.
- **Per table:** a `Row` type, an `Insert` type (columns with a default or generated value, or that are nullable, are optional; generated columns are absent) and an `Update` type (all optional).
- **Per function:** `Args` and `Returns`.

## Studio

A "Data API" panel per branch:
- the endpoint URL and the public key, with a copy button and Rotate;
- secret keys:
  - create, with a name and a role (default `authenticated`); the key is shown once with a copy button;
  - list, showing prefix, role and last used;
  - revoke and rotate;
- **the RLS warning:** every table `anon` or `authenticated` can reach that has RLS off. It reads "Anyone with your public key can read these rows", plus "and change them" when writes were granted to `anon`, and "Enable RLS before production." Each table gets a copyable `ALTER TABLE … ENABLE ROW LEVEL SECURITY;` and a note that RLS without a policy blocks those roles entirely.

The database settings form gains `max_rows`, `statement_timeout_ms`, `max_connections` and `default_schema`.

## Testing

- **Rust unit:**
  - the parser: unknown fields, bad operators and wrong value shapes are rejected;
  - SQL golden tests per shape;
  - the shape hash ignores values, row count and `in` length;
  - visibility rules, the error mapping and the key format;
  - the gateway's `Auth::DataKey`: the cache, the unknown-key cache and the rate limit.
- **Contract fixtures:** `packages/client/fixtures/*.json` pair SDK calls with the trees they must produce. Vitest checks that the SDK builds them, and Rust tests check that the server parses them.
- **Live tests** (ignored, need the deps stack), against a real compute:
  - every operation and a batch rollback;
  - casts for enum, domain, uuid, timestamptz, numeric, arrays and jsonb;
  - the default grants: `anon` reads, and its writes get 403;
  - a secret key with a custom role;
  - an RLS policy is enforced;
  - unfiltered writes are refused;
  - DDL is picked up on the next request;
  - a restore that rewinds the schema serves the old shape correctly;
  - a dropped `cheladb` schema is reinstalled by the bootstrap;
  - a moved compute (after a reset) is reconnected;
  - an unreachable table looks the same as a missing one.
- **`just data-e2e`:** the story in "Done when".
- **Studio:** vitest for the panel, keys and warning, then a Playwright pass.

## Out of scope

- App users, JWTs, `db.auth` and the users table: step 6b.
- Relations (nested selects through foreign keys) and upsert: the second pass.
- `count`, pagination cursors, JSON-path filters and full-text search operators.
- A one-click "Enable RLS" in Studio.
- Scale to zero and waking a stopped compute: step 7. Until then a stopped compute is `unavailable`.
- TLS on the gateway: the encryption-in-transit step.
- Neon's auth broker, `local_proxy` and `pg_session_jwt`: not used; the fork's spec decides whether to keep them.
