# The MCP sandbox runtime: workerd with one runner worker (runner.js) that runs model-written
# code, one fresh isolate per run, through the Worker Loader (experimental: `serve
# --experimental`). Stricter than the functions runtime: there is no egress service at all.
#
#   runner  the only worker; port 8080 for the `mcp` service (POST /run), 127.0.0.1:9090 for
#           the watchdog's probe. Its own outbound is `none`; it reaches only HOST.
#   host    the `mcp` service's internal listener (`mcp-internal:8096`, its compose address;
#           `--external-addr host=<host:port>` moves it). The runner calls it at two paths
#           only: GET /internal/spec and POST /internal/exec/{exec_id}/request.
#   none    no address at all.
#
# Loaded workers get `globalOutbound: null` from the runner (every fetch and connect throws) and
# an env with nothing but what runner.js lists.
using Workerd = import "/workerd/workerd.capnp";

const config :Workerd.Config = (
  services = [
    (name = "runner", worker = .runner),
    (name = "host", external = (address = "mcp-internal:8096", http = ())),
    (name = "none", network = (allow = [])),
  ],
  sockets = [
    (name = "http", address = "*:8080", http = (), service = "runner"),
    (name = "probe", address = "127.0.0.1:9090", http = (),
     service = (name = "runner", entrypoint = "probe")),
  ],
  # Each isolate's V8 heap stops at 128 MiB, well inside the container's memory limit. The
  # pinned workerd has no per-isolate memory limit for loaded workers and aborts the process
  # when any isolate reaches this cap; the watchdog exits with it and the restart policy starts
  # it again (one run's unbounded allocation costs every in-flight run, like a busy loop).
  # ArrayBuffer and typed-array memory sits outside the V8 heap and this cap doesn't count it:
  # only the container's memory limit ends an unbounded Uint8Array allocation (the kernel kills
  # workerd and the restart policy starts it again). Run the container with swap equal to memory
  # (compose: memswap_limit = mem_limit) so that limit is the whole bound.
  v8Flags = ["--max-old-space-size=128"],
  # What a request leaves behind in a long-lived isolate (Request, Response, streams, stubs) is a
  # few bytes each to V8 but 10-25 KiB of C++ memory, freed only by a major GC of that isolate.
  # Left alone V8 runs one only after thousands of requests or more, and the watchdog's probe is
  # a request every 2 s to two such isolates (the runner and the probe worker): an idle runtime
  # climbs to about 400 MiB in a day and a half. This gate (cloudflare/workerd#8496, in
  # 1.20261119.1, off by default while it rolls out) reports those objects' size to V8, so it
  # collects in time: about 70 MiB after 2000 runs, 115-130 MiB with the probes' share, and level
  # from there. workerd ignores a gate name it doesn't know, so tests/memory.test.mjs is what
  # says the line still works after a workerd bump; drop it once a release has it on by default.
  autogates = ["workerd-autogate-wrappable-external-memory"],
);

const runner :Workerd.Worker = (
  modules = [(name = "runner.js", esModule = embed "runner.js")],
  compatibilityDate = "2026-10-01",
  globalOutbound = "none",
  bindings = [
    (name = "LOADER", workerLoader = ()),
    (name = "HOST", service = "host"),
  ],
);
