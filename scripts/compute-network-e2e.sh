#!/usr/bin/env bash
# RT1 T1 (R11, P4, W14, S27): what a compute can reach, checked from a probe container attached
# like a compute (the stack's `compute` and `control` networks only). Runs in the stack's runner
# (`just compute-network-e2e`).
#
# 0. No route off the compute and control networks (compute-network-reach): the probe is attached
#    exactly like a compute (the two internal networks, the egress resolver, and a
#    host.docker.internal alias so the host name exists to be tried). It has no default route and
#    no IPv6 address, an outside name doesn't resolve (with the egress resolver and with none), and
#    a packet sent to the egress service's address for a destination outside the stack goes
#    nowhere. The gateway's listen port is read from the gateway container (the script fails when
#    it can't be read) and probed at every address a compute could try to reach the host by: the
#    gateway's default-network address, the default network's gateway address, the gateway address
#    of each network the probe joined (a probe it can't form, such as an isolated network's
#    missing gateway address, is reported as skipped, never counted as a pass), the host-gateway
#    address and host.docker.internal. These checks don't depend on the engine's routing settings.
#    Failure path: COMPUTE_NETWORK_E2E_COMPUTE_NET=<a network that is not internal> makes the
#    probe join it instead of the stack's compute network, and the no-default-route check fails.
# 1. Reachability: a compute can't reach NATS, Valkey, any database, the identity service, Mailpit, SeaweedFS,
#    the storage controller, the gateway, Studio or the orchestrator's management API (#318), by
#    name or by their default-network address; it reaches what it needs (pageserver, safekeeper,
#    the orchestrator's hooks listener).
# 2. Port audit (S27): every TCP listener of every container on the compute network (read from
#    each container's /proc/net/tcp{,6}, loopback excluded) must be in the table below, and each
#    is probed: it refuses a caller without a credential (and with the old public development
#    bearers), or it is public by design (health, Postgres password logins). An unlisted
#    listener fails the run. `compute-egress` is in the table: its resolver answers the
#    allow-listed names only, and each forwarded port is a listen port of an allow-list entry.
# 3. The worker's own network migration (compute-network-reach m2), live: the running worker's
#    reconcile pass, with the legacy names from its own CHELABASE_LEGACY_COMPUTE_NETWORKS. A
#    database is made through the API and a marker row written; its running compute is attached
#    to the first legacy network (created here, internal) with `docker network connect`, and a
#    stopped container that isn't a compute is left attached to the second. Within bounded
#    retries the worker removes that compute and a new one (another container id) comes up on
#    exactly the compute and control networks, the marker row reads back through the proxy, the
#    first legacy network is removed once empty, and the second is kept (the worker logs its
#    stopped member) and still starts that container. Both legacy networks must be absent before
#    the step; it removes everything it made on every exit path and checks they are gone.
#    COMPUTE_NETWORK_E2E_SKIP_MIGRATION=1 skips it and the run then ends "incomplete" with exit 1
#    unless COMPUTE_NETWORK_E2E_ALLOW_INCOMPLETE=1 is also set; KEEP=1 leaves its database.
# 4. hostssl (#248): on that database's compute, before the migration moves it, a password login
#    from the probe straight to the compute's address is refused with sslmode=disable (pg_hba, "no
#    encryption") and accepted with sslmode=require, over TLS. Skipped with the migration.
#
# Collects every failure, then exits 1 if there was one.
set -euo pipefail

LOG_PREFIX=compute-network-e2e
# shellcheck source=lib/neon-common.sh
. "$(dirname "$0")/lib/neon-common.sh"
project="${CHELABASE_COMPOSE_PROJECT:?set by chelabase workbench (stack.env)}"
image="${CHELABASE_WORKBENCH_IMAGE:?set by chelabase workbench (stack.env)}"
egress_ip="${CHELABASE_COMPUTE_EGRESS_IP:?set by chelabase workbench (stack.env)}"
compute_net="${COMPUTE_NETWORK_E2E_COMPUTE_NET:-${project}_compute-internal}"
control_net="${project}_control-internal"
failures=0
skipped_steps=""
say() { echo "[$LOG_PREFIX] $*"; }
fail() {
  echo "[$LOG_PREFIX] FAIL: $*" >&2
  failures=$((failures + 1))
}
ok() { say "ok   $*"; }

docker network inspect "$compute_net" >/dev/null 2>&1 || {
  echo "[$LOG_PREFIX] FAIL: the stack has no $compute_net network" >&2
  exit 1
}

# The gateway's listen port, from the gateway container's own setting (CHELABASE_GATEWAY_LISTEN is
# host:port); no fallback to the runner's environment.
gateway_container="$(docker ps -q --filter "label=com.docker.compose.project=$project" --filter "label=com.docker.compose.service=gateway" | head -n1)"
[[ -n "$gateway_container" ]] || {
  echo "[$LOG_PREFIX] FAIL: no running gateway container in project $project" >&2
  exit 1
}
gateway_listen="$(docker inspect -f '{{range .Config.Env}}{{println .}}{{end}}' "$gateway_container" |
  sed -n 's/^CHELABASE_GATEWAY_LISTEN=//p' | head -n1)"
gateway_port="${gateway_listen##*:}"
[[ "$gateway_port" =~ ^[0-9]+$ ]] || {
  echo "[$LOG_PREFIX] FAIL: can't read the gateway's listen port from its container (CHELABASE_GATEWAY_LISTEN='$gateway_listen')" >&2
  exit 1
}
say "the gateway listens on $gateway_port (read from its container)"

