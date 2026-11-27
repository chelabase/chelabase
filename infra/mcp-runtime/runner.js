// The MCP sandbox runner: runs one piece of model-written code per request in a fresh isolate.
// Only the `mcp` service reaches it (m8 puts the two alone on an internal network).
//
// POST /run, JSON {exec_id, kind: "search"|"execute", code, timeout_ms, spec_hash}:
//   exec_id     [A-Za-z0-9_-]{1,128}; names the run in execute's host callback
//   code        the text of an async arrow function, at most 256 KiB (413 beyond)
//   timeout_ms  1..60000
//   spec_hash   search only: sha256 (lowercase hex) of the spec bundle's bytes
// The answer (200) is {ok: true, result, logs} or {ok: false, error: {code, message, stack?},
// logs}, and for execute also `unreceived` (below), with codes invalid_code (does not parse, or
// is not a function), threw, timeout and not_serializable (a function, symbol, bigint or cycle
// in the value, or over 4 MiB). A request the runner can't take is 400/405/413 (409 for the
// exec_id of a run still in progress); a spec it can't get from the host (within 10 s) is 502.
//
// Each run goes through LOADER.load(): an anonymous worker, never cached or reused, so no
// isolate, global or module state is shared between runs or callers, and the isolate is
// released when the run ends (a named LOADER.get() would keep every one). The loaded worker
// gets `globalOutbound: null` (every fetch and connect throws) and an env holding only:
//   search   nothing; the code sees a module-level `spec` (the bundle the runner fetched once
//            per spec_hash from the host at GET /internal/spec, its bytes checked against the
//            hash, kept in memory)
//   execute  `chelabase`: a stub of this worker's `Host` entrypoint, bound to the exec_id, whose
//            request({method, path, query, body, headers}) POSTs that JSON to the host at
//            /internal/exec/{exec_id}/request and returns {status, ok, headers, body} from the
//            host's 200 answer {status, headers, body}. The code chooses none of host, path or
//            id; the request's own headers (a caller's credential) never reach the sandbox.
//            Each call is numbered (1, 2, … in the order the code makes it) and its callback
//            carries the number as `chelabase-call`. When the run ends, however it ends,
//            `unreceived` lists the numbers of the calls sent to the host whose answer the runner
//            had not handed back yet: a call cut when the code settled, or one whose answer
//            failed or couldn't be read. The host reports those as `unknown`.
// The pinned workerd accepts the loader's `limits` but doesn't enforce them, and offers no
// memory limit for loaded workers; the timeout, the watchdog (busy loops), the V8 heap cap in
// config.capnp (unbounded JS heap) and the container's memory limit (ArrayBuffer memory, which
// the heap cap doesn't count) bound a run instead.
import { WorkerEntrypoint } from "cloudflare:workers";

const MAX_CODE_BYTES = 256 << 10;
const MAX_BODY_BYTES = 2 << 20; // a JSON-escaped 256 KiB of code fits
const MAX_SPEC_BYTES = 16 << 20;
const MAX_SPECS = 4;
const MAX_ANSWER_BYTES = 6 << 20; // a 4 MiB result plus logs
// The host's own caps line up with these (services/mcp/src/host.rs): it reads an envelope of up
// to 4 MiB (MAX_ENVELOPE_BYTES) and answers a body over 1 MiB with request_too_large, and it
// answers response_too_large rather than send an answer over 8 MiB (RUNNER_ANSWER_MAX_BYTES),
// so the code gets those answers instead of a throw.
const MAX_HOST_REQUEST_BYTES = 4 << 20;
const MAX_HOST_ANSWER_BYTES = 8 << 20;
const MAX_TIMEOUT_MS = 60_000;
const SPEC_TIMEOUT_MS = 10_000;
const EXEC_ID_RE = /^[A-Za-z0-9_-]{1,128}$/;
const HASH_RE = /^[0-9a-f]{64}$/;
const COMPAT_DATE = "2026-10-01";
const CODES = new Set(["invalid_code", "threw", "not_serializable"]);
const METHODS = new Set(["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"]);

class Refused extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

