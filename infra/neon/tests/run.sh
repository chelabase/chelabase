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
check "neon_dev_tag is neon_tag plus -dev" test "${neon_dev_tag:-}" = "${neon_tag}-dev"

# --- the fork -------------------------------------------------------------------------------
neon_ref_on_fork() {
  if command -v gh >/dev/null && gh api "repos/chelabase/neon/commits/$neon_ref" >/dev/null 2>&1; then
    return 0
  fi
  # No gh: a one-commit fetch of the sha (ls-remote only matches ref names).
  local tmp
  tmp="$(mktemp -d)"
  git -C "$tmp" init -q &&
    git -C "$tmp" fetch -q --depth 1 https://github.com/chelabase/neon "$neon_ref" 2>/dev/null
  local status=$?
  rm -rf "$tmp"
  return $status
}
if git ls-remote --exit-code https://github.com/chelabase/neon HEAD >/dev/null 2>&1 ||
  { command -v gh >/dev/null && gh api repos/chelabase/neon >/dev/null 2>&1; }; then
  check "neon_ref_on_fork" neon_ref_on_fork
else
  pass "offline or no access to chelabase/neon: skipped neon_ref_on_fork"
fi
compose_defaults_match_ref() { # both pins (storage and compute) carry NEON_REF's tag
  test "$(grep -c ":${neon_tag}-dev@sha256:" "$root/infra/compose/docker-compose.yml")" -eq 2
}
check "compose_defaults_match_ref" compose_defaults_match_ref
readme_names_the_compose_file() { # the README points at the one file that holds the pins
  grep -q 'pinned by digest in `infra/compose/docker-compose.yml`' "$here/README.md" &&
    ! grep -Eq 'shared\.yml|slot\.yml' "$here/README.md"
}
check "the README names the Compose file that holds the pins" readme_names_the_compose_file
build_sh_uses_fork() {
  grep -q 'chelabase/neon' "$here/build.sh" && ! grep -q 'neondatabase/neon' "$here/build.sh"
}
check "build_sh_uses_fork" build_sh_uses_fork
build_sh_pins_build_tools() {
  grep -q 'sha256sum "\$src/build-tools/Dockerfile" | cut -c1-12' "$here/build.sh" &&
    grep -q -- '--build-arg "TAG=\$build_tools_tag"' "$here/build.sh" &&
    grep -q -- '--build-arg REPOSITORY=ghcr.io/chelabase' "$here/build.sh" &&
    grep -q -- '--build-arg IMAGE=neon-build-tools' "$here/build.sh"
}
check "build_sh_pins_build_tools" build_sh_pins_build_tools

# --- build.sh argument handling (must fail before touching docker or the disk) ------------
check "build.sh without a target exits 2" refuses 2 "$here/build.sh"
check "build.sh with an unknown target exits 2" refuses 2 "$here/build.sh" bogus
check "build.sh no longer builds vm-compute" refuses 2 "$here/build.sh" vm-compute

# --- Compose ------------------------------------------------------------------------------
# One file, one project (`chelabase`).
compose_file="$root/infra/compose/docker-compose.yml"
check "one Compose file" \
  test "$(find "$root/infra/compose" -name '*.yml' ! -name moon.yml | sort | tr '\n' ' ')" = "$compose_file "
check "no hook-relay image or service left" \
  bash -c '! test -e "$1/infra/compose/hook-relay.Dockerfile" && ! grep -q "hook-relay" "$2"' _ "$root" "$compose_file"
check "NEON_IMAGE defaults to a digest-pinned GHCR image" \
  grep -Eq '\$\{NEON_IMAGE:-ghcr\.io/chelabase/neon-storage:[^@}]+@sha256:[0-9a-f]{64}\}' "$compose_file"
check "COMPUTE_NODE_IMAGE defaults to a digest-pinned GHCR image" \
  grep -Eq '\$\{COMPUTE_NODE_IMAGE:-ghcr\.io/chelabase/neon-compute-v17:[^@}]+@sha256:[0-9a-f]{64}\}' "$compose_file"

