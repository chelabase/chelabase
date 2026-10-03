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
# One file, one project (`cheladb`).
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
  # env -u: the check must hold with nothing set, as for a plain `just deps-up`.
  compose=(env -u NEON_IMAGE -u COMPUTE_NODE_IMAGE docker compose -f "$compose_file")
  services() { "${compose[@]}" "$@" config --services | sort | tr '\n' ' '; }
  check "the stack has every service (NATS, Valkey and the proxy too; no hook relay)" \
    test "$(services)" = "create-bucket mailpit metadata-db nats pageserver proxy proxy-tls register-safekeepers safekeeper1 seaweedfs storage-broker storage-controller storcon-db valkey zitadel zitadel-db "
  check "the project is cheladb" \
    test "$("${compose[@]}" config --format json | jq -r '.name')" = "cheladb"
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
  check "the proxy waits for its TLS certificate" \
    test "$(jq -r '.depends_on["proxy-tls"].condition' <<<"$proxy")" = "service_completed_successfully"
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
local_neon="cheladb/neon-storage:$neon_dev_tag"
local_compute="cheladb/neon-compute-v17:$neon_dev_tag"
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

# --- one stack: no slot recipes; live runs queue (scripts/live.sh) --------------------------
show() { just --justfile "$root/justfile" --show "$1" 2>/dev/null; }
recipes=" $(just --justfile "$root/justfile" --summary 2>/dev/null) "
for gone in stack stack-free stack-claim stack-migrate stack-test hook-relay-build; do
  check "no $gone recipe" bash -c '[[ "$2" != *" $1 "* ]]' _ "$gone" "$recipes"
done
check "nothing loads scripts/lib/slots.sh" \
  bash -c '! test -e "$1/scripts/lib/slots.sh" && ! grep -q "lib/slots.sh" "$1/justfile"' _ "$root"
check "live queues a just command" grep -Eq 'scripts/live\.sh[}"]* run' <<<"$(show live)"
check "live-status shows the queue" grep -Eq 'scripts/live\.sh[}"]* status' <<<"$(show live-status)"
check "live-test runs the queue's tests" grep -q 'scripts/tests/live.sh' <<<"$(show live-test)"
check "wt-add writes no shared cargo config and takes no slot" \
  bash -c '! grep -Eqi "\.cargo/config\.toml|slot" <<<"$1"' _ "$(show wt-add)"
check "sweep-builds sweeps every worktree's target" grep -q 'worktree list' <<<"$(show sweep-builds)"
check "dev records which worktree runs it" grep -q '/worktree"' <<<"$(show dev)"
check "stack-reset-slots runs its script" grep -q 'scripts/stack-reset-slots.sh' <<<"$(show stack-reset-slots)"
check "stack-reset-slots' offline checks pass" \
  bash -c '"$1/scripts/tests/stack-reset-slots.sh" >/dev/null 2>&1' _ "$root"

# The lock check: a recipe that touches the stack refuses (exit 3, holder printed) while another
# worktree holds it, before touching anything; inside `just live` (CHELADB_LIVE_HOLDER=1) it
# skips the check. Safe by construction, even when a recipe ignores the check: the recipes run
# in a sandbox copy (justfile, scripts, infra; an empty .cheladb), and every command that could
# reach a real service (docker, cargo, pnpm, curl, psql, ss, git) is a fake that logs and fails.
# The recipes get none of the caller's environment (env -i): a temp HOME, PATH = the fakes, just
# itself and the system directories.
lock="$(mktemp -d)"
trap 'rm -rf "$stub" "$lock"' EXIT
sandbox="$lock/repo"
mkdir -p "$sandbox/.cheladb" "$lock/bin" "$lock/home"
printf 'p:\n    @echo {{just_executable()}}\n' >"$lock/justfile"
ln -s "$(just --justfile "$lock/justfile" p)" "$lock/bin/just"
rm "$lock/justfile"
cp -r "$root/justfile" "$root/scripts" "$root/infra" "$sandbox/"
printf '%s\n' '#!/usr/bin/env bash' 'echo "live.sh $*" >>"$LOCK_LOG"' \
  'if [ "$1" = run ]; then echo "swap: ${LIVE_SWAP_DEV:-}" >>"$LOCK_LOG"; fi' \
  'if [ "$1" = check ]; then echo "the stack is held by /elsewhere (just neon-e2e)" >&2; exit 3; fi' \
  >"$lock/live.sh"
for cmd in docker cargo pnpm curl psql ss git; do
  printf '%s\n' '#!/usr/bin/env bash' "echo \"touched: $cmd \$*\" >>\"\$LOCK_LOG\"" 'exit 1' >"$lock/bin/$cmd"
