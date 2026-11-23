#!/usr/bin/env bash
# compute-network-reach: what a compute can reach through `compute-egress`, proved from a real
# compute (a database made through the API) with `dblink`, `postgres_fdw` and a subscription.
# Runs in the stack's runner (`just compute-egress-e2e`; the stack needs the `compute-egress`
# service, which the workbench starts with its test allow-list: egress-allowed.test:5432,
# egress-second.test:5432@15432, host.docker.internal:5432@15433, egress-blocked.test:5432@15436,
# and its test deny list, 192.88.99.16/28).
#
# The forwarder dials public addresses only, so the test destinations need addresses that count as
# public without leaving the host. The script makes its own bridge network on 192.88.99.0/24, the
# deprecated 6to4 relay anycast block: IANA classes it as globally routable (the forwarder's
# `is_public` accepts it) but nothing is routed there in practice. The network is `internal` (no
# route off the host and no NAT rule for it), so the few real addresses are unreachable from the
# host for the run. Two throwaway Postgres containers on it (aliases egress-allowed.test and
# egress-denied.test on the first, egress-second.test on the second) are the destinations, a third
# (egress-blocked.test, at 192.88.99.17 inside the deny list) is one the forwarder must refuse, and the
# stack's compute-egress container is attached to the network for the run only. No setting or code
# path of the forwarder is changed for the test (plan.md, open question 6).
#
# Checks (a failure is collected; the run exits 1 at the end):
#   - the forwarder's process has no capability, a non-root uid, no-new-privileges; forwarding is
#     off in its namespace; the compute's resolver is the egress address; the live compute and
#     control networks are internal and isolated;
#   - a listed destination answers (dblink and a postgres_fdw foreign table) at its real port, and a
#     second destination on the same port answers at its assigned listen port, each with its own row;
#   - an unlisted name doesn't resolve; a listed name on a port that is no entry's listen port is
#     refused; a destination's own address (a direct address) has no route and fails within the
#     timeout; the listed host.docker.internal entry (a non-public address) is refused by the
#     forwarder, with its reason in the log; the listed egress-blocked.test, whose public test
#     address is in CHELABASE_COMPUTE_EGRESS_DENY, is refused per connection, with that reason in
#     the log;
#   - a subscription (CREATE SUBSCRIPTION ... WITH (connect = true)) to a publication on the
#     destination is created (its slot exists on the destination) and its table is copied.
#
# On every exit path (success, failure, interrupt) the service is detached from the test network,
# the destinations, the network and the database are removed, and the service is checked to be on
# exactly its two stack networks again. KEEP=1 leaves the database.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
LOG_PREFIX=compute-egress-e2e
# shellcheck source=lib/neon-common.sh
. scripts/lib/neon-common.sh
project="${CHELABASE_COMPOSE_PROJECT:?run through just workbench run}"
image="${CHELABASE_WORKBENCH_IMAGE:?run through just workbench run}"
egress_ip="${CHELABASE_COMPUTE_EGRESS_IP:?run through just workbench run}"
GATEWAY="${GATEWAY_URL:-$CHELABASE_ENTRY_API_URL}"
PROXY_PG_PORT="${CHELABASE_PROXY_PG_PORT:-4432}"
PROXY_HOSTADDR="${CHELABASE_PROXY_HOSTADDR:?run through just workbench run}"
ca="${CHELABASE_PROXY_CA_FILE:?run through just workbench run}"
DEMO_TENANT_ID="00000000-0000-4000-8000-000000000001"
DEMO_PROJECT_ID="00000000-0000-4000-8000-000000000002"
compute_net="${project}_compute-internal"
control_net="${project}_control-internal"
egress_net="${project}_compute-egress"
test_subnet="192.88.99.0/24"
allowed_ip=192.88.99.11
second_ip=192.88.99.12
blocked_ip=192.88.99.17
connect_timeout=5
tmp="$(mktemp -d)"
rand="$(hex 4)"
test_net="cnr-egress-e2e-$rand"
dst_allowed="cnr-egress-allowed-$rand"
dst_second="cnr-egress-second-$rand"
dst_blocked="cnr-egress-blocked-$rand"
db_id=""
attached=""
failures=0
t0=$SECONDS

pass() { echo "[$LOG_PREFIX] ok   $* ($((SECONDS - t0)) s)"; }
fail() {
  echo "[$LOG_PREFIX] FAIL: $*" >&2
  failures=$((failures + 1))
}
# This script's own format for fatal errors (overrides the shared die).
die() {
  echo "[$LOG_PREFIX] FAIL: $*" >&2
  exit 1
}

