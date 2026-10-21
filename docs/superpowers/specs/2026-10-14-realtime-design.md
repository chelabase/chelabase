# TE-RT: realtime rows, presence and app channels (design)

Branch `table-realtime`. Roadmap step TE-RT, before TE-B2; it answers the TE-B1 merge's open questions (a) Studio event stream, (b) realtime row changes and (c) the save conflict check. Decisions (1)–(18) are in the roadmap's "TE-RT" note; design parts 1–8 were approved by the user on 2026-10-14. Choices made while writing this spec (not asked) are **S-rulings** at the end.

## Goal

People editing a table in Studio see each other's changes and each other's selected cells live, and a save never silently overwrites a change made since the editor opened. Apps get broadcast channels with presence through `@chelabase/client`. All of it runs through one new service, `services/realtime`, over one WebSocket per tab or client. No row value ever travels through realtime: Studio receives primary keys only and refetches through console/query as the viewer's own role, so Postgres decides what each viewer may see.

**Delivery:** one spec, two plans and branches.

- **Neon fork round 3 (its own step, before RT1):** the `chelabase_keys` output plugin (part 3), plus the parked fork items (Members (c), TE-A R5, PB R12, compute_ctl migrations before `running`). One image build, one `NEON_REF` bump.
- **RT1, core + Studio:** the realtime service, the gateway route, tickets, the change source, per-role key filtering, Studio presence, platform events, the conflict check and the Studio UI.
- **RT2, apps:** the "Enable realtime" switch, `realtime.authorize`, app channels and presence, resumable channels, server publish, the `@chelabase/client` surface and docs.

**Not in scope:** row-change keys for apps (decision 9: apps get broadcast channels), the Studio Realtime tab (later step), pointer/cursor tracking (presence is the selected cell only).

## 1. The service and how requests reach it

- **`services/realtime`**, a new binary on axum 0.8.9 (pinned; latest) with the `ws` feature. Listens on `CHELABASE_REALTIME_ADDR` (default `127.0.0.1:8087`, public via the gateway) and `CHELABASE_REALTIME_INTERNAL_ADDR` (default `127.0.0.1:8088`, health and internal calls). Compose, the workbench and `just` run it like rest.
- **Routing:** `Service::Realtime` in `crates/chelabase-routes/src/lib.rs` (`name()` = `"realtime"`, the internal identity `aud`), an address in the gateway's `Upstreams`.

| Route | Auth | Purpose |
|---|---|---|
| `POST /v1/realtime/tickets` | `Management` | ticket for a Studio member (RT1) |
| `POST /data/v1/realtime/tickets` | `DataKey` (+ optional `chelabase-user`) | ticket for an app client (RT2) |
| `POST /data/v1/realtime/publish` | `SecretDataKey` | server-side publish (RT2) |
| `GET /v1/realtime/socket?ticket=…` | `Public` (the ticket is the credential) | WebSocket upgrade |

