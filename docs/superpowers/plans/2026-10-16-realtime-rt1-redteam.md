# RT1 wider red-team pass (2026-10-16)

Scope: how RT1 (spec `docs/superpowers/specs/2026-10-14-realtime-design.md`, rulings R1-R22) interacts with the code it touches, read against `table-realtime` (main merged) and the fork clones (`chelabase-neon` with `vendor/postgres-v17`). Findings W1-W35: 3 Critical, 20 Important, 12 Minor. **D** = needs a user decision; everything else is mechanical with the ruling given.

## (a) Findings

### Critical

**W1. The conflict check is defeated by live splicing.**
- Evidence: `apps/studio/src/components/table-editor/rows-view.tsx:315-317`. `saveCell` finds the row by key in `rows` *at save time* and builds the `WHERE` from it. §6.1 splices refetched rows into the page, and §6.3/R18 take each edited column's "original value" from that row.
- Scenario: A opens a cell editor. B saves a new value. Realtime splices B's value into A's page. A saves, and the `WHERE` compares against B's value, so it matches and B's change is overwritten silently. That is exactly what §6.3 exists to prevent. The row panel has the same problem.
- Ruling: capture each edited column's original value (in R18's text form) when the editor or panel opens, and keep it with the open editor. Splicing never touches captured originals; the "changed since you started" marker compares the spliced value with the captured one.
- Test: vitest plus browser pass. B edits while A's editor is open, and A's save must hit the conflict dialog.

**W2. Bypass viewers get keys without schema `USAGE`.**
- Evidence: §4 step 2 and R2 probe `has_table_privilege`, the PK-column SELECT privileges and `row_security_active`. None of them checks `USAGE` on the table's schema.
- Scenario: role R has SELECT on `private.t` but no USAGE on `private`, and RLS is off. The console read as R fails (`permission denied for schema`), yet R is classified as a bypass group and receives every key (PK values, possibly emails or other data). This breaks the §4 invariant.
- Ruling: the probe *is* a real read as R, `SELECT <pk>::text FROM <rel> LIMIT 0` (plus `row_security_active`), run in the console's exact session. Any error means `forbidden` (at join) or keyless `changed` (per window).
- The model test must generate schema `USAGE`, table and column grants and `REVOKE`s.

**W3. R6's fixed output settings inside the check transaction break the invariant and the key form.**
- Evidence: R6 says "the check preamble sets the same [DateStyle, TimeZone, IntervalStyle, extra_float_digits, bytea_output] before parsing keys". The plugin also forces `lc_monetary=C` and `search_path=''` (`contrib/chelabase_keys/chelabase_keys.c:342-370`).
- Scenario 1 (policies): RLS predicates and plpgsql policy functions depend on these settings, for example `created_at::date = current_date` (TimeZone) or a plpgsql helper resolving an unqualified table through `search_path` at run time. A check run under UTC and `search_path=''` returns rows the viewer's own console read (under the database's or role's settings) would not, or the other way round. That is a leak.
- Scenario 2 (key form): `to_jsonb(timestamptz)` and `::text` under UTC differ from the console's output, so keys don't match Studio's page.
- Ruling: the check transaction runs exactly the console's session and preamble (the same `RunSettings`, reset SQL, `default_schema`/`search_path`) and never sets the plugin's settings.
  - Plugin key texts are parsed by plain casts. The ISO forms the plugin emits are input-setting independent: ISO date and timestamp, `+00` offsets, ISO 8601 intervals, hex `bytea`, shortest-exact floats.
  - PK types whose input depends on settings (`money`, anything else not on a pinned allowlist of type categories) make the relation `changed`.
- Model test: role- and database-level `ALTER … SET timezone/search_path/datestyle`, plus a TimeZone-dependent policy.

### Important

**W4. Studio's key form is lossy and can't be reproduced server-side.**
- Evidence:
  - `apps/studio/src/lib/table-editor/selection.ts:53-55`: `rowKey` is `JSON.stringify` of values from `JSON.parse` of the console's `json_agg` (`crates/chelabase-query/src/sql.rs:479`).
  - So a bigint above 2^53 is rounded, a numeric loses its scale (`1.50` becomes `1.5`), and floats are re-printed by JS.
  - `apps/studio/src/lib/console.ts:114` already refuses unsafe-number keys for writes.
