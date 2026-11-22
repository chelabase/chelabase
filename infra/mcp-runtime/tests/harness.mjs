// Test harness for the MCP sandbox runtime: a stub host (what the `mcp` service's internal
// listener serves the runner: /internal/spec and /internal/exec/{exec_id}/request) and the
// runtime image in a throwaway container, hardened as compose will run it. The container
// reaches the stub as `mcp-internal` (the host gateway), so a sandbox escape that got through
// would reach a live server and show up in `requests`.
//
// MCP_RUNTIME_IMAGE picks the image (default: the bake target's local tag).
import { execFile } from "node:child_process";
import { createHash } from "node:crypto";
import { createServer } from "node:http";
import { promisify } from "node:util";

const run = promisify(execFile);
export const IMAGE = process.env.MCP_RUNTIME_IMAGE ?? "chelabase/mcp-runtime:latest";

export const sha256 = (text) => createHash("sha256").update(text).digest("hex");

// The stub host. `spec` is the JSON text /internal/spec serves; `requests` records every request
// as "METHOD /path" with its parsed JSON body; `reply` answers an exec callback
// ({status, headers, body} by default 200 with an echo of what the runner sent).
export async function startHost({ spec = { paths: {} } } = {}) {
  const host = {
    specText: JSON.stringify(spec),
    requests: [],
    reply: (sent) => ({ status: 200, headers: { "content-type": "application/json" }, body: { echo: sent } }),
  };
  const server = createServer((req, res) => {
    let raw = "";
    req.on("data", (c) => {
      raw += c;
    });
    req.on("end", () => {
      let body;
      try {
        body = raw === "" ? undefined : JSON.parse(raw);
      } catch {
        body = raw;
      }
      host.requests.push({ line: `${req.method} ${req.url}`, headers: req.headers, body });
      if (req.method === "GET" && req.url === "/internal/spec") {
        res.writeHead(200, { "content-type": "application/json" });
        res.end(host.specText);
        return;
      }
      if (req.method === "POST" && /^\/internal\/exec\/[^/]+\/request$/.test(req.url)) {
        res.writeHead(200, { "content-type": "application/json" });
        res.end(JSON.stringify(host.reply(body)));
        return;
      }
      res.writeHead(404);
      res.end("not here");
    });
  });
  await new Promise((resolve) => server.listen(0, "0.0.0.0", resolve));
  host.port = server.address().port;
  host.specHash = () => sha256(host.specText);
  host.close = () => new Promise((resolve) => server.close(resolve));
  return host;
}

export async function docker(...args) {
  const { stdout } = await run("docker", args, { maxBuffer: 16 << 20 });
  return stdout.trim();
}

// Starts the runtime pointed at the stub host. Extra names resolve to the host gateway too, so
// a fetch to them would reach the stub if the sandbox let it.
export const HOST_NAMES = ["mcp-internal", "mcp", "gateway", "metadata-db"];
export async function startRuntime(host, { restart = "no", memory = "512m" } = {}) {
  const hosts = HOST_NAMES.flatMap((n) => ["--add-host", `${n}:host-gateway`]);
  const id = await docker(
    "run", "-d", "--restart", restart,
    "--read-only", "--cap-drop", "ALL", "--security-opt", "no-new-privileges",
    "--memory", memory, "--pids-limit", "256",
    ...hosts, "-p", "127.0.0.1::8080",
    IMAGE,
    // The image's command, with the host moved to the stub's port.
    "/usr/local/bin/workerd", "serve", "--experimental",
    "--external-addr", `host=mcp-internal:${host.port}`,
    "/etc/mcp-runtime/config.capnp",
  );
  // Docker publishes the port only once the container runs (again after a restart).
  const hostPort = async (ms = 20_000) => {
    const deadline = Date.now() + ms;
    for (;;) {
      try {
        const port = (await docker("port", id, "8080/tcp")).split("\n")[0].split(":").pop()?.trim();
        if (port) return port;
      } catch (e) {
        if (Date.now() >= deadline) throw e;
      }
      if (Date.now() >= deadline) throw new Error(`container ${id} never published 8080/tcp`);
      await new Promise((r) => setTimeout(r, 200));
    }
  };
  const rt = {
    id,
    url: `http://127.0.0.1:${await hostPort()}`,
    refresh: async () => {
      rt.url = `http://127.0.0.1:${await hostPort()}`;
    },
    inspect: async () => JSON.parse(await docker("inspect", id))[0],
    logs: async () => {
      const { stdout, stderr } = await run("docker", ["logs", id], { maxBuffer: 16 << 20 });
      return stdout + stderr;
    },
    // Anonymous memory of the container's cgroup, in bytes (workerd's heap and its isolates).
    anonBytes: async () => {
      const stat = await docker("exec", id, "/usr/local/bin/busybox", "cat", "/sys/fs/cgroup/memory.stat");
      return Number(/^anon (\d+)$/m.exec(stat)[1]);
    },
    stop: () => docker("rm", "-f", id).catch(() => {}),
  };
  await waitReady(rt, 30_000);
  return rt;
}

// Any HTTP answer from the runner (GET / is a 404) means workerd is serving.
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

// POST /run. Returns {status, body} (body parsed as JSON when it is JSON).
export async function runCode(rt, req, { timeout = 15_000 } = {}) {
  const res = await fetch(new URL("/run", rt.url), {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: typeof req === "string" ? req : JSON.stringify(req),
    signal: AbortSignal.timeout(timeout),
  });
  const text = await res.text();
  let body;
  try {
    body = JSON.parse(text);
  } catch {
    body = text;
  }
  return { status: res.status, body };
}

let seq = 0;
// A /run request: search by default, with a fresh exec_id.
export function job(code, { kind = "search", host, timeout_ms = 5000, exec_id } = {}) {
  seq += 1;
  const req = { exec_id: exec_id ?? `exec_${process.pid}_${seq}`, kind, code, timeout_ms };
  if (kind === "search") req.spec_hash = host.specHash();
  return req;
}
