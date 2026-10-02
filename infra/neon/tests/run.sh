#!/usr/bin/env bash
# Offline checks for the Neon build, the Compose services and the neon scripts: no
# network and no Neon images needed (Docker only for `docker compose config`).
set -euo pipefail

here="$(cd "$(dirname "$0")/.." && pwd)"
root="$(cd "$here/../.." && pwd)"
failures=0
pass() { echo "ok   $1"; }
fail() {
  echo "FAIL $1" >&2
  failures=$((failures + 1))
}
check() { # check <name> <command...>
  local name="$1"
  shift
  if "$@"; then pass "$name"; else fail "$name"; fi
}
refuses() { # refuses <expected exit> <command...>
  local want="$1" status=0
  shift
  "$@" >/dev/null 2>&1 || status=$?
  [ "$status" -eq "$want" ]
}

# shellcheck source=../lib/tags.sh
. "$here/lib/tags.sh"

# --- tags ---------------------------------------------------------------------------------
check "NEON_REF is a full commit sha" grep -Eq '^[0-9a-f]{40}$' <<<"$neon_ref"
check "neon_tag is the first 12 chars of NEON_REF" test "$neon_tag" = "${neon_ref:0:12}"

# --- build.sh argument handling (must fail before touching docker or the disk) ------------
check "build.sh without a target exits 2" refuses 2 "$here/build.sh"
check "build.sh with an unknown target exits 2" refuses 2 "$here/build.sh" bogus
check "build.sh no longer builds vm-compute" refuses 2 "$here/build.sh" vm-compute

# --- Compose ------------------------------------------------------------------------------
compose_file="$root/infra/compose/docker-compose.yml"
check "NEON_IMAGE defaults to a digest-pinned GHCR image" \
  grep -Eq '\$\{NEON_IMAGE:-ghcr\.io/cheladb/neon:[^@}]+@sha256:[0-9a-f]{64}\}' "$compose_file"
check "COMPUTE_NODE_IMAGE defaults to a digest-pinned GHCR image" \
  grep -Eq '\$\{COMPUTE_NODE_IMAGE:-ghcr\.io/cheladb/neon-compute-v17:[^@}]+@sha256:[0-9a-f]{64}\}' "$compose_file"

