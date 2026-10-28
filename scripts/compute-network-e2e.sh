#!/usr/bin/env bash
# RT1 T1 (R11, P4, W14, S27): what a compute can reach, checked from a probe container attached
# like a compute (the stack's `compute` and `control` networks only). Runs in the stack's runner
# (`just compute-network-e2e`).
#
# 1. Reachability: a compute can't reach NATS, Valkey, any database, Zitadel, Mailpit, SeaweedFS,
#    the storage controller, the gateway or Studio, by name or by their default-network address;
#    it reaches what it needs (pageserver, safekeeper, the orchestrator's hooks listener).
# 2. Port audit (S27): every TCP listener of every container on the compute network (read from
#    each container's /proc/net/tcp{,6}, loopback excluded) must be in the table below, and each
#    is probed: it refuses a caller without a credential (and with the old public development
#    bearers), or it is public by design (health, Postgres password logins). An unlisted
#    listener fails the run.
#
# Collects every failure, then exits 1 if there was one.
set -euo pipefail

LOG_PREFIX=compute-network-e2e
project="${CHELABASE_COMPOSE_PROJECT:?set by chelabase workbench (stack.env)}"
image="${CHELABASE_WORKBENCH_IMAGE:?set by chelabase workbench (stack.env)}"
gateway_port="${CHELABASE_GATEWAY_PORT:?set by chelabase workbench (stack.env)}"
compute_net="${project}_compute"
control_net="${project}_control"
failures=0
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

probe="chelabase-t1-probe-$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')"
cleanup() { docker rm -f "$probe" >/dev/null 2>&1 || true; }
trap cleanup EXIT
docker run -d --rm --name "$probe" --network "$compute_net" --entrypoint sleep "$image" 900 >/dev/null
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

# --- 1. reachability ---------------------------------------------------------------------------
for target in nats:4222 nats:8222 valkey:6379 metadata-db:5432 zitadel-db:5432 storcon-db:5432 zitadel:8080 \
  mailpit:8025 mailpit:1025 seaweedfs:8333 storage-controller:1234 storage-broker:50051 "gateway:$gateway_port" studio:3000; do
  host="${target%%:*}" port="${target#*:}"
  if tcp_open "$host" "$port"; then fail "a compute reaches $target"; else ok "no route to $target"; fi
  ip="$(default_ip "$host")"
  if [[ -n "$ip" ]]; then
    if tcp_open "$ip" "$port"; then fail "a compute reaches $host by its address $ip:$port"; else ok "no route to $host at $ip"; fi
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
    orchestrator:8081)
      [[ "$(status GET http://orchestrator:8081/health)" == 200 ]] || fail "orchestrator:8081 /health"
      refused GET http://orchestrator:8081/v1/me "the public API" ;;
    orchestrator:8083)
      refused GET "http://orchestrator:8083/compute/api/v2/computes/ep-$zero32/spec" "the spec hook"
      refused PATCH "http://orchestrator:8083/compute/api/v2/computes/ep-$zero32/ddl" "the DDL hook"
      refused GET "http://orchestrator:8083/proxy/api/v1/get_endpoint_access_control?endpointish=ep-x&role=x" "the proxy API"
      refused PUT http://orchestrator:8083/storcon/notify-attach "the storage controller hook" -H 'Content-Type: application/json' -d '{}'
      refused GET "http://orchestrator:8083/internal/data/keys/$sha" "the data-key lookup"
      refused GET "http://orchestrator:8083/internal/data/branches/$uuid/connection" "the connection route"
      refused POST http://orchestrator:8083/internal/signup/check "the sign-up gate" -H 'Content-Type: application/json' -d '{"org":"x"}'
      refused POST http://orchestrator:8083/internal/signup/claimed "the setup claim" -H 'Content-Type: application/json' -d '{"org":"x"}'
      refused GET "http://orchestrator:8083/internal/app/sessions/$uuid" "session liveness"
      refused POST http://orchestrator:8083/internal/app/sync "the app-users sync" -d '{}'
      refused POST http://orchestrator:8083/internal/zitadel/events "the Zitadel webhook" -H 'Content-Type: application/json' -d '{}' ;;
    worker:8082) health_only worker:8082 "worker:8082" ;;
    rest:8085) refused POST http://rest:8085/data/v1/query "the data API" -H 'Content-Type: application/json' -d '{}' ;;
    rest:8086)
      refused GET "http://rest:8086/internal/schema/$uuid" "rest's internal schema route"
      refused POST "http://rest:8086/internal/console/$uuid/query" "rest's internal console route" -d '{}' ;;
    # Studio's sign-in backend: public by design (it is what the gateway serves to anyone), but
    # ruling (e): a compute can't pick its rate-limit key through X-Forwarded-For. 61 lookups of
    # an unknown sign-in (60 a minute per address, answered 410 without Zitadel), each claiming a
    # new address: the limit still applies to the compute's own address. The body's field is
    # `authRequest` (camelCase): with any other field the JSON extractor answers 422 before the
    # handler's rate limit runs, so nothing is ever limited (RT1 final fix round, test bug).
    auth:8084)
      local i got limited=""
      for i in $(seq 61); do
        got="$(status POST http://auth:8084/auth/v1/auth-request -H 'Content-Type: application/json' \
          -H "X-Forwarded-For: 198.51.100.$i" -d '{"authRequest":"not-a-request"}')"
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
    *) return 1 ;;
  esac
}

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
[[ "$listed" -gt 0 ]] || fail "no listener found on the compute network (the audit read nothing)"

if [[ "$failures" -gt 0 ]]; then
  echo "[$LOG_PREFIX] $failures failure(s)" >&2
  exit 1
fi
say "ok"