done
chmod +x "$lock/live.sh" "$lock/bin/"*
in_sandbox() { # in_sandbox [--ask] <env assignments...> -- <recipe>: runs the recipe in the sandbox
  local envs=() yes=--yes
  if [ "$1" = --ask ]; then yes=() && shift; fi
  while [ "$1" != -- ]; do envs+=("$1"); shift; done
  shift
  env -i HOME="$lock/home" PATH="$lock/bin:/usr/bin:/bin" LOCK_LOG="$lock/log" \
    CHELADB_LIVE_SCRIPT="$lock/live.sh" "${envs[@]}" \
    "$lock/bin/just" --justfile "$sandbox/justfile" --working-directory "$sandbox" ${yes[@]+"${yes[@]}"} "$@" </dev/null
}
# The sandbox shares nothing with the caller's environment: with the stack free (the check
# passes), recipes that delete files run to the end, and a canary named by CHELADB_ZITADEL_DIR
# and CHELADB_MAIN in the outer environment survives.
printf '%s\n' '#!/usr/bin/env bash' 'echo "live.sh $*" >>"$LOCK_LOG"' 'exit 0' >"$lock/live-free.sh"
chmod +x "$lock/live-free.sh"
canary="$lock/canary"
mkdir -p "$canary/zitadel" "$canary/main/.cheladb/slots" "$canary/main/.worktrees/target"
echo keep >"$canary/zitadel/admin.pat"
echo keep >"$canary/main/.cheladb/slots/1.env"
(
  export CHELADB_ZITADEL_DIR="$canary/zitadel" CHELADB_MAIN="$canary/main"
  in_sandbox CHELADB_LIVE_SCRIPT="$lock/live-free.sh" -- zitadel-reset >/dev/null 2>&1 || true
  in_sandbox CHELADB_LIVE_SCRIPT="$lock/live-free.sh" -- stack-reset-slots --yes >/dev/null 2>&1 || true
)
check "the sandbox ignores CHELADB_ZITADEL_DIR from outside" test -f "$canary/zitadel/admin.pat"
check "the sandbox ignores CHELADB_MAIN from outside" \
  test -f "$canary/main/.cheladb/slots/1.env" -a -d "$canary/main/.worktrees/target"
for recipe in dev deps-up deps-down sweep-dbs cleanup zitadel-reset zitadel-bootstrap stack-reset-slots neon-smoke \
  neon-e2e branch-e2e data-e2e console-e2e role-e2e auth-e2e neon-test-live; do
  : >"$lock/log"
  status=0
  in_sandbox -- "$recipe" >"$lock/out" 2>&1 || status=$?
  check "$recipe refuses while another worktree holds the stack" \
    bash -c 'test "$1" -eq 3 && grep -q "held by /elsewhere" "$2" && ! grep -q "^touched" "$3"' \
    _ "$status" "$lock/out" "$lock/log"
done
: >"$lock/log"
in_sandbox CHELADB_LIVE_HOLDER=1 -- deps-down >/dev/null 2>&1 || true
check "inside just live the check is skipped" \
  bash -c '! grep -q "live.sh check" "$1" && grep -q "^touched: docker" "$1"' _ "$lock/log"
# With the lock free but just dev running from another worktree, a plain recipe would hit that
# worktree's apps: it warns (stderr, no failure). Quiet inside just live, when dev runs here, or
# when it doesn't run.
mkdir -p "$sandbox/.cheladb/dev"
warn_run() { # warn_run <worktree file content|-> [env...]: deps-down with the stack free
  local wt="$1"
  shift
  rm -f "$sandbox/.cheladb/dev/worktree"
  if [ "$wt" != - ]; then echo "$wt" >"$sandbox/.cheladb/dev/worktree"; fi
  in_sandbox CHELADB_LIVE_SCRIPT="$lock/live-free.sh" "$@" -- deps-down >"$lock/out" 2>"$lock/err" || true
}
warn_run /elsewhere
check "a plain recipe warns when just dev runs from another worktree" \
  bash -c 'grep -q "/elsewhere" "$1" && grep -qi "warning" "$1" && ! grep -q "/elsewhere" "$2"' _ "$lock/err" "$lock/out"