rand="$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')"
probe="chelabase-t1-probe-$rand"
tmp="$(mktemp -d)"
# The API, for section 3 (and its cleanup): neon-common's `api` calls it.
GATEWAY="${GATEWAY_URL:-${CHELABASE_ENTRY_API_URL:-}}"
# Section 3's state, for the cleanup.
mig_rand="$(hex 4)"
mig_label="chelabase.dev/network-e2e=$mig_rand"
mig_db=""
mig_foreign="cnr-legacy-foreign-$mig_rand"
mig_nets=()
cleanup() {
  local rc=$? n c status left=""
  trap - EXIT
  set +e
  docker rm -f "$probe" "$probe-nodns" "$probe-route" "$mig_foreign" >/dev/null 2>&1
  if [[ -n "$mig_db" && "${KEEP:-}" != 1 ]]; then
    status="$(api DELETE "/v1/databases/$mig_db")"
    [[ "$status" == 2* ]] || left="$left; deleting database $mig_db answered '$status'"
  fi
  for n in "${mig_nets[@]}"; do
    # Only a network this run made (its label), never one that was there before.
    [[ "$(docker network inspect -f "{{index .Labels \"chelabase.dev/network-e2e\"}}" "$n" 2>/dev/null)" == "$mig_rand" ]] || continue
    for c in $(docker network inspect -f '{{range $id, $_ := .Containers}}{{$id}} {{end}}' "$n" 2>/dev/null); do
      docker network disconnect -f "$n" "$c" >/dev/null 2>&1
    done
    docker network rm "$n" >/dev/null 2>&1
  done
  for n in "${mig_nets[@]}"; do
    docker network inspect "$n" >/dev/null 2>&1 && left="$left; the legacy network $n is still there"
  done
  rm -rf "$tmp"
  if [[ -n "$left" ]]; then
    echo "[$LOG_PREFIX] FAIL: cleanup:${left#;}" >&2
    rc=1
  fi
  exit "$rc"
}
trap 'exit 130' INT
trap 'exit 143' TERM
trap cleanup EXIT
# Like a compute: the compute network first, the control network joined after, the egress resolver
# (HostConfig.Dns), no extra capability; plus the host name alias a compute could try.
docker run -d --rm --name "$probe" --network "$compute_net" --dns "$egress_ip" \
  --add-host host.docker.internal:host-gateway --cap-drop ALL --entrypoint sleep "$image" 900 >/dev/null
docker network connect "$control_net" "$probe"

in_probe() { docker exec "$probe" "$@"; }
tcp_open() { in_probe timeout 3 bash -c "</dev/tcp/$1/$2" >/dev/null 2>&1; }
# status <method> <url> [curl args…]: the HTTP status the probe gets ("000" = no answer).
status() {
  local method="$1" url="$2"
  shift 2
  in_probe curl -sS -o /dev/null -w '%{http_code}' --max-time 5 -X "$method" "$@" "$url" 2>/dev/null || true
}
default_ip() {
  docker inspect -f "{{with index .NetworkSettings.Networks \"${project}_default\"}}{{.IPAddress}}{{end}}" \
    "$(docker ps -q --filter "label=com.docker.compose.project=$project" --filter "label=com.docker.compose.service=$1" | head -n1)" 2>/dev/null || true
}

# --- 0. no route off the compute and control networks ------------------------------------------
# Both networks are Docker `internal` with an isolated bridge (no host address): a container on them
# has no default route, so nothing depends on the engine's routing settings.
for n in "$compute_net" "$control_net"; do
  [[ "$(docker network inspect -f '{{.Internal}}' "$n")" == true ]] && ok "$n is internal" || fail "$n is not an internal network"
done
if in_probe awk '$2 == "00000000" && $8 == "00000000" { f = 1 } END { exit !f }' /proc/net/route; then
  fail "the probe has a default route: $(in_probe cat /proc/net/route | tr '\t\n' ' ' | cut -c1-200)"
else
  ok "the probe has no default route"
fi
# IPv6: no address beyond loopback and link-local, and no default route.
v6="$(in_probe awk '$6 != "lo" && $1 !~ /^fe80/ { print $1 }' /proc/net/if_inet6 2>/dev/null || true)"
[[ -z "$v6" ]] && ok "the probe has no IPv6 address (link-local aside)" || fail "the probe has an IPv6 address: $v6"
if in_probe awk '$1 == "00000000000000000000000000000000" && $2 == "00" && $10 != "lo" { f = 1 } END { exit !f }' /proc/net/ipv6_route 2>/dev/null; then
  fail "the probe has an IPv6 default route"
else
  ok "the probe has no IPv6 default route"
fi
# A name outside the stack doesn't resolve: with the egress resolver (the probe's), and with none.
if in_probe timeout 20 getent ahosts example.com >/dev/null 2>&1; then fail "example.com resolves in the probe (egress resolver set)"; else ok "an outside name doesn't resolve with the egress resolver"; fi
if docker run --rm --name "$probe-nodns" --network "$compute_net" --cap-drop ALL --entrypoint timeout "$image" 20 getent ahosts example.com >/dev/null 2>&1; then
  fail "example.com resolves with no resolver set"
else
  ok "an outside name doesn't resolve with no resolver set"
fi
# A packet routed through the egress service's address for a destination outside the stack goes
# nowhere: a throwaway container (with NET_ADMIN, to set the route a compute can't) sends to
# the egress address as its next hop; the forwarder doesn't forward packets.
docker run -d --rm --name "$probe-route" --network "$compute_net" --user 0 --cap-drop ALL --cap-add NET_ADMIN \
  --entrypoint sleep "$image" 60 >/dev/null
if docker exec "$probe-route" ip route add default via "$egress_ip" 2>/dev/null; then
  routed=""
  for dest in 1.1.1.1:443 192.88.99.1:5432 8.8.8.8:53; do
    docker exec "$probe-route" timeout 3 bash -c "</dev/tcp/${dest%%:*}/${dest#*:}" >/dev/null 2>&1 && routed="$routed $dest"
  done
  [[ -z "$routed" ]] && ok "nothing sent through the egress address (as next hop) reaches an outside address" ||
    fail "a packet routed through the egress address reached:$routed"
else
  fail "couldn't set a default route through $egress_ip in a throwaway container (it has NET_ADMIN and iproute2): the forwarding check can't be made"
fi
# Positive control for the check above: "no answer" must not just mean "no return path". The same
# container, with its route through the egress address in place, still reaches a peer on the
# compute network (the orchestrator's hooks listener).
if docker exec "$probe-route" timeout 3 bash -c "</dev/tcp/orchestrator/8083" >/dev/null 2>&1; then
  ok "control: the route container reaches orchestrator:8083 on the compute network"
else
  fail "control: the route container can't reach orchestrator:8083 on the compute network, so its silence towards outside addresses proves nothing"
fi
docker rm -f "$probe-route" >/dev/null 2>&1 || true

# The gateway's listen port at every address class a compute could try the host by. The control: a
# container on the default network does reach it (so the port is the right one).
gateway_ip="$(default_ip gateway)"
[[ -n "$gateway_ip" ]] || fail "can't read the gateway's address on ${project}_default"
if docker run --rm --network "${project}_default" --cap-drop ALL --entrypoint timeout "$image" 5 bash -c "</dev/tcp/$gateway_ip/$gateway_port" >/dev/null 2>&1; then
  ok "control: a container on the default network reaches the gateway at $gateway_ip:$gateway_port"
else
  fail "control: the gateway doesn't answer at $gateway_ip:$gateway_port from the default network (wrong port?)"
fi
formed_default_gw=0 formed_host_gw=0
# gw_probe <class> <address> <may-skip>: a probe that can't be formed is a failure, except where
# may-skip is "isolated" (the network's bridge is isolated, so the engine reports no gateway
# address): that one is said to be skipped. The return status is 0 when the probe was formed and
# 1 when it was not.
gw_probe() {
  if [[ ! "$2" =~ ^([0-9]{1,3}(\.[0-9]{1,3}){3}|[0-9a-fA-F:]*:[0-9a-fA-F:.]*)$ ]]; then
    if [[ "${3:-}" == isolated ]]; then
      say "skip: $1 (the isolated bridge has no gateway address: '${2:-}')"
    else
      fail "$1: the probe can't be formed (the engine reports no address: '${2:-}')"
    fi
    return 1
  fi
  if tcp_open "$2" "$gateway_port"; then
    fail "a compute reaches the gateway's port at $1 ($2:$gateway_port)"
  else
    ok "no answer on the gateway's port at $1 ($2:$gateway_port)"
  fi
}
network_gateway() { docker network inspect -f '{{range .IPAM.Config}}{{.Gateway}} {{end}}' "$1" | awk '{ print $1 }'; }
# The first host address of the network's IPv4 subnet (.1 of a /22 base): where a bridge gateway
# would sit, tried even when the engine reports none.
subnet_first_host() {
  docker network inspect -f '{{range .IPAM.Config}}{{.Subnet}} {{end}}' "$1" | tr ' ' '\n' |
    awk -F'[./]' 'NF == 5 { printf "%d.%d.%d.%d\n", $1, $2, $3, $4 + 1; exit }'
}
# Both gateway modes of an isolated bridge, as the engine stores them in the network's Options.
network_isolated() {
  [[ "$(docker network inspect -f '{{index .Options "com.docker.network.bridge.gateway_mode_ipv4"}}' "$1" 2>/dev/null)" == isolated &&
    "$(docker network inspect -f '{{index .Options "com.docker.network.bridge.gateway_mode_ipv6"}}' "$1" 2>/dev/null)" == isolated ]]
}
gw_probe "the gateway's default-network address" "$gateway_ip" && formed_default_gw=$((formed_default_gw + 1))
gw_probe "the default network's gateway address" "$(network_gateway "${project}_default")" && formed_default_gw=$((formed_default_gw + 1))
for n in $(docker inspect -f '{{range $n, $_ := .NetworkSettings.Networks}}{{$n}} {{end}}' "$probe"); do
  mode=""
  if network_isolated "$n"; then
    ok "$n is isolated (gateway_mode_ipv4 and gateway_mode_ipv6)"
    mode=isolated
  else
    fail "$n isn't an isolated bridge (gateway_mode_ipv4 and gateway_mode_ipv6 must both be isolated)"
  fi
  gw_probe "the gateway address of $n" "$(network_gateway "$n")" "$mode" || true
  first_host="$(subnet_first_host "$n")"
  [[ -n "$first_host" ]] || fail "$n has no IPv4 subnet to take the first host address from"
  gw_probe "the first host address of $n" "$first_host" || true
done
host_gateways="$(in_probe getent ahosts host.docker.internal | awk '{ print $1 }' | sort -u)"
[[ -n "$host_gateways" ]] || fail "the probe has no host.docker.internal alias (the host-gateway probe can't be formed)"
for host_gateway in $host_gateways; do
  gw_probe "the host-gateway address" "$host_gateway" && formed_host_gw=$((formed_host_gw + 1))
done
[[ "$formed_default_gw" -ge 2 ]] || fail "the default-network gateway probes weren't both formed ($formed_default_gw of 2)"
[[ "$formed_host_gw" -ge 1 ]] || fail "no host-gateway probe was formed"
if tcp_open host.docker.internal "$gateway_port"; then fail "a compute reaches the gateway's port by host.docker.internal"; else ok "no answer on the gateway's port at host.docker.internal"; fi

# --- 1. reachability ---------------------------------------------------------------------------
for target in nats:4222 nats:8222 valkey:6379 metadata-db:5432 storcon-db:5432 identity:8080 \
  mailpit:8025 mailpit:1025 seaweedfs:8333 storage-controller:1234 storage-broker:50051 "gateway:$gateway_port" studio:3000 \
  orchestrator:8081 orchestrator-api:8081; do
  host="${target%%:*}" port="${target#*:}"
  if tcp_open "$host" "$port"; then fail "a compute reaches $target"; else ok "no route to $target"; fi
  ip="$(default_ip "$host")"
  if [[ -n "$ip" ]]; then
    if tcp_open "$ip" "$port"; then fail "a compute reaches $host by its address $ip:$port"; else ok "no route to $host at $ip"; fi
  else
    say "skip: $host has no address on ${project}_default; the by-address probe isn't made"
  fi
done
for target in pageserver:6400 safekeeper1:5454 safekeeper2:5454 safekeeper3:5454 orchestrator:8083; do
  if tcp_open "${target%%:*}" "${target#*:}"; then ok "a compute reaches $target"; else fail "a compute can't reach $target (it needs it)"; fi
done
[[ "$(status GET http://orchestrator:8083/health)" == 200 ]] && ok "the hooks listener answers as orchestrator" ||
  fail "the hooks listener doesn't answer at http://orchestrator:8083 (CHELABASE_CONTROL_PLANE_URI)"

# --- 2. port audit -----------------------------------------------------------------------------
# The public development bearers of before S27: none may open anything.
dev_tokens=(chelabase-dev-rest-internal chelabase-dev-data-keys chelabase-dev-app-auth chelabase-dev-app-sessions
  chelabase-dev-proxy-api chelabase-dev-storcon-hook chelabase-dev-realtime-internal)
uuid=00000000-0000-4000-8000-000000000000
zero32=00000000000000000000000000000000
sha=0000000000000000000000000000000000000000000000000000000000000000

# refused <method> <url> <what> [curl args…]: no credential and every dev bearer get 401 or 403.
refused() {
  local method="$1" url="$2" what="$3" got t
  shift 3
  got="$(status "$method" "$url" "$@")"
  [[ "$got" == 401 || "$got" == 403 || "$got" == "${no_cred_also:-}" ]] || { fail "$what: $method $url without a credential answers $got"; return; }
  for t in "${dev_tokens[@]}"; do
    got="$(status "$method" "$url" -H "Authorization: Bearer $t" "$@")"
    [[ "$got" == 401 || "$got" == 403 ]] || { fail "$what: $method $url with the public bearer $t answers $got"; return; }
  done
  ok "$what refuses $method $url"
}
health_only() { # health_only <host:port> <what>
  [[ "$(status GET "http://$1/health")" == 200 ]] || fail "$2: /health doesn't answer 200"
  local got
  got="$(status GET "http://$1/")"
  [[ "$got" == 404 || "$got" == 401 ]] || fail "$2: / answers $got (only /health is public)"
  ok "$2 serves only /health unauthenticated"
}
# pg_needs_auth <host> <port> <what>: a startup without a password or token is refused.
pg_needs_auth() {
  local out
  out="$(in_probe env PGCONNECT_TIMEOUT=5 psql "host=$1 port=$2 user=cloud_admin dbname=postgres sslmode=disable" -w -Atc 'select 1' 2>&1 || true)"
  if grep -Eqi 'password|authentication|jwt|unauthori[sz]ed' <<<"$out"; then ok "$3 requires a credential"; else fail "$3 accepts a startup without a credential: $out"; fi
}

# The storage HTTP APIs keep /v1/status open (pageserver/src/http/routes.rs, safekeeper/src/http/routes.rs):
# public by design (health). Fork round 4 closed the profiling routes and /metrics (a `metrics`
# scope token or an admin/own-service token opens /metrics; profiles are admin only), so there are
# no known gaps left: every other route probed must refuse an unauthenticated caller (401/403).
known_gaps=()
storage_http_allowlist() { # storage_http_allowlist <host:port> <what>
  local got p
  got="$(status GET "http://$1/v1/status")"
  [[ "$got" == 200 ]] && ok "$2: /v1/status is public by design" || fail "$2: /v1/status answers $got"
  for p in "${known_gaps[@]}"; do
    got="$(status GET "http://$1$p?seconds=0")"
    [[ "$got" == 401 || "$got" == 403 ]] && ok "$2: $p is refused (the known gap is closed; drop it from known_gaps)" ||
      say "KNOWN GAP: $2 serves $p unauthenticated ($got)"
  done
  for p in /profile/cpu /profile/heap /metrics; do
    got="$(status GET "http://$1$p?seconds=0")"
    [[ "$got" == 401 || "$got" == 403 ]] && ok "$2: $p is refused without a credential" ||
      fail "$2: $p answers $got without a credential (not an allowlisted gap)"
  done
  for p in /v1/tenant /v1/utilization; do
    got="$(status GET "http://$1$p")"
    [[ "$got" == 401 || "$got" == 403 || "$got" == 404 ]] || fail "$2: $p answers $got without a credential (not an allowlisted gap)"
  done
}

audit() { # audit <service> <port>: the check for one listener; returns 1 when it isn't in the table
  local svc="$1" port="$2"
  case "$svc:$port" in
    # The management API (#318) binds its default-network alias and loopback only, so from the
    # compute and control networks nothing answers: by name, and at the orchestrator's address on
    # each of them. (Its hooks listener, 8083, is the positive control.)
    orchestrator:8081)
      local oip net
      if tcp_open orchestrator 8081; then fail "a compute reaches the management API at orchestrator:8081"; else ok "orchestrator:8081 (the management API) isn't reachable by name"; fi
      for net in "$compute_net" "$control_net"; do
        oip="$(docker inspect -f "{{with index .NetworkSettings.Networks \"$net\"}}{{.IPAddress}}{{end}}" "$name")"
        if [[ -z "$oip" ]]; then
          fail "the orchestrator has no address on $net (the by-address probe can't be made)"
        elif tcp_open "$oip" 8081; then
          fail "the management API answers on the orchestrator's $net address $oip"
        else
          ok "the management API isn't reachable at the orchestrator's $net address $oip"
        fi
      done ;;
    orchestrator:8083)
      refused GET "http://orchestrator:8083/compute/api/v2/computes/ep-$zero32/spec" "the spec hook"
      refused PATCH "http://orchestrator:8083/compute/api/v2/computes/ep-$zero32/ddl" "the DDL hook"
      refused GET "http://orchestrator:8083/proxy/api/v1/get_endpoint_access_control?endpointish=ep-x&role=x" "the proxy API"
      refused PUT http://orchestrator:8083/storcon/notify-attach "the storage controller hook" -H 'Content-Type: application/json' -d '{}'
      refused GET "http://orchestrator:8083/internal/data/keys/$sha" "the data-key lookup"
      refused GET "http://orchestrator:8083/internal/data/branches/$uuid/connection" "the connection route"
      refused POST http://orchestrator:8083/internal/signup/check "the sign-up gate" -H 'Content-Type: application/json' -d '{}'
      refused POST http://orchestrator:8083/internal/signup/claimed "the setup claim" -H 'Content-Type: application/json' -d '{}'
      refused GET "http://orchestrator:8083/internal/app/sessions/$uuid" "session liveness"
      refused POST http://orchestrator:8083/internal/app/sync "the app-users sync" -d '{}' ;;
    worker:8082) health_only worker:8082 "worker:8082" ;;
    rest:8085) refused POST http://rest:8085/data/v1/query "the data API" -H 'Content-Type: application/json' -d '{}' ;;
    rest:8086)
      refused GET "http://rest:8086/internal/schema/$uuid" "rest's internal schema route"
      refused GET "http://rest:8086/internal/types/$uuid?role=authenticated" "rest's internal types route"
      refused POST "http://rest:8086/internal/console/$uuid/query" "rest's internal console route" -d '{}' ;;
    # Studio's sign-in backend: public by design (it is what the gateway serves to anyone), but
    # ruling (e): a compute can't pick its rate-limit key through X-Forwarded-For. 61 reads of
    # the instance info (60 a minute per address), each claiming a new address: the limit still
    # applies to the compute's own address.
    auth:8084)
      local i got limited=""
      for i in $(seq 61); do
        got="$(status GET http://auth:8084/auth/v1/instance -H "X-Forwarded-For: 198.51.100.$i")"
        [[ "$got" == 429 ]] && { limited=1; break; }
      done
      [[ -n "$limited" ]] && ok "auth:8084 is public by design and ignores a compute's X-Forwarded-For" ||
        fail "auth:8084 trusts X-Forwarded-For from a compute (61 spoofed addresses, never rate-limited)" ;;
    # Postgres wire and serverless logins: public by design, password-authenticated.
    # (Postgres wire 4432 and serverless 4444; any other proxy port stays unclassified.)
    proxy:*)
      [[ "$port" == 4432 || "$port" == 4444 ]] || return 1
      ok "proxy:$port is public by design (password logins)" ;;
    pageserver:9898)
      refused GET http://pageserver:9898/v1/tenant "the pageserver's management API"
      storage_http_allowlist pageserver:9898 "the pageserver's HTTP API" ;;
    pageserver:6400) pg_needs_auth pageserver 6400 "the pageserver's page service" ;;
    safekeeper[123]:7676)
      refused GET "http://$svc:7676/v1/tenant/$zero32/timeline/$zero32" "$svc's HTTP API"
      storage_http_allowlist "$svc:7676" "$svc's HTTP API" ;;
    safekeeper[123]:5454) pg_needs_auth "$svc" 5454 "$svc's WAL service" ;;
    realtime:8087)
      local got
      got="$(status GET http://realtime:8087/v1/realtime/socket -H 'Connection: Upgrade' -H 'Upgrade: websocket' \
        -H 'Sec-WebSocket-Version: 13' -H 'Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==')"
      [[ "$got" != 101 && "$got" != 000 ]] && ok "realtime:8087 refuses a socket without a ticket ($got)" ||
        fail "realtime:8087: a socket without a ticket answers $got" ;;
    realtime:8088) health_only realtime:8088 "realtime:8088" ;;
    # Object storage's public API (#153): every key route needs the gateway's identity token
    # (X-Chelabase-Identity; a forged one is refused too); the keyless signed-URL routes need a
    # valid URL token. Only /health answers without one.
    storage:8089)
      health_only storage:8089 "storage:8089"
      refused GET http://storage:8089/storage/v1/bucket "storage's bucket list"
      refused GET http://storage:8089/storage/v1/bucket "storage's bucket list (a forged identity)" -H 'X-Chelabase-Identity: eyJhbGciOiJFZERTQSJ9.e30.AAAA'
      refused POST http://storage:8089/storage/v1/object/files/x.txt "storage's upload" -H 'Content-Type: text/plain' -d x
      refused POST http://storage:8089/storage/v1/object/resolve "storage's ref resolve" -H 'Content-Type: application/json' -d '{"refs":[]}'
      refused GET http://storage:8089/storage/v1/object/sign/files/x.txt "storage's signed download (no token)"
      refused GET "http://storage:8089/storage/v1/object/sign/files/x.txt?token=bad" "storage's signed download (a bad token)"
      refused PUT "http://storage:8089/storage/v1/object/upload/sign/files/x.txt?token=bad" "storage's signed upload (a bad token)" -H 'Content-Type: text/plain' -d x ;;
    # Its console listener: the orchestrator's bearer (tokens/storage-internal.token) first, before
    # the caller header is read, so a forged caller without the bearer is refused.
    storage:8090)
      local caller='X-Chelabase-Console: {"owner":true,"database":"x","branch_id":"'"$uuid"'"}'
      refused GET http://storage:8090/storage/v1/bucket "storage's console bucket list"
      refused GET http://storage:8090/storage/v1/bucket "storage's console bucket list (a forged caller)" -H "$caller"
      refused POST http://storage:8090/storage/v1/object/files/x.txt "storage's console upload" -H "$caller" -H 'Content-Type: text/plain' -d x
      refused POST http://storage:8090/storage/v1/object/sign/files/x.txt "storage's console signing" -H "$caller" -H 'Content-Type: application/json' -d '{"expires_in":60}' ;;
    # The functions service joins the compute network only to dial computes as its dispatch login
    # (edge functions m13b, #186). Its listeners bind addresses on other networks (public 8091 on
    # the default network, the internal 8080 on `functions` and `functions-control`), so from the
    # compute network nothing answers: prove it with a connection to its compute address.
    functions:8080 | functions:8091)
      local fip
      fip="$(docker inspect -f "{{with index .NetworkSettings.Networks \"$compute_net\"}}{{.IPAddress}}{{end}}" "$name")"
      if [[ -z "$fip" ]]; then
        fail "functions has no address on the compute network"
      elif tcp_open "$fip" "$port"; then
        fail "functions:$port answers on its compute-network address $fip"
      else
        ok "functions:$port isn't reachable from the compute network (bound on other networks only)"
      fi ;;
    compute:5432) ok "compute:5432 is public by design (password logins)" ;;
    # compute_ctl's external API answers 401 (with WWW-Authenticate: Bearer) to a request without
    # a bearer, on every route, /metrics and /autoscaling_metrics included (K3/K4). Its internal
    # server listens on loopback TCP only: a listener on :3081 is not in this table,
    # so the run fails on it.
    compute:3080)
      local p got
      for p in /status /metrics /autoscaling_metrics; do
        got="$(status GET "http://$compute_host:3080$p")"
        [[ "$got" == 401 ]] || fail "compute_ctl's external API: GET $p without a credential answers $got, want 401"
        refused GET "http://$compute_host:3080$p" "compute_ctl's external API"
      done ;;
    # compute-egress (compute-network-reach): the resolver on 53 answers the allow-listed names
    # with its own address and nothing else; every other listener is the listen port of one allow-list
    # entry (an unlisted port has no listener at all). That each forwarded port reaches only its
    # listed destination is proved by scripts/compute-egress-e2e.sh, which has real destinations.
    compute-egress:*)
      local allow entry listen name_ ips="" listens=" " resolved
      allow="$(docker inspect -f '{{range .Config.Env}}{{println .}}{{end}}' "$name" | sed -n 's/^CHELABASE_COMPUTE_EGRESS_ALLOW=//p' | head -n1)"
      for entry in ${allow//,/ }; do
        entry="${entry#*=}"
        if [[ "$entry" == *@* ]]; then listen="${entry##*@}"; entry="${entry%@*}"; else listen="${entry##*:}"; fi
        listens+="$listen "
        name_="${entry%:*}"
        # host.docker.internal is in the probe's /etc/hosts, so it would resolve without the resolver.
        [[ "$name_" == host.docker.internal || "$name_" =~ ^[0-9.]+$ ]] || ips+="$name_ "
      done
      if [[ "$port" == 53 ]]; then
        egress53_audited=1
        [[ -n "${ips// /}" ]] || fail "compute-egress:53: the allow-list has no name to use as a positive control"
        for name_ in $ips; do
          resolved="$(in_probe getent hosts "$name_" | awk 'NR == 1 { print $1 }')"
          [[ "$resolved" == "$egress_ip" ]] && ok "compute-egress:53 answers the listed name $name_ with its own address" ||
            fail "compute-egress:53 answers the listed name $name_ with '${resolved:-nothing}' (wanted $egress_ip)"
        done
        if in_probe timeout 20 getent hosts "unlisted-$rand.example" >/dev/null 2>&1; then
          fail "compute-egress:53 answers a name that isn't in the allow-list"
        else
          ok "compute-egress:53 doesn't answer a name outside the allow-list"
        fi
      elif [[ "$listens" == *" $port "* ]]; then
        ok "compute-egress:$port is a forwarder listener of an allow-list entry"
      else
        return 1
      fi ;;
    *) return 1 ;;
  esac
}

