# The functions runtime: workerd with one supervisor worker (supervisor.js) that loads member
# bundles through the Worker Loader (an experimental workerd feature: `serve --experimental`).
#
#   supervisor  the only worker; port 8080 for the functions service, 127.0.0.1:9090 for the
#               watchdog's probe. Its own outbound is `none`: it reaches only BUNDLES.
#   bundles     the functions service's bundle store (`functions-internal:8080`, its alias on the
#               internal `functions` network;
#               `--external-addr bundles=<host:port>` moves it).
#   chelabase-api  the gateway (`gateway-functions:8000`, its alias on the `functions` network;
#               compose moves the port with `--external-addr chelabase-api=<host:port>`). Only
#               the supervisor's `chelabaseApi` entrypoint calls it, always on the API host
#               (CHELABASE_API_HOST); loaded workers reach it as `env.CHELABASE` (#183).
#   egress      the loaded workers' global outbound: public addresses only. DNS answers are
#               filtered per connection, so a name that resolves to a denied address fails.
#   none        no address at all.
using Workerd = import "/workerd/workerd.capnp";

const config :Workerd.Config = (
  services = [
    (name = "supervisor", worker = .supervisor),
    (name = "bundles", external = (address = "functions-internal:8080", http = ())),
    (name = "chelabase-api", external = (address = "gateway-functions:8000", http = ())),
    (name = "egress",
     network = (
       allow = ["public"],
       # "public" already excludes these; denied again by name so a change to its meaning can't
       # open them: private and local blocks, link-local (cloud metadata), CGNAT, "this network",
       # IPv4-mapped and NAT64 IPv6 (which can carry a private IPv4), ULA, benchmarking,
       # multicast and reserved.
       deny = ["private", "local", "unix", "unix-abstract",
               "0.0.0.0/8", "10.0.0.0/8", "100.64.0.0/10", "127.0.0.0/8", "169.254.0.0/16",
               "172.16.0.0/12", "192.0.0.0/24", "192.168.0.0/16", "198.18.0.0/15",
               "224.0.0.0/4", "240.0.0.0/4",
               "::/128", "::1/128", "::ffff:0:0/96", "64:ff9b::/96", "64:ff9b:1::/48",
               "fc00::/7", "fe80::/10", "ff00::/8"],
       tlsOptions = (trustBrowserCas = true))),
    (name = "none", network = (allow = [])),
  ],
  sockets = [
    (name = "http", address = "*:8080", http = (), service = "supervisor"),
    (name = "probe", address = "127.0.0.1:9090", http = (),
     service = (name = "supervisor", entrypoint = "probe")),
  ],
);

const supervisor :Workerd.Worker = (
  modules = [(name = "supervisor.js", esModule = embed "supervisor.js")],
  compatibilityDate = "2026-10-01",
  # Fetcher.scheduled() on a loaded worker's entrypoint (m7's scheduled runs).
  compatibilityFlags = ["service_binding_extra_handlers"],
  globalOutbound = "none",
  bindings = [
    (name = "LOADER", workerLoader = ()),
    (name = "BUNDLES", service = "bundles"),
    (name = "EGRESS", service = "egress"),
    (name = "GATEWAY", service = "chelabase-api"),
    (name = "API_HOST", fromEnvironment = "CHELABASE_API_HOST"),
  ],
);