egress="$(docker ps -q --filter "label=com.docker.compose.project=$project" --filter "label=com.docker.compose.service=compute-egress" | head -n1)"
[[ -n "$egress" ]] || die "the stack has no running compute-egress container (just workbench up starts it)"
[[ -x "${CHELABASE_CLI:-}" ]] || die "CHELABASE_CLI is unset or not executable (run through: just workbench run scripts/<name>.sh)"
[[ -s "$ca" ]] || die "$ca is missing (run just workbench up)"
# The destinations' image is the stack's own Postgres (already local, nothing is pulled).
pg_image="$(docker inspect -f '{{.Config.Image}}' "$(docker ps -q --filter "label=com.docker.compose.project=$project" --filter "label=com.docker.compose.service=metadata-db" | head -n1)")"
[[ -n "$pg_image" ]] || die "can't read the Postgres image of the stack's metadata-db"
since="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

# The service's networks: exactly its two stack networks (also after the run).
egress_networks() { docker inspect -f '{{range $n, $_ := .NetworkSettings.Networks}}{{$n}} {{end}}' "$egress" | tr ' ' '\n' | sed '/^$/d' | sort | tr '\n' ' '; }
want_networks="$(printf '%s\n' "$compute_net" "$egress_net" | sort | tr '\n' ' ')"
[[ "$(egress_networks)" == "$want_networks" ]] || die "compute-egress is on [$(egress_networks)] before the run, want [$want_networks]"

cleanup() {
  local rc=$?
  trap - EXIT
  set +e
  if [[ -n "$db_id" && "${KEEP:-}" != 1 ]]; then
    api DELETE "/v1/databases/$db_id" >/dev/null
  fi
  [[ -z "$attached" ]] || docker network disconnect -f "$test_net" "$egress" >/dev/null 2>&1
  docker rm -f "$dst_allowed" "$dst_second" "$dst_blocked" >/dev/null 2>&1
  docker network rm "$test_net" >/dev/null 2>&1
  rm -rf "$tmp"
  local left=""
  [[ "$(egress_networks)" == "$want_networks" ]] || left="compute-egress is on [$(egress_networks)] after the run, want [$want_networks]"
  docker network inspect "$test_net" >/dev/null 2>&1 && left="$left; the test network $test_net is still there"
  if [[ -n "$left" ]]; then
    echo "[$LOG_PREFIX] FAIL: cleanup: $left" >&2
    rc=1
  fi
  if [[ $rc -eq 0 && "$failures" -gt 0 ]]; then
    echo "[$LOG_PREFIX] $failures failure(s)" >&2
    rc=1
  fi
  [[ $rc -ne 0 ]] || echo "[$LOG_PREFIX] all passed ($((SECONDS - t0)) s)"
  exit "$rc"
}
# An interrupt exits with its own status, which runs the EXIT trap with that status (never 0).
trap 'exit 130' INT
trap 'exit 143' TERM
trap cleanup EXIT

# --- 1. the stack's own guarantees --------------------------------------------------------------
status_of() { docker exec "$egress" cat /proc/1/status; }
caps="$(status_of | awk '/^Cap(Inh|Prm|Eff|Bnd|Amb):/ { if ($2 != "0000000000000000") print $1 " " $2 }')"
[[ -z "$caps" ]] && pass "the forwarder's process has no capability" || fail "the forwarder holds capabilities: $caps"
uid="$(status_of | awk '/^Uid:/ { print $2 }')"
[[ -n "$uid" && "$uid" != 0 ]] && pass "the forwarder runs as uid $uid (not root)" || fail "the forwarder runs as uid '${uid:-?}'"
[[ "$(status_of | awk '/^NoNewPrivs:/ { print $2 }')" == 1 ]] && pass "the forwarder has no-new-privileges" || fail "the forwarder lacks no-new-privileges"
[[ "$(docker exec "$egress" cat /proc/sys/net/ipv4/ip_forward)" == 0 ]] && pass "forwarding is off in the forwarder's namespace" ||
  fail "ip_forward is on in the forwarder's namespace"