const isObject = (v) => v !== null && typeof v === "object" && !Array.isArray(v);
const hex = (buf) => [...new Uint8Array(buf)].map((b) => b.toString(16).padStart(2, "0")).join("");

// Reads a body, refusing it (`refusal`) past `max` bytes.
async function readCapped(stream, max, refusal) {
  if (!stream) return new Uint8Array();
  const reader = stream.getReader();
  const parts = [];
  let size = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    size += value.byteLength;
    if (size > max) {
      await reader.cancel();
      throw refusal;
    }
    parts.push(value);
  }
  const out = new Uint8Array(size);
  let at = 0;
  for (const p of parts) {
    out.set(p, at);
    at += p.byteLength;
  }
  return out;
}

const decode = (bytes) => new TextDecoder("utf-8", { fatal: true }).decode(bytes);

async function parseRun(request) {
  const tooLarge = new Refused(413, "the request is too large");
  const len = Number(request.headers.get("content-length") ?? 0);
  if (len > MAX_BODY_BYTES) throw tooLarge;
  let req;
  try {
    req = JSON.parse(decode(await readCapped(request.body, MAX_BODY_BYTES, tooLarge)));
  } catch (e) {
    if (e instanceof Refused) throw e;
    throw new Refused(400, "the body is not JSON");
  }
  if (!isObject(req)) throw new Refused(400, "the body is not a JSON object");
  const { exec_id, kind, code, timeout_ms, spec_hash } = req;
  if (typeof exec_id !== "string" || !EXEC_ID_RE.test(exec_id)) throw new Refused(400, "exec_id must match [A-Za-z0-9_-]{1,128}");
  if (kind !== "search" && kind !== "execute") throw new Refused(400, 'kind must be "search" or "execute"');
  if (typeof code !== "string") throw new Refused(400, "code must be a string");
  if (new TextEncoder().encode(code).byteLength > MAX_CODE_BYTES) throw new Refused(413, "code is over 256 KiB");
  if (!Number.isInteger(timeout_ms) || timeout_ms < 1 || timeout_ms > MAX_TIMEOUT_MS) {
    throw new Refused(400, `timeout_ms must be an integer from 1 to ${MAX_TIMEOUT_MS}`);
  }
  if (kind === "search" && (typeof spec_hash !== "string" || !HASH_RE.test(spec_hash))) {
    throw new Refused(400, "spec_hash must be a lowercase hex sha256");
  }
  return { exec_id, kind, code, timeout_ms, spec_hash };
}

// exec_id -> the execute run's calls: {next: the last call number, sent: numbers sent to the
// host whose answer hasn't been handed back}. Lives while the run does.
const execs = new Map();

// spec_hash -> Promise of the spec's JSON text (checked once, kept for later runs).
const specs = new Map();

function specText(env, hash) {
  let p = specs.get(hash);
  if (!p) {
    p = fetchSpec(env, hash);
    specs.set(hash, p);
    p.catch(() => specs.delete(hash));
    if (specs.size > MAX_SPECS) specs.delete(specs.keys().next().value);
  }
  return p;
}

async function fetchSpec(env, hash) {
  // The whole exchange, body included, within SPEC_TIMEOUT_MS: a stalled host fails the run.
  const signal = AbortSignal.timeout(SPEC_TIMEOUT_MS);
  const stalled = () => new Refused(502, `the host did not send the spec within ${SPEC_TIMEOUT_MS / 1000} s`);
  let res;
  let bytes;
  try {
    res = await env.HOST.fetch("http://mcp-internal/internal/spec", { redirect: "manual", signal });
    if (res.status !== 200) {
      await res.body?.cancel();
      throw new Refused(502, `the host answered ${res.status} for the spec`);
    }
    bytes = await readCapped(res.body, MAX_SPEC_BYTES, new Refused(502, "the spec is over 16 MiB"));
  } catch (e) {
    if (e instanceof Refused) throw e;
    if (signal.aborted) throw stalled();
    throw new Refused(502, `the spec request failed: ${String(e?.message ?? e).slice(0, 256)}`);
  }
  if (hex(await crypto.subtle.digest("SHA-256", bytes)) !== hash) throw new Refused(502, "the spec does not match spec_hash");
  let text;
  try {
    text = decode(bytes);
    if (!isObject(JSON.parse(text))) throw new Error();
  } catch {
    throw new Refused(502, "the spec is not a JSON object");
  }
  return text;
}

