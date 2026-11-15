// The SQL binding (pooled-endpoints m12) from user code. ?op= picks what it does:
//
//   env      the env's keys, CHELABASE_DB_URL, and how many requests this loaded worker served
//   connect  env.CHELABASE_DB.connect(?addr, {secureTransport: ?tls}), writes ?msg, reads until
//            it comes back (or 3 s), and reports what it read or the error
//   fetch    env.CHELABASE_DB.fetch(?url): the status and body
//   rpc      calls a method the entrypoint doesn't have
//   raw      cloudflare:sockets connect(?addr) (the global outbound): whether it opened
//   walk     every string reachable from env, the stub, the request and globalThis
import { connect } from "cloudflare:sockets";

let served = 0;

async function readUntil(socket, want, ms) {
  const reader = socket.readable.getReader();
  const deadline = Date.now() + ms;
  let got = "";
  while (!got.includes(want)) {
    const left = deadline - Date.now();
    if (left <= 0) break;
    const r = await Promise.race([reader.read(), new Promise((res) => setTimeout(() => res({ done: true }), left))]);
    if (r.done) break;
    got += new TextDecoder().decode(r.value);
  }
  reader.releaseLock();
  return got;
}

// Every string reachable from `roots`: own property names and symbols, values, getters (called),
// prototypes, Map/Set/Headers entries.
function walk(roots) {
  const seen = new Set();
  const strings = [];
  const queue = [...roots];
  while (queue.length > 0 && seen.size < 50_000) {
    const v = queue.shift();
    if (typeof v === "string") {
      strings.push(v);
      continue;
    }
    if (typeof v === "symbol") {
      strings.push(String(v.description));
      continue;
    }
    if (v === null || (typeof v !== "object" && typeof v !== "function") || seen.has(v)) continue;
    seen.add(v);
    try {
      if (typeof v === "function") strings.push(String(v));
    } catch {}
    try {
      if (v instanceof Headers || v instanceof Map) for (const [k, x] of v) queue.push(k, x);
      if (v instanceof Set) for (const x of v) queue.push(x);
    } catch {}
    let keys = [];
    try {
      keys = Reflect.ownKeys(v);
    } catch {}
    for (const k of keys) {
      queue.push(typeof k === "symbol" ? k : String(k));
      try {
        queue.push(Reflect.get(v, k));
      } catch {}
      try {
        const d = Object.getOwnPropertyDescriptor(v, k);
        if (d?.get) queue.push(d.get.call(v));
      } catch {}
    }
    try {
      queue.push(Object.getPrototypeOf(v));
    } catch {}
  }
  return strings;
}

export default {
  async fetch(request, env) {
    served++;
    const q = new URL(request.url).searchParams;
    const out = { served, bindings: Object.keys(env).sort(), url: env.CHELABASE_DB_URL ?? null };
    try {
      switch (q.get("op")) {
        case "connect": {
          const tls = q.get("tls");
          const socket = env.CHELABASE_DB.connect(q.get("addr") ?? "chelabase-db:5432", tls ? { secureTransport: tls } : undefined);
          const writer = socket.writable.getWriter();
          await writer.write(new TextEncoder().encode(q.get("msg") ?? "ping"));
          writer.releaseLock();
          out.echo = await readUntil(socket, q.get("msg") ?? "ping", 3000);
          if (tls === "starttls") {
            try {
              await socket.startTls().opened;
              out.startTls = "opened";
            } catch (e) {
              out.startTls = String(e?.message);
            }
          }
          await socket.close().catch(() => {});
          break;
        }
        case "fetch": {
          const res = await env.CHELABASE_DB.fetch(q.get("url") ?? "http://anything/");
          out.status = res.status;
          out.body = await res.text();
          break;
        }
        case "rpc":
          out.rpc = await env.CHELABASE_DB.grant();
          break;
        case "raw": {
          const socket = connect(q.get("addr"));
          await Promise.race([socket.opened, new Promise((_, rej) => setTimeout(() => rej(new Error("timeout")), 4000))]);
          out.opened = true;
          await socket.close().catch(() => {});
          break;
        }
        case "walk":
          out.strings = walk([env, env.CHELABASE_DB, request, request.headers, globalThis]);
          if (env.CHELABASE_DB) out.strings.push(...walk([Object.getPrototypeOf(env.CHELABASE_DB)]));
          break;
      }
    } catch (e) {
      out.error = String(e?.message ?? e);
    }
    return Response.json(out);
  },
};