if command -v docker >/dev/null; then
  # env -u: the check must hold with nothing set, as for a plain `just deps-up`.
  compose=(env -u NEON_IMAGE -u COMPUTE_NODE_IMAGE docker compose -f "$compose_file")
  services() { "${compose[@]}" "$@" config --services | sort | tr '\n' ' '; }
  check "the base config has every service" \
    test "$(services)" = "create-bucket mailpit metadata-db nats pageserver proxy proxy-tls register-safekeepers safekeeper1 seaweedfs storage-broker storage-controller storcon-db valkey zitadel zitadel-db "
  neon_json="$("${compose[@]}" config --format json)"
  published="$(jq <<<"$neon_json" -r '
    [.services | to_entries[]
      | select(.key | IN("seaweedfs","storage-broker","safekeeper1","storcon-db","storage-controller","pageserver","proxy"))
      | .value.ports[]?
      | "\(.host_ip // "0.0.0.0"):\(.published)"] | sort | join(" ")')"
  check "neon publishes only 1234, 4432, 4444 and 9898, on 127.0.0.1" \
    test "$published" = "127.0.0.1:1234 127.0.0.1:4432 127.0.0.1:4444 127.0.0.1:9898"
  unexposed="$(jq <<<"$neon_json" -r '
    [.services | to_entries[] | .key as $s | .value.ports[]?
      | select(.host_ip != "127.0.0.1") | "\($s):\(.published)"] | join(" ")')"
  check "every published port is bound to 127.0.0.1${unexposed:+ (not: $unexposed)}" \
    test -z "$unexposed"
  zitadel="$("${compose[@]}" config --format json | jq -r '.services.zitadel')"
  check "zitadel is v4.19.1, pinned by digest" \
    grep -Eq '"ghcr\.io/zitadel/zitadel:v4\.19\.1@sha256:[0-9a-f]{64}"' <<<"$zitadel"
  check "zitadel sends sign-ins to Studio's /login" \
    test "$(jq -r '.environment.ZITADEL_OIDC_DEFAULTLOGINURLV2' <<<"$zitadel")" = "http://localhost:3000/login?authRequest="
  check "zitadel writes the login-client token" \
    test "$(jq -r '.environment.ZITADEL_FIRSTINSTANCE_LOGINCLIENTPATPATH' <<<"$zitadel")" = "/zitadel-bootstrap/login-client.pat"
  token="$(CHELADB_STORCON_HOOK_TOKEN=from-env "${compose[@]}" config --format json |
    jq -r '.services["storage-controller"].command[]' | grep -- '--control-plane-jwt-token=' || true)"
  check "the hook token comes from CHELADB_STORCON_HOOK_TOKEN" \
    test "$token" = "--control-plane-jwt-token=from-env"
  storcon="$(jq -r '.services["storage-controller"]' <<<"$neon_json")"
  check "the storage controller calls the orchestrator's internal listener" \
    grep -qx -- '--control-plane-url=http://host.docker.internal:8083/storcon' <(jq -r '.command[]' <<<"$storcon")
  check "the storage controller can resolve host.docker.internal" \
    grep -q 'host-gateway' <<<"$(jq -c '.extra_hosts' <<<"$storcon")"
  proxy="$(jq -r '.services.proxy' <<<"$neon_json")"
  proxy_args="$(jq -r '.command[]' <<<"$proxy")"
  check "the proxy asks the orchestrator's internal listener" \
    grep -qx -- '--auth-endpoint=http://host.docker.internal:8083/proxy/api/v1' <<<"$proxy_args"
  check "the proxy serves Postgres wire and serverless" \
    bash -c "grep -qx -- '--proxy=0.0.0.0:4432' <<<\"\$1\" && grep -qx -- '--wss=0.0.0.0:4444' <<<\"\$1\"" _ "$proxy_args"
  check "the proxy caches role secrets for at most a minute" \
    grep -qx -- '--project-info-cache=size=10000,ttl=1m,max_roles=64,gc_interval=60m' <<<"$proxy_args"
  check "the proxy uses Valkey (cache clearing, query cancel)" \
    bash -c "grep -qx -- '--redis-auth-type=plain' <<<\"\$1\" && grep -qx -- '--redis-plain=redis://valkey:6379' <<<\"\$1\"" _ "$proxy_args"
  valkey="$(jq -r '.services.valkey' <<<"$neon_json")"
  check "valkey is 9.1.2, pinned by digest" \
    grep -Eq '"valkey/valkey:9\.1\.2-alpine@sha256:[0-9a-f]{64}"' <<<"$valkey"
  check "valkey is published on 127.0.0.1:6379 only" \
    test "$(jq -r '[.ports[] | "\(.host_ip):\(.published)"] | join(" ")' <<<"$valkey")" = "127.0.0.1:6379"
  check "the proxy can resolve host.docker.internal" \
    grep -q 'host-gateway' <<<"$(jq -c '.extra_hosts' <<<"$proxy")"
  check "the proxy waits for a healthy valkey" \
    test "$(jq -r '.depends_on.valkey.condition' <<<"$proxy")" = "service_healthy"
  check "valkey's healthcheck has an explicit timeout" \
    test "$(jq -r '.healthcheck.timeout // empty' <<<"$valkey")" != ""
  ptoken="$(CHELADB_PROXY_API_TOKEN=from-env "${compose[@]}" config --format json |
    jq -r '.services.proxy.environment.NEON_PROXY_TO_CONTROLPLANE_TOKEN')"
  check "the proxy's token comes from CHELADB_PROXY_API_TOKEN" test "$ptoken" = "from-env"
else
  pass "no docker: skipped the compose config checks"
fi

# --- scripts/neon-images.sh (against a stub docker) ---------------------------------------
stub="$(mktemp -d)"
trap 'rm -rf "$stub"' EXIT
cat >"$stub/docker" <<'EOF'
#!/usr/bin/env bash
# Stub: the images named in $STUB_LOCAL exist locally; pulls succeed when STUB_PULL=ok.
echo "$*" >>"$STUB_LOG"
case "$1 $2" in
  "image inspect") [[ " $STUB_LOCAL " == *" $3 "* ]] ;;
  "pull -q") [[ "$STUB_PULL" == ok ]] ;;
  *) exit 99 ;;