// The loaded worker's modules. prelude.js runs first (code.js imports it before anything else):
// it captures console output and the builtins main.js uses. code.js holds the code as the
// default export, after the declarations of its free names. The code is untrusted text in its
// own module: anything it does there (even closing the expression early) runs only with the
// env below.
const PRELUDE = `
const stringify = JSON.stringify;
const MAX_LINES = 200, MAX_LINE = 4096, MAX_BYTES = 64 << 10;
export const logs = [];
let bytes = 0, dropped = 0;
const fmt = (a) => {
  if (typeof a === "string") return a;
  try { const s = stringify(a); return s === undefined ? String(a) : s; } catch {}
  try { return String(a); } catch { return "[unprintable]"; }
};
for (const level of ["log", "info", "warn", "error", "debug"]) {
  console[level] = (...args) => {
    let line = (level === "log" ? "" : "[" + level + "] ") + args.map(fmt).join(" ");
    if (line.length > MAX_LINE) line = line.slice(0, MAX_LINE) + "…";
    if (logs.length >= MAX_LINES || bytes + line.length > MAX_BYTES) { dropped++; return; }
    bytes += line.length;
    logs.push(line);
  };
}
export const allLogs = () => dropped ? [...logs, "… " + dropped + " more lines dropped"] : logs;
export { stringify };
`;

const MAIN = `
import { allLogs, stringify } from "./prelude.js";
import fn from "./code.js";
const MAX_RESULT = 4 << 20;
const cap = (s, n) => (s.length > n ? s.slice(0, n) + "…" : s);
const answer = (head, result) =>
  new Response('{"ok":' + head + (result === undefined ? "" : ',"result":' + result) + ',"logs":' + stringify(allLogs()) + "}");
const failed = (code, message, stack) =>
  answer("false" + ',"error":' + stringify(stack === undefined ? { code, message } : { code, message, stack }));
const reject = (k, v) => {
  if (typeof v === "function" || typeof v === "symbol") throw new TypeError("the value holds a " + typeof v);
  return v;
};
export default {
  async fetch() {
    if (typeof fn !== "function") return failed("invalid_code", "the code is not a function");
    let value;
    try {
      value = await fn();
    } catch (e) {
      let message = "", stack;
      try { message = cap(String(e?.message ?? e), 4096); } catch {}
      try { if (typeof e?.stack === "string") stack = cap(e.stack, 8192); } catch {}
      return failed("threw", message, stack);
    }
    let text;
    try {
      text = stringify(value, reject) ?? "null";
    } catch (e) {
      return failed("not_serializable", "the value is not JSON: " + cap(String(e?.message), 1024));
    }
    if (text.length > MAX_RESULT) return failed("not_serializable", "the value is over 4 MiB as JSON");
    return answer("true", text);
  },
};
`;

function workerCode(job, spec, ctx) {
  const decls =
    job.kind === "search"
      ? 'import specText from "./spec.txt";\nconst spec = JSON.parse(specText);\n'
      : 'import { env as runnerEnv } from "cloudflare:workers";\nconst chelabase = runnerEnv.chelabase;\n';
  const modules = {
    "main.js": MAIN,
    "prelude.js": PRELUDE,
    "code.js": `import "./prelude.js";\n${decls}export default (\n${job.code}\n);\n`,
  };
  if (job.kind === "search") modules["spec.txt"] = { text: spec };
  return {
    compatibilityDate: COMPAT_DATE,
    mainModule: "main.js",
    modules,
    env: job.kind === "execute" ? { chelabase: ctx.exports.Host({ props: { execId: job.exec_id } }) } : {},
    globalOutbound: null,
  };
}