if command -v docker >/dev/null; then
  # A parametrized project (W2): the checks render one stack from the workbench fixture.
  fixture="$root/scripts/tests/fixtures/workbench-stack.env"
  compose=(env -u NEON_IMAGE -u COMPUTE_NODE_IMAGE docker compose -p chelabase-feat-x -f "$compose_file" --env-file "$fixture")
  services() { "${compose[@]}" "$@" config --services | sort | tr '\n' ' '; }
  check "the stack has every service (infra, app tier, no hook relay)" \
    test "$(services)" = "auth create-bucket gateway mailpit metadata-db nats orchestrator pageserver proxy proxy-tls realtime register-safekeepers rest safekeeper1 safekeeper2 safekeeper3 seaweedfs storage-broker storage-controller storcon-db studio valkey worker zitadel zitadel-db "
  check "the project name comes from -p" \
    test "$("${compose[@]}" config --format json | jq -r '.name')" = "chelabase-feat-x"
  check "the compose file names no project" bash -c '! grep -qE "^name:" "$1"' _ "$compose_file"
  neon_json="$("${compose[@]}" config --format json)"
  published="$(jq <<<"$neon_json" -r '
    [.services | to_entries[]
      | select(.key | IN("seaweedfs","storage-broker","safekeeper1","safekeeper2","safekeeper3","storcon-db","storage-controller","pageserver","proxy"))
      | .value.ports[]?
      | "\(.host_ip):\(.published)"] | sort | join(" ")')"
  check "neon publishes only the proxy's Postgres port, on loopback" \
    test "$published" = "127.0.0.1:4537 ::1:4537"
  zitadel="$(jq -r '.services.zitadel' <<<"$neon_json")"
  check "zitadel is v4.19.1, pinned by digest" \
    grep -Eq '"ghcr\.io/zitadel/zitadel:v4\.19\.1@sha256:[0-9a-f]{64}"' <<<"$zitadel"
  check "zitadel sends sign-ins to Studio's /login" \
    test "$(jq -r '.environment.ZITADEL_OIDC_DEFAULTLOGINURLV2' <<<"$zitadel")" = "http://studio.feat-x.chelabase.localhost:8137/login?authRequest="
  check "zitadel writes the login-client token" \
    test "$(jq -r '.environment.ZITADEL_FIRSTINSTANCE_LOGINCLIENTPATPATH' <<<"$zitadel")" = "/zitadel-bootstrap/login-client.pat"
  storcon="$(jq -r '.services["storage-controller"]' <<<"$neon_json")"
  # RT1 T1 (S27): the hook token is a generated secret; fork round 4 (F6): the controller reads
  # it from its mount itself (--control-plane-jwt-token-file), so it is in no argument and not
  # in its environment.
  storcon_args="$(jq -r '[(.entrypoint // [])[], (.command // [])[]] | join(" ")' <<<"$storcon")"
  check "the hook token comes from the mounted /run/storcon-hook.token" \
    grep -qF -- '--control-plane-jwt-token-file=/run/storcon-hook.token' <<<"$storcon_args"
  check "no hook token in the storage controller's command" \
    bash -c "! grep -qE -- '--control-plane-jwt-token([= ]|\$)' <<<\"\$1\"" _ "$storcon_args"
  check "the storage controller reads its pageserver and safekeeper tokens from their mounts" \
    bash -c "grep -qF -- '--pageserver-jwt-token-file=/run/pageserver-api.jwt' <<<\"\$1\" && grep -qF -- '--safekeeper-jwt-token-file=/run/safekeeper-data.jwt' <<<\"\$1\"" _ "$storcon_args"
  check "no token in the storage controller's environment (PUBLIC_KEY is a public key)" \
    bash -c "! grep -qE 'PAGESERVER_JWT_TOKEN|SAFEKEEPER_JWT_TOKEN|CONTROL_PLANE_JWT_TOKEN' <<<\"\$1\"" _ "$storcon_args"
  pageserver_args="$(jq -r '.services.pageserver | [(.entrypoint // [])[], (.command // [])[]] | join(" ")' <<<"$neon_json")"
  check "the pageserver reads its safekeeper token from its mount" \
    grep -qF -- '--auth-token-file=/run/safekeeper-data.jwt' <<<"$pageserver_args"
  check "no NEON_AUTH_TOKEN in the pageserver's environment" \
    bash -c "! grep -q NEON_AUTH_TOKEN <<<\"\$1\"" _ "$pageserver_args"
  check "the storage controller calls the orchestrator's internal listener" \
    grep -qx -- '--control-plane-url=http://orchestrator:8083/storcon' <(jq -r '.command[]' <<<"$storcon")
  proxy="$(jq -r '.services.proxy' <<<"$neon_json")"
  proxy_args="$(jq -r '.command[]' <<<"$proxy")"
  check "the proxy asks the orchestrator's internal listener" \
    grep -qx -- '--auth-endpoint=http://orchestrator:8083/proxy/api/v1' <<<"$proxy_args"
  check "the proxy serves Postgres wire and serverless" \
    bash -c "grep -qx -- '--proxy=0.0.0.0:4432' <<<\"\$1\" && grep -qx -- '--wss=0.0.0.0:4444' <<<\"\$1\"" _ "$proxy_args"
  check "the proxy caches role secrets for at most a minute" \
    grep -qx -- '--project-info-cache=size=10000,ttl=1m,max_roles=64,gc_interval=60m' <<<"$proxy_args"
  # RT1 T1 (S6): as the `proxy` ACL user, its password read from its mount by the entrypoint.
  proxy_entry="$(jq -r '.entrypoint // [] | join(" ")' <<<"$proxy")"
  check "the proxy uses Valkey (cache clearing, query cancel) as its ACL user" \
    bash -c "grep -qx -- '--redis-auth-type=plain' <<<\"\$1\" && grep -q -- '--redis-plain=redis://proxy@valkey:6379' <<<\"\$2\" && grep -q -- '--redis-plain-password-file=/run/valkey-proxy.password' <<<\"\$2\" && ! grep -Eq 'redis://proxy:[^@ ]*@' <<<\"\$2\"" _ "$proxy_args" "$proxy_entry"
  valkey="$(jq -r '.services.valkey' <<<"$neon_json")"
  check "valkey is 9.1.2, pinned by digest" \
    grep -Eq '"valkey/valkey:9\.1\.2-alpine@sha256:[0-9a-f]{64}"' <<<"$valkey"
  check "valkey is not published" \
    test "$(jq -r '.ports // [] | length' <<<"$valkey")" = "0"
  check "the proxy waits for a healthy valkey" \
    test "$(jq -r '.depends_on.valkey.condition' <<<"$proxy")" = "service_healthy"
  check "valkey's healthcheck has an explicit timeout" \
    test "$(jq -r '.healthcheck.timeout // empty' <<<"$valkey")" != ""
  check "the proxy waits for its TLS certificate" \
    test "$(jq -r '.depends_on["proxy-tls"].condition' <<<"$proxy")" = "service_completed_successfully"
  # RT1 T1 (S27): a generated secret from its mount, never a literal in the environment.
  check "the proxy's token comes from the mounted /run/proxy-api.token" \
    bash -c "grep -q -e '--control-plane-token-file=/run/proxy-api.token' <<<\"\$1\"" _ "$proxy_entry"
  check "no proxy token literal in its environment" \
    test "$(jq -r '.environment.NEON_PROXY_TO_CONTROLPLANE_TOKEN // empty' <<<"$proxy")" = ""
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
local_neon="chelabase/neon-storage:$neon_dev_tag"
local_compute="chelabase/neon-compute-v17:$neon_dev_tag"
pinned_neon='NEON_IMAGE=ghcr\.io/chelabase/neon-storage:[^@]+@sha256:[0-9a-f]{64}'
pinned_compute='COMPUTE_NODE_IMAGE=ghcr\.io/chelabase/neon-compute-v17:[^@]+@sha256:[0-9a-f]{64}'

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
  grep -qF "can't pull ghcr.io/chelabase/neon-storage (docker's error is above). The images are public on ghcr.io; build locally with just neon-build (~40 min, ~40 GB free disk)." "$stub/err"