- **Tickets:** 32 random bytes (base64url), valid 30 s, single use. Valkey key `rt:ticket:<sha256(ticket)>` holds the verified identity the gateway minted (`x-chelabase-identity`, verified by the realtime service with the gateway's public key), consumed with `GETDEL` at connect (a replay finds nothing). Valkey is reached through the workspace's existing BSD-3-Clause client crate (`redis` 1.7.1, `Cargo.toml:98`). The gateway's access log omits the `ticket` query parameter.
- **Gateway:** upgrades are allowed on the socket route only, with the 1 h idle timeout site hosts get (`services/gateway/src/proxy.rs:111-118`, `:128-140`); every other API route keeps the default read timeout and is never upgraded. No other pipeline change.
- **Connection lifetime:**
  - Studio sockets are bound to the OIDC token's `exp`. Studio sends `{"type":"auth","token":…}` whenever oidc-client renews; the service verifies it as the gateway does (same JWKS settings) and closes with 4001 at `exp` without a renewal. The token's `sub` must equal the ticket's; a different `sub` closes with 4003.
  - App sockets re-check their data key and end-user session every 30 s (the gateway's own cache freshness): `/internal/data/keys/{sha256}` on the orchestrator and the session liveness route; a revoked key or session closes with 4003.
- **Orchestrator internal routes**, behind a new static bearer `CHELABASE_REALTIME_INTERNAL_TOKEN` (the existing one-token-per-caller pattern, `services/orchestrator/src/internal/app.rs:72-101`):
  - **new** `POST /internal/realtime/authorize {token, target}` → runs the existing `access::authorize` (`services/orchestrator/src/access.rs:219`) for the member the token names; returns `{allowed, reason?}`. The one Members check, never re-implemented.
  - **new** `POST /internal/realtime/projects {token}` → the project ids that person can see (`list_projects`' filter, `projects.rs:179-210`) and, per project, the database ids they may see (`authorize_database`).
  - **new** `POST /internal/realtime/visible` → runs a part-4 check query on the orchestrator's owner pool (`console.rs:4-8`) for owner viewers.
  - **reused** `GET /internal/data/branches/{id}/connection` (as rest uses it, `services/rest/src/conn.rs:199-250`) and `/internal/data/keys/{sha256}`.
- **NATS:** subjects under `chelabase.rt.`; core NATS for fan-out between instances. KV bucket `CHELABASE_RT_LEASES`: one key per (branch, Postgres db), TTL 10 s, renewed every 3 s; the holder runs that database's decoder.
- **Instances:** each socket lives on one instance; an instance subscribes on NATS only to channels its sockets have joined. No sticky routing beyond the socket.

## 2. The socket protocol

- **Framing:** JSON text frames; subprotocol `chelabase.realtime.v1` (`Sec-WebSocket-Protocol`; version only, never a credential); an unknown version is refused at upgrade. The server pings every 25 s and closes (4008) when no pong arrives within 10 s. Inbound frames over 64 KB are refused.
- **Channels:**

| Channel | Who | Join params |
|---|---|---|
| `table:<database_id>/<branch_id>/<pg_db>/<schema>/<table>` | Studio | `role`: a role name, or `null` for the owner (console/query's meaning) |
| `project:<project_id>` | Studio | none |
| `app:<name>` | apps (RT2) | `resume_from?` (a sequence) |

  Switching "Act as" leaves and rejoins (a fresh authorization). Unknown channel kinds are refused (`invalid`).
- **Client → server:** `auth {token}`; `join {ref, channel, params}`; `leave {ref, channel}`; `presence {channel, state}` (table state `{row: [pk…] | null, column: string | null, editing: bool}`; app state free-form JSON); `publish {ref, channel, event, payload}` (RT2).
- **Server → client:**
  - `joined {ref, channel, info}`; for a table, `info = {pk: [column names] | null, live: bool, reason?}`.
  - `error {ref?, channel?, code, message}`; codes `forbidden`, `not_found`, `limit`, `invalid`, `unavailable`.
  - `rows {channel, seq, changes}`; each change is `{op: "insert"|"update", keys}`, `{op: "delete", keys}` (bypass viewers only), `{op: "delete"}` (keyless), or `{op: "changed"}` (no PK, bulk, TRUNCATE, or a failed check: refetch the page).
  - `presence_state {channel, members}` after `joined`, then `presence_diff {channel, joins, leaves}` (an update is a leave plus a join of the same `id`). A Studio member is `{id, sub, name, email, role, color_seed, state}`; `id` is per socket and channel.
  - `status {channel, live, reason?}`: live updates on a joined table channel started or paused (part 3.3).
  - `project {channel, event}` (part 5).
  - `message {channel, event, payload, seq?}` (RT2).
  - `resync {channel, reason}`: changes may have been missed; refetch everything shown.
- **Keys** are arrays of the PK columns' Postgres text output in PK order (no bigint/numeric precision loss).
- **`seq`** counts per channel per socket; a gap means missed frames and is handled like `resync`. On a resumable app channel `seq` is the JetStream sequence, which `resume_from` takes.
- **Close codes:** 4001 token expired, 4003 access revoked or identity mismatch, 4008 pong timeout, 4029 socket over a limit, 1012 service restarting (reconnect with backoff).

## 3. The change source

### 3.1 The `chelabase_keys` output plugin (fork round 3)

- In the Postgres fork, `contrib/chelabase_keys`; a logical decoding output plugin (loaded by name at slot creation, no `shared_preload_libraries`). Decodes at commit only (no streaming of in-progress transactions).
- **Output:** one JSON message per committed transaction:

```json
{"v":1,"lsn":"0/1A2B3C","changes":[
  {"rel":16412,"s":"public","t":"orders","op":"i","k":[["42"],["43"]]},
  {"rel":16412,"s":"public","t":"orders","op":"d","k":[["7"]]},
  {"rel":16500,"s":"public","t":"events","op":"c"}]}
```

  - `op`: `i` insert, `u` update, `d` delete, `c` changed without keys. Changes are grouped per relation and op within a transaction; a key appears at most once per (relation, op).
  - `k`: the PK columns' text output (the type's output function) in PK column order.
  - **Keys only when the replica identity is the PK** (`REPLICA IDENTITY DEFAULT` with a PK, or `USING INDEX` on the PK index). No PK, `FULL`, `NOTHING`, or `USING INDEX` on another index → `c`.
  - **An update that changes the PK** emits the old key as `d` and the new key as `i`.
  - `TRUNCATE` → `c` for each truncated relation.
  - **Bounded:** option `max_keys_per_table` (default 1000) collapses a relation to `c` once a transaction exceeds it; option `max_tables` (default 256) collapses the transaction to `{"v":1,"lsn":…,"all":true}`.
  - A transaction with no row changes emits nothing. Catalog changes are not emitted.
  - Options are validated at `START_REPLICATION`; an unknown option is an error.
- **Tests (fork round 3):** regression tests (`make check` in the contrib directory) for every rule above, including composite and non-text PKs, PK updates, TRUNCATE, the collapse limits, and replica identity variants; one live test on a real Neon compute: a temporary slot with the plugin, changes decoded, compute restart, the slot gone.

### 3.2 Replication login and limits

- `chelabase_realtime`: `LOGIN REPLICATION NOBYPASSRLS`, not a superuser, created by the compute spec (`crates/chelabase-neon/src/spec.rs`), password a platform secret held by the realtime service only, never shown in Studio. A replication login can create a slot with any plugin (e.g. `test_decoding`, which emits row values), so this credential is guarded like the owner's.
- `spec.rs`: `max_replication_slots` and `max_wal_senders` 10 → 20 (applies when a compute next starts).
- `CHELABASE_REALTIME_MAX_SLOTS_PER_COMPUTE` (default 10): realtime never holds more, so the customer keeps at least 10.

### 3.3 Slot lifecycle (per branch, Postgres db)

1. The first `table:` join on that database makes the joining instance try to take the lease (create-if-absent in `CHELABASE_RT_LEASES`). If another instance holds it, nothing more happens here: the holder is told over NATS (`chelabase.rt.watch.<branch_id>.<db_oid>`, with a heartbeat per watching instance every 5 s) that the database is watched.
2. The holder connects with `replication=database` as `chelabase_realtime`, runs `CREATE_REPLICATION_SLOT rt_<random> TEMPORARY LOGICAL chelabase_keys`, then `START_REPLICATION` from the slot's consistent point.
3. It acknowledges an LSN (standby status update) once that transaction's keys are published to NATS.
4. 30 s after the last watching heartbeat stops, it disconnects; the temporary slot goes with the session; the lease is released.

- **Gaps:** a lost lease, a crashed holder or a compute restart means the next holder creates a fresh slot starting "now". Every watcher of that database gets `resync`. Correct by construction: Studio holds no state that keys alone could have kept fresh.
- **No free slot** (the compute's limit or realtime's cap): joins still succeed with `info.live = false, reason: "no_slot"`; the holder retries every 30 s; when it succeeds, watchers get `resync` and a `status {channel, live: true}` frame.
- **Merging in the service:** keys are merged per relation over a 100 ms window; more than 1000 keys in a window become `changed`. Published on `chelabase.rt.rows.<branch_id>.<db_oid>.<rel_oid>`. A join resolves (schema, table) to its OID; a `resync` re-resolves it, so a rename is picked up.
- **Rust replication client:** our tokio-postgres fork has no replication support. The plan's pre-flight checks whether `pg-walstream` 0.9.0 (2026-09-20; aws-lc-rs) accepts a custom plugin and its options; if not, CopyBoth support goes into our `rust-postgres` fork. Shown to the user as a plan ruling.

## 4. Row access and filtering (Studio)

**Invariant (stated in the code, tested by a model test):** a viewer receives a row's key only if, at the moment of the check, that viewer's own console/query read would return that row: the same role, the same claims, the same transaction preamble (`crates/chelabase-query/src/engine.rs:55-67`). Anything uncertain degrades to keyless `changed`, never to keys.

- **Joining `table:…` with role R:**
  1. `POST /internal/realtime/authorize` with the member's current token and `Target::Role { database, branch, pg_database, role }`, read access. So the owner (`role: null`) needs Admin (R7), a named role needs a grant or the R21 chain. Refused → `error forbidden`.
  2. As R: `has_table_privilege(oid, 'SELECT')`, SELECT on every PK column, and `row_security_active(oid)`. No SELECT → `forbidden`. RLS not active for R on this table (RLS off, R `BYPASSRLS`, or R owns the table without `FORCE`) → a **bypass viewer**; otherwise an **RLS viewer**.
- **Re-authorization:** every joined channel is re-authorized on each `auth` renewal, on an access event, and every 60 s. The orchestrator publishes `chelabase.rt.access.<tenant_id>` after any metadata write that changes grants, roles, membership or project access; realtime then re-authorizes that tenant's channels at once. A failed re-authorization → `error forbidden` and the channel is left. The privilege/RLS probe (step 2) is rerun on re-authorization and on `resync`.
- **Filtering**, per merge window and relation:
  - Viewers are grouped by **check identity** (role, claims, owner or not); N viewers acting as `billing` cost one query.
  - Bypass groups get every key, deletes included.
  - RLS groups: one read-only check query as that identity, `statement_timeout` 2 s: `SELECT <pk cols>::text… FROM <schema>.<table> WHERE (<pk cols>) IN (VALUES ($1::<type>, …), …)`. Insert/update keys go to the groups whose query returned them; deletes go to RLS groups keyless.
  - A check that times out or errors → that group gets keyless `changed`; logged.
- **Check connections:** realtime keeps `authenticator` pools per (branch, db) from the connection lookup (60 s cache) and applies the preamble through `chelabase-query`. **Realtime never holds owner credentials:** owner viewers' checks go to `POST /internal/realtime/visible` (the orchestrator's owner pool).
- **Visibility cache:** (check identity, relation, key) → visible, until a change for that key arrives or 30 s pass. Presence (part 5) uses it.
- **Degraded states:** compute unreachable, or orchestrator unreachable during re-authorization → the channel stays joined but receives only keyless `changed` and a `resync` every 30 s; after 3 failed re-authorizations in a row it is left with `unavailable`.

## 5. Presence and platform events

### 5.1 Studio presence (`table:` channels)

- A member: `{id, sub, name, email, role, color_seed, state}`. `name`/`email` from the verified token; `color_seed` = a hash of `sub` (stable across tabs and tables).
- Everyone with the table open sees every member (the avatar stack). `state.row`, `state.column` and `state.editing` reach a viewer only if that viewer's check identity can see the row (cache, else one check query); otherwise the viewer gets the member with `state` blanked to `{row: null, column: null, editing: false}`.
- When a `rows` batch touches a key that someone has selected, that selection is re-checked per group and a change in visibility is sent as `presence_diff`.
- Studio debounces selection changes by 50 ms. The server accepts 20 presence frames per second per socket; over that, frames are dropped and the latest state wins.
- "Ana is editing this cell" is computed client-side from another member's `editing: true` on the same (row, column). No lock.

### 5.2 Presence across instances (Studio and apps)

- Each instance owns its sockets' members and publishes joins, leaves and state changes on `chelabase.rt.presence.<channel_hash>`, plus a heartbeat every 5 s on `chelabase.rt.presence.hb.<instance_id>`.
- An instance that starts watching a channel requests a snapshot (`chelabase.rt.presence.sync.<channel_hash>`, request/reply; every instance answers with its local members).
- Three missed heartbeats drop that instance's members (`presence_diff` leaves). A closed socket leaves at once.
- Presence is never stored; a full restart starts empty and clients re-send their state on reconnect.

### 5.3 Platform events (`project:` channels)

- Join allowed if the project is in the person's visible list (`/internal/realtime/projects`); re-checked like part 4 (renewal, access events, 60 s).
- **Publishers** (right after the metadata write commits):
  - the worker: branch status changes (`finish_branch`, `set_branch_status`: `services/worker/src/branch_create.rs:567-600`, `crates/chelabase-metadata/src/branches.rs:569`) and branch deletion;
  - the orchestrator: database and branch create/delete in its API handlers;
  - the DDL hook (`services/orchestrator/src/internal/ddl.rs`): a Postgres database created or dropped inside a branch.
- **Payload:** ids and status only: `{event: "branch.status", database_id, branch_id, status}`, `{event: "branch.created"|"branch.deleted", database_id, branch_id}`, `{event: "database.created"|"database.deleted", database_id}`, `{event: "pg_database.created"|"pg_database.dropped", database_id, branch_id, name}`.
- Events scoped to a database go only to people who may see that database (R17 narrowing), from the cached per-project database list.
- Studio uses an event only to **invalidate** the matching TanStack Query keys; the refetch goes through the API. While the socket is live, the Branch picker's `pollWhile` (`apps/studio/src/components/table-editor/table-editor.tsx:131`) and the `/v1/me` refetch for new databases are off; when it isn't, today's polling is the fallback. A failed publish is logged; every reconnect and `resync` invalidates everything the editor shows.

## 6. Studio UI and the conflict check

### 6.1 Live rows (`rows-view.tsx`)

- The view joins `table:…` for its target and act-as role; it leaves and rejoins when either changes.
- **Keys on the current page (insert/update):** one refetch of exactly those rows by key as the viewer's role, spliced into the page; changed cells flash briefly (no motion under `prefers-reduced-motion`). A key whose refetch returns nothing (the row left the role's view or the filter) fades out.
- **Insert keys not on the page:** a chip "N new rows · Show" (refetches the page and the count).
- **Keyed delete:** the row is struck through, then removed on the next page refetch. **Keyless delete, `changed`, `resync`, a `seq` gap:** page and count refetch, debounced 500 ms.
- **An open cell editor or row panel whose row changes** is never overwritten: the cell shows a "changed since you started" marker with the new value on hover.
- **Header:** a "Live" dot, or "Live updates paused" with its reason (no free slot, reconnecting, unavailable).

### 6.2 Presence rendering

- Another member's selected cell: a 2px box in their color plus their row tinted at low alpha; hover → a tag with name, email and acting-as role.
- While they edit: the cell gets a filled tint; opening an editor on the same cell shows "Ana is editing this cell".
- An avatar stack in the table header: initials in each member's color, at most 5 then "+N", the same tag on hover.
- The viewer's own selection is never drawn as presence.
- Colors: 8 hues from the lagoon palette, chosen by `color_seed`.
- One `presence-layer` component over the grid, separate from the cell components.
- No separate Impeccable shape pass: the TE-B1 surface brief (`apps/studio/.impeccable/surfaces/src-components-table-editor-table-editor-tsx.md`) gets a short presence and live-state section; the finish reviewer checks it at the end of RT1.

### 6.3 Conflict check

- `saveCell` (`rows-view.tsx:315-346`) and the row panel's update put, besides the PK, each edited column's **original value** in the `WHERE`: `eq` the value, or `is null` when it was NULL (`chelabase-query` `CondOp::Is`, `query.rs:248`).
- Columns whose type has no equality operator (`json`, `xml`, geometric types) are left out of the comparison; the save shows "No conflict check for this column type". The plan's pre-flight confirms the op tree can't compare them as text.
- **0 rows updated:** re-read the row by key. Missing → "This row was deleted or is hidden from this role." Present → "Changed by someone else", showing their value and yours, with **Overwrite** (save again with the PK only) and **Take theirs**.
- Deletes keep today's behaviour ("deleted fewer rows than asked"); inserts can't conflict.

## 7. App channels (RT2)

- **"Enable realtime"**, per (database, branch, Postgres db), shaped like the data API's enable endpoint, Developer+, refused unless the data API is on. Worker RPC `chelabase.rpc.realtime.enable` creates, owned by the database owner:

```sql
CREATE SCHEMA realtime;
CREATE TABLE realtime.channel_settings (
  pattern      text PRIMARY KEY CHECK (pattern ~ '^[A-Za-z0-9_:.*-]{1,128}$'),
  resumable    boolean  NOT NULL DEFAULT false,
  retention    interval NOT NULL DEFAULT '2 minutes' CHECK (retention > '0' AND retention <= '24 hours'),
  max_messages integer  NOT NULL DEFAULT 1000 CHECK (max_messages BETWEEN 1 AND 10000)
);
```

  A metadata flag records it. **Disabling** refuses new app sockets and closes open ones, keeping the schema (no app SQL lost). Branches and merges carry the schema as ordinary DDL. Studio gets only the switch, next to the data API's.
- **Tickets:** `POST /data/v1/realtime/tickets` with the data key (+ `chelabase-user`); the ticket holds the gateway's `DataIdentity`. When the end user's token refreshes, the SDK takes a new ticket and swaps sockets (open the new one, re-join, then close the old); claims are never changed in place.
- **Authorization:** `realtime.authorize(channel text, action text) returns boolean`, written by the app; `action` ∈ `subscribe`, `publish`, `presence`. Called as the socket's role and claims with the data API's preamble, timeout 1 s; on every join and every presence track; for publish, cached per (socket, channel) for 60 s.
  - A **secret key** (the service key) skips the function.
  - A public key with a missing function, `false`, an error or a timeout is refused (fails closed).
  - Channel names: 1–128 characters of `[A-Za-z0-9_:.-]`.
- **Delivery:**
  - **Ephemeral** (default): NATS core on `chelabase.rt.app.<branch_id>.<db_oid>.<name_hash>`; at most once; on reconnect the SDK emits `gap`.
  - **Resumable:** a channel matching a `channel_settings` row with `resumable` gets a JetStream stream per (database, settings row), created on first publish, with that row's `retention` as `max_age` and `max_messages` as `max_msgs_per_subject`. Patterns use `*` for one `:`-separated segment (`room:*`); the most specific match wins (fewest `*`, then longest). Settings are re-read with a 30 s cache. At most 50 streams per database. A join with `resume_from` replays messages after that sequence; older than retention → `resync {reason: "gap"}`.
- **Server publish:** `POST /data/v1/realtime/publish {channel, event, payload}` with a secret key.
- **App presence:** `track(state)` needs `presence`; seeing members needs `subscribe`. State at most 1 KB; at most 1000 members per channel. A member is `{id, state, sub?}` (`sub` from claims; no names or emails).
- **SDK (`@chelabase/client`, still no runtime dependencies):**

```ts
const room = client.realtime.channel("room:42", { resume: true });
room.on("message", ({ event, payload, seq }) => …);
room.on("gap", () => resyncFromTables());
await room.subscribe();
await room.publish("typing", { on: true });
await room.presence.track({ name: "Ana" });
room.presence.on("sync" | "join" | "leave", …);
room.presence.list();
```

  One socket per client, opened lazily; reconnect with backoff and jitter; re-join every channel (resumable channels from their last `seq`, ephemeral ones emit `gap`); global `WebSocket` (the plan checks the package's `engines` range).
- **Docs:** a "Realtime" guide in `apps/docs` (enabling, `realtime.authorize` examples, settings, presence, limits, delivery guarantees) and the two endpoints in the data API's OpenAPI document.

## 8. Limits, failures and testing

### 8.1 Limits (settings with defaults; self-hosters may change them)

| Limit | Default |
|---|---|
| Sockets per member (Studio) / per data key (apps) | 20 / 10 000 |
| Channels per socket | 50 |
| Inbound frame / app payload / presence state | 64 KB / 32 KB / 1 KB |
| Presence frames / publishes per socket | 20/s / 20/s |
| Members per channel | 1000 |
| Slots realtime may hold per compute | 10 (of 20) |
| Resumable streams per database | 50 |
| Merge window / keys per window before `changed` | 100 ms / 1000 |

A limit hit → `error limit` on that frame, or close 4029 for a socket-level limit. Nothing is dropped silently except presence frames over the rate (latest state wins).

### 8.2 Failures

- **Realtime down:** tickets 502; Studio falls back to today's polling, no live rows; the SDK reconnects with backoff.
- **NATS down:** sockets stay up; every channel gets `resync`; Studio shows "Live updates paused".
- **Valkey down:** no new tickets (503); open sockets unaffected.
- **Orchestrator down:** new joins fail `unavailable`; joined channels degrade per part 4.
- **Compute restart:** the slot is gone; watchers get `resync`.
- **Instance crash:** its sockets reconnect; others drop its presence after 15 s; its lease expires after 10 s and another instance takes the slot.
- **Shutdown (SIGTERM):** close every socket with 1012, release leases, close replication connections (their temporary slots go at once).

### 8.3 Testing

Strict TDD, coordinator-written RED tests.

- **Model test of the part-4 invariant:** a property test over generated policies, roles and change sequences, comparing "keys a viewer receives" with "rows that viewer's own read returns", on a fake at the unit level and once live against Postgres RLS.
- **Unit:** frame parsing and validation; channel grammar; ticket single use and expiry; merging and the switch to `changed`; pattern matching and specificity; presence diffing and instance expiry; SDK reconnect, resume and socket swap with a fake WebSocket.
- **Gateway:** upgrades only on the socket route; the 1 h timeout there only; `ticket` absent from the access log.
- **Live** (`just workbench test`, the plan's one stack):
  - `realtime_live` (RT1): join and refusal; revocation within a second of an access event; owner vs member role vs RLS role; keyed vs keyless deletes; slot lifecycle and `resync` after a compute restart; slot exhaustion; project events; the conflict check.
  - `realtime_app_live` (RT2): enable/disable; authorize outcomes (true, false, missing, error, timeout); resumable replay and `gap`; server publish; presence.
- **Studio:** vitest for the live-rows reducer, the presence layer and the conflict dialog; RT1's e2e and browser pass with two signed-in members in isolated Playwright contexts.
- **Fork round 3:** the plugin's regression and live tests (3.1).

### 8.4 Process

An opus red-team pass on this spec before fork round 3 (ticket replay, revocation windows, the replication credential, filter bypasses, presence leaks, authorize failing open, resumable streams across tenants); its findings become spec rulings. Then fork round 3, then RT1's plan, then RT2's.

## Red-team rulings (2026-10-14)

An opus red-team pass attacked this spec against the code (22 findings: 5 critical, 12 important, 5 minor; 6 factual errors). Each ruling below **supersedes** the section it names; where a section and a ruling disagree, the ruling wins. U-marked rulings were decided by the user; the rest are mechanical fixes.

### Critical

- **R1 (RT-1, §3.2; U: restrict in the fork).** `chelabase_realtime` is **not** a spec role: compute_ctl makes spec roles `LOGIN`, `BYPASSRLS` members of `neon_superuser` (roadmap "Superuser SQL" note). The worker creates it with `admin_sql` as `cloud_admin`, like `authenticator` (`services/worker/src/provision.rs:20-40`): `LOGIN REPLICATION NOBYPASSRLS NOINHERIT`, no memberships, no proxy secret. **Fork round 3 adds a walsender restriction for this role:** replication connections may only `CREATE_REPLICATION_SLOT … TEMPORARY LOGICAL chelabase_keys` and `START_REPLICATION` on slots that same session created; no SQL, no other plugin, no `DROP`/`ADVANCE`/`ALTER` of any slot; its non-replication logins are refused. Regression tests for each refusal.
- **R2 (RT-2, §4, S6).** Bypass status is never trusted across windows: in every merge window, a bypass group's check transaction first reruns `row_security_active(oid)` and the PK-column SELECT privileges; if either changed, the group is reclassified before any key is released. A change to `chelabase.schema_version` in the decoded stream (it is bumped on every `ddl_command_end`, `services/worker/src/data_api.rs:330`, `:391-393`) pauses that database's channels (keyless `changed` only) until every group's probe has rerun. Role-attribute changes (`ALTER ROLE … NOBYPASSRLS`) fire no event trigger, so the per-window probe is what catches them.
- **R3 (RT-3, §4 join).** A join runs **the console's gates**, through the orchestrator: data API `On` (`services/orchestrator/src/console.rs:408-410`), a non-Backup `Ready` branch (`:387-392`), and the worker's `check_role` (`:421`, `:768-800`: refuses owner-level roles, grants the role to `authenticator`). Every check transaction also runs rest's `OWNER_LEVEL_SQL` (`services/rest/src/exec.rs:38-42`, cached ≤30 s like `exec.rs:86`); an owner-level role → `forbidden` and the channel is left.
- **R4 (RT-4, §5.2).** `channel_hash = hex(sha256(scope ‖ channel))`, full length, where scope is `branch_id ‖ db_oid` for `table:`/`app:` channels and `project_id` for `project:`. Every presence and fan-out message carries its full scope; a receiver drops a message whose scope doesn't match its channel.
- **R5 (RT-5, §7 resumable).** Streams are named `RT_` + hex(sha256(branch_id ‖ db_oid ‖ pattern)). Each stream gets `max_bytes` (default 64 MB) and a subject cap (`max_msgs_per_subject` from the row, plus at most 10 000 subjects); per database at most 50 streams and 1 GB; per tenant 5 GB (settings). Streams are deleted on branch delete, database delete and "Disable realtime".

### Important

- **R6 (RT-6, §2 Keys, §3.1, §6.1).** Keys reach clients in **Studio's key form**: `JSON.stringify` of the PK values as console/query returns them (`apps/studio/src/lib/table-editor/selection.ts:53`), built server-side in the check query with `to_jsonb` of each PK column; bypass groups' keys go through the same conversion query (no RLS check needed, but the same output). The plugin emits PK text output under **fixed settings** it forces while decoding (`DateStyle=ISO, YMD`, `TimeZone=UTC`, `IntervalStyle=iso_8601`, `extra_float_digits=3`, `bytea_output=hex`); the check preamble sets the same before parsing keys. Only keys the check query itself returned are delivered. Presence `state.row` uses the same JSON key form; realtime validates it by the setter's own check (R15).
- **R7 (RT-7, §3.3).** The plugin also emits each transaction's `xid`. A check transaction whose snapshot doesn't yet see that xid (`pg_xact_status` / snapshot `xmax` comparison) waits up to 2 s (polling); still not visible → keyless `changed`. Cause: with `synchronous_standby_names=walproposer` (`crates/chelabase-neon/src/spec.rs:201`) a committed transaction stays in progress for snapshots until the safekeepers ack, while the walsender has already decoded it.
- **R8 (RT-8, §6.1, §3.3).** Studio refetches its page after `joined {live: true}` and after every `status {live: true}`; when a slot is (re)created after viewers joined, they get `resync`.
- **R9 (RT-9, §1).** A Studio socket's first frame must be `auth {token}` within 5 s, otherwise close 4001 (the ticket's gateway identity carries no OIDC `exp`). A renewal must match the ticket's `sub` **and** `org_id` (extends S2). Studio tickets may join only `table:`/`project:`; app tickets only `app:` and never send `auth`. App sockets close at the end user token's `exp` (the SDK swaps before). Ticket responses carry `Cache-Control: no-store`.
- **R10 (RT-10, §1 internal routes).** `/internal/realtime/authorize` and `/internal/realtime/projects` take the raw OIDC token and the orchestrator verifies it with its own `JwtVerifier`; never a bare sub/org. Resolving the member is **read-only** (no member auto-creation, no first-member admin claim, unlike `resolve_member`, `services/orchestrator/src/lib.rs:1725-1733`); an unknown member → refused. `/internal/realtime/visible` takes (token, branch, pg db, relation OID, keys), re-runs the owner authorization itself, builds its SQL by OID with bound keys, and is used only for the owner's privilege probe (R19). The reused routes (`/internal/data/branches/{id}/connection`, `/internal/data/keys/{sha256}`) move to `require_any_bearer` with the realtime token added (they accept one caller's token today, `services/orchestrator/src/internal/mod.rs:429-447`).
- **R11 (RT-11, §1; U: both, before RT1).** RT1's first task (or a short chore before it, decided at the plan): **NATS accounts** with per-service publish/subscribe permissions (realtime: `chelabase.rt.>` only; orchestrator and worker: their job, RPC and `chelabase.rt.` publish subjects), credentials as platform secrets; and a **compute network** that cannot reach NATS or Valkey (computes keep only what they need: pageserver, safekeepers, storage broker, the control-plane hooks). Today's job subjects are covered by the same change.
- **R12 (RT-12, §4 visibility cache).** The visibility cache lives at most one merge window and is flushed for a whole database on any decoded change in it and on a `schema_version` change (policies may read other tables). It exists only to deduplicate checks within a window.
- **R13 (RT-13, §7).** `realtime.authorize` for `subscribe` and `presence` is re-run every 60 s per (socket, channel) and on a socket swap; `publish` is cached 60 s as before. Only `true` allows; `false`, `NULL`, an error or a timeout refuse and leave the channel.
- **R14 (RT-14, §7).** App ticket and publish requests name the Postgres database; it is checked like rest's `pick_database` (`services/rest/src/exec.rs:175`), including `DataIdentity.databases`.
- **R15 (RT-15, §4, §6.1, §8.1).**
  - Check queries per (branch, db) run under a concurrency bound (default 8); overflow → keyless `changed` for the waiting groups.
  - Studio's keyless refetches have a minimum interval of 2 s with backoff to 10 s while they keep arriving.
  - Re-authorizations triggered by an access event are coalesced per member (one per member per event burst, 1 s).
  - Joins are rate-limited per socket (5/s) and per data key (500/s); tickets per client IP (10/s for app keys, mirroring the gateway's limiter).
  - A presence `state.row` is checked as the **setter** first (cache or one query); a row the setter can't see is refused (`invalid`).
- **R16 (RT-16, §3.3, §4).** Changes to a partition are mapped to the partition tree's root (`pg_partition_root`) before matching channels; checks query the joined relation (the root), so its RLS applies.
- **R17 (RT-17, §3.3).**
  - The lease is renewed by compare-and-set on its KV revision; a holder whose renewal fails stops decoding and drops its replication connection before the TTL ends (fencing).
  - Watching instances watch the lease key; a revision change or deletion → `resync` to their sockets and a takeover attempt.
  - The per-compute cap is enforced from the compute itself: before creating a slot, the holder counts `rt_%` slots in `pg_replication_slots` under a cluster-wide advisory lock; cap = min(`CHELABASE_REALTIME_MAX_SLOTS_PER_COMPUTE`, live `max_replication_slots` / 2), so computes still on 10 slots give realtime 5.
  - If publishing to NATS fails for 30 s, the holder drops the slot (its connection), and watchers get `status {live: false, reason: "unavailable"}`; WAL is never retained without bound.

### Minor

- **R18 (RT-18, §6.3).** "Comparable" is decided by Postgres: the column's type (a domain's base type) has a default btree or hash operator class, and it is not `json`, a composite, or a multi-dimensional array. The original values used in the `WHERE` are the row's values **re-fetched as text** with the page (`<col>::text`), compared as `<col>::text = $orig` for numeric, float, bigint, jsonb and timestamp types, so JSON number parsing never causes a spurious conflict. Non-comparable columns are left out, as before. Nondeterministic collations: compare with `COLLATE "C"`.
- **R19 (RT-19, §4; U: only for keys seen visible).** A new client frame `view {channel, keys}` reports the keys on the socket's screen (JSON key form, at most 1000; replaced on each send). A delete reaches an RLS group's socket **keyed, and only** when the key is in that socket's view set or was delivered to it as visible earlier in the session; other deletes send nothing to RLS groups. Bypass groups get keyed deletes as before. (Corrects factual error 6: the owner, `postgres`, is a spec role with `BYPASSRLS`, so owner viewers are always bypass viewers; `/internal/realtime/visible` serves only the owner's privilege probe.)
- **R20 (RT-20, §4 degraded).** A channel in the degraded state receives no presence (no members, no diffs) until re-authorization succeeds.
- **R21 (RT-21, §5.3).** An event for a database not in the person's cached list triggers a refresh of that list, never delivery by default.
- **R22 (RT-22, §2, §3).** A read-only compute (static or replica) reports `live: false, reason: "read_only"` and never tries a slot. Realtime's own logs omit the socket URL's query string and the contents of `auth` frames.

