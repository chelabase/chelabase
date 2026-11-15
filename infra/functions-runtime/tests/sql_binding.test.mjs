// The SQL binding (pooled-endpoints m12): a loaded worker whose invocation carries
// `x-chelabase-sql-grant` gets `env.CHELABASE_DB`, whose connect("chelabase-db:5432") (workerd
// requires an address; any is ignored) always reaches PG_RELAY (here an echo stand-in) behind a
// `CHELABASE-SQL 1 <grant>` preamble, and `env.CHELABASE_DB_URL`.
// The grant never enters the user isolate; direct sockets to the relay or the proxy stay refused.
//
//   node --test infra/functions-runtime/tests/
import assert from "node:assert/strict";
import { randomBytes } from "node:crypto";
import { after, before, describe, test } from "node:test";
import { setTimeout as delay } from "node:timers/promises";
import { invoke, manifest, startBundleStore, startRelay, startRuntime } from "./harness.mjs";

const DB = "db_sqlbinding";
const URL_STRING = "postgresql://function@chelabase-db/postgres";
const b64url = (n) => randomBytes(n).toString("base64url");
// The shape m14 mints: v1.<kid>.<payload>.<mac>.
const newGrant = () => `v1.k1.${b64url(96)}.${b64url(32)}`;
// What the supervisor bounds its grant map at (MAX_VERIFIED).
const MAX_GRANTS = 4096;

// Shared by both runtimes below.
async function callWith(rt, store, bundle, query, { grant, fn = "br_main/fn_one", headers = {} } = {}) {
  const h = { ...headers };
  if (fn) h["x-chelabase-function"] = fn;
  if (grant) h["x-chelabase-sql-grant"] = grant;
  const res = await invoke(rt, DB, bundle, { path: `/?${new URLSearchParams(query)}`, headers: h, timeout: 15_000 });
  const text = await res.text();
  assert.equal(res.status, 200, text);
  return { ...JSON.parse(text), text };
}

