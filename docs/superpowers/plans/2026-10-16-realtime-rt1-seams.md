# RT1 seam pass (2026-10-16)

Checkout `.worktrees/table-realtime` @ `0baf7aa3`. Plan `docs/superpowers/plans/2026-10-16-realtime-rt1.md`, read against the code, the spec (R1–R22), the red-team (W1–W35) and the pre-flight scan. 33 findings: S1–S33. **(D)** = needs a user decision (one: S27); everything else is a mechanical ruling with exact text to pin. Severity: C = would break a later task or fail open, I = rework or a wrong behaviour, M = minor.

---

## A. NATS, Valkey, secrets (T1 → T5, T6, T7, T8)

### S1 (C) T1 → T5/T6/T7/T8: secret and credential env names aren't pinned, and `stack.env` is shared by every app
- Evidence: plan "Shared interfaces" pins only `CHELABASE_NATS_{URL,USER,PASSWORD,PASSWORD_FILE,INBOX_PREFIX}`. T1 generates the `chelabase_realtime` password, the ticket key, the realtime internal token and the Valkey passwords; T5, T6, T7 and T8 read them, but no name is pinned. Every workbench service loads the same `stack.env` (`infra/compose/docker-compose.yml:40-42`, `tools/cli/src/workbench/env.rs:149-174`), so a per-service user or password can't live there. Key files are mounted one per reading service (U10, `docker-compose.yml:56-80`), and `scripts/tests/compose-key-visibility.sh` checks the exact set.
- Failure: T5 and T8 each invent a name for the same password file. Or a value ends up in `stack.env` and every app container can read the orchestrator's NATS password.
- Ruling (pin under "Shared interfaces", T1 writes, consumers read):

  | Env | Value | Set for (compose `environment:`, not stack.env) | File mount (U10) |
  |---|---|---|---|
  | `CHELABASE_NATS_USER` | `orchestrator` / `worker` / `realtime` / `runner` | each service + runner | — |
  | `CHELABASE_NATS_PASSWORD_FILE` | `<keys>/nats/<user>.password` | same | own file only |
  | `CHELABASE_NATS_INBOX_PREFIX` | `_INBOX_<user>` | same | — |
  | `CHELABASE_VALKEY_USER` / `CHELABASE_VALKEY_PASSWORD_FILE` | `control` (orchestrator and worker share one ACL user) / `realtime` / `runner`; `<keys>/valkey/<user>.password` | same; `CHELABASE_VALKEY_URL` stays without credentials | own file only |
  | `CHELABASE_REALTIME_PG_PASSWORD_FILE` | `<keys>/realtime/pg.password` | worker, realtime, runner | yes |
  | `CHELABASE_REALTIME_TICKET_KEY_FILE` | `<keys>/realtime/ticket.key` (32 random bytes, raw) | realtime, runner | yes |
  | `CHELABASE_REALTIME_INTERNAL_TOKEN_FILE` | `<keys>/realtime/internal.token` | orchestrator, realtime, runner | yes |

  - `NatsConfig::from_env`: if `_FILE` and the plain `CHELABASE_NATS_PASSWORD` are both set, that's an error. A user without a password is an error. Neither set means anonymous (unit tests only).
  - The NATS server config holds bcrypt hashes, and Valkey's `users.acl` holds `#<sha256>` hashes. The workbench renders both into the stack dir (0600) from the secret files; plaintext never goes in them.
  - `compose-key-visibility.sh` lists each new mount per service.
  - T7 adds realtime's mounts; T1 adds every other consumer's, including the worker's `pg.password` and the runner's three realtime files.

### S2 (C) T1 → T7/T8: realtime's NATS permissions (P3/W13) miss what async-nats actually sends
- Evidence:
  - KV `watch`/`keys` create ordered push consumers with `flow_control: true` (`async-nats-0.50.0/src/jetstream/kv/mod.rs:739-783`, `consumer/push.rs:484`). The client has to publish to the flow-control reply `$JS.FC.<stream>.>`.
  - `create_key_value` first reads `$JS.API.INFO` (`context.rs:2550`).
  - `create_or_update_key_value` needs `STREAM.UPDATE`.
  - Presence sync is request/reply between realtime instances (spec §5.2), so realtime is a responder and needs `allow_responses`.
  - The T1 text says realtime may publish all of `chelabase.rt.>`, which includes forging `rt.access.*`, `rt.project.*` and `rt.compute.stopping.*`.