- Scenario: R6's server-side `to_jsonb` keys never equal Studio's keys for those PKs. Updates are missed, the "N new rows" chip is wrong, and R19's view set never matches, so RLS viewers never get keyed deletes.
  - Owner viewers make it worse: their session carries the owner's role-level settings (TimeZone and others), and realtime has no owner connection to reproduce them.
- Ruling: one lossless key form, `JSON.stringify([pk1::text, …])`, produced under the console's session:
  - by the check query, for RLS groups;
  - by a conversion query on the authenticator pool with the same preamble, for bypass groups;
  - by `/internal/realtime/visible` on the orchestrator's owner pool, for owner viewers.
- Studio reads the PK columns as text alongside the page and refetches by text key. That needs W5.

**W5 (D). R18 and W4 need casts the op tree doesn't have.**
- Evidence: `crates/chelabase-query/src/query.rs:258-269`. `CondOp` has no casts, and no select expression takes `::text`. R18's `<col>::text = $orig` filters and "re-fetched as text with the page" can't be expressed. chelabase-query is the public data API's grammar too.
- Decision:
  - **(a) Console-only extension (Recommended):** a `text_of` select and filter form accepted only on the console route, refused by the data API parser.
  - (b) A public data API grammar feature (OpenAPI, docs, SDK).
  - (c) Drop the text comparison: compare typed values and accept spurious conflicts for floats and JSON numbers.

**W6. R7's `pg_xact_status` alternative is wrong.**
- Evidence: Postgres `RecordTransactionCommit` updates clog (`TransactionIdCommitTree`) *before* `SyncRepWaitForLSN`, while the proc stays in the ProcArray. So `pg_xact_status` says `committed` while every new snapshot still treats the xid as running. That window is exactly the walproposer sync wait (`crates/chelabase-neon/src/spec.rs:272`).
- Ruling:
  - Visibility is `pg_visible_in_snapshot($xid::xid8, pg_current_snapshot())`, evaluated in the same statement as the check query (READ COMMITTED takes a snapshot per statement) or in one REPEATABLE READ transaction.
  - Poll at most 2 s at about 50 ms intervals, releasing the R15 concurrency permit between polls; still not visible means keyless `changed`.
  - The plugin's `xid` is a full xid (`chelabase_keys.c:883-891`), so compare it as `xid8`.

**W7. Creating a slot can block indefinitely.**
- Evidence: `CREATE_REPLICATION_SLOT … LOGICAL` waits for a consistent snapshot, which means every transaction running at the start must finish, prepared ones included.
- Scenario: a customer's idle-in-transaction session or a dangling `PREPARE TRANSACTION` blocks the holder for hours. Meanwhile it occupies a walsender and a slot, and the half-built slot pins `catalog_xmin`.
- Ruling: a creation timeout (10 s). On timeout, close the connection (the temporary slot goes with it), report `live:false, reason:"waiting"`, and retry every 30 s with backoff.
- Live test: an open transaction on the database blocks creation, and realtime reports "waiting" and recovers after `COMMIT`.

**W8. WAL retention on a quiet watched database.**
- Evidence:
  - Slots are per database, but WAL is cluster-wide.
  - `spec.rs:261-278` sets `wal_keep_size=0` and no `max_slot_wal_keep_size`.
  - Neon's monitor drops logical slots whose `restart_lsn` needs too many `.snap` files, killing the owning walsender (`pgxn/neon/logical_replication_monitor.c:216-309`).
- Scenario: "acknowledge once that transaction's keys are published" (§3.3.3) means a watched database with no writes never acknowledges, while other databases write. The slot's `restart_lsn` stays put, so WAL piles up on the compute's disk and snapshot aux files accumulate on the pageserver. Eventually ls_monitor kills the slot, and the cycle becomes a `resync` loop.
- Ruling:
  - Acknowledge the keepalive's `wal_end` whenever nothing is in flight.
  - Add `max_slot_wal_keep_size` to `spec.rs` (for example 1 GB; a stuck slot is invalidated instead of filling the disk).
  - A lag watchdog: confirmed lag above X MB or 60 s means drop and recreate.
  - Slot invalidation, or termination by ls_monitor, counts as a gap (`resync`).

**W9 (D). Memory and spill on the compute.**
- Evidence:
  - The plugin has no stream callbacks; "large ones spill to disk" (`chelabase_keys.c:36-38`).
  - Each walsender keeps up to `logical_decoding_work_mem` (default 64 MB) of reorder buffer, and every slot decodes the *whole* WAL and spills independently into `pg_replslot/<slot>/`.
