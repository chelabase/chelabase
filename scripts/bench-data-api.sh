#!/usr/bin/env bash
# Data API round-trip benchmark (step 6e), needs the stack: just workbench up (run it through just bench-data-api).
# Creates a throwaway database, finds its compute container, runs tools/bench-data-api (which adds
# netem delay on the container's eth0), and always removes the qdisc and the database afterwards.
# Extra args go to the harness (--quick, --rtts-ms 0,0.5,2, ...). Without --quick (or --no-throughput) the results go to
# docs/benchmarks/<date>-data-api-pipelining.md. --variants C,R --repeats 2 --no-throughput is the
# shipped-path check (R = rest's engine); its table goes to stdout, JSON to target/bench-data-api. KEEP=1 leaves the database in place.
# --tests-only seeds the fixture and runs tests/variants_live.rs against it instead of the benchmark.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
LOG_PREFIX=bench-data-api
# shellcheck source=lib/neon-common.sh
. scripts/lib/neon-common.sh
GATEWAY="${GATEWAY_URL:-$CHELABASE_ENTRY_API_URL}"
INTERNAL_URL="${CHELABASE_INTERNAL_URL:-${CHELABASE_CONTROL_PLANE_URI:-http://orchestrator:8083}}"
INTERNAL_TOKEN="$(secret_value CHELABASE_REST_INTERNAL_TOKEN)"
# The stack's generated bearer; never a development default (the stack refuses those).
[[ -n "$INTERNAL_TOKEN" ]] ||
  { echo "FAIL CHELABASE_REST_INTERNAL_TOKEN (or _FILE) is unset: run this through just bench-data-api" >&2; exit 1; }
DEMO_TENANT_ID="00000000-0000-4000-8000-000000000001"
DEMO_PROJECT_ID="00000000-0000-4000-8000-000000000002"
# Keep in step with NETSHOOT in tools/bench-data-api/src/netem.rs (bump both together).
NETSHOOT="nicolaka/netshoot:v0.16@sha256:b09d9b21381f47a79b3cbcb30da25266dc17186ea00ae65e99fdc51396f48e70"
tmp="$(mktemp -d)"
db_id=""
container=""
t0=$SECONDS

pass() { echo "ok   $* ($((SECONDS - t0)) s)"; }
die() {
  echo "FAIL $*" >&2
  exit 1
}
# netem_run <tc args...>: tc in the compute's network namespace.
netem_run() {
  docker run --rm --net "container:$container" --cap-add NET_ADMIN "$NETSHOOT" tc "$@"
}
cleanup() {
  # The qdisc first (it must go even if the harness died mid-run), then the database.
  if [[ -n "$container" ]]; then
    local out
    if ! out="$(netem_run qdisc del dev eth0 root 2>&1)"; then
      case "$out" in
        *"Cannot delete qdisc with handle of zero"* | *"No such file"*) ;;
        *) echo "warn: removing the qdisc: $out" >&2 ;;
      esac
    fi
  fi
  if [[ -n "$db_id" && "${KEEP:-}" != 1 ]]; then
    api DELETE "/v1/databases/$db_id" >/dev/null || true
  fi
  rm -rf "$tmp"
}

command -v docker >/dev/null || die "docker is missing"
command -v jq >/dev/null || die "jq is missing"
[[ -x "${CHELABASE_CLI:-}" ]] || die "CHELABASE_CLI is unset or not executable (run through: just workbench run scripts/<name>.sh)"
demo_env="${CHELABASE_ZITADEL_DIR:?run through just workbench run}/demo.env"
[[ -f "$demo_env" ]] || die "$demo_env is missing (run just workbench up)"
set -a
# shellcheck disable=SC1091
source "$demo_env"
set +a
ca="${CHELABASE_PROXY_CA_FILE:?run through just workbench run}"
[[ -s "$ca" ]] || die "$ca is missing (run just workbench up)"
trap cleanup EXIT

# ok <method> <path> <expected status> [body]: the call, or die.
ok() {
  local status
  status="$(api "$1" "$2" "${4:-}")"
  [[ "$status" == "$3" ]] || die "$1 $2: $status (wanted $3) $(cat "$tmp/body")"
}

# wait_db: until the database is ready.
wait_db() {
  local s=""
  for _ in $(seq 120); do
    ok GET "/v1/databases/$db_id" 200
    s="$(jq -r '.status' "$tmp/body")"
    case "$s" in
      ready) return ;;
      failed) die "database $db_id failed: $(jq -r '.error' "$tmp/body")" ;;
    esac
    sleep 1
  done
  die "database $db_id is still $s after 120 s"
}

# reveal: sets bhost, bpw and bdb from the main branch's connection details.
reveal() {
  ok GET "/v1/databases/$db_id/branches/$db_id/connection" 200
  bhost="$(jq -r '.host' "$tmp/body")"
  bpw="$(jq -r '.password' "$tmp/body")"
  bdb="$(jq -r '.database' "$tmp/body")"
}

