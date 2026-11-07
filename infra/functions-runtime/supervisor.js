// The functions runtime's supervisor: loads member bundles through workerd's Worker Loader and
// runs their `fetch` handler. Only the functions service reaches it (the internal `functions`
// network), and it sets the two headers this reads:
//
//   x-chelabase-bundle  {database_id}/{sha256}
//   x-chelabase-env     base64 of a JSON object of string env vars (already decrypted)
//
// The bundle is fetched from BUNDLES at /internal/bundles/{database_id}/{sha256}: a JSON
// manifest
//
//   {"compatibilityDate": "YYYY-MM-DD", "compatibilityFlags": [..], "mainModule": "<name>",
//    "modules": [{"name": "<name>", "type": "js"|"wasm"|"text"|"data", "base64": "<bytes>"}]}
//
// whose bytes, exactly as served, must hash (sha256, lowercase hex) to the requested sha256: the
// producer (m2) serialises it canonically once and the store serves those bytes. Anything else
// gets 502 and nothing is loaded. A bundle that loads but can't start answers 502 `bundle failed
// to start`; a handler that throws, 500. Every refusal names its reason in `x-chelabase-error`.
//
// A loaded worker is cached by the loader under `{database_id}/{sha256}:{sha256(env header)}`,
// so two deployments sharing a bundle never share an env. User workers get only their own env,
// the `egress` service as their global outbound (public addresses only), and the request
// without any x-chelabase-* header.
const BUNDLE_HEADER = "x-chelabase-bundle";
const ENV_HEADER = "x-chelabase-env";
const PRIVATE_PREFIX = "x-chelabase-";
const ERROR_HEADER = "x-chelabase-error";
const START_FAILED = "bundle failed to start";
const BUNDLE_RE = /^([A-Za-z0-9_-]{1,64})\/([0-9a-f]{64})$/;
const DATE_RE = /^\d{4}-\d{2}-\d{2}$/;
const MAX_MANIFEST_BYTES = 64 << 20;
const MAX_ENV_HEADER = 256 << 10;

// Loader ids whose manifest passed the checks in this isolate. A first request verifies before
// it calls the loader, because the loader caches a failed load under its id; later requests
// (or a reload after the loader evicted the worker) fetch inside the loader callback.
const verified = new Set();
const MAX_VERIFIED = 4096;

class Refused extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

const hex = (buf) => [...new Uint8Array(buf)].map((b) => b.toString(16).padStart(2, "0")).join("");
const sha256 = async (bytes) => hex(await crypto.subtle.digest("SHA-256", bytes));

function fromBase64(s) {
  const bin = atob(s);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

function parseEnv(header) {
  if (header === "") return {};
  if (header.length > MAX_ENV_HEADER) throw new Refused(400, "x-chelabase-env is too large");
  let value;
  try {
    value = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(fromBase64(header)));
  } catch {
    throw new Refused(400, "x-chelabase-env is not base64 JSON");
  }
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw new Refused(400, "x-chelabase-env is not a JSON object");
  }
  const env = {};
  for (const [k, v] of Object.entries(value)) {
    if (typeof v !== "string") throw new Refused(400, `x-chelabase-env: ${k} is not a string`);
    Object.defineProperty(env, k, { value: v, enumerable: true, writable: true, configurable: true });
  }
  return env;
}

function badManifest(why) {
  return new Refused(502, `bundle manifest: ${why}`);
}