- Scenario: 10 `rt_` slots on one compute need about 640 MB of RAM, and a 5 GB bulk load spills about 5 GB × slots onto local disk, which can crash a small compute.
- Mechanical part: the worker sets `ALTER ROLE chelabase_realtime SET logical_decoding_work_mem = '8MB'`. Verify with a live test that a `replication=database` walsender applies role settings.
- Decision:
  - **(a) Next fork round (Recommended, with U16):** stream callbacks in `chelabase_keys` that, once Postgres starts streaming a transaction, keep no keys and emit `all` (or per-relation `c`) at stream commit. Memory stays bounded and nothing spills.
  - (b) Accept the spill and lower the per-compute slot cap.

**W10. An `all` message hides a `schema_version` change.**
- Evidence:
  - The plugin collapses a transaction to `{"all":true}` past `max_tables` or `max_message_bytes` (`chelabase_keys.c:514-518`, `:439-443`). That is typically a large migration.
  - The bump runs inside the DDL's own transaction (`services/worker/src/data_api.rs:340-342`, `:391-392`).
- Scenario: R2's pause never fires. Bypass groups keep stale classifications and receive the same transaction's keys.
- Ruling:
  - `all`, or any op on `chelabase.schema_version` (matched by the OID resolved per database, not by name), counts as a schema change: pause, rerun the probes, and re-resolve the channels' OIDs.
  - Inside one transaction or window, the `schema_version` change is handled *before* any key of that window is released.

**W11. The "cluster-wide advisory lock" for the slot cap doesn't exist.**
- Evidence:
  - `pg_advisory_lock` locks are per database (the lock tag includes the database OID).
  - Slot creation happens on a replication connection that may not run SQL (`vendor/postgres-v17/src/backend/replication/walsender.c:2275-2279`).
- Scenario: the holders of `db_a` and `db_b` on the same compute both count 9 `rt_` slots and both create one, so the cap is exceeded and customer slots run out.
- Ruling:
  - Serialize slot acquisition per compute (branch) with a NATS KV semaphore: CAS-incremented `slots.<branch_id>`, or one key per slot with a TTL.
  - Also count `rt_%` in `pg_replication_slots` (readable by `authenticator`) as a cross-check.
  - Postgres's `max_replication_slots` error is the backstop and maps to `no_slot`.

**W12. KV lease expiry is silent.**
- Evidence: NATS removes `max_age`- or TTL-expired messages without a KV watch event unless the stream has subject delete markers (NATS 2.11+). Compose runs 2.15 (`infra/compose/docker-compose.yml:189`), and async-nats has `server_2_11` enabled (`Cargo.toml:58`).
- Scenario: R17's "watch the lease key; a deletion means takeover" never fires when a holder crashes, so watchers sit without a decoder until their next join or heartbeat.
- Ruling:
  - Create the bucket with per-key TTL and limit markers (or poll the key every 3 s).
  - Fencing uses a local monotonic deadline: stop decoding at last successful renew + TTL − 3 s, whatever the in-flight renew does.
  - Release on SIGTERM with a CAS delete at the holder's own revision, never a blind delete.

**W13. R11's NATS permissions versus request/reply and JetStream.**
- Evidence:
  - Every `chelabase.rpc.*` call (`crates/chelabase-nats/src/lib.rs:60-537`) replies on `_INBOX.>`.
  - JetStream (`CHELABASE_CONTROL`, and the new KV) is driven through `$JS.API.>`.
  - Only orchestrator and worker connect today. Tests and tools connect too: `services/rest/tests/rest_live.rs`, `services/orchestrator/tests/api.rs`, `crates/chelabase-query/tests/app_users_live.rs`, `tools/bench-data-api`.
- Scenario:
  - A blanket `_INBOX.>` lets any service read every other service's RPC replies.
  - A blanket `$JS.API.>` lets realtime delete `CHELABASE_CONTROL`.
  - Tests break once anonymous access goes.