// The loaded worker's answer is untrusted (the code shares its isolate): rebuilt from checked
// parts, so the runner's own answer is always well-formed.
function checkAnswer(text) {
  const bad = { ok: false, error: { code: "not_serializable", message: "the run's answer was malformed" }, logs: [] };
  let a;
  try {
    a = JSON.parse(text);
  } catch {
    return bad;
  }
  if (!isObject(a)) return bad;
  const logs = Array.isArray(a.logs) && a.logs.every((l) => typeof l === "string") ? a.logs.slice(0, 201) : [];
  if (a.ok === true) return { ok: true, result: a.result ?? null, logs };
  const e = a.error;
  if (a.ok !== false || !isObject(e) || !CODES.has(e.code) || typeof e.message !== "string") return bad;
  const error = { code: e.code, message: e.message.slice(0, 4096) };
  if (typeof e.stack === "string") error.stack = e.stack.slice(0, 8192);
  return { ok: false, error, logs };
}

// Runs the job; an execute run's outcome carries `unreceived`.
async function run(env, ctx, job, spec) {
  if (job.kind !== "execute") return runJob(env, ctx, job, spec, () => {});
  if (execs.has(job.exec_id)) throw new Refused(409, "a run with this exec_id is in progress");
  const calls = { next: 0, sent: new Set() };
  execs.set(job.exec_id, calls);
  let unreceived;
  // Taken the moment the run's end is known, before a call settling later can count as received.
  const settled = () => {
    unreceived ??= [...calls.sent].sort((a, b) => a - b);
  };
  try {
    const outcome = await runJob(env, ctx, job, spec, settled);
    settled();
    return { ...outcome, unreceived };
  } finally {
    execs.delete(job.exec_id);
  }
}

async function runJob(env, ctx, job, spec, settled) {
  const worker = env.LOADER.load(workerCode(job, spec, ctx));
  const abort = new AbortController();
  let timer;
  const timedOut = new Promise((resolve) => {
    timer = setTimeout(() => {
      settled();
      abort.abort();
      resolve({ ok: false, error: { code: "timeout", message: `the run took over ${job.timeout_ms} ms` }, logs: [] });
    }, job.timeout_ms);
  });
  const ran = (async () => {
    let res;
    try {
      res = await worker.getEntrypoint().fetch("http://sandbox/", { method: "POST", signal: abort.signal });
      settled();
    } catch (e) {
      settled();
      if (abort.signal.aborted) return null;
      // workerd ends a run early when it can prove it never settles (awaiting nothing that can
      // resolve): that is the timeout too, just known sooner. A run that calls abortIsolate()
      // gets the very same error, so the message names both.
      if (/detected that your Worker's code had hung/.test(String(e?.message))) {
        return { ok: false, error: { code: "timeout", message: "the run never settles, or ended its own isolate" }, logs: [] };
      }
      // A remote exception is the loaded worker's handler failing (the code broke main.js's
      // builtins); anything else is the worker failing to start: a syntax error, or a throw at
      // module level. workerd's message quotes the code, which is the caller's own.
      if (e?.remote) return { ok: false, error: { code: "threw", message: String(e?.message ?? e).slice(0, 4096) }, logs: [] };
      return { ok: false, error: { code: "invalid_code", message: String(e?.message ?? e).slice(0, 4096) }, logs: [] };
    }
    const bytes = await readCapped(res.body, MAX_ANSWER_BYTES, "too large").catch((e) => e);
    if (bytes === "too large") return { ok: false, error: { code: "not_serializable", message: "the value is over 4 MiB as JSON" }, logs: [] };
    if (!(bytes instanceof Uint8Array)) throw bytes;
    return checkAnswer(new TextDecoder().decode(bytes));
  })();
  ran.catch(() => {});
  try {
    return await Promise.race([ran, timedOut]);
  } finally {
    clearTimeout(timer);
  }
}

export default {
  async fetch(request, env, ctx) {
    try {
      if (new URL(request.url).pathname !== "/run") throw new Refused(404, "not found");
      if (request.method !== "POST") throw new Refused(405, "POST /run only");
      const job = await parseRun(request);
      const spec = job.kind === "search" ? await specText(env, job.spec_hash) : undefined;
      return Response.json(await run(env, ctx, job, spec));
    } catch (e) {
      if (e instanceof Refused) return Response.json({ error: e.message }, { status: e.status });
      throw e;
    }
  },
};