# --quick among the args? --tests-only is the recipe's own: seed, run the live tests, clean up.
quick=0
latency_only=0
tests_only=0
args=()
for a in "$@"; do
  case "$a" in
    --quick)
      quick=1
      args+=("$a")
      ;;
    --no-throughput)
      latency_only=1
      args+=("$a")
      ;;
    --tests-only) tests_only=1 ;;
    *) args+=("$a") ;;
  esac
done

# 1. A throwaway database.
name="bench_$(hex 2)"
status="$(api POST /v1/databases "$(jq -nc --arg t "$DEMO_TENANT_ID" --arg p "$DEMO_PROJECT_ID" --arg n "$name" \
  '{tenant_id: $t, project_id: $p, name: $n}')")"
[[ "$status" == 202 || "$status" == 201 ]] || die "POST /v1/databases: $status $(cat "$tmp/body")"
db_id="$(jq -r '.database_id' "$tmp/body")"
[[ -n "$db_id" && "$db_id" != null ]] || die "POST /v1/databases returned no database_id: $(cat "$tmp/body")"
wait_db
reveal
pass "database $name ($db_id) is ready"

# 2. The compute: the Postgres address rest itself uses (the compute on the stack network), and its container.
code="$(curl -sS --max-time 10 -o "$tmp/conn" -w '%{http_code}' \
  -H "Authorization: Bearer $INTERNAL_TOKEN" \
  "$INTERNAL_URL/internal/data/branches/$db_id/connection" || true)"
[[ "$code" == 200 ]] || die "internal connection lookup: $code $(cat "$tmp/conn" 2>/dev/null)"
chost="$(jq -r '.host' "$tmp/conn")"
cport="$(jq -r '.port' "$tmp/conn")"
[[ -n "$chost" && "$chost" != null && "$cport" =~ ^[0-9]+$ ]] || die "unexpected connection body: $(cat "$tmp/conn")"
ep="${bhost%%.*}"
names="$(docker ps --filter "label=chelabase.dev/stack=$CHELABASE_STACK" --filter "label=chelabase.dev/endpoint-id=$ep" --format '{{.Names}}')"
[[ -n "$names" && "$(wc -l <<<"$names")" -eq 1 ]] || die "wanted exactly one compute container for endpoint $ep in this stack, got: ${names:-none}"
container="$names"
pass "compute container $container ($chost:$cport)"

# 3. The harness, straight to the compute (no proxy).
pw_enc="$(jq -rn --arg p "$bpw" '$p|@uri')"
owner_url="postgresql://postgres:$pw_enc@$chost:$cport/$bdb"
# Built on the host by the recipe (the runner has no toolchain).
bench_bin="${CHELABASE_BENCH_BIN:?run through: just bench-data-api}"
[[ -x "$bench_bin" ]] || die "$bench_bin is missing (run through: just bench-data-api)"
if [[ "$tests_only" == 1 ]]; then
  CHELABASE_INTERNAL_URL="$INTERNAL_URL" CHELABASE_REST_INTERNAL_TOKEN="$INTERNAL_TOKEN" \
    "$bench_bin" \
    --owner-url "$owner_url" --branch "$db_id" --compute-container "$container" --seed-only ||
    die "seeding the fixture"
  pass "fixture seeded"
  BENCH_OWNER_URL="$owner_url" BENCH_BRANCH="$db_id" \
    CHELABASE_INTERNAL_URL="$INTERNAL_URL" CHELABASE_REST_INTERNAL_TOKEN="$INTERNAL_TOKEN" \
    "${CHELABASE_BENCH_TEST_BIN:?run through: just bench-data-api}" --ignored --test-threads=1 ||
    die "live tests"
  pass "live tests"
  echo "all passed ($((SECONDS - t0)) s)"
  exit 0
fi
extra=()
if [[ "$quick" == 0 && "$latency_only" == 0 ]]; then
  mkdir -p docs/benchmarks
  extra=(--results-md "docs/benchmarks/$(date +%F)-data-api-pipelining.md")
fi
CHELABASE_INTERNAL_URL="$INTERNAL_URL" CHELABASE_REST_INTERNAL_TOKEN="$INTERNAL_TOKEN" \
  "$bench_bin" \
  --owner-url "$owner_url" --branch "$db_id" --compute-container "$container" \
  "${extra[@]}" "${args[@]}" || die "bench-data-api failed"
pass "harness finished"

# 4. With --quick, the netem qdisc must be gone again.
if [[ "$quick" == 1 ]]; then
  shown="$(netem_run qdisc show dev eth0)" || die "tc qdisc show"
  [[ "$shown" != *netem* ]] || die "netem is still on $container's eth0: $shown"
  pass "no netem qdisc left on $container"
fi
echo "all passed ($((SECONDS - t0)) s)"