- Ruling:
  - Per-service custom inbox prefixes (`_INBOX_<svc>.>`) and `allow_responses` for responders.
  - Realtime's JetStream rights limited to its bucket: `$KV.CHELABASE_RT_LEASES.>`, `$JS.API.STREAM.{INFO,CREATE}.KV_CHELABASE_RT_LEASES`, `$JS.API.DIRECT.GET.KV_CHELABASE_RT_LEASES.>`, `$JS.API.CONSUMER.{CREATE,DELETE}.KV_CHELABASE_RT_LEASES.>`, `$JS.API.INFO`. Alternatively, a realtime account with its own JetStream and an import of `chelabase.rt.>`.
  - Credentials for the runner, the tests and the bench tool.
  - Pin the topology in the plan.

**W14. The compute network must exclude more than NATS and Valkey.**
- Evidence: computes run on the stack's `default` network (`crates/chelabase-neon/src/compute.rs:106-160`; the tests use `<stack>_default`). The same network carries:
  - `metadata-db` (dev password `chelabase`, compose `:173-179`), `zitadel-db`, `storcon-db` and mailpit;
  - the orchestrator public API, rest internal, the gateway, and unauthenticated Valkey and NATS (compose `:188-206`).
- Scenario: customer SQL reaches Postgres-protocol hosts through dblink or postgres_fdw, if the image ships them.
- Ruling: the compute network holds only:
  - the computes, pageserver, safekeepers, storage broker and the orchestrator's hooks listener;
  - the clients that dial computes (rest, realtime, proxy, orchestrator, worker, runner).
- Every listener those clients expose on that network authenticates: realtime 8087/8088 and rest internal. Check the compute image's extension list.

**W15. A Valkey ticket value can be copied.**
- Evidence:
  - The Neon proxy shares the unauthenticated Valkey (compose `:203-206`, `:533`).
  - The stored value is a bearer identity token valid for 60 s (`crates/chelabase-auth/src/internal.rs:20`).
- Scenario: anyone who can read and write Valkey copies a pending ticket's value under `rt:ticket:<sha256(own)>` and connects as that user.
  - RT1 Studio sockets are saved by R9's `sub`-matching `auth` frame.
  - RT2 app tickets are not.
- Ruling:
  - Seal the value with a realtime-only key, with AAD = the ticket hash.
  - Give realtime a Valkey ACL user limited to `rt:*`.
  - Platform mode: Valkey auth and TLS. The `redis` crate is built with `tokio-comp` only (`Cargo.toml:100`), so add a rustls/aws-lc TLS feature.

**W16. The gateway logs the ticket in its error logs.**
- Evidence:
  - pingora's `request_summary` prints the raw path with its query (`pingora-core/src/protocols/http/v1/server.rs:563-569`).
  - It is logged on every proxy failure (`pingora-proxy/src/lib.rs:1303`, `:1357`, `:1384`).
  - The gateway doesn't override it (`services/gateway/src/proxy.rs`).
- Scenario: realtime down means a 502, and `ticket=…` lands in the logs. Fact correction 4 missed this.
- Ruling: override `request_summary` to print method, path without query and host. Test it with a failing upstream.

**W17. API routes pass upgrades with the API timeout.**
- Evidence:
  - `services/gateway/src/proxy.rs:136-145`: `(None, Some(plan))` uses `self.read_timeout`.
  - The downstream read timeout for an upgrade is set only for sites (`:111-118`).
  - Nothing refuses `Upgrade` on other API routes.
- Ruling:
  - Endpoints get a `websocket` flag in `chelabase-routes`.
  - Every other API route answers an `Upgrade` with 400 before proxying.
  - The socket route sets both the session and peer read timeouts to 1 h.
  - Realtime checks the ticket (`GETDEL`) before answering 101, so no unauthenticated socket lives.

**W18. Platform events miss most status writers.**
- Evidence: branch status is also written at:
  - `services/worker/src/branch_job.rs:154`, `:493`, `:620`;
  - `services/worker/src/branch_switch.rs:586`, `:702`, `:910`, `:1064`, `:1178`;
  - `services/worker/src/merge/finish.rs:697`;
  - `services/orchestrator/src/branches.rs:742`.

  The spec names only `branch_create.rs` and `set_branch_status`.
- Scenario: with polling off while the socket is live (§5.3), any unpublished transition (resets, merges, switches), or a crash between commit and publish, leaves Studio stale until a reconnect.
  - `chelabase.rt.access.*` has the same gap: member and grant writes in the API, invites, and the Zitadel webhook (`services/orchestrator/src/internal/app.rs:104`).
