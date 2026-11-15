// The SQL binding (pooled-endpoints m12): a loaded worker whose invocation carries
// `x-chelabase-sql-grant` gets `env.CHELABASE_DB`, whose connect() always reaches PG_RELAY (here
// an echo stand-in) behind a `CHELABASE-SQL 1 <grant>` preamble, and `env.CHELABASE_DB_URL`.
// The grant never enters the user isolate; direct sockets to the relay or the proxy stay refused.
//
//   node --test infra/functions-runtime/tests/
import assert from "node:assert/strict";
import { randomBytes } from "node:crypto";
import { after, before, describe, test } from "node:test";
import { invoke, manifest, startBundleStore, startRelay, startRuntime } from "./harness.mjs";

const DB = "db_sqlbinding";
const URL_STRING = "postgresql://function@chelabase-db/postgres";
const b64url = (n) => randomBytes(n).toString("base64url");
// The shape m14 mints: v1.<kid>.<payload>.<mac>.
const newGrant = () => `v1.k1.${b64url(96)}.${b64url(32)}`;

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
  const call = async (bundle, query, { grant, fn = "br_main/fn_one", headers = {} } = {}) => {
    const h = { ...headers };
    if (fn) h["x-chelabase-function"] = fn;
    if (grant) h["x-chelabase-sql-grant"] = grant;
    const res = await invoke(rt, DB, bundle, { path: `/?${new URLSearchParams(query)}`, headers: h, timeout: 15_000 });
    const text = await res.text();
    assert.equal(res.status, 200, text);
    return { ...JSON.parse(text), text };
  };

  test("connect reaches the relay: the preamble with the grant, then the bytes, and the echo", async () => {
    const grant = newGrant();
    const before = relay.conns.length;
    const out = await call(bundleOf(), { op: "connect", msg: "startup-bytes" }, { grant });
    assert.equal(out.error, undefined, out.text);
    assert.equal(out.echo, "startup-bytes");
    assert.equal(out.url, URL_STRING);
    assert.deepEqual(out.bindings, ["CHELABASE", "CHELABASE_DB", "CHELABASE_DB_URL"]);
    const conns = relay.conns.slice(before);
    assert.equal(conns.length, 1);
    assert.equal(conns[0].preamble, `CHELABASE-SQL 1 ${grant}\n`);
    assert.equal(conns[0].data.toString(), "startup-bytes");
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
    // starttls opens the plain socket, but the upgrade is refused: no TLS ever crosses the hop.
    const st = await call(b, { op: "connect", addr: "x:1", tls: "starttls", msg: "plain" }, { grant });
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
    const second = await call(b, { op: "connect", msg: "after-g2" }, { grant: g2 });
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
      const out = await call(b, { op: "connect", msg: c.fn }, c);
      assert.equal(out.served, 1, `${c.fn} got a loaded worker of its own`);
      assert.equal(out.echo, c.fn, out.text);
      assert.equal(relay.conns[before].preamble, `CHELABASE-SQL 1 ${c.grant}\n`, c.fn);
    }
    // And each keeps its own after the others were stored.
    for (const c of callers) {
      const before = relay.conns.length;
      await call(b, { op: "connect", msg: "again" }, { fn: c.fn, grant: c.grant });
      assert.equal(relay.conns[before].preamble, `CHELABASE-SQL 1 ${c.grant}\n`, c.fn);
    }
  });

  test("nothing reachable from user code holds the grant", async () => {
    const grant = newGrant();
    const b = bundleOf();
    await call(b, { op: "connect", msg: "warm" }, { grant });
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
});