for n in "$compute_net" "$control_net"; do
  inspect="$(docker network inspect -f '{{.Internal}} {{index .Options "com.docker.network.bridge.gateway_mode_ipv4"}} {{index .Options "com.docker.network.bridge.gateway_mode_ipv6"}}' "$n")"
  [[ "$inspect" == "true isolated isolated" ]] && pass "$n is internal and isolated (IPv4 and IPv6)" || fail "$n is '$inspect' (wanted: true isolated isolated)"
done

# --- 2. the destinations ------------------------------------------------------------------------
docker network inspect "$test_net" >/dev/null 2>&1 && die "$test_net exists"
docker network create --internal --subnet "$test_subnet" --label "chelabase.dev/egress-e2e=$rand" "$test_net" >/dev/null ||
  die "can't create $test_net on $test_subnet (is another run or network using it?)"
dst_password="$(hex 16)"
start_dst() { # start_dst <container> <ip> <aliases…>
  local name="$1" ip="$2" aliases=() a
  shift 2
  for a in "$@"; do aliases+=(--network-alias "$a"); done
  docker run -d --name "$name" --network "$test_net" --ip "$ip" "${aliases[@]}" --label "chelabase.dev/egress-e2e=$rand" \
    -e POSTGRES_USER=egress -e POSTGRES_PASSWORD="$dst_password" -e POSTGRES_DB=egressdb \
    "$pg_image" -c wal_level=logical >/dev/null
}
start_dst "$dst_allowed" "$allowed_ip" egress-allowed.test egress-denied.test
start_dst "$dst_second" "$second_ip" egress-second.test
start_dst "$dst_blocked" "$blocked_ip" egress-blocked.test
dst_sql() { docker exec -e PGPASSWORD="$dst_password" "$1" psql -h 127.0.0.1 -U egress -d egressdb -v ON_ERROR_STOP=1 -qtA -c "$2"; }
for dst in "$dst_allowed" "$dst_second" "$dst_blocked"; do
  ready=""
  for _ in $(seq 60); do
    # TCP on loopback: the image's temporary init server listens on its socket only.
    if docker exec "$dst" pg_isready -h 127.0.0.1 -U egress -d egressdb >/dev/null 2>&1; then
      ready=1
      break
    fi
    sleep 1
  done
  [[ -n "$ready" ]] || die "$dst never became ready"
done
dst_sql "$dst_allowed" "CREATE TABLE probe (who text); INSERT INTO probe VALUES ('allowed'); CREATE PUBLICATION p FOR TABLE probe;"
dst_sql "$dst_second" "CREATE TABLE probe (who text); INSERT INTO probe VALUES ('second');"
docker network connect "$test_net" "$egress"
attached=1
pass "the destinations run on $test_net ($test_subnet) and compute-egress is attached to it"

# --- 3. a database and its compute --------------------------------------------------------------
name="egress_$(hex 2)"
status="$(api POST /v1/databases "$(jq -nc --arg t "$DEMO_TENANT_ID" --arg p "$DEMO_PROJECT_ID" --arg n "$name" \
  '{tenant_id: $t, project_id: $p, name: $n}')")"
[[ "$status" == 202 || "$status" == 201 ]] || die "POST /v1/databases: $status $(cat "$tmp/body")"
db_id="$(jq -r '.database_id' "$tmp/body")"
[[ -n "$db_id" && "$db_id" != null ]] || die "POST /v1/databases returned no database_id: $(cat "$tmp/body")"
for i in $(seq 120); do
  status="$(api GET "/v1/databases/$db_id")"
  [[ "$status" == 200 ]] || die "GET /v1/databases/$db_id: $status $(cat "$tmp/body")"
  case "$(jq -r '.status' "$tmp/body")" in
    ready) break ;;
    failed) die "database $db_id failed: $(jq -r '.error' "$tmp/body")" ;;
  esac
  [[ $i -lt 120 ]] || die "database $db_id is still not ready after 120 s"
  sleep 1
done
status="$(api GET "/v1/databases/$db_id/connection")"
[[ "$status" == 200 ]] || die "GET /connection: $status $(cat "$tmp/body")"
pw="$(jq -r '.password' "$tmp/body")"
host="$(jq -r '.host' "$tmp/body")"
dbname="$(jq -r '.database' "$tmp/body")"
ep="${host%%.*}"
[[ "$ep" =~ ^ep-[0-9a-f]{12}$ ]] || die "unexpected host '$host' in /connection"