### Factual corrections

1. The replication login cannot come from the compute spec (R1).
2. The reused internal routes are bound to other callers' tokens today (R10).
3. `CondOp::Is` is at `crates/chelabase-query/src/query.rs:266`.
4. The gateway access log already logs the path only (`services/gateway/src/proxy.rs:282`); the exposure to close is realtime's own logs (R22).
5. Studio's check identity is (role, `{}` claims): the console runs named roles with empty claims on the secret-key path (`services/rest/src/exec.rs:159-171`), and the owner has none.
6. Owner viewers are always bypass viewers (R19); `authenticator` can log in only where the data API is on (`services/worker/src/data_api.rs:394`), which R3 requires anyway.

## S-rulings (made while writing; not asked)

- **S1** Ports `8087` (public) and `8088` (internal), the next free after rest's `8085`/`8086`. Cost if wrong: an env default.
- **S2** A renewed Studio token must carry the ticket's `sub`; otherwise close 4003. Cost if wrong: none (a stricter check).
- **S3** `/internal/realtime/projects` also returns per-project database ids, so database-scoped events need no extra call. Cost if wrong: one more internal route.
- **S4** A `status {channel, live}` frame tells a joined table channel when live updates start or pause (for the slot retry). Cost if wrong: one frame type.
- **S5** Watching instances heartbeat a watched database to the lease holder every 5 s; the holder stops 30 s after the last heartbeat. Cost if wrong: slot held up to 30 s longer than needed.
- **S6** The privilege/RLS probe reruns on re-authorization and `resync`, so `ALTER TABLE … ENABLE/FORCE ROW LEVEL SECURITY` or a revoked SELECT is picked up within 60 s even without an access event. Cost if wrong: one probe per channel per minute.
- **S7** `channel_settings.pattern` is checked with the channel grammar plus `*`. Cost if wrong: a looser check.
- **S8** The SDK's socket swap opens the new socket and re-joins before closing the old, so an ephemeral channel may deliver a message twice across the swap rather than lose it; `message.seq` is absent there, so the docs say ephemeral delivery is at most once per socket. Cost if wrong: a docs line.