- Ruling:
  - Publish from one choke point, the metadata port wrapper (status, create and delete for branches and databases; access-changing writes).
  - Keep a slow fallback poll (30 s) of the branch list and `/v1/me` while live.

**W19. Re-authorization failures are mapped too coarsely.**
- Evidence: re-auth sends the latest OIDC token to the orchestrator, which verifies it itself (R10). A token near `exp`, or a JWKS outage, fails verification.
- Scenario: if that is mapped to `forbidden`, the channel is left and Studio shows a false "no access".
- Ruling:
  - `/internal/realtime/authorize` returns `{allowed, reason: forbidden|not_found|token_expired|unavailable, tenant_id}`.
  - `token_expired` waits for the renewal (4001 at `exp`); `unavailable` takes the degraded path; only `forbidden` and `not_found` leave the channel.
  - `tenant_id` is needed for the `chelabase.rt.access.<tenant_id>` subscription.

**W20. Connection budget on computes.**
- Evidence: `spec.rs:267` sets `max_connections=100`.
- Scenario: realtime pools per (branch, db) per instance with a concurrency bound of 8. With rest's pools on top, 3 instances × 5 databases = 120 connections, so customer logins are refused.
- Ruling:
  - A per-compute cap per instance across databases (for example 4), with a short idle timeout.
  - The plan pins where checks run: each instance for its own groups (Recommended), or the holder.
  - The plan states the connection arithmetic.

**W21. `CONNECT` and born-closed databases.**
- Evidence: a `replication=database` login needs `CONNECT` and `ALLOW_CONNECTIONS`. The owner may `REVOKE CONNECT … FROM PUBLIC`, and protected branches create databases born closed (fork round 3, D5).
- Scenario: no slot, and the cause is not reported.
- Ruling:
  - The worker grants `CONNECT` to `chelabase_realtime` on every served database: at provisioning, at data API enable, and from the DDL hook on database create.
  - Realtime reports `live:false, reason:"no_connect"` and retries.

**W22 (D). U16 versus RT1's live pass.**
- Evidence:
  - The roadmap says U16 must land before RT1 runs realtime on real computes.
  - RT1's `realtime_live` (slot lifecycle, compute restart) runs on the plan's stack with real computes.
  - Aux files are inherited by child branches made while a slot is open.