# sql <psql args…>: in the database as its owner, through the proxy (starts the compute).
sql() {
  PGPASSWORD="$pw" PGCONNECT_TIMEOUT=10 timeout 120 psql \
    "host=$host hostaddr=$PROXY_HOSTADDR port=$PROXY_PG_PORT user=postgres dbname=$dbname sslmode=verify-full sslrootcert=$ca" \
    -v ON_ERROR_STOP=1 -qtA "$@"
}
for i in $(seq 90); do
  sql -c 'select 1' >/dev/null 2>"$tmp/err" && break
  [[ $i -lt 90 ]] || die "the compute never answered through the proxy: $(cat "$tmp/err")"
  sleep 2
done
pass "database $name's compute answers through the proxy"

compute="$(docker ps -q --filter "name=compute-$ep" | head -n1)"
[[ -n "$compute" ]] || die "no running compute container for $ep"
dns="$(docker inspect -f '{{range .HostConfig.Dns}}{{.}} {{end}}' "$compute")"
[[ "$dns" == "$egress_ip " ]] && pass "the compute's resolver is the egress address $egress_ip" || fail "the compute's resolver is '$dns' (wanted $egress_ip)"
nets="$(docker inspect -f '{{range $n, $_ := .NetworkSettings.Networks}}{{$n}} {{end}}' "$compute" | tr ' ' '\n' | sed '/^$/d' | sort | tr '\n' ' ')"
[[ "$nets" == "$(printf '%s\n' "$compute_net" "$control_net" | sort | tr '\n' ' ')" ]] && pass "the compute is on its two internal networks only" ||
  fail "the compute is on [$nets]"

# --- 4. from the compute ------------------------------------------------------------------------
sql -c 'CREATE EXTENSION IF NOT EXISTS dblink' -c 'CREATE EXTENSION IF NOT EXISTS postgres_fdw' >/dev/null ||
  die "can't create dblink and postgres_fdw in the database"
conninfo() { # conninfo <host> <port>
  echo "host=$1 port=$2 dbname=egressdb user=egress password=$dst_password connect_timeout=$connect_timeout"
}
dblink_who() { # dblink_who <host> <port>: the destination's row through dblink
  sql -c "SELECT who FROM dblink('$(conninfo "$1" "$2")', 'SELECT who FROM probe') AS t(who text)"
}

# A listed destination answers, with its own row: dblink and postgres_fdw, at its real port and at
# its assigned listen port.
[[ "$(dblink_who egress-allowed.test 5432 2>"$tmp/err")" == allowed ]] && pass "dblink to egress-allowed.test:5432 returns the first destination's row" ||
  fail "dblink to egress-allowed.test:5432: $(cat "$tmp/err")"
[[ "$(dblink_who egress-second.test 15432 2>"$tmp/err")" == second ]] && pass "dblink to egress-second.test:15432 returns the second destination's row" ||
  fail "dblink to egress-second.test:15432: $(cat "$tmp/err")"
fdw_setup() { # fdw_setup <label> <host> <port>
  echo "CREATE SERVER s_$1 FOREIGN DATA WRAPPER postgres_fdw OPTIONS (host '$2', port '$3', dbname 'egressdb', connect_timeout '$connect_timeout');
CREATE USER MAPPING FOR CURRENT_USER SERVER s_$1 OPTIONS (user 'egress', password '$dst_password');
CREATE FOREIGN TABLE ft_$1 (who text) SERVER s_$1 OPTIONS (table_name 'probe');"
}
sql -c "$(fdw_setup allowed egress-allowed.test 5432)" -c "$(fdw_setup second egress-second.test 15432)" >/dev/null 2>"$tmp/err" ||
  fail "creating the foreign servers: $(cat "$tmp/err")"
[[ "$(sql -c 'SELECT who FROM ft_allowed' 2>"$tmp/err")" == allowed ]] && pass "a postgres_fdw foreign table on egress-allowed.test:5432 returns the first destination's row" ||
  fail "postgres_fdw on egress-allowed.test:5432: $(cat "$tmp/err")"
[[ "$(sql -c 'SELECT who FROM ft_second' 2>"$tmp/err")" == second ]] && pass "a postgres_fdw foreign table on egress-second.test:15432 returns the second destination's row" ||
  fail "postgres_fdw on egress-second.test:15432: $(cat "$tmp/err")"