# The egress container must exist and sit on the compute network at $egress_ip, or the egress
# checks below (no listener, an outside name doesn't resolve) would pass with nothing there.
egress_container="$(docker ps -q --filter "label=com.docker.compose.project=$project" --filter "label=com.docker.compose.service=compute-egress" | head -n1)"
egress_listens=""
if [[ -z "$egress_container" ]]; then
  fail "no running compute-egress container in project $project (the egress checks would pass vacuously)"
else
  egress_member="$(docker inspect -f "{{with index .NetworkSettings.Networks \"$compute_net\"}}{{.IPAddress}}{{end}}" "$egress_container" 2>/dev/null || true)"
  if [[ "$egress_member" == "$egress_ip" ]]; then
    ok "compute-egress is a member of $compute_net at $egress_ip"
  else
    fail "compute-egress isn't on $compute_net at $egress_ip (its address there: '${egress_member:-none}')"
  fi
  egress_allow="$(docker inspect -f '{{range .Config.Env}}{{println .}}{{end}}' "$egress_container" | sed -n 's/^CHELABASE_COMPUTE_EGRESS_ALLOW=//p' | head -n1)"
  [[ -n "$egress_allow" ]] || fail "compute-egress has an empty allow-list: the positive controls can't be made"
  # Positive control: the first entry's listen port accepts a connection on the egress address.
  first_entry="${egress_allow%%,*}"
  first_entry="${first_entry#*=}"
  if [[ "$first_entry" == *@* ]]; then egress_listens="${first_entry##*@}"; else egress_listens="${first_entry##*:}"; fi
  if [[ -n "$egress_listens" ]] && tcp_open "$egress_ip" "$egress_listens"; then
    ok "control: $egress_ip:$egress_listens (an allow-list entry's listen port) accepts a connection"
  else
    fail "control: nothing answers on $egress_ip:${egress_listens:-?} (an allow-list entry's listen port)"
  fi
fi
egress53_audited=""

containers="$(docker network inspect "$compute_net" -f '{{range $id, $c := .Containers}}{{$id}} {{end}}')"
listed=0
for id in $containers; do
  name="$(docker inspect -f '{{.Name}}' "$id" | tr -d /)"
  [[ "$name" == "$probe" ]] && continue
  svc="$(docker inspect -f '{{index .Config.Labels "com.docker.compose.service"}}' "$id" 2>/dev/null || true)"
  compute_host=""
  if [[ -n "$(docker inspect -f '{{index .Config.Labels "chelabase.dev/endpoint-id"}}' "$id" 2>/dev/null || true)" ]]; then
    svc=compute
    compute_host="$(docker inspect -f "{{with index .NetworkSettings.Networks \"$compute_net\"}}{{.IPAddress}}{{end}}" "$id")"
  fi
  # This run's own runner container has no listeners worth auditing beyond what the table lists.
  [[ -n "$svc" ]] || { fail "$name on the compute network is neither a stack service nor a compute"; continue; }
  # Loopback is all of 127.0.0.0/8 (Docker's embedded DNS listens on 127.0.0.11 in every
  # container on a user network), ::1 and ::ffff:127.0.0.0/104. /proc/net/tcp{,6} print each
  # 32-bit word in host (little-endian) order, so 127.x.y.z ends in "7F".
  ports="$(docker exec "$id" cat /proc/net/tcp /proc/net/tcp6 2>/dev/null |
    awk '$4 == "0A" { split($2, a, ":"); ip = toupper(a[1])
      if (length(ip) == 8 && ip ~ /7F$/) next
      if (ip == "00000000000000000000000001000000") next
      if (length(ip) == 32 && ip ~ /^0000000000000000FFFF0000/ && ip ~ /7F$/) next
      print a[2] }' |
    sort -u)" || { fail "$name: can't read its listeners"; continue; }
  for hex in $ports; do
    port=$((16#$hex))
    listed=$((listed + 1))
    audit "$svc" "$port" || fail "$svc ($name) listens on $port, which the audit table doesn't classify (auth or public by design)"
  done
done
# A port that is no entry's listen port has no listener on the egress address.
for p in 22 80 443 5433 8080 15434; do
  if tcp_open "$egress_ip" "$p"; then fail "compute-egress listens on $p (no allow-list entry has it)"; else ok "no listener on $egress_ip:$p"; fi
done
[[ -n "$egress53_audited" ]] || fail "the audit never classified compute-egress:53 (no listener of the egress container was found on $compute_net)"
[[ "$listed" -gt 0 ]] || fail "no listener found on the compute network (the audit read nothing)"

# --- 4. computes take non-loopback password logins over TLS only (#248) -------------------------
# hostssl_check <compute container> <password> <dbname>: from the probe (another container on the
# compute network, attached like a compute), a password login straight to the compute's address
# with a valid password is refused by pg_hba with sslmode=disable and accepted with
# sslmode=require, over TLS (pg_stat_ssl).
hostssl_check() {
  local cid="$1" pw="$2" dbname="$3" addr out
  addr="$(docker inspect -f "{{with index .NetworkSettings.Networks \"$compute_net\"}}{{.IPAddress}}{{end}}" "$cid")"
  [[ -n "$addr" ]] || { fail "hostssl: compute ${cid:0:12} has no address on $compute_net"; return; }
  out="$(in_probe env PGPASSWORD="$pw" PGCONNECT_TIMEOUT=10 psql \
    "host=$addr port=5432 user=postgres dbname=$dbname sslmode=disable" -w -Atc 'select 1' 2>&1 || true)"
  if grep -q 'pg_hba.conf' <<<"$out" && grep -q 'no encryption' <<<"$out"; then
    ok "hostssl: a plaintext password login to the compute ($addr) is refused by pg_hba"
  else
    fail "hostssl: a plaintext password login to the compute ($addr) wasn't refused by pg_hba: $out"
  fi
  out="$(in_probe env PGPASSWORD="$pw" PGCONNECT_TIMEOUT=10 psql \
    "host=$addr port=5432 user=postgres dbname=$dbname sslmode=require" -w -Atc \
    'select ssl from pg_stat_ssl where pid = pg_backend_pid()' 2>&1 || true)"
  if [[ "$out" == t ]]; then
    ok "hostssl: the same login with sslmode=require is accepted, over TLS"
  else
    fail "hostssl: the same login with sslmode=require wasn't accepted over TLS: $out"
  fi
}

# --- 3. the worker's network migration ---------------------------------------------------------
migration() {
  local worker legacy stack w1 w2 since status ep host pw dbname compute old_id new_id nets want i
  local proxy_port="${CHELABASE_PROXY_PG_PORT:-4432}"
  local proxy_hostaddr="${CHELABASE_PROXY_HOSTADDR:?run through just workbench run}"
  local ca="${CHELABASE_PROXY_CA_FILE:?run through just workbench run}"
  local tenant=00000000-0000-4000-8000-000000000001 project_id=00000000-0000-4000-8000-000000000002
  [[ -x "${CHELABASE_CLI:-}" && -n "$GATEWAY" ]] || { fail "migration: CHELABASE_CLI or the API URL is unset (run through just workbench run)"; return; }
  worker="$(docker ps -q --filter "label=com.docker.compose.project=$project" --filter "label=com.docker.compose.service=worker" | head -n1)"
  [[ -n "$worker" ]] || { fail "migration: no running worker"; return; }
  # The legacy names the running worker migrates: its own setting, not the runner's.
  legacy="$(docker inspect -f '{{range .Config.Env}}{{println .}}{{end}}' "$worker" | sed -n 's/^CHELABASE_LEGACY_COMPUTE_NETWORKS=//p' | head -n1)"
  IFS=, read -r w1 w2 _ <<<"${legacy// /}"
  stack="$(docker inspect -f '{{range .Config.Env}}{{println .}}{{end}}' "$worker" | sed -n 's/^CHELABASE_STACK=//p' | head -n1)"
  [[ -n "$stack" ]] || { fail "migration: the worker has no CHELABASE_STACK"; return; }
  [[ -n "$w1" && -n "$w2" ]] || { fail "migration: the worker's CHELABASE_LEGACY_COMPUTE_NETWORKS names fewer than two networks ('$legacy')"; return; }
  for n in "$w1" "$w2"; do
    docker network inspect "$n" >/dev/null 2>&1 && { fail "migration: the legacy network $n already exists; not touching it"; return; }
  done
  ok "the worker migrates $w1 and $w2 (its CHELABASE_LEGACY_COMPUTE_NETWORKS)"
  mig_nets=("$w1" "$w2")

  # A database, its compute, and a marker row.
  status="$(api POST /v1/databases "$(jq -nc --arg t "$tenant" --arg p "$project_id" --arg n "netmig_$mig_rand" '{tenant_id: $t, project_id: $p, name: $n}')")"
  [[ "$status" == 202 || "$status" == 201 ]] || { fail "migration: POST /v1/databases: $status $(cat "$tmp/body")"; return; }
  mig_db="$(jq -r '.database_id' "$tmp/body")"
  for i in $(seq 120); do
    status="$(api GET "/v1/databases/$mig_db")"
    [[ "$status" == 200 && "$(jq -r '.status' "$tmp/body")" == ready ]] && break
    [[ "$(jq -r '.status' "$tmp/body" 2>/dev/null)" != failed && $i -lt 120 ]] || { fail "migration: database $mig_db never became ready: $(cat "$tmp/body")"; return; }
    sleep 1
  done
  status="$(api GET "/v1/databases/$mig_db/connection")"
  [[ "$status" == 200 ]] || { fail "migration: GET /connection: $status"; return; }
  pw="$(jq -r '.password' "$tmp/body")" host="$(jq -r '.host' "$tmp/body")" dbname="$(jq -r '.database' "$tmp/body")"
  ep="${host%%.*}"
  db_sql() {
    PGPASSWORD="$pw" PGCONNECT_TIMEOUT=10 timeout 60 psql \
      "host=$host hostaddr=$proxy_hostaddr port=$proxy_port user=postgres dbname=$dbname sslmode=verify-full sslrootcert=$ca" \
      -v ON_ERROR_STOP=1 -qtA "$@"
  }
  for i in $(seq 60); do
    db_sql -c "CREATE TABLE IF NOT EXISTS netmig (v text); INSERT INTO netmig VALUES ('$mig_rand')" >/dev/null 2>"$tmp/err" && break
    [[ $i -lt 60 ]] || { fail "migration: the compute never answered: $(cat "$tmp/err")"; return; }
    sleep 2
  done
  old_id="$(docker ps -q --no-trunc --filter "label=chelabase.dev/endpoint-id=$ep" --filter "label=chelabase.dev/stack=$stack" | head -n1)"
  [[ -n "$old_id" ]] || { fail "migration: no running compute for $ep"; return; }
  ok "database $mig_db's compute ${old_id:0:12} answers and holds a marker row"

  hostssl_check "$old_id" "$pw" "$dbname"

  # Attach it to the first legacy network. A worker pass can remove the empty network between the
  # create and the connect: retry the pair.
  since="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  for i in $(seq 5); do
    docker network inspect "$w1" >/dev/null 2>&1 || docker network create --internal --label "$mig_label" "$w1" >/dev/null 2>&1
    docker network connect "$w1" "$old_id" 2>/dev/null && break
    [[ $i -lt 5 ]] || { fail "migration: can't attach the compute to $w1"; return; }
  done
  # A stopped container that isn't a compute, attached to the second (it ran and exited).
  for i in $(seq 5); do
    docker network inspect "$w2" >/dev/null 2>&1 || docker network create --internal --label "$mig_label" "$w2" >/dev/null 2>&1
    docker rm -f "$mig_foreign" >/dev/null 2>&1
    docker run --name "$mig_foreign" --network "$w2" --label "$mig_label" --cap-drop ALL --entrypoint true "$image" >/dev/null 2>&1 && break
    [[ $i -lt 5 ]] || { fail "migration: can't leave a stopped container on $w2"; return; }
  done
  [[ "$(docker inspect -f '{{.State.Status}}' "$mig_foreign")" == exited ]] || fail "migration: $mig_foreign isn't stopped"
  ok "compute ${old_id:0:12} is on $w1 too, and the stopped $mig_foreign is on $w2"

  # The worker's pass (every 30 s) removes the compute; the reconcile creates it again.
  for i in $(seq 90); do
    docker inspect "$old_id" >/dev/null 2>&1 || break
    [[ $i -lt 90 ]] || { fail "migration: the worker didn't remove compute ${old_id:0:12} within 180 s"; return; }
    sleep 2
  done
  ok "the worker removed compute ${old_id:0:12} (on a legacy network)"
  for i in $(seq 90); do
    new_id="$(docker ps -q --no-trunc --filter "label=chelabase.dev/endpoint-id=$ep" --filter "label=chelabase.dev/stack=$stack" --filter status=running | head -n1)"
    [[ -z "$new_id" ]] || break
    [[ $i -lt 90 ]] || { fail "migration: no compute for $ep came back within 180 s (no query was sent to wake it)"; return; }
    sleep 2
  done
  [[ "$new_id" != "$old_id" ]] && ok "the reconcile created compute ${new_id:0:12} again, without a query to wake it" || fail "migration: the compute kept its container id"
  nets="$(docker inspect -f '{{range $n, $_ := .NetworkSettings.Networks}}{{$n}} {{end}}' "$new_id" | tr ' ' '\n' | sed '/^$/d' | sort | tr '\n' ' ')"
  want="$(printf '%s\n' "${project}_compute-internal" "$control_net" | sort | tr '\n' ' ')"
  [[ "$nets" == "$want" ]] && ok "the new compute is on exactly [$nets]" || fail "migration: the new compute is on [$nets], want [$want]"
  local marker=""
  for i in $(seq 30); do
    if [[ "$(db_sql -c 'SELECT v FROM netmig' 2>"$tmp/err")" == "$mig_rand" ]]; then
      marker=1
      break
    fi
    sleep 2
  done
  [[ -n "$marker" ]] && ok "its database answers SQL and still has the marker row" ||
    fail "migration: the marker row doesn't read back: $(cat "$tmp/err")"

  # The first legacy network goes once empty; the second stays for its stopped member.
  for i in $(seq 45); do
    docker network inspect "$w1" >/dev/null 2>&1 || break
    [[ $i -lt 45 ]] || { fail "migration: $w1 is still there 90 s after its compute left"; break; }
    sleep 2
  done
  worker_log() { docker logs --since "$since" "$worker" 2>&1 | sed $'s/\x1b\\[[0-9;]*m//g'; }
  if ! docker network inspect "$w1" >/dev/null 2>&1; then
    worker_log | grep -F 'removed legacy network' | grep -qF "network=$w1" && ok "the worker removed $w1 once empty (logged)" ||
      fail "migration: $w1 is gone, but the worker's log has no 'removed legacy network' for it"
  fi
  for i in $(seq 45); do
    worker_log | grep -F 'legacy network still has members; keeping it' | grep -F "network=$w2" | grep -qF "$mig_foreign" && break
    [[ $i -lt 45 ]] || { fail "migration: the worker never logged keeping $w2 for $mig_foreign"; break; }
    sleep 2
  done
  if docker network inspect "$w2" >/dev/null 2>&1; then
    ok "the worker kept $w2 for its stopped member $mig_foreign"
    docker start -a "$mig_foreign" >/dev/null 2>&1 && ok "$mig_foreign still starts on $w2" || fail "migration: $mig_foreign doesn't start on $w2"
  else
    fail "migration: the worker removed $w2, which a stopped container is attached to"
  fi
}
if [[ "${COMPUTE_NETWORK_E2E_SKIP_MIGRATION:-}" == 1 ]]; then
  say "skip: the worker's network migration (COMPUTE_NETWORK_E2E_SKIP_MIGRATION=1)"
  skipped_steps="the worker's network migration"
else
  migration
fi

if [[ "$failures" -gt 0 ]]; then
  echo "[$LOG_PREFIX] $failures failure(s)" >&2
  exit 1
fi
if [[ -n "$skipped_steps" ]]; then
  if [[ "${COMPUTE_NETWORK_E2E_ALLOW_INCOMPLETE:-}" == 1 ]]; then
    say "incomplete (allowed by COMPUTE_NETWORK_E2E_ALLOW_INCOMPLETE=1): skipped $skipped_steps; not a full run"
    exit 0
  fi
  echo "[$LOG_PREFIX] incomplete: skipped $skipped_steps; this is not a full run (set COMPUTE_NETWORK_E2E_ALLOW_INCOMPLETE=1 to accept it)" >&2
  exit 1
fi
say "ok"