describe("the SQL binding", () => {
  let store;
  let relay;
  let rt;
  before(async () => {
    store = await startBundleStore();
    relay = await startRelay();
    rt = await startRuntime(store, { relay: relay.port });
  });
  after(async () => {
    await rt?.stop();
    await store?.close();
    await relay?.close();
  });

  // A bundle of its own per call (a fresh loaded worker) unless `bundle` is given.
  const bundleOf = (tag = b64url(6)) => store.add(DB, manifest("sql", { [`tag-${tag}.txt`]: tag }));
  const call = (bundle, query, opts) => callWith(rt, store, bundle, query, opts);

  test("connect reaches the relay: the preamble with the grant, then the bytes, and the echo", async () => {
    const grant = newGrant();
    const before = relay.conns.length;
    const out = await call(bundleOf(), { op: "connect", addr: "chelabase-db:5432", msg: "startup-bytes" }, { grant });
    assert.equal(out.error, undefined, out.text);
    assert.equal(out.echo, "startup-bytes");
    assert.equal(out.url, URL_STRING);
    assert.deepEqual(out.bindings, ["CHELABASE", "CHELABASE_DB", "CHELABASE_DB_URL"]);
    const conns = relay.conns.slice(before);
    assert.equal(conns.length, 1);
    assert.equal(conns[0].preamble, `CHELABASE-SQL 1 ${grant}\n`);
    assert.equal(conns[0].data.toString(), "startup-bytes");
  });

  test("connect() without an address throws at once and reaches nothing: the documented call is connect(\"chelabase-db:5432\")", async () => {
    const before = relay.conns.length;
    const started = Date.now();
    const out = await call(bundleOf(), { op: "connect", msg: "no-address" }, { grant: newGrant() });
    assert.match(out.error ?? "", /SocketAddress|undefined or null/, out.text);
    assert.equal(out.echo, undefined, out.text);
    assert.ok(Date.now() - started < 3000, "the call fails before the read deadline, it doesn't hang");
    assert.equal(relay.conns.length, before);
  });

  test("the caller's address is ignored and secureTransport on is refused", async () => {
    const grant = newGrant();
    const b = bundleOf();
    for (const addr of ["proxy:4432", "169.254.169.254:80", "metadata-db:5432"]) {
      const before = relay.conns.length;
      const out = await call(b, { op: "connect", addr, msg: `to-${addr}` }, { grant });
      assert.equal(out.echo, `to-${addr}`, out.text);
      const conns = relay.conns.slice(before);
      assert.equal(conns.length, 1, addr);
      assert.equal(conns[0].preamble, `CHELABASE-SQL 1 ${grant}\n`, addr);
    }
    const before = relay.conns.length;
    const on = await call(b, { op: "connect", addr: "x:1", tls: "on" }, { grant });
    assert.match(on.error ?? "", /TLS/, on.text);
    assert.equal(relay.conns.length, before, "a TLS connect never reaches the relay");
    // starttls opens a plain socket to the relay (preamble and all); only startTls() is refused,
    // so no TLS ever crosses the hop.
    const plain = relay.conns.length;
    const st = await call(b, { op: "connect", addr: "x:1", tls: "starttls", msg: "plain" }, { grant });
    assert.equal(st.echo, "plain", st.text);
    assert.equal(relay.conns.length, plain + 1);
    assert.equal(relay.conns[plain].preamble, `CHELABASE-SQL 1 ${grant}\n`);
    assert.match(st.startTls ?? "", /not supported/, st.text);
  });

  test("fetch answers 404 and nothing leaves the supervisor; no RPC method answers", async () => {
    const grant = newGrant();
    const b = bundleOf();
    const before = relay.conns.length;
    for (const url of ["http://anything/", "http://proxy:4432/", "https://functions-sql/x"]) {
      const out = await call(b, { op: "fetch", url }, { grant });
      assert.equal(out.status, 404, out.text);
    }
    const rpc = await call(b, { op: "rpc" }, { grant });
    assert.ok(rpc.error, rpc.text);
    assert.equal(rpc.rpc, undefined);
    assert.equal(relay.conns.length, before);
    assert.deepEqual(store.other, []);
  });

  test("without a grant the binding and the URL are absent, even for a worker that had one", async () => {
    const b = bundleOf();
    const none = await call(b, { op: "env" });
    assert.deepEqual(none.bindings, ["CHELABASE"]);
    assert.equal(none.url, null);
    const withGrant = await call(b, { op: "env" }, { grant: newGrant() });
    assert.deepEqual(withGrant.bindings, ["CHELABASE", "CHELABASE_DB", "CHELABASE_DB_URL"]);
    const again = await call(b, { op: "env" });
    assert.deepEqual(again.bindings, ["CHELABASE"]);
    // Without a function header there is no binding either: the grant is refused.
    const res = await invoke(rt, DB, b, { path: "/?op=env", headers: { "x-chelabase-sql-grant": newGrant() } });
    assert.equal(res.status, 400);
    assert.match(res.headers.get("x-chelabase-error") ?? "", /x-chelabase-function/);
  });

  test("a malformed grant is refused before anything loads", async () => {
    const b = bundleOf();
    for (const grant of ["has space", "a".repeat(600), "v1.k1.x;y.z"]) {
      const res = await invoke(rt, DB, b, {
        path: "/?op=env",
        headers: { "x-chelabase-function": "br_main/fn_one", "x-chelabase-sql-grant": grant },
      });
      assert.equal(res.status, 400, grant);
      assert.match(res.headers.get("x-chelabase-error") ?? "", /x-chelabase-sql-grant/);
    }
  });

  test("a cached worker connects with the newest grant", async () => {
    const b = bundleOf();
    const g1 = newGrant();
    const g2 = newGrant();
    const first = await call(b, { op: "env" }, { grant: g1 });
    const before = relay.conns.length;
    const second = await call(b, { op: "connect", addr: "chelabase-db:5432", msg: "after-g2" }, { grant: g2 });
    assert.equal(second.served, first.served + 1, "the same loaded worker served both");
    assert.equal(second.echo, "after-g2", second.text);
    assert.equal(relay.conns[before].preamble, `CHELABASE-SQL 1 ${g2}\n`);
  });

  test("two functions or branches sharing a bundle and env get their own worker and grant", async () => {
    const b = bundleOf();
    const callers = [
      { fn: "br_main/fn_one", grant: newGrant() },
      { fn: "br_main/fn_two", grant: newGrant() },
      { fn: "br_dev/fn_one", grant: newGrant() },
    ];
    for (const c of callers) {
      const before = relay.conns.length;
      const out = await call(b, { op: "connect", addr: "chelabase-db:5432", msg: c.fn }, c);
      assert.equal(out.served, 1, `${c.fn} got a loaded worker of its own`);
      assert.equal(out.echo, c.fn, out.text);
      assert.equal(relay.conns[before].preamble, `CHELABASE-SQL 1 ${c.grant}\n`, c.fn);
    }
    // And each keeps its own after the others were stored.
    for (const c of callers) {
      const before = relay.conns.length;
      await call(b, { op: "connect", addr: "chelabase-db:5432", msg: "again" }, { fn: c.fn, grant: c.grant });
      assert.equal(relay.conns[before].preamble, `CHELABASE-SQL 1 ${c.grant}\n`, c.fn);
    }
  });

  test("nothing reachable from user code holds the grant", async () => {
    const grant = newGrant();
    const b = bundleOf();
    await call(b, { op: "connect", addr: "chelabase-db:5432", msg: "warm" }, { grant });
    const out = await call(b, { op: "walk" }, { grant, headers: { "x-other": "visible-marker" } });
    assert.equal(out.error, undefined, out.text.slice(0, 2000));
    assert.ok(out.strings.includes("visible-marker"), "the walk reaches the request headers");
    assert.ok(out.strings.includes(URL_STRING), "the walk reaches env");
    const [, kid, payload, mac] = grant.split(".");
    for (const piece of [grant, payload, mac]) {
      assert.ok(!out.text.includes(piece), "a grant piece is visible to user code");
    }
    assert.ok(kid);
  });

  test("direct sockets to the relay, the proxy or a compute address are refused", async () => {
    const b = bundleOf();
    const before = relay.conns.length;
    for (const addr of [`functions-sql:${relay.port}`, `proxy:${relay.port}`, "proxy:4432", `172.17.0.1:${relay.port}`, `10.0.0.5:${relay.port}`]) {
      const out = await call(b, { op: "raw", addr }, { grant: newGrant() });
      assert.notEqual(out.opened, true, `${addr}: ${out.text}`);
    }
    assert.equal(relay.conns.length, before, "no direct socket reached the relay");
  });
  test(`after more than ${MAX_GRANTS} newer grants an old worker's connect reaches no relay`, async () => {
    // Two SQL workers schedule a connect for later. A flood of newer grants from other functions
    // (against a missing bundle: the grant is held before the bundle is fetched) pushes both out
    // of the map. B then brings its grant again before its connect fires; A doesn't.
    const DELAY_MS = 20_000;
    const a = { b: bundleOf(), fn: "br_main/fn_old", grant: newGrant() };
    const bw = { b: bundleOf(), fn: "br_main/fn_back", grant: newGrant() };
    const before = relay.conns.length;
    const started = Date.now();
    for (const w of [a, bw]) {
      await call(w.b, { op: "later", ms: String(DELAY_MS), addr: "chelabase-db:5432", msg: w.fn }, w);
    }
    const missing = "0".repeat(64);
    const flood = Array.from({ length: MAX_GRANTS + 16 }, (_, i) => i);
    for (let i = 0; i < flood.length; i += 64) {
      await Promise.all(
        flood.slice(i, i + 64).map(async (n) => {
          const res = await invoke(rt, DB, missing, {
            headers: { "x-chelabase-function": `br_flood/fn_${n}`, "x-chelabase-sql-grant": newGrant() },
            timeout: 15_000,
          });
          await res.arrayBuffer();
          assert.equal(res.status, 502);
        }),
      );
    }
    await call(bw.b, { op: "env" }, bw);
    const left = DELAY_MS - (Date.now() - started);
    assert.ok(left > 2000, `the flood took too long (${Date.now() - started} ms) for this test's delay`);
    await delay(left + 3000);
    const conns = relay.conns.slice(before);
    assert.deepEqual(conns.map((c) => c.data.toString()), [bw.fn], "only the re-granted worker reached the relay");
    assert.equal(conns[0].preamble, `CHELABASE-SQL 1 ${bw.grant}\n`);
  });
});