esac
EOF
chmod +x "$stub/docker"
images() { # images <local images> <ok|fail>: runs neon-images.sh against the stub
  : >"$stub/log"
  env PATH="$stub:$PATH" STUB_LOG="$stub/log" STUB_LOCAL="$1" STUB_PULL="$2" \
    "$root/scripts/neon-images.sh" 2>"$stub/err"
}
local_neon="cheladb/neon:$neon_tag"
local_compute="cheladb/compute-node-v17:$neon_tag"
pinned_neon='NEON_IMAGE=ghcr\.io/cheladb/neon:[^@]+@sha256:[0-9a-f]{64}'
pinned_compute='COMPUTE_NODE_IMAGE=ghcr\.io/cheladb/neon-compute-v17:[^@]+@sha256:[0-9a-f]{64}'

out="$(images "$local_neon $local_compute" fail || true)"
check "local builds are used for both images" \
  test "$out" = "NEON_IMAGE=$local_neon"$'\n'"COMPUTE_NODE_IMAGE=$local_compute"
check "local builds need no pull" bash -c "! grep -q '^pull' '$stub/log'"

out="$(images "" ok || true)"
check "without local builds, NEON_IMAGE is the pinned ref" grep -Eqx "$pinned_neon" <<<"$out"
check "without local builds, COMPUTE_NODE_IMAGE is the pinned ref" grep -Eqx "$pinned_compute" <<<"$out"

out="$(images "$local_neon" ok || true)"
check "a local neon build is used on its own" grep -qx "NEON_IMAGE=$local_neon" <<<"$out"
check "the compute image is still pulled then" grep -Eqx "$pinned_compute" <<<"$out"

status=0
images "" fail >/dev/null || status=$?
check "a failed pull exits 1" test "$status" -eq 1
check "a failed pull says to log in or build" \
  grep -qF "can't pull ghcr.io/cheladb/neon (docker's error is above). The packages are private: run docker login ghcr.io, or build locally with just neon-build (~40 min, ~40 GB free disk)." "$stub/err"

# --- scripts/neon-smoke.sh (offline: nothing listens on port 1) ----------------------------
status=0
STORCON_URL=http://127.0.0.1:1 PAGESERVER_URL=http://127.0.0.1:1 \
  "$root/scripts/neon-smoke.sh" >/dev/null 2>"$stub/smoke-err" || status=$?
check "smoke against a dead stack exits 1" test "$status" -eq 1
check "smoke names the failing call" \
  grep -qF "FAIL: GET http://127.0.0.1:1/control/v1/safekeeper" "$stub/smoke-err"
check "smoke passes on curl's own error" grep -qF "curl: (7)" "$stub/smoke-err"

# --- scripts/neon-smoke.sh cleanup (against a stub storage controller) --------------------
if command -v python3 >/dev/null; then
  cat >"$stub/storcon.py" <<'EOF'
# Stub storage controller + pageserver for neon-smoke.sh. argv: port file, mode, request log.
# Modes: tenant-500 (tenant create fails), timeline-500 (timeline create fails),
# delete-409 (timeline create fails and deletes are refused), hang (GETs never answer).
import http.server, json, sys, time

port_file, mode, log_path = sys.argv[1:4]
log = open(log_path, "a")


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def reply(self, code, body):
        data = json.dumps(body).encode()
        self.send_response(code)
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def record(self):
        log.write(f"{self.command} {self.path}\n")
        log.flush()

    def do_GET(self):
        self.record()
        if mode == "hang":
            time.sleep(10)
        if self.path == "/control/v1/safekeeper":
            return self.reply(200, [{"id": 1, "scheduling_policy": "Active"}])
        if self.path == "/v1/status":
            return self.reply(200, {})
        if self.path.startswith("/control/v1/tenant/"):
            return self.reply(200, {"shards": [{"is_pending_compute_notification": False}]})
        self.reply(404, {"msg": "not found"})

    def do_POST(self):
        self.rfile.read(int(self.headers.get("content-length", 0)))
        self.record()
        if self.path == "/v1/tenant" and mode != "tenant-500":
            return self.reply(201, {})
        self.reply(500, {"msg": "boom"})

    def do_DELETE(self):
        self.record()
        self.reply(409 if mode == "delete-409" else 200, {})


server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
with open(port_file, "w") as f:
    f.write(str(server.server_port))