// Fetches and checks a bundle; returns the loader's WorkerCode.
async function loadCode(bindings, db, sha, env) {
  const res = await bindings.BUNDLES.fetch(`http://functions/internal/bundles/${db}/${sha}`);
  if (res.status !== 200) {
    await res.body?.cancel();
    throw new Refused(502, `bundle ${db}/${sha}: the bundle store answered ${res.status}`);
  }
  const body = new Uint8Array(await res.arrayBuffer());
  if (body.byteLength > MAX_MANIFEST_BYTES) throw badManifest("too large");
  if ((await sha256(body)) !== sha) throw badManifest("does not match its sha256");
  let m;
  try {
    m = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(body));
  } catch {
    throw badManifest("not JSON");
  }
  if (m === null || typeof m !== "object") throw badManifest("not an object");
  if (typeof m.compatibilityDate !== "string" || !DATE_RE.test(m.compatibilityDate)) {
    throw badManifest("compatibilityDate is not YYYY-MM-DD");
  }
  const flags = m.compatibilityFlags ?? [];
  if (!Array.isArray(flags) || flags.some((f) => typeof f !== "string")) {
    throw badManifest("compatibilityFlags is not a list of strings");
  }
  if (!Array.isArray(m.modules) || m.modules.length === 0) throw badManifest("no modules");
  const modules = {};
  for (const mod of m.modules) {
    const { name, type, base64 } = mod ?? {};
    if (typeof name !== "string" || name === "" || typeof base64 !== "string") {
      throw badManifest("a module needs a name and base64");
    }
    if (Object.hasOwn(modules, name)) throw badManifest(`module ${name} appears twice`);
    let bytes;
    try {
      bytes = fromBase64(base64);
    } catch {
      throw badManifest(`module ${name} is not base64`);
    }
    let entry;
    if (type === "js") entry = { js: new TextDecoder().decode(bytes) };
    else if (type === "text") entry = { text: new TextDecoder().decode(bytes) };
    else if (type === "wasm") entry = { wasm: bytes };
    else if (type === "data") entry = { data: bytes };
    else throw badManifest(`module ${name} has unknown type ${JSON.stringify(type)}`);
    Object.defineProperty(modules, name, { value: entry, enumerable: true, writable: true, configurable: true });
  }
  if (typeof m.mainModule !== "string" || !modules[m.mainModule]?.js) {
    throw badManifest("mainModule is not one of its js modules");
  }
  return {
    compatibilityDate: m.compatibilityDate,
    compatibilityFlags: flags,
    mainModule: m.mainModule,
    modules,
    env,
    globalOutbound: bindings.EGRESS,
  };
}

function remember(id) {
  if (verified.size >= MAX_VERIFIED) verified.clear();
  verified.add(id);
}

const PROBE_CODE = {
  compatibilityDate: "2026-10-01",
  mainModule: "probe.js",
  modules: { "probe.js": 'export default { fetch() { return new Response("ok") } }' },
  env: {},
  globalOutbound: null,
};

export default {
  async fetch(request, bindings) {
    try {
      const match = BUNDLE_RE.exec(request.headers.get(BUNDLE_HEADER) ?? "");
      if (!match) throw new Refused(400, "x-chelabase-bundle must be {database_id}/{sha256}");
      const [, db, sha] = match;
      const envHeader = request.headers.get(ENV_HEADER) ?? "";
      const env = parseEnv(envHeader);
      const id = `${db}/${sha}:${await sha256(new TextEncoder().encode(envHeader))}`;
      let worker;
      if (verified.has(id)) {
        worker = bindings.LOADER.get(id, () => loadCode(bindings, db, sha, env));
      } else {
        const code = await loadCode(bindings, db, sha, env);
        worker = bindings.LOADER.get(id, async () => code);
        remember(id);
      }
      const headers = new Headers();
      for (const [k, v] of request.headers) {
        if (!k.toLowerCase().startsWith(PRIVATE_PREFIX)) headers.append(k, v);
      }
      try {
        return await worker.getEntrypoint().fetch(new Request(request, { headers }));
      } catch (e) {
        // An exception from the handler comes back marked `remote` and stays a 500. Anything
        // else is the loader failing to start the worker (a syntax error, an unknown
        // compatibility flag, a future compatibility date, a missing module, a top-level throw):
        // 502, without workerd's message, which quotes the member's code.
        if (e?.remote) throw e;
        throw new Refused(502, START_FAILED);
      }
    } catch (e) {
      // `x-chelabase-error` carries the reason to the functions service (its `last_error`); it
      // strips every x-chelabase-* header before the response reaches the caller.
      if (e instanceof Refused) {
        return new Response(`${e.message}\n`, { status: e.status, headers: { [ERROR_HEADER]: e.message } });
      }
      throw e;
    }
  },
};

// The watchdog's and the healthcheck's probe (loopback socket only): loads and runs a trivial
// worker through the loader, so it fails whenever the event loop is stuck.
export const probe = {
  async fetch(_request, bindings) {
    const worker = bindings.LOADER.get("__probe", async () => PROBE_CODE);
    return worker.getEntrypoint().fetch("http://probe/");
  },
};