- Failure: KV watches stall after the first flow-control message, so W12 takeover never fires. Presence snapshots get no answers. Realtime can forge compute-stopping, which drops every slot.
- Ruling: replace P3's realtime line with:
  - **realtime:**
    - pub `chelabase.rt.rows.>`, `chelabase.rt.watch.>`, `chelabase.rt.presence.>`, `$KV.CHELABASE_RT_LEASES.>`, `$KV.CHELABASE_RT_SLOTS.>`, `$JS.API.INFO`, `$JS.API.STREAM.{INFO,CREATE,UPDATE}.KV_CHELABASE_RT_{LEASES,SLOTS}`, `$JS.API.CONSUMER.{CREATE,DELETE}.KV_CHELABASE_RT_{LEASES,SLOTS}.>`, `$JS.API.DIRECT.GET.KV_CHELABASE_RT_{LEASES,SLOTS}.>`, `$JS.FC.KV_CHELABASE_RT_{LEASES,SLOTS}.>`;
    - sub `chelabase.rt.>`, `_INBOX_realtime.>`;
    - `allow_responses: true`.
  - **orchestrator:** pub `chelabase.rt.project.>`, `chelabase.rt.access.>`.
  - **worker:** pub `chelabase.rt.project.>`, `chelabase.rt.compute.stopping.>`.
  - **runner:** the union of every service's permissions (tests run worker, orchestrator and realtime in-process, S24).
  - No user gets `$JS.API.>` or `_INBOX.>`.

### S3 (I) T1 RED tests: a refused core publish returns `Ok` on the client
- Evidence: NATS reports a permissions violation asynchronously (`-ERR`). `client.publish` succeeds.
- Failure: "realtime can't publish `chelabase.provision`" passes even with no permissions at all, a false GREEN.
- Ruling (T1 test brief): a refused publish is proven by (1) an `Event::ServerError` permission violation from `ConnectOptions::event_callback` within 1 s, and (2) no delivery to a runner-user subscriber within 500 ms. A refused subscribe is proven by the violation event. Each test also checks its positive control (the allowed publish *is* delivered).

### S4 (C) T1/T8: KV bucket options, and CAS renewals can't carry a per-key TTL
- Evidence: `Store::update` (the CAS) sends no TTL header (`kv/mod.rs:937-982`; `update_maybe_ttl` is private), and only `create_with_ttl` takes one (`:300`). `limit_markers` turns on `allow_msg_ttl` and subject delete markers (`context.rs:2547-2556`). No task says who creates the buckets or with which options.
- Failure: a lease created with a TTL and renewed by `update` never expires. A crashed holder then blocks takeover forever: W12 and R17 fail.
- Ruling (pin, T8 creates, T1 grants):
  - Realtime creates both buckets at startup with `create_or_update_key_value(kv::Config { bucket, history: 1, max_age: 10 s, storage: Memory, limit_markers: Some(5 s), .. })`.
  - The lease TTL is the bucket's `max_age`: every revision expires 10 s after it's written, and a renewal is a new revision.
  - Create uses `create` (create-if-absent), renew uses `update(rev)`, release uses `delete_expect_revision(rev)`. No per-key TTL anywhere.