warn_run /elsewhere CHELADB_LIVE_HOLDER=1
check "inside just live there is no warning" bash -c '! grep -q "/elsewhere" "$1"' _ "$lock/err"
warn_run "$sandbox"
check "no warning when just dev runs here" bash -c '! grep -qi "warning" "$1"' _ "$lock/err"
warn_run -
check "no warning when just dev isn't running" bash -c '! grep -qi "warning" "$1"' _ "$lock/err"
rm -f "$sandbox/.cheladb/dev/worktree"
# sweep-builds without cargo-sweep: a hint and a clean exit (cleanup's last step must not fail).
status=0
in_sandbox -- sweep-builds >"$lock/out" 2>&1 || status=$?
check "sweep-builds without cargo-sweep hints and skips" \
  bash -c 'test "$1" -eq 0 && grep -q "cargo install cargo-sweep --locked" "$2"' _ "$status" "$lock/out"
# cleanup checks the lock before it asks for confirmation (no --yes, nothing on stdin).
: >"$lock/log"
status=0
in_sandbox --ask -- cleanup >"$lock/out" 2>&1 || status=$?
check "cleanup refuses before asking" \
  bash -c 'test "$1" -eq 3 && grep -q "held by /elsewhere" "$2" && ! grep -q "^touched" "$3"' \
  _ "$status" "$lock/out" "$lock/log"
# `just live`: a recipe runs through just; any other command (just itself, cargo) as given; the
# dev swap is passed to the queue.
: >"$lock/log"
in_sandbox -- live neon-e2e >/dev/null 2>&1 || true
check "just live <recipe> queues just <recipe>" grep -Eq '^live.sh run [^ ]*just neon-e2e$' "$lock/log"
: >"$lock/log"
in_sandbox -- live cargo nextest run -p rest >/dev/null 2>&1 || true
check "just live <command> queues the command as given" grep -qx 'live.sh run cargo nextest run -p rest' "$lock/log"
: >"$lock/log"
in_sandbox -- live just neon-e2e >/dev/null 2>&1 || true
check "just live just <recipe> queues just <recipe>" grep -qx 'live.sh run just neon-e2e' "$lock/log"
check "just live passes the dev swap" grep -Eq '^swap: .*--working-directory "?[^ ]*repo"? _live-swap-dev' "$lock/log"
# The dev swap `just live` runs while holding the stack (same sandbox and fakes).
mkdir -p "$sandbox/.cheladb/dev"
: >"$lock/log"
rm -f "$sandbox/.cheladb/dev/pids" "$sandbox/.cheladb/dev/worktree"
in_sandbox CHELADB_LIVE_HOLDER=1 -- _live-swap-dev >/dev/null 2>&1 || true
check "the dev swap does nothing when just dev isn't running" bash -c '! grep -q "^touched" "$1"' _ "$lock/log"
# A live pid (a sleep: dev-down's cmdline guard would never stop it).
sleep 300 &
alive=$!
echo "$alive" >"$sandbox/.cheladb/dev/pids"
echo "$sandbox" >"$sandbox/.cheladb/dev/worktree"
in_sandbox CHELADB_LIVE_HOLDER=1 -- _live-swap-dev >/dev/null 2>&1 || true
check "the dev swap does nothing when just dev runs here" \
  bash -c '! grep -q "^touched" "$1" && test -f "$2"' _ "$lock/log" "$sandbox/.cheladb/dev/pids"
kill "$alive" 2>/dev/null || true
wait "$alive" 2>/dev/null || true
# Here, but every pid is gone (a reboot, a crash): restart it.
: >"$lock/log"
echo 999999999 >"$sandbox/.cheladb/dev/pids"
in_sandbox CHELADB_LIVE_HOLDER=1 -- _live-swap-dev >"$lock/out" 2>&1 || true
check "the dev swap restarts a stale just dev here" \
  bash -c 'test ! -e "$1" && grep -q "^touched: docker" "$2"' _ "$sandbox/.cheladb/dev/pids" "$lock/log"
echo 999999999 >"$sandbox/.cheladb/dev/pids"
: >"$lock/log"
echo /elsewhere >"$sandbox/.cheladb/dev/worktree"
in_sandbox -- _live-swap-dev >"$lock/out" 2>&1 || true
check "the dev swap stops just dev from another worktree" \
  bash -c 'test ! -e "$1" && grep -q "moving it to" "$2"' _ "$sandbox/.cheladb/dev/pids" "$lock/out"
check "then starts just dev here (deps-up first)" grep -q "^touched: docker" "$lock/log"
check "the dev swap waits for the gateway and Studio" \
  bash -c 'grep -q "127.0.0.1:8000/health" <<<"$1" && grep -q "localhost:3000" <<<"$1"' _ "$(show _live-swap-dev)"

if [ "$failures" -gt 0 ]; then
  echo "$failures failure(s)" >&2
  exit 1
fi
echo "all passed"