# --- scripts/neon-smoke.sh (offline: nothing listens on port 1) ----------------------------
status=0
STORCON_URL=http://127.0.0.1:1 PAGESERVER_URL=http://127.0.0.1:1 NEON_SMOKE_MAX_TIME=3 \
  "$root/scripts/neon-smoke.sh" >/dev/null 2>"$stub/smoke-err" || status=$?
check "smoke against a dead stack exits 1" test "$status" -eq 1
check "smoke names the failing call" \
  grep -qF "FAIL: GET http://127.0.0.1:1/control/v1/safekeeper" "$stub/smoke-err"
check "smoke passes on curl's own error" grep -qE "curl: \((7|28)\)" "$stub/smoke-err"

# --- scripts/neon-smoke.sh cleanup (against a stub storage controller) --------------------
if command -v python3 >/dev/null; then
  cat >"$stub/storcon.py" <<'EOF'
# Stub storage controller + pageserver for neon-smoke.sh. argv: port file, mode, request log.
# Modes: tenant-500 (tenant create fails), timeline-500 (timeline create fails),
# delete-409 (timeline create fails and deletes are refused), hang (GETs never answer),
# pending (the tenant's compute notification stays pending: nobody answered the hook).
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
            return self.reply(200, {"shards": [{"is_pending_compute_notification": mode == "pending"}]})
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

  smoke_against pending
  check "a pending hook notification fails the smoke" test "$status" -eq 1
  check "a pending hook notification is named" grep -q "compute notification is still pending" "$stub/smoke-err"
  check "a pending hook notification still deletes the tenant" deleted_tenant

  started=$SECONDS
  smoke_against hang
  check "a stack that never answers times out" test "$status" -eq 1
  check "the timeout is NEON_SMOKE_MAX_TIME" test $((SECONDS - started)) -lt 5
else
  pass "no python3: skipped the smoke cleanup checks"
fi


if [ "$failures" -gt 0 ]; then
  echo "$failures failure(s)" >&2
  exit 1
fi
echo "all passed"
