// Test harness for the functions runtime: a stub bundle store (what the functions service serves
// at /internal/bundles/{database_id}/{sha256}) and the runtime image in a throwaway container,
// hardened as compose runs it. The container reaches the stub as `functions` (the host gateway);
// the names of internal services resolve there too, so an egress check that gets through would
// reach a live server instead of failing on DNS.
//
// FUNCTIONS_RUNTIME_IMAGE picks the image (default: the bake target's local tag).
import { execFile } from "node:child_process";
import { createHash } from "node:crypto";
import { readdirSync, readFileSync } from "node:fs";
import { createServer } from "node:http";
import { join } from "node:path";
import { promisify } from "node:util";

const run = promisify(execFile);
export const IMAGE = process.env.FUNCTIONS_RUNTIME_IMAGE ?? "chelabase/functions-runtime:latest";
export const FIXTURES = new URL("../fixtures/", import.meta.url).pathname;
export const COMPAT_DATE = "2026-10-01";
// Names user code must not reach; each resolves to the host running the stub store.
export const INTERNAL_NAMES = ["functions", "metadata-db", "nats", "orchestrator"];

const TYPES = { ".js": "js", ".wasm": "wasm", ".txt": "text", ".bin": "data" };

// The manifest of a fixture dir: every file a module, main.js the main module. `extra` adds
// modules ({name: string|Uint8Array}) so a test can make a new hash of the same code.
export function manifest(fixture, extra = {}) {
  const dir = join(FIXTURES, fixture);
  const modules = readdirSync(dir)
    .sort()
    .map((name) => {
      const type = TYPES[name.slice(name.lastIndexOf("."))];
      if (!type) throw new Error(`fixture ${fixture}: no module type for ${name}`);
      return { name, type, base64: readFileSync(join(dir, name)).toString("base64") };
    });
  for (const [name, body] of Object.entries(extra)) {
    modules.push({ name, type: TYPES[name.slice(name.lastIndexOf("."))], base64: Buffer.from(body).toString("base64") });
  }
  return { compatibilityDate: COMPAT_DATE, compatibilityFlags: [], mainModule: "main.js", modules };
}

export const sha256 = (bytes) => createHash("sha256").update(bytes).digest("hex");

// The stub bundle store. add() serves a manifest under its own hash; addRaw() serves any bytes
// under any hash (a mismatch). `hits` counts requests per path; `other` collects any request
// that isn't a bundle fetch (an egress probe that got through).
export async function startBundleStore() {
  const bodies = new Map();
  const hits = new Map();
  const other = [];
  const server = createServer((req, res) => {
    hits.set(req.url, (hits.get(req.url) ?? 0) + 1);
    const body = bodies.get(req.url);
    if (body) {
      res.writeHead(200, { "content-type": "application/json" });
      res.end(body);
      return;
    }
    if (!req.url.startsWith("/internal/bundles/")) other.push(`${req.method} ${req.url}`);
    res.writeHead(404);
    res.end("no such bundle");
  });
  await new Promise((resolve) => server.listen(0, "0.0.0.0", resolve));
  const port = server.address().port;
  const addRaw = (db, sha, body) => {
    bodies.set(`/internal/bundles/${db}/${sha}`, Buffer.from(body));
    return sha;
  };
  return {
    port,
    hits,
    other,
    addRaw,
    add(db, m) {
      const body = Buffer.from(JSON.stringify(m));
      return addRaw(db, sha256(body), body);
    },
    close: () => new Promise((resolve) => server.close(resolve)),
  };
}

async function docker(...args) {
  const { stdout } = await run("docker", args, { maxBuffer: 16 << 20 });
  return stdout.trim();
}

// Starts the runtime pointed at the stub store. Returns its base URL and helpers.
export async function startRuntime(store, { restart = "no" } = {}) {
  const hosts = INTERNAL_NAMES.flatMap((n) => ["--add-host", `${n}:host-gateway`]);
  const id = await docker(
    "run", "-d", "--restart", restart,
    "--read-only", "--cap-drop", "ALL", "--security-opt", "no-new-privileges",
    "--memory", "512m", "--pids-limit", "256",
    ...hosts, "-p", "127.0.0.1::8080",
    IMAGE,
    // The image's command, with the bundle store moved to the stub's port.
    "/usr/local/bin/workerd", "serve", "--experimental",
    "--external-addr", `bundles=functions:${store.port}`,
    "/etc/functions-runtime/config.capnp",
  );
  const hostPort = async () => (await docker("port", id, "8080/tcp")).split("\n")[0].split(":").pop();
  const rt = {
    id,
    url: `http://127.0.0.1:${await hostPort()}`,
    // A restart may publish 8080 on a new host port.
    refresh: async () => {
      rt.url = `http://127.0.0.1:${await hostPort()}`;
    },
    inspect: async () => JSON.parse(await docker("inspect", id))[0],
    logs: async () => {
      const { stdout, stderr } = await run("docker", ["logs", id], { maxBuffer: 16 << 20 });
      return stdout + stderr;
    },
    stop: () => docker("rm", "-f", id).catch(() => {}),
  };
  await waitReady(rt, 30_000);
  return rt;
}

// Any HTTP answer from the supervisor (a request without a bundle header gets a 400) means
// workerd is serving.
export async function waitReady(rt, ms) {
  const deadline = Date.now() + ms;
  let last;
  while (Date.now() < deadline) {
    try {
      await fetch(rt.url, { signal: AbortSignal.timeout(1000) });
      return;
    } catch (e) {
      last = e;
      await new Promise((r) => setTimeout(r, 250));
    }
  }
  throw new Error(`runtime ${rt.id.slice(0, 12)} not serving after ${ms}ms: ${last}\n${await rt.logs()}`);
}

// Invokes a bundle the way the functions service will: the bundle and env headers.
export function invoke(rt, db, sha, { env, headers = {}, path = "/", timeout = 10_000 } = {}) {
  const h = { "x-chelabase-bundle": `${db}/${sha}`, ...headers };
  if (env !== undefined) h["x-chelabase-env"] = Buffer.from(JSON.stringify(env)).toString("base64");
  return fetch(new URL(path, rt.url), { headers: h, signal: AbortSignal.timeout(timeout) });
}