# expect_refused <what> <regex> <host> <port> [max seconds]: the dblink fails, with an error that
# matches, within the bound.
expect_refused() {
  local what="$1" re="$2" h="$3" p="$4" bound="${5:-$((connect_timeout + 15))}" start=$SECONDS out
  if out="$(dblink_who "$h" "$p" 2>&1)"; then
    fail "$what: the connection worked: $out"
  elif ! grep -Eqi "$re" <<<"$out"; then
    fail "$what: failed, but not as expected ($re): $out"
  elif ((SECONDS - start > bound)); then
    fail "$what: failed after $((SECONDS - start)) s (bound $bound s)"
  else
    pass "$what fails ($((SECONDS - start)) s): $(tr '\n' ' ' <<<"$out" | cut -c1-160)"
  fi
}
expect_refused "an unlisted name (egress-denied.test, on the same destination)" 'translate host name|not known|resolv' egress-denied.test 5432
expect_refused "a listed name on an unlisted port (egress-allowed.test:5433)" 'refused|timeout expired|Connection refused' egress-allowed.test 5433
expect_refused "a destination's own address ($allowed_ip:5432)" 'unreachable|timeout expired|could not connect|no route' "$allowed_ip" 5432
# A listed name at a non-public address: the forwarder takes the connection and refuses to dial.
expect_refused "the listed host.docker.internal entry (not a public address)" 'closed the connection|could not receive|EOF|reset|terminat|no response|timeout expired|server closed' host.docker.internal 15433
refused_log="$(docker logs --since "$since" "$egress" 2>&1 | sed $'s/\x1b\\[[0-9;]*m//g' | grep 'outcome="refused"' | grep 'host.docker.internal' || true)"
[[ -n "$refused_log" ]] && pass "the forwarder's log has the refusal with its reason: $(tail -n1 <<<"$refused_log" | sed 's/^[^ ]* *//' | cut -c1-200)" ||
  fail "the forwarder's log has no refusal for host.docker.internal since $since"

# A listed name whose (public) address is in the deny list: refused on every connection, with the
# deny list as the reason (the log line names the address and why it wasn't dialled).
for attempt in 1 2; do
  expect_refused "the listed egress-blocked.test entry (its address is in the deny list), connection $attempt" 'closed the connection|could not receive|EOF|reset|terminat|no response|timeout expired|server closed' egress-blocked.test 15436
done
denied_log="$(docker logs --since "$since" "$egress" 2>&1 | sed $'s/\x1b\\[[0-9;]*m//g' | grep 'outcome="refused"' | grep 'egress-blocked.test' | grep "$blocked_ip" | grep 'denied by CHELABASE_COMPUTE_EGRESS_DENY' || true)"
[[ "$(grep -c . <<<"$denied_log")" -ge 2 ]] && pass "the forwarder's log has a deny-list refusal per connection: $(tail -n1 <<<"$denied_log" | sed 's/^[^ ]* *//' | cut -c1-220)" ||
  fail "the forwarder's log has fewer than two deny-list refusals for egress-blocked.test since $since: ${denied_log:-none}"

# A subscription to a publication on the destination is created (and copies its table).
sql -c 'CREATE TABLE probe (who text)' >/dev/null 2>"$tmp/err" || fail "creating the subscriber table: $(cat "$tmp/err")"
if sql -c "CREATE SUBSCRIPTION egress_sub CONNECTION '$(conninfo egress-allowed.test 5432)' PUBLICATION p WITH (connect = true)" >"$tmp/out" 2>"$tmp/err"; then
  slot="$(dst_sql "$dst_allowed" "SELECT slot_name FROM pg_replication_slots WHERE slot_name = 'egress_sub'")"
  [[ "$slot" == egress_sub ]] && pass "a subscription to the publication on egress-allowed.test was created (its slot exists on the destination)" ||
    fail "the subscription was created but the destination has no slot 'egress_sub'"
  copied=""
  for _ in $(seq 30); do
    [[ "$(sql -c 'SELECT who FROM probe' 2>/dev/null)" == allowed ]] && {
      copied=1
      break
    }
    sleep 1
  done
  [[ -n "$copied" ]] && pass "the subscription copied the publisher's row" || fail "the subscription didn't copy the row within 30 s"
  sql -c 'ALTER SUBSCRIPTION egress_sub DISABLE' -c 'ALTER SUBSCRIPTION egress_sub SET (slot_name = NONE)' -c 'DROP SUBSCRIPTION egress_sub' >/dev/null 2>&1 || true
else
  fail "CREATE SUBSCRIPTION ... WITH (connect = true): $(cat "$tmp/err")"
fi