- Decision:
  - **(a) Next fork round (U16 plus W9's stream callbacks) before RT1's live pass (Recommended).**
  - (b) Run the live pass on today's images. The stack is purged after the merge; U16 lands before any real deployment.

**W23 (D). Scale to zero.**
- Evidence:
  - compute_ctl counts any walsender other than walproposer as activity (`compute_tools/src/monitor.rs:268-279`).
  - Today `suspend_timeout_seconds = -1` (`spec.rs:295`), so it has no effect yet.
- Scenario: a held slot keeps a compute awake as long as any Studio tab has the table open, even idle for days.
- Decision (platform mode):
  - **(a, Recommended)** Studio leaves table channels when its tab has been hidden more than 5 min (and rejoins with a refetch).
  - (b) Realtime also drops slots on computes the platform wants to suspend.
  - (c) Keep the compute awake.

### Minor

**W24. Channel grammar.** `/`-joined identifiers are ambiguous, since Postgres identifiers may contain `/`. Ruling: structured join params `{database_id, branch_id, pg_database, schema, table}`; the channel id and the R4 hash use a canonical encoding.

**W25. Relation kinds.**
- R16 maps changes to the root only, but Studio can open a leaf or intermediate partition. Fan out to every `pg_partition_ancestors` entry, each channel checked against its own joined relation.
- A legacy inheritance parent reads its children's rows, so it gets `changed` (or `live:false`).
- Views, matviews and foreign tables join with `live:false, reason:"not_a_table"`.

**W26. An RLS update that hides a row.** The key isn't returned, so nothing is sent and the row stays stale. Ruling: a key in the socket's R19 view set that the check didn't return is sent as a keyed `delete` (the viewer saw the row before).

**W27. Check query shape.**
- 1000 keys × up to 32 PK columns makes up to 32 000 parameters, and there are per-type casts.
- Ruling: `unnest($1::text[], …)`, one parameter per PK column, cast to `format_type(oid)` schema-qualified. A missing type `USAGE` is an error, which means `changed`.

**W28. Re-authorization load.** Every channel every 60 s, through the orchestrator. Ruling: cache by (member, target, access) for the cycle, shared by the instance's sockets.

**W29. `SNAPSHOT 'export'`.** The restriction allows it (`walsender.c:2126-2131`), and it pins an xmin until the next command. Ruling: realtime always sends `SNAPSHOT 'nothing'`; the plan pins it.

**W30. Reserved role lists.** `chelabase_realtime` must not appear in:
- role lists, act-as pickers, 6g merge role diffs and member-role reconciles;
- `services/worker/src/roles.rs:1564`, `:2028` (reserved names) and `services/worker/src/data_api.rs:496`.

  `services/worker/src/resync.rs:419` already excludes `chelabase_` names. Test each list.

**W31. No source restriction in hba for `chelabase_realtime`.** Ruling: reuse fork round 3's spec-driven CIDR rule (platform mode).

**W32. Studio sign-out.** `revokeTokensOnSignout` (`apps/studio/src/lib/auth.ts:31`) doesn't invalidate a JWT access token. Ruling: Studio closes the socket on `userSignedOut`/`userUnloaded`.

**W33. Per-instance limits.** Sockets per member and similar limits are counted per instance. Ruling: a Valkey counter with a TTL, or document that the limit is N × the setting.

**W34. Event storms.** Database create/drop loops through the DDL hook trigger a Studio refetch storm. Ruling: coalesce events per project per 1 s.

**W35. Ticket minting rate.** Ruling: rate-limit tickets per member, since each one is a Valkey key for 30 s.

## (b) Per-area checklist for implementers

### Replication/decoder
- Slot creation:
  - `CREATE_REPLICATION_SLOT rt_<rand> TEMPORARY LOGICAL chelabase_keys (SNAPSHOT 'nothing')`; no TWO_PHASE or FAILOVER.
  - `application_name` is `chelabase_realtime`, never `walproposer`.
  - TLS verify-full against the platform CA, using `ConnInfo.server_name`.
- Creation blocked by an open transaction or a prepared transaction: times out at 10 s, reports "waiting", recovers (W7).
- Acknowledging:
  - keepalive `wal_end` when idle (W8);
  - in order, only after publish;
  - lag watchdog;
  - `max_slot_wal_keep_size` invalidation, and termination by ls_monitor, mean `resync`.
- Message contract:
  - `xid` as `xid8`; `lsn` is `end_lsn`;
  - one key under several ops (i+u, i+d in one transaction);
  - `all`; `c`; unknown fields ignored; `v != 1` means `changed` for everything.
- `schema_version`:
  - change, or `all`, means pause, reprobe and re-resolve OIDs before releasing the window's keys (W10);
  - DDL and DML in one transaction;
  - `schema_version` matched by OID per database.
- Partitions:
  - a leaf change reaches root, intermediate and leaf channels;
  - detach and attach under the cache;
  - a leaf whose replica identity differs from the root's.
- Restart and reset:
  - compute restart, branch reset, switch or merge into the branch, branch delete with a slot open;
  - a child branch made while a slot is open (aux files; U16).
- Limits:
  - the slot cap across two databases on one compute, races included (W11);
  - computes still on 10 slots give realtime 5;
  - `max_replication_slots` errors map to `no_slot`.
- Read-only compute: `read_only`, no slot.
- Database access:
  - a database without `CONNECT` for `chelabase_realtime`, born-closed databases (W21);
  - a SQL_ASCII database: the plugin refuses it at startup, so `live:false`.
- Memory: `logical_decoding_work_mem` role setting applied in the walsender (W9); bulk load of more than 1M rows.
- `wal_sender_timeout`: status updates sent while the pipeline is backed up.

### Tickets & auth
- Tickets:
  - single use (`GETDEL`) and 30 s expiry;
  - replay;
  - a copied Valkey value under another hash is refused (W15);
  - Valkey down means 503 and open sockets stay up;
  - `Cache-Control: no-store`.
- Ticket checked before 101; a bad ticket gets 401 and no socket (W17).
- First frame `auth` within 5 s, or close 4001:
  - `sub` and `org_id` must match;
  - a renewal with another `sub` or org closes 4003;
  - an app ticket sending `auth` is refused;
  - a Studio ticket joining `app:` is refused.
- Close 4001 at `exp`; a renewal just before `exp` and a late renewal frame; clock skew and leeway.
- `/internal/realtime/authorize`:
  - read-only member resolution (no auto-create, no first-admin claim, no tenant soft-bind);
  - unknown member;
  - removed member;
  - pending invite;
  - `in_operator_org`.
- Reply reasons mapped correctly (W19): expired token, JWKS down, orchestrator 5xx.
- Joins run the same `Access` as `ep::CONSOLE_QUERY`: data API On, a non-Backup Ready branch, `check_role` refusal, owner-level role (R3).
- Re-authorization:
  - within 1 s of an access event;
  - coalesced per member;
  - every 60 s; three failures leave with `unavailable`.
- Revocation paths:
  - grant removed, member removed, project access removed, database deleted, branch deleted;
  - Postgres-level `REVOKE SELECT` or `ALTER ROLE … BYPASSRLS` in SQL (per-window probe).
- Bearer tokens:
  - `/internal/data/branches/{id}/connection` accepts both rest's and realtime's tokens and refuses others (`require_any_bearer`);
  - the new routes live where `AppState` (access) exists, not on the compute-reachable hooks listener (or they authenticate there).

### NATS & leases
- Lease:
  - create-if-absent; CAS renewal;
  - renewal failure or timeout stops decoding by the local deadline (W12);
  - takeover after a crash within about 10 s (with delete markers);
  - SIGTERM release by CAS delete at the holder's own revision;
  - two instances racing the same lease;
  - lease lost mid-transaction (no duplicate keys published after fencing).
- Watch heartbeats: the holder stops 30 s after the last one; a watcher that restarts.
- Permissions (W13):
  - realtime can't publish `chelabase.provision`, `chelabase.delete` or `chelabase.rpc.*`, can't read other inboxes, can't touch `CHELABASE_CONTROL`;
  - orchestrator and worker can publish `chelabase.rt.access.*` and `chelabase.rt.project.*` (or the plan's names) only;
  - tests, the bench tool and the runner have credentials.
- NATS down: `resync` everywhere, "Live updates paused", publish failure drops the slot after 30 s (R17).
- Presence snapshots: request/reply with no answer, a slow responder, an instance that just started.

### Gateway
- The socket route upgrades with 1 h client and upstream idle timeouts.
- Every other API route refuses `Upgrade`; a site's upgrade behaviour is unchanged (W17).
- No `ticket=` in access, error or retry logs, including an upstream failure (W16); realtime's own logs omit the query string and `auth` frames (R22).
- `Public` route: an incoming identity header is stripped (`services/gateway/src/pipeline.rs:29`).
- Subprotocol `chelabase.realtime.v1`: required, echoed; an unknown version is refused at upgrade.
- Realtime restart: close 1012, and clients reconnect with jitter (no refetch thundering herd).
- A frame over 64 KB is refused; pong timeout closes 4008.

### Filtering & checks
- The check session is the console's exact session (no fixed settings, W3):
  - the same `RunSettings`, reset SQL and `search_path`;
  - claims `{}`;
  - `statement_timeout` 2 s.
- Probe is a real `LIMIT 0` read as R, covering schema `USAGE`, table and column privilege (W2), plus `row_security_active` per window.
- Key form is `[pk::text…]` under the console session (W4):
  - bigint above 2^53, numeric `1.50`, float `-0`/NaN/Infinity;
  - timestamptz under a non-UTC role or database TimeZone;
  - `bytea`; interval; enum; domain; array; composite; citext;
  - nondeterministic collation; text with quotes or backslashes; non-ASCII in a LATIN1 database.
- Key parsing:
  - a PK type whose input depends on settings (`money`) means `changed`;
  - a PK type in a schema R lacks `USAGE` on means `changed`;
  - a renamed enum label between decode and check.
- Visibility:
  - an xid not yet visible under walproposer sync waits up to 2 s, otherwise `changed` (W6);
  - the visibility check and the check query share one snapshot.
- Deletes:
  - bypass groups get keyed deletes;
  - RLS groups get keyed deletes only for keys in their view set or delivered earlier (R19);
  - an update that hides a row from an RLS viewer is sent as a keyed delete (W26).
- Owner viewers: privilege probe and key conversion on the orchestrator owner pool; realtime never holds owner credentials.
- Limits and failures:
  - concurrency bound 8, overflow means `changed`;
  - a check timing out or erroring means `changed`, never keys;
  - more than 1000 keys in a window means `changed`;
  - pool limits per compute (W20).
- Table identity:
  - a table dropped and recreated under the same name (new OID);
  - OID reuse;
  - a rename picked up on `resync`.
- Model or property test of the §4 invariant over generated grants, schema `USAGE`, RLS policies (including TimeZone- and `search_path`-dependent ones), role attributes and change sequences. Fake and live.

### Presence
- R4 scope check: a message from another branch, database or project with the same channel string is dropped.
- R15 setter check: a selected row the setter can't see is refused (`invalid`).
- Viewer visibility: per check identity, blanked state for viewers who can't see the row, a re-check when a `rows` batch touches a selected key.
- R12 cache: flushed every window, on any change in the database and on `schema_version`.
- Degraded channels (R20) get no presence.
- Instances: three missed heartbeats drop that instance's members; a closed socket leaves at once; members are per socket and channel (two tabs of one person).
- Rate: 20 frames/s per socket, latest wins; state over 1 KB is refused; more than 1000 members.
- Multi-instance live test (two realtime processes on the stack).

### Platform events
- Every branch status writer publishes, from one choke point (W18): reset, switch, merge finish, create, delete, restore; and the database and Postgres-database create/drop paths.
- Narrowing:
  - an event for a database not in the person's list refreshes the list (R21) and is never delivered by default;
  - narrowing by per-project database visibility.
- Access events from:
  - grants, members and invites;
  - the Zitadel webhook;
  - project delete;
  - the tenant admin flag.
- Publish after commit; a lost publish is covered by the slow fallback poll.
- Event storm coalescing (W34).

### Studio
- Conflict originals captured when the editor or row panel opens, never read from the live page (W1).
- Concurrent edit:
  - conflict dialog with Overwrite and Take theirs;
  - "deleted or hidden";
  - non-comparable column notice;
  - `COLLATE "C"`.
- Keys (W4):
  - text-form keys on the page and in refetch by key;
  - unsafe-number PKs still get live updates.
- Refetch rules:
  - `joined {live:true}` and `status {live:true}` trigger a refetch (R8);
  - a `seq` gap, `resync` or keyless change triggers a debounced refetch (500 ms, at least 2 s, backoff to 10 s);
  - a spliced row that leaves the filter fades.
- Rejoins and lifecycle:
  - an "Act as" switch leaves and rejoins;
  - a tab hidden more than N minutes leaves channels (W23, if chosen);
  - sign-out closes the socket (W32).
- Polling:
  - TanStack invalidation of exactly the orval keys (`getListBranchesQueryKey`, `getListBranchDatabasesQueryKey`, `/v1/me`);
  - the slow fallback poll stays while live;
  - polling resumes when the socket isn't live.
- Renewal: `auth` frame sent on oidc-client `userLoaded`; a failed renewal leads to 4001 and the polling fallback.
- Presence layer: never drawn for the viewer's own selection; reduced motion; at most 5 avatars then "+N".

### Lifecycle/infra
- Wiring:
  - compose service, stack.env addresses, workbench health, `just` recipes, moon project;
  - gateway `Upstreams` and `Service::Realtime`;
  - the internal-verify key mount;
  - the realtime-only secrets (`chelabase_realtime` password, internal token, Valkey seal key), never overwritten.
- `chelabase_realtime` creation:
  - created by the worker as `cloud_admin` on provisioning;
  - backfilled on existing branches;
  - inherited by child branches;
  - password rotation across branches;
  - never in the proxy's role secrets (`routable_role_secret`), role lists or merges (W30);
  - `CONNECT` grants (W21).
- `spec.rs`:
  - slots and senders 10 to 20;
  - `max_slot_wal_keep_size` (W8);
  - `logical_decoding_work_mem` for the role (W9).
- Compute network (R11/W14):
  - computes can't reach NATS, Valkey, metadata-db, zitadel-db, storcon-db or the app listeners;
  - rest, realtime, proxy, orchestrator, worker and the runner still reach computes;
  - hooks still work (spec, DDL, `notify-attach`).
- SIGTERM:
  - 1012 to every socket;
  - CAS lease release;
  - replication connections closed (temporary slots dropped at once);
  - within compose's stop grace period.
- Multi-instance: two realtime instances in one live test (lease handover, presence across instances, fan-out).
- U16 sequencing per the W22 decision.