// The binding a loaded worker gets as `chelabase` (execute only). Its one method is the only
// way out of the sandbox: to the host, at the run's own callback path, with the exec_id the
// runner bound in props. Keep every other function outside this class: each method on it is
// callable from the sandbox.
export class Host extends WorkerEntrypoint {
  async request(opts) {
    return hostRequest(this.env, this.ctx.props?.execId, opts);
  }
}

async function hostRequest(env, execId, opts) {
  if (typeof execId !== "string" || !EXEC_ID_RE.test(execId)) throw new Error("chelabase.request: this run has no exec_id");
  if (!isObject(opts)) throw new TypeError("chelabase.request takes {method, path, query?, body?, headers?}");
  const method = typeof opts.method === "string" ? opts.method.toUpperCase() : opts.method === undefined ? "GET" : null;
  if (!METHODS.has(method)) throw new TypeError(`chelabase.request: method must be one of ${[...METHODS].join(", ")}`);
  const { path, query, headers, body } = opts;
  if (typeof path !== "string" || !/^\/(?!\/)\S{0,8191}$/.test(path)) {
    throw new TypeError('chelabase.request: path must be an absolute path such as "/v1/databases"');
  }
  if (query !== undefined && !isObject(query)) throw new TypeError("chelabase.request: query must be an object");
  if (headers !== undefined && (!isObject(headers) || Object.values(headers).some((v) => typeof v !== "string"))) {
    throw new TypeError("chelabase.request: headers must be an object of strings");
  }
  let envelope;
  try {
    envelope = JSON.stringify({ method, path, query, headers, body });
  } catch {
    throw new TypeError("chelabase.request: body is not JSON");
  }
  // Bytes, as the host counts them (a string's length counts UTF-16 units).
  if (new TextEncoder().encode(envelope).byteLength > MAX_HOST_REQUEST_BYTES) {
    throw new RangeError("chelabase.request: the request is over 4 MiB");
  }
  // Numbered once it is sent; un-numbered again only when its answer is handed back.
  const calls = execs.get(execId);
  if (!calls) throw new Error("chelabase.request: this run has ended");
  const n = ++calls.next;
  calls.sent.add(n);
  const res = await env.HOST.fetch(`http://mcp-internal/internal/exec/${execId}/request`, {
    method: "POST",
    headers: { "content-type": "application/json", "chelabase-call": String(n) },
    body: envelope,
    redirect: "manual",
  });
  if (res.status !== 200) {
    await res.body?.cancel();
    throw new Error(`chelabase.request: the host answered ${res.status}`);
  }
  const tooLarge = new Error("chelabase.request: the answer is over 8 MiB");
  let a;
  try {
    a = JSON.parse(decode(await readCapped(res.body, MAX_HOST_ANSWER_BYTES, tooLarge)));
  } catch (e) {
    if (e === tooLarge) throw e;
    throw new Error("chelabase.request: the host's answer is not JSON");
  }
  if (!isObject(a) || !Number.isInteger(a.status) || a.status < 100 || a.status > 599) {
    throw new Error("chelabase.request: the host's answer has no status");
  }
  calls.sent.delete(n);
  return { status: a.status, ok: a.status >= 200 && a.status < 300, headers: isObject(a.headers) ? a.headers : {}, body: a.body ?? null };
}

const PROBE_CODE = {
  compatibilityDate: COMPAT_DATE,
  mainModule: "probe.js",
  modules: { "probe.js": 'export default { fetch() { return new Response("ok") } }' },
  env: {},
  globalOutbound: null,
};

// The watchdog's and the healthcheck's probe (loopback socket only): runs a trivial worker
// through the loader, so it fails whenever the event loop is stuck. The probe's worker is the one
// named (cached) worker in this runtime; no run's code is ever loaded under a name.
export const probe = {
  async fetch(_request, env) {
    return env.LOADER.get("probe", async () => PROBE_CODE).getEntrypoint().fetch("http://probe/");
  },
};