server.serve_forever()
EOF
  smoke_against() { # smoke_against <mode>: runs neon-smoke.sh against the stub; sets status
    local pid port
    rm -f "$stub/port" && : >"$stub/requests"
    python3 "$stub/storcon.py" "$stub/port" "$1" "$stub/requests" &
    pid=$!
    for _ in $(seq 50); do
      if [ -s "$stub/port" ]; then break; fi
      sleep 0.1
    done
    port="$(cat "$stub/port")"
    status=0
    STORCON_URL="http://127.0.0.1:$port" PAGESERVER_URL="http://127.0.0.1:$port" NEON_SMOKE_MAX_TIME=1 \
      "$root/scripts/neon-smoke.sh" >/dev/null 2>"$stub/smoke-err" || status=$?
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  }
  deleted_tenant() { grep -Eq '^DELETE /v1/tenant/[0-9a-f]{32}$' "$stub/requests"; }

  smoke_against timeline-500
  check "a failed timeline create exits 1" test "$status" -eq 1
  check "a failed timeline create names the call" grep -qE 'FAIL: POST .*/timeline: 500' "$stub/smoke-err"
  check "a failed timeline create deletes the tenant" deleted_tenant

  smoke_against tenant-500
  check "a failed tenant create still deletes the tenant" deleted_tenant

  smoke_against delete-409
  check "a refused cleanup says so" grep -qE 'could not delete tenant [0-9a-f]{32} \(HTTP 409\)' "$stub/smoke-err"

  started=$SECONDS
  smoke_against hang
  check "a stack that never answers times out" test "$status" -eq 1
  check "the timeout is NEON_SMOKE_MAX_TIME" test $((SECONDS - started)) -lt 5
else
  pass "no python3: skipped the smoke cleanup checks"
fi

# --- CHELADB_INTERNAL_ADDR validation (fails before touching docker) -----------------------
for script in neon-e2e; do
  for bad in localhost 127.0.0.1: 127.0.0.1:http ":8083" "[::1]" "127.0.0.1:99999"; do
    status=0
    CHELADB_INTERNAL_ADDR="$bad" "$root/scripts/$script.sh" >/dev/null 2>"$stub/addr-err" || status=$?
    check "$script rejects CHELADB_INTERNAL_ADDR='$bad'" \
      bash -c 'test "$1" -eq 1 && grep -q "CHELADB_INTERNAL_ADDR=" "$2"' _ "$status" "$stub/addr-err"
  done
done
addr_ok() { # addr_ok <value>: passes the shared validator
  bash -c 'LOG_PREFIX=t; . "$1/scripts/lib/neon-common.sh"; check_internal_addr "$2"' _ "$root" "$1" >/dev/null 2>&1
}
check "CHELADB_INTERNAL_ADDR accepts 127.0.0.1:8083" addr_ok 127.0.0.1:8083
check "CHELADB_INTERNAL_ADDR accepts [::1]:8083" addr_ok "[::1]:8083"
check "CHELADB_INTERNAL_ADDR accepts a host name" addr_ok host.docker.internal:8083

# --- justfile -----------------------------------------------------------------------------
check "neon-ps shows exited one-shots too" bash -c "just --justfile '$root/justfile' --show neon-ps | grep -q ' ps -a '"
check "deps-down also removes demo computes" bash -c "just --justfile '$root/justfile' --show deps-down | grep -q 'cheladb-compute-'"
check "deps-up copies the proxy CA" \
  bash -c "just --justfile '$root/justfile' --show deps-up | grep -q 'cp proxy:/tls/ca.crt'"
check "dev exports CHELADB_COMPUTE_IMAGE" \
  bash -c "just --justfile '$root/justfile' --show dev | grep -q 'export CHELADB_COMPUTE_IMAGE='"
check "worker exports CHELADB_COMPUTE_IMAGE" \
  bash -c "just --justfile '$root/justfile' --show worker | grep -q 'export CHELADB_COMPUTE_IMAGE='"
check "branch-e2e recipe exists" \
  bash -c "just --justfile '$root/justfile' --show branch-e2e | grep -q 'scripts/branch-e2e.sh'"
check "data-e2e recipe exists" \
  bash -c "just --justfile '$root/justfile' --show data-e2e | grep -q 'scripts/data-e2e.sh'"
check "role-e2e recipe exists" \
  bash -c "just --justfile '$root/justfile' --show role-e2e | grep -q 'scripts/role-e2e.sh'"
check "dev starts rest" \
  bash -c "just --justfile '$root/justfile' --show dev | grep -q 'cargo run -p rest'"

if [ "$failures" -gt 0 ]; then
  echo "$failures failure(s)" >&2
  exit 1
fi
echo "all passed"
