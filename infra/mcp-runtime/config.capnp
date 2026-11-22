# The MCP sandbox runtime: workerd with one runner worker (runner.js) that runs model-written
# code, one fresh isolate per run, through the Worker Loader (experimental: `serve
# --experimental`). Stricter than the functions runtime: there is no egress service at all.
#
#   runner  the only worker; port 8080 for the `mcp` service (POST /run), 127.0.0.1:9090 for
#           the watchdog's probe. Its own outbound is `none`; it reaches only HOST.
#   host    the `mcp` service's internal listener (`mcp-internal:8080`;
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
    (name = "host", external = (address = "mcp-internal:8080", http = ())),
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
  v8Flags = ["--max-old-space-size=128"],
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