### S5 (C) P8: "count by key listing before creating" is a race
- Evidence: P8 says one key per slot, counted with `keys()` and then created. Two holders on one compute both count 9 and both create, so the cap is exceeded. The `pg_replication_slots` cross-check has the same window, and `max_replication_slots` only guards the compute's total of 20, not the customer's 10.
- Ruling (replace P8's semaphore sentence): "The per-compute slot cap is a set of index keys `<branch_id>.<n>` for `n` in `0..cap` in `CHELABASE_RT_SLOTS`. A holder takes the lowest `n` whose `create` succeeds (atomic create-if-absent), renews it with `update` alongside its lease, and deletes it at its own revision on release. No free index means `no_slot`. `cap` = min(`CHELABASE_REALTIME_MAX_SLOTS_PER_COMPUTE`, live `max_replication_slots`/2), read on the check pool. The `rt_%` count in `pg_replication_slots` is a second check after taking an index: over the cap, give it back and report `no_slot`."

### S6 (C) T1 → proxy/T7: Valkey ACLs miss commands that are actually issued
- Evidence:
  - The Neon proxy also keeps cancellation keys in Valkey: `SET … EX`, `EXPIRE`, `HGET`, `GET` on `cancel:<hex>` (`chelabase-neon/proxy/src/cancellation.rs:136-154`, `redis/keys.rs:18`). It subscribes to `neondb-proxy-ws-updates` (`redis/notifications.rs:15,22`) and sends `PING` (`connection_with_credentials_provider.rs:99`). P5 limits it to "today's PUBLISH/SUBSCRIBE channels".
  - P16 (`rt:cnt:*` counters) needs more than P5's realtime list of `GETDEL SET`.
  - The proxy takes credentials only on its command line: `--redis-plain` has no env var or file form (`proxy/src/binary/proxy.rs:176-178`), and RT1 doesn't change the fork.
- Failure: query cancellation through the proxy breaks silently. Realtime's limit counters fail, either closed (every socket refused) or open (limits off).
- Ruling (pin the ACL file, `infra/compose/valkey/users.acl` template):
  - `user default off`.
  - `user proxy on #<h> resetchannels &neondb-proxy-ws-updates ~cancel:* -@all +subscribe +ping +set +get +hget +expire +client|setinfo`.
  - `user control on #<h> resetchannels &neondb-proxy-ws-updates -@all +publish +ping +client|setinfo`.
  - `user realtime on #<h> resetchannels ~rt:* -@all +set +getdel +incr +decr +expire +zadd +zrem +zremrangebyscore +zcard +ping +client|setinfo`.
  - `user runner` = realtime's ∪ control's rules.
  - P16's socket counter is a sorted set `rt:cnt:sock:<sub>` (member = socket id, score = expiry). It's refreshed every 30 s, entries older than 90 s are trimmed on read, and an instance crash heals in 90 s. A plain counter can't heal after a crash.
  - The proxy's password goes in by an `entrypoint: ["sh","-c","exec proxy … --redis-plain=redis://proxy:$(cat /run/valkey-proxy.password)@valkey:6379"]` wrapper with a narrow mount, never through `stack.env`.

### S7 (I) No task owns P5's Valkey TLS feature, and Cargo.toml would collide in wave 1
- Evidence: P5 says "feature compiled in". `redis` has only `tokio-comp` (`Cargo.toml:100`). T1's file list has no `Cargo.toml`, and T2 bumps `Cargo.toml`/`Cargo.lock` in parallel.
- Ruling: "T7 adds `redis` features `tokio-rustls-comp` + the aws-lc-rs provider (latest feature names checked on crates.io) in its workspace-dependency commit, with `CHELABASE_VALKEY_TLS=off` as the stack default. T1 changes no `Cargo.toml`."

## B. Replication client (T2 → T8)

### S8 (C) T2 → T8: the pinned `LogicalReplicationStream` parses pgoutput
- Evidence: in the Neon client T2 ports, `LogicalReplicationStream::poll_next` runs `LogicalReplicationMessage::parse` (pgoutput) on every XLogData (`~/.cargo/git/checkouts/rust-postgres-2f27f41dd67515b9/f3cf448/tokio-postgres/src/replication.rs:180-199`). `chelabase_keys` emits JSON, so every message fails with `Error::parse`. Also `standby_status_update` takes `timestamp: SystemTime`, not `ts` (`:144-151`).
- Ruling (replace the rust-postgres pin):
  - "`tokio_postgres::replication::ReplicationStream::new(CopyBothDuplex<Bytes>)`, a `Stream<Item = Result<ReplicationMessage<Bytes>, Error>>` with `ReplicationMessage::{XLogData(XLogDataBody<Bytes>), PrimaryKeepAlive(PrimaryKeepAliveBody)}` (`wal_start()`, `wal_end()`, `timestamp()`, `data()`, `reply()`), and `standby_status_update(write: PgLsn, flush: PgLsn, apply: PgLsn, ts: SystemTime, reply: u8)`."
  - T2 ports `ReplicationStream` and may leave out the pgoutput `LogicalReplicationStream`. T8 parses `data()` as `chelabase_keys` JSON.
  - T2 adds a test that a non-pgoutput payload (a JSON body from `test_decoding`) arrives byte-identical.

## C. chelabase-query (T4 → T6, T8, T9)

### S9 (C) T4 → T9/T8: the pinned JSON forms and enum names don't match the grammar
- Evidence:
  - Filters are `{"col": "x", "eq": v}`, with the operator as the key and exactly one operator key (`crates/chelabase-query/src/query.rs:310-355`); Studio sends that form (`apps/studio/src/lib/console.ts:115`).
  - `Select` is a struct, and its items are `SelectItem::{Col, Aliased}` (`query.rs:93-128`).
  - `parse(body)` has no mode and is called by rest's data route and console route (`services/rest/src/exec.rs:407`, `:430`), the orchestrator console (`services/orchestrator/src/console.rs:422`), the bench tool and tests.
  - The shape cache keys plans by shape hash (`engine.rs:16-17`).
- Failure: T9 sends `{"op":"eq","value":…}`, which is refused as an unknown field. Or T4 builds a second grammar. If `text` isn't in the shape hash, the cache serves a typed plan for a text filter (wrong rows, wrong conflict results).
- Ruling (replace the chelabase-query pin):
  - Select item `{"text_of": "<col>", "as": "<alias>"}` (`as` required) → `SelectItem::TextOf { col, alias }`, emitting `<col>::text AS <alias>`.
  - Filter `{"col": "<c>", "eq": "<text>", "text": true}` or `{"col": "<c>", "in": ["<text>", …], "text": true}` → `Filter::Cond { col, op, value, text: true }`, emitting `<col>::text COLLATE "C" = $v` or `= ANY($v::text[])`. `text` is allowed only with `eq`/`in`, and its values must be strings.
  - `pub enum ParseMode { DataApi, Console }`, `pub fn parse_with(body: &[u8], mode: ParseMode)`. `parse(body)` stays `DataApi`. `ParseMode::Console` is used by `console.rs:422` and rest's internal console route (`exec.rs:430`) only.
  - `text_of` and `text: true` are part of `shape_hash`. Under `DataApi` they give the same `invalid_query` error as any unknown key.
  - T4's files must include `services/orchestrator/src/console.rs`.
  - T9 picks its aliases as `$pk0…` / `$o0…` (renaming on a clash with a real column).
  - When it adds `text_of` items, T9 lists every column explicitly, since `select: null` means all columns and can't be extended.

### S10 (I) T4 → T6/T8: `console_session` signature doesn't fit the engine
- Evidence: the engine runs transactions with `with_transaction(client: &impl QueryClient, reset_sql, request_id, f: AsyncFnOnce(&Client))` (`engine.rs:377-381`), not with `tokio_postgres::Transaction`. Roles are `RunAs::{Role{role, claims, oid}, Owner{user}}` (`:98-112`). Errors are `QueryFailure`, while `QueryError` is the parser's error (`lib.rs:31-33`).
- Ruling (replace the pin): `pub async fn console_session<C: QueryClient + ?Sized, T>(client: &C, run_as: RunAs<'_>, settings: &RunSettings, request_id: &str, f: impl AsyncFnOnce(&Client, &str /* schema version */) -> Result<T, QueryFailure>) -> Result<T, QueryFailure>`.
  - It sends exactly `begin_sql(settings.reset_sql)` + `PREAMBLE_SQL`/`OWNER_PREAMBLE_SQL` as the sequential path does, runs `f`, and commits.
  - Realtime calls it with `RunAs::Role { role, claims: &json!({}), oid: None }` and `statement_timeout_ms: 2000`.
  - T6's owner path calls it with `RunAs::Owner`.

### S11 (I) T4/T6/T8 → T9: the key string has to be byte-identical to `JSON.stringify`
- Evidence: P6 says the key is "`JSON.stringify([pk1::text, …])`". Postgres `json_build_array(...)::text` and `jsonb::text` put a space after commas (`["1", "x"]`); JS does not (`["1","x"]`).
- Failure: every server key misses Studio's page keys, so there are no splices, R19 view sets never match, and RLS viewers never get keyed deletes.
- Ruling: "The key string is built in Rust as `serde_json::to_string(&Vec<String>)` over the PK texts the check, conversion or `/visible` query returned. It's never built by a Postgres JSON function. T4 commits a shared fixture, `crates/chelabase-query/tests/fixtures/text_keys.json` (`[{texts: [...], key: "..."}]`, covering quotes, backslashes, control characters, non-ASCII and composite keys). T8's and T9's tests both assert against it."

## D. Orchestrator internal routes (T6 → T7, T8)

### S12 (C) T6 → T8: `/internal/realtime/visible` can't do what T8 needs, and its SQL builder lives in the wrong wave
- Evidence:
  - The pinned request has no `database_id`, and `access::authorize`'s `Target::Role` needs it (`access.rs:49-54`). It has no `xid`, but P7 requires visibility in the same statement for every group, bypass and owner included: otherwise Studio's refetch of a not-yet-visible row returns nothing and the row fades.
  - The key-type allowlist (P6) and the `unnest`/`format_type` builder (W27) are assigned to T8 (wave 5), but T6 (wave 3) has to build the same SQL "by OID with bound keys".
  - T6 doesn't list T4 as a dependency, yet it uses `console_session`.
- Ruling:
  - "`POST /internal/realtime/visible {token, database_id, branch_id, pg_database, rel_oid, xid: string|null (xid8 decimal), keys: [[string]]|null}` → `{probe: {ok, bypass}, visible: bool, keys: [string]}`. `visible: false` means the xid isn't in the snapshot yet; `keys` is empty then. One attempt per call; **realtime** polls (50 ms, ≤2 s, P7)."
  - "The check/convert SQL builder, the P6 type allowlist and the visibility clause are a new module `crates/chelabase-query/src/realtime.rs`, written by **T6** (opus) and reused by T8."
  - T6's dependencies: T1, T4.

### S13 (I) T6 → T7: authorize reasons for the console gates aren't mapped, and `tenant_id` is optional in one place and required in the other
- Evidence: the console gates answer `409 branch_not_ready` (`console.rs:454`), `409 backup`, `data_api_disabled`, `404 database_not_found`, `422` owner-as-role (`:457`), and from `check_role`: `403 role_refused`, `409 not_ready`, `504`, `503` (`:897-916`). P13 has `tenant_id` required, the pin has `tenant_id?`.
- Failure: a branch reset or switch (status not Ready for seconds to minutes) comes back from the 60 s re-authorization as `forbidden` and kicks every viewer, contradicting W19's intent.
- Ruling (pin under the T6 routes):
  - `branch_not_ready`, check_role `not_ready`, 503, 504, metadata errors → `unavailable`.
  - Backup branch, data API off, `role_refused`, owner-level, the owner's own name as `role`, and the access refusals → `forbidden`.
  - Unknown database/branch/table target → `not_found`.
  - `role: ""` → 400.
  - `tenant_id` and `member {sub, name, email}` are present iff `allowed`.
  - Realtime maps HTTP 400/401/5xx and timeouts to `unavailable` and logs 400/401 as configuration errors.

### S14 (I) T6 → T7: timeouts and expiry don't line up
- Evidence:
  - `check_role` waits up to 15 s (`RESET_TIMEOUT`, `services/orchestrator/src/lib.rs:1296`, `:1417-1419`).
  - `JwtVerifier`'s JWKS fetch uses `reqwest::Client::new()` with no timeout (`crates/chelabase-auth/src/lib.rs:111-118`, `:181-188`).
  - Verifier leeway is 60 s (`:142`).
  - Expiry shows up only as `AuthError::InvalidToken(String)` (`:84-93`).
  - `VerifiedClaims` has no `exp` (`:71-78`).
- Ruling:
  - "T6 adds `AuthError::Expired` (from `ErrorKind::ExpiredSignature`), `VerifiedClaims.exp: u64` (unix s), and a 3 s timeout on the JWKS client.
  - Realtime's calls to the orchestrator time out after 20 s (joins and re-authorization) and 5 s (`/visible`, `/projects` excepted at 20 s).
  - Realtime closes 4001 at `exp` exactly, with no leeway. An `auth` frame whose `exp ≤ now` closes 4001, and its `sub`/`org_id` come from the same `build_claims`."

### S15 (I) T6 → T7/T8: the two orchestrator listeners need two base URLs
- Evidence: the reused `/internal/data/branches/{id}/connection` is on the internal router (8083, `internal/mod.rs:444-450`). The new `/internal/realtime/*` routes go where `AppState` exists (the public listener 8081, pre-flight §4). `SERVICE_KEYS` defines `CHELABASE_ORCHESTRATOR_URL` and `CHELABASE_INTERNAL_URL` (`env.rs:158-160`).
- Ruling: "Realtime calls `/internal/realtime/*` at `CHELABASE_ORCHESTRATOR_URL` and `/internal/data/*` at `CHELABASE_INTERNAL_URL`, both with `CHELABASE_REALTIME_INTERNAL_TOKEN_FILE`'s token. T6 adds a test that the gateway answers 404 for `/internal/realtime/authorize` (it does for unmatched routes, `services/gateway/src/pipeline.rs:164-165`), and one that the 8083 router doesn't serve `/internal/realtime/*`."

## E. Realtime core ↔ change source (T7 ↔ T8)

### S16 (C) T7 ↔ T8: the `ChangeSource` pin has no path back to sockets and contradicts its own name
- Evidence: the plan says "a `ChangeSink` trait T8 implements", then shows `ChangeSource` with only `watch` and `view`. T7 needs, through it:
  - a delivery path for `rows`/`status`/`resync`;
  - R15's setter visibility check for presence ("through the ChangeSource's visibility call");
  - S6's reprobe on re-authorization and resync;
  - leave;
  - `db_oid`, for R4's scope (`branch_id ‖ db_oid`).
- Failure: T8 has to redesign T7's registry, presence and re-authorization, or T7 ships a fake that T8 can't replace.
- Ruling (replace the trait block):
  ```rust
  pub struct CheckIdentity { pub role: Option<String> /* None = owner */, pub token: Arc<str> /* for the owner path */ }
  pub struct JoinInfo { pub pk: Option<Vec<String>>, pub live: bool, pub reason: Option<StatusReason>, pub db_oid: u32, pub rel_oid: u32 }
  pub enum JoinRefusal { Forbidden, NotFound, Unavailable, Invalid(String) }   // → error codes
  pub enum ChannelEvent { Rows(Vec<Change>), Status { live: bool, reason: Option<StatusReason> }, Resync { reason: String } }
  #[async_trait] pub trait ChangeSource: Send + Sync {
      async fn watch(&self, ch: &TableChannel, id: &CheckIdentity, sink: mpsc::Sender<ChannelEvent>) -> Result<(WatchHandle, JoinInfo), JoinRefusal>;
      async fn view(&self, h: &WatchHandle, keys: Vec<String>);
      async fn visible(&self, h: &WatchHandle, keys: &[String]) -> Result<Vec<bool>, ()>;   // presence (R15), Err = treat as not visible
      async fn reprobe(&self, h: &WatchHandle) -> Result<JoinInfo, JoinRefusal>;
  }
  // Dropping WatchHandle unwatches. T7 assigns seq; the sink is per (socket, channel).
  ```
  Also: `StatusReason` and every frame type live in T7's `protocol.rs`, and T8 adds none. Subscribing to `ROWS`/`WATCH` belongs to T8.

### S17 (I) T7 → T9: socket protocol details T9 must match exactly
- Evidence:
  - Spec §2: `join {ref, channel, params}` and presence `state.row: [pk…]`. R6 and P6 say keys are strings.
  - P11 drops the role from the channel id, so two joins of one table with different roles on one socket (an Act-as switch racing its own leave) collide.
  - `project {channel, event}` combined with `#[serde(tag="event")]` nests `event.event`.
- Ruling (pin under "Socket protocol"):
  - Frames are tagged `"type"`, and `ref` is a string.
  - Table join: `{"type":"join","ref","channel":"table","params":{database_id,branch_id,pg_database,schema,table,role}}`. `joined.channel` is the canonical id `table:` + compact JSON `[database_id,branch_id,pg_database,schema,table]`, and the client uses that string for every later frame. A second join of a joined id on the same socket gets `error invalid "already joined"`.
  - Project join: `{"channel":"project:<uuid>"}`.
  - `presence.state.row` is a key string or `null`. `view.keys` and `rows.changes[].keys` are key strings (S11).
  - `seq` is on `rows` only, starting at 1 after `joined`.
  - `project` frame: `{"type":"project","channel","event":"branch.status",...fields}` (the `ProjectEvent` flattened).
  - Status reasons gain `unsupported` (a SQL_ASCII database, pre-flight §2.5).
  - Studio's socket URL is `VITE_API_URL` with `http`→`ws`, `/v1/realtime/socket?ticket=`, subprotocol `chelabase.realtime.v1`.

### S18 (I) T3 → T7/T9: route policies and defaults aren't pinned
- Evidence: `Policy` needs `auth`, `cors` and `access` (`crates/chelabase-routes/src/lib.rs:97-104`). `auth_and_access_agree` forbids `Public`/`DataKey` access on Management routes (`:1683-1697`). The gateway strips `authorization` (`pipeline.rs:29`), so realtime can't resolve membership at ticket time. Upstream defaults are per service (`services/gateway/src/config.rs:86-90`).
- Ruling:
  - `REALTIME_TICKET = {Post, "/v1/realtime/tickets", Management, Cors::Studio, Access::SignedIn}` (membership is checked at join).
  - `REALTIME_SOCKET = {Get, "/v1/realtime/socket", Public, Cors::None, Access::Public, websocket: true}`.
  - The gateway's `CHELABASE_REALTIME_URL` defaults to `http://127.0.0.1:8087`.
  - The ticket route has no OpenAPI or orval hook, so T9 calls it with a hand-written `fetch` carrying the access token.

## F. Worker (T5) and its neighbours

### S19 (I) T5: compute-stopping has one real choke point, and P9's file list misses callers
- Evidence: every worker compute stop is `ComputeBackend::delete(endpoint_id)` (`crates/chelabase-neon/src/backend.rs:110-115`; `branch_switch.rs:976-982` `stop_compute`; recreate paths `branch_switch.rs:352-367`). Callers outside T5's list: `branch_delete.rs`, `reconcile.rs` (recreate, stale rehearsal), `schema_base.rs:497` (static), `merge/apply.rs`, `data_api.rs` recreate.
- Ruling (replace P9's first sentence): "The worker wraps its `ComputeBackend` so that `delete(endpoint_id)` first publishes `chelabase.rt.compute.stopping.<branch_id>`, with the branch resolved by `WorkerMetadata::endpoint_branch`. Static and rehearsal computes have no branch row and publish nothing. One wrapper, one test per caller family."

### S20 (I) T5 → test harnesses: the publisher wrapper needs ids and must not make `build()` connect NATS
- Evidence:
  - `WorkerMetadata` status methods take `BranchId`/`DatabaseId` only (`services/worker/src/ports.rs:87-363`), but the subject needs `project_id`.
  - `rest_live`, `app_users_live` and the worker live tests build `RealWorker` with `worker::config::build` (`services/rest/tests/rest_live.rs:43`, `:70`).
- Ruling: "The wrapper resolves `(database_id, project_id)` with `get_branch`/`get_database`. It looks them up **before** the write for deletes, and caches them per branch. The publisher is `Arc<dyn RtPublisher>`, passed to `build` (no-op in unit tests; the runner's NATS client in live tests). `build` never connects NATS itself."

### S21 (I) T5 RED live test: `SHOW` as `chelabase_realtime` is forbidden by the fork
- Evidence: in a walsender, the fork refuses any SQL, and `SHOW` gets `42501` (pre-flight §2 "`chelabase_realtime` restriction"). psql can't stream CopyBoth, and `pg_recvlogical` can't make TEMPORARY slots.
- Failure: T5's test "shows `logical_decoding_work_mem = 8MB`" can't be written as planned.
- Ruling: "T5 asserts the role setting in `pg_db_role_setting` (as `cloud_admin`). The walsender effect moves to T8's live suite: stream a ~20 MB transaction through an `rt_` slot and assert `pg_stat_replication_slots.spill_txns > 0` for it (8 MB applies; the 64 MB default would not spill)."

### S22 (I) T6 → T5: the DDL hook triggers a member-roles reconcile, not a resync, on database create
- Evidence: on `makes_database` the hook sends `request_member_reconcile` (`services/orchestrator/src/internal/ddl.rs:778-783`). The resync it sends is for roles (`:772-774`). P15's "from the DDL-hook database create" names no worker path, and T5's files have no `member_roles/*`.
- Ruling: "T5 grants `CONNECT … TO chelabase_realtime` in the member-roles reconcile, in the same step that installs the protected-branch guard on a new database (so a born-closed database gets it before it opens), and at provisioning and data-API enable. T5's files add `services/worker/src/member_roles/{mod.rs,…}`."

### S23 (M) P15's reserved lists: who edits which file
- Evidence: prefix rules already cover `chelabase_` (`crates/chelabase-types/src/role_name.rs:24`, `crates/chelabase-schema/src/exclusions.rs:43`). The explicit lists that don't are `services/orchestrator/src/sql_editor/routes.rs:65-71` (orchestrator), `services/worker/src/data_api.rs:496` and `services/worker/src/roles.rs` (reserved names).
- Ruling: "T5 edits the worker lists. T6 edits `sql_editor/routes.rs` `RESERVED_ROLES`. One test per list."

## G. Live runs, waves and lifecycle

### S24 (C) T7/T8 live runs: a task worktree's realtime code isn't what runs on the stack
- Evidence:
  - Apps run from the stack checkout's `target/debug` (`docker-compose.yml:557-560`, CLAUDE.md "one stack per plan"). T8 works in `.worktrees/table-realtime-t8`, so the stack's realtime is T7's merge.
  - T7 wants "two realtime instances", but compose runs one.
  - T7's "use the `nats-server` binary if present" can't run here: neither `nats-server` nor `valkey-server` is installed. A test that silently skips is a false GREEN.
  - P2 rebuilds the apps only after T1 and T7, but the end-of-step pass needs T8's code deployed.
- Ruling:
  - "`realtime_live` starts realtime instances **in-process** (`realtime::app::build(Config)` on ephemeral ports) against the stack's NATS, Valkey, orchestrator and computes with the runner's credentials. Only the gateway-route test uses the compose `realtime` service."
  - "Every test that needs NATS or Valkey is `#[ignore]` and run through `workbench test`. There's no binary detection."
  - "P2 adds a third app rebuild (`workbench up` in the step worktree) after the final fix round, before the end-of-step live pass."
  - The T5/T6 live runs work as planned: worker live tests run the worker in-process (`services/worker/tests/common/mod.rs:1`, `:56`), and orchestrator `api.rs` runs in-process with the metadata DB.

### S25 (M) T5/T8: what happens after compute-stopping
- Evidence: P9 releases the lease. R17 watchers see the deletion and try to take over at once, which creates slots on a compute that is going down.
- Ruling: "After `compute.stopping`, takeover attempts wait until `/internal/data/branches/{id}/connection` answers 200 (backoff 1 s → 30 s). Until then the reason stays `compute_stopping`, then `resync` + `status {live:true}`."

### S26 (M) R22 `read_only`: the connection route doesn't say what kind of compute it is
- Evidence: the route returns host/port/server_name/user/password/settings/databases (`services/orchestrator/src/internal/data.rs:139-163`). Static computes exist (`crates/chelabase-neon/src/spec.rs:24-56`).
- Ruling: "Realtime detects `read_only` with `SELECT pg_is_in_recovery()` on the check pool when it first opens it. The route is unchanged."

### S27 (D) T1 (P4): computes reach every listener of the services that join the compute network
- Evidence:
  - Docker networks are per container, not per port. With orchestrator, rest, auth, worker, proxy and realtime on `compute`, a compute reaches orchestrator 8081 and 8083 (incl. `/internal/data/branches/{id}/connection`, which hands out authenticator passwords), rest 8085/8086, auth 8084, worker 8082 and proxy 4444.
  - Dev bearer tokens are constants (`tools/cli/src/dev/env_files.rs:29-31`: `chelabase-dev-rest-internal`, …).
  - Whether customer SQL can make HTTP requests depends on the compute image's extensions (W14: "check the extension list").
- Options:
  - **(a, Recommended)** Keep P4. In the stack, every internal bearer becomes a generated secret like the realtime token (rest, data-keys, app-auth, app-sessions, proxy). T1 adds a test listing every port a compute can open and asserting each one is authenticated or public-by-design (health, OpenAPI). Cost: a handful of secrets and mounts.
  - (b) Clients reach computes through a dedicated egress-only network: each service binds its listeners to its `default`-network IP only (static IPs per stack), so the compute network exposes no service listener but the hooks listener. Cost: static IPs and per-service listen addresses; more compose churn.
  - (c) Accept today's tokens in self-hosted mode and decide for platform mode later.

### S28 (M) T1 rt module: details that drift
- Ruling:
  - "`publish_project/publish_access/publish_compute_stopping` return `Result<(), NatsError>` (the crate's error type, `crates/chelabase-nats/src/lib.rs:666-676`). Callers log and continue."
  - "Within one 1 s window, realtime sends each distinct event once, in arrival order (W34)."
  - "A writer that doesn't know the member's `sub` (an invite, a whole-tenant change) publishes `member_sub: null`."

### S29 (M) T8 lease timing
- Ruling:
  - "Realtime's JetStream context timeout is 2 s, below the 3 s renewal period. A fenced holder stops renewing and lets the key expire (≤10 s); it never sends a blind delete."
  - "A new holder starts its 30 s idle timer when it takes the lease, so it doesn't wait for a first heartbeat."

### S30 (M) T8 window ordering
- Evidence: a window waits up to 2 s for visibility plus up to 2 s per check.
- Ruling: "Windows are processed in order per (channel, check identity). A window still unresolved 5 s after it closed is sent as keyless `changed`."

### S31 (M) File lists that miss sites
- Ruling:
  - T1 adds `crates/chelabase-neon/src/compute.rs` (the locator's network), `services/{orchestrator/src/internal/config.rs:77, worker/src/config.rs:112}` (`CHELABASE_COMPUTE_NETWORK` default `chelabase_default`) and `crates/chelabase-neon/tests/docker_backend_live.rs:177`.
  - W13's claim that `rest_live`, `api.rs`, `app_users_live` and `bench-data-api` connect to NATS is wrong. Only `crates/chelabase-nats/src/lib.rs:1304` and the in-process worker or orchestrator do, so T1's test-credential work is the runner's NATS and Valkey users plus `RealWorker`'s Valkey URL.
  - T5 adds `branch_delete.rs`, `member_roles/*` (S19, S22). T4 adds `console.rs` (S9).

### S32 (I) T4 → T9: Studio's key filters must switch to text
- Evidence: `whereByKey` sends typed values and refuses unsafe numbers (`apps/studio/src/lib/console.ts:108-117`).
- Ruling: "T9 builds the by-key `WHERE` (refetch by key, `saveCell`, the row panel) from the page's `text_of` PK values as `{col, eq: <text>, text: true}` (or `in` for a page of keys). With text keys, the unsafe-number refusal no longer applies. The existing exact-tree tests (`rows-view.test.tsx:74`, `row-panel.test.tsx:27`) move to that form."

### S33 (M) Text contradictions to fix in the plan (no behaviour change beyond the rulings above)
- "`ChangeSink` trait T8 implements" → `ChangeSource` (S16).
- P13 `tenant_id` required vs the pin's `tenant_id?` → S13.
- The spec §2 "Keys" and R6 `to_jsonb`/fixed-settings text is superseded by P6 and S11; say so in T8's and T9's behaviour lines.
- T7's "(test container via the stack is not available: use the `nats-server` binary if present…)" → S24.
- T5's "a `replication=database` session as the role shows `logical_decoding_work_mem`" → S21.

---

**Wave and agent-budget check:** the waves are feasible under the rules (at most 2 pipelines, disjoint files) once these are pinned: S7 (no Cargo.toml in T1); S12 (T6 depends on T4 and writes `chelabase-query/src/realtime.rs`; nothing else touches chelabase-query in wave 3); S23 (T6 owns the orchestrator's reserved list). Live runs don't conflict: T4 runs after T1's stack is up; T5 and T6 are in-process, on their own tenants; T7 runs after the re-up; T8 is in-process (S24) on its own branches.