describe("the SQL grant's age", () => {
  // SQL_GRANT_MAX_AGE_MS may only shorten the 10-minute lifetime; this runtime sets 1.5 s.
  let store;
  let relay;
  let rt;
  before(async () => {
    store = await startBundleStore();
    relay = await startRelay();
    rt = await startRuntime(store, { relay: relay.port, env: { SQL_GRANT_MAX_AGE_MS: "1500" } });
  });
  after(async () => {
    await rt?.stop();
    await store?.close();
    await relay?.close();
  });

  test("a connect after the grant's age reaches no relay; one before it does", async () => {
    const bundle = store.add(DB, manifest("sql", { "age.txt": "age" }));
    const before = relay.conns.length;
    const early = await callWith(rt, store, bundle, { op: "later", ms: "200", addr: "chelabase-db:5432", msg: "early" }, { fn: "br_main/fn_early", grant: newGrant() });
    const late = await callWith(rt, store, bundle, { op: "later", ms: "3000", addr: "chelabase-db:5432", msg: "late" }, { fn: "br_main/fn_late", grant: newGrant() });
    assert.equal(early.error ?? late.error, undefined);
    await delay(6000);
    assert.deepEqual(relay.conns.slice(before).map((c) => c.data.toString()), ["early"]);
  });
});
