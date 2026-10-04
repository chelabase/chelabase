#!/usr/bin/env bash
# Live check of roadmap step 4, through the public API (needs just deps-up and just dev):
# - creates a database and waits until it is ready;
# - psql, SQL over HTTP and the serverless driver answer through Neon's proxy;
# - a wrong password is refused, and query cancel works;
# - a password reset takes effect at once (the old password is refused, on the Postgres port and /sql);
# - deleting the database removes the compute and the storage-controller tenant.
# KEEP=1 leaves the database in place on failure.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
LOG_PREFIX=neon-e2e
# shellcheck source=lib/neon-common.sh
. scripts/lib/neon-common.sh
[[ -z "${CHELABASE_INTERNAL_ADDR:-}" ]] || check_internal_addr "$CHELABASE_INTERNAL_ADDR"
STORCON="${STORCON_URL:-http://127.0.0.1:1234}"
GATEWAY="${GATEWAY_URL:-http://127.0.0.1:8000}"
PROXY_PG_PORT="${CHELABASE_PROXY_PG_PORT:-4432}"
PROXY_HTTP_PORT="${CHELABASE_PROXY_HTTP_PORT:-4444}"
DEMO_TENANT_ID="00000000-0000-4000-8000-000000000001"
DEMO_PROJECT_ID="00000000-0000-4000-8000-000000000002"
tmp="$(mktemp -d)"
db_id=""

pass() { echo "ok   $*"; }
# This script's own format (overrides the shared die).
die() {
  echo "FAIL $*" >&2
  exit 1
}
cleanup() {
  if [[ -n "$db_id" && "${KEEP:-}" != 1 ]]; then
    api DELETE "/v1/databases/$db_id" >/dev/null || true
  fi
  rm -rf "$tmp"
}

# refused <file>: the proxy's own auth failure, not a connection error or a downed proxy.
refused() { grep -q "password authentication failed for user 'postgres'" "$1"; }

# Built up front: a cold compile between the first connect and the cache check (step 5) could
# outlast the proxy's 1-minute cache TTL.
cargo build -q -p chelabase || die "cargo build -p chelabase"

# The demo tenant's identity (the token is minted per request: it is short-lived).
[[ -f .chelabase/zitadel/demo.env ]] || die ".chelabase/zitadel/demo.env is missing (run just dev)"
set -a
# shellcheck disable=SC1091
source .chelabase/zitadel/demo.env
set +a
ca="$root/.chelabase/neon/proxy-ca.crt"
[[ -s "$ca" ]] || die "$ca is missing (run just deps-up)"

trap cleanup EXIT

# 1. Create a database through the gateway and wait until it is ready.
name="e2e_$(hex 2)"
status="$(api POST /v1/databases "$(jq -nc --arg t "$DEMO_TENANT_ID" --arg p "$DEMO_PROJECT_ID" --arg n "$name" \
  '{tenant_id: $t, project_id: $p, name: $n}')")"
[[ "$status" == 202 || "$status" == 201 ]] || die "POST /v1/databases: $status $(cat "$tmp/body")"
db_id="$(jq -r '.database_id' "$tmp/body")"
[[ -n "$db_id" && "$db_id" != null ]] || die "POST /v1/databases returned no database_id: $(cat "$tmp/body")"
db_status=""
for i in $(seq 120); do
  status="$(api GET "/v1/databases/$db_id")"
  [[ "$status" == 200 ]] || die "GET /v1/databases/$db_id: $status $(cat "$tmp/body")"
  db_status="$(jq -r '.status' "$tmp/body")"
  case "$db_status" in
    ready) break ;;
    failed) die "database $db_id failed: $(jq -r '.error' "$tmp/body")" ;;
  esac
  [[ $i -lt 120 ]] || die "database $db_id is still $db_status after 120 s"
  sleep 1
done
pass "database $name ($db_id) is ready"

# The storage controller's attach hook reached the orchestrator's internal listener (:8083/storcon):
# once it answers, the tenant's compute notification is no longer pending.
hook_tenant="$(metadata_sql -v id="$db_id" <<<"SELECT neon_tenant_id FROM databases WHERE id = :'id';")"
[[ "$hook_tenant" =~ ^[0-9a-f]{32}$ ]] || die "no tenant for database $db_id in metadata (got '$hook_tenant')"
for i in $(seq 20); do
  shards="$(call GET "$STORCON/control/v1/tenant/$hook_tenant" | jq -c '.shards')"
  jq -e 'length > 0 and all(.[]; .is_pending_compute_notification == false)' <<<"$shards" >/dev/null && break
  [[ $i -lt 20 ]] || die "tenant $hook_tenant's compute notification is still pending after 10 s (is the orchestrator's internal listener answering /storcon?): $shards"
  sleep 0.5
done
pass "the storage controller's hook reached the orchestrator (tenant $hook_tenant)"

# 2. The owner credentials.
status="$(api GET "/v1/databases/$db_id/connection")"
[[ "$status" == 200 ]] || die "GET /connection: $status $(cat "$tmp/body")"
old="$(jq -r '.password' "$tmp/body")"
host="$(jq -r '.host' "$tmp/body")"
dbname="$(jq -r '.database' "$tmp/body")"
http_url="$(jq -r '.http_url' "$tmp/body")"
uri="$(jq -r '.uri' "$tmp/body")"
ep="${host%%.*}"
[[ "$ep" =~ ^ep-[0-9a-f]{12}$ ]] || die "unexpected host '$host' in /connection"
[[ "$uri" == "postgresql://postgres:$old@$host:${PROXY_PG_PORT}/$dbname?sslmode=verify-full" ]] ||
  die "unexpected uri in /connection (host $host, database $dbname)"
pass "/connection reveals $host, database $dbname"

# sql <password> <psql args…>
sql() {
  local pw="$1"
  shift
  PGPASSWORD="$pw" PGCONNECT_TIMEOUT=10 psql \
    "host=$host hostaddr=127.0.0.1 port=${PROXY_PG_PORT} user=postgres dbname=$dbname sslmode=verify-full sslrootcert=$ca" \
    -v ON_ERROR_STOP=1 -qtA "$@"
}
# http_sql <password> <query>: prints the HTTP status; the body goes to $tmp/body.
http_sql() {
  curl -sS --max-time 30 --cacert "$ca" -o "$tmp/body" -w '%{http_code}' -X POST "$http_url" \
    -H "Neon-Connection-String: postgresql://postgres:$1@$host/$dbname" \
    -H 'content-type: application/json' \
    -d "$(jq -nc --arg q "$2" '{query: $q, params: []}')" || true
}
# http_refused <password>: the proxy answers /sql with its auth failure (400 and the message).
http_refused() {
  local status
  status="$(http_sql "$1" 'select 1')"
  [[ "$status" == 400 ]] && jq -e '.message | test("password authentication failed")' "$tmp/body" >/dev/null
}

# 3. The compute answers through the proxy ("the database system is starting up" until then).
for i in $(seq 90); do
  if sql "$old" -c 'select 1' >/dev/null 2>"$tmp/err"; then break; fi
  [[ $i -lt 90 ]] || die "the compute never answered through the proxy: $(cat "$tmp/err")"
  sleep 2
done
pass "the compute answers through the proxy on $PROXY_PG_PORT"

# 4. Postgres wire.
sql "$old" -c 'CREATE TABLE e2e (n int); INSERT INTO e2e VALUES (1), (2), (3);'
pass "psql created a table and inserted 3 rows"

# SQL over HTTP.
status="$(http_sql "$old" 'SELECT count(*)::int AS n FROM e2e')"
[[ "$status" == 200 && "$(jq -r '.rows[0].n' "$tmp/body")" == 3 ]] ||
  die "SQL over HTTP: $status $(cat "$tmp/body")"
pass "SQL over HTTP on $PROXY_HTTP_PORT read the rows back"

# The serverless driver, over WebSocket and HTTP.
DATABASE_URL="postgresql://postgres:$old@$host/$dbname" NODE_EXTRA_CA_CERTS="$ca" EXPECT=3 \
  CHELABASE_PROXY_HTTP_PORT="$PROXY_HTTP_PORT" \
  pnpm --filter @chelabase/neon-e2e --silent check || die "@neondatabase/serverless"

# A wrong password is refused, over Postgres wire and over HTTP.
if sql wrong-password -c 'select 1' >/dev/null 2>"$tmp/err"; then die "a wrong password connected"; fi
refused "$tmp/err" || die "unexpected refusal on $PROXY_PG_PORT: $(cat "$tmp/err")"
http_refused wrong-password || die "unexpected answer to a wrong password on /sql: $(cat "$tmp/body")"
pass "a wrong password is refused ($PROXY_PG_PORT and /sql)"

# Query cancel goes through the proxy (it keeps cancel keys in Valkey).
started=$SECONDS
PGPASSWORD="$old" timeout -s INT 2 psql \
  "host=$host hostaddr=127.0.0.1 port=${PROXY_PG_PORT} user=postgres dbname=$dbname sslmode=verify-full sslrootcert=$ca" \
  -c 'SELECT pg_sleep(30)' >/dev/null 2>"$tmp/err" || true
[[ $((SECONDS - started)) -lt 10 ]] || die "cancel took $((SECONDS - started)) s"
grep -q 'canceling statement due to user request' "$tmp/err" || die "query cancel: $(cat "$tmp/err")"
pass "query cancel works through the proxy"

# 5. A password reset works at once. The proxy has the old secret cached from the connects
# above: only the worker's /password_updated publish can make it refuse the old password.
sql "$old" -c 'select 1' >/dev/null 2>"$tmp/err" || die "the old password stopped working before the reset: $(cat "$tmp/err")"
status="$(api POST "/v1/databases/$db_id/reset-password")"
[[ "$status" == 204 ]] || die "POST /reset-password: $status $(cat "$tmp/body")"
status="$(api GET "/v1/databases/$db_id/connection")"
[[ "$status" == 200 ]] || die "GET /connection after the reset: $status $(cat "$tmp/body")"
new="$(jq -r '.password' "$tmp/body")"
[[ -n "$new" && "$new" != null && "$new" != "$old" ]] || die "the reset did not change the password"
sql "$new" -c 'select 1' >/dev/null 2>"$tmp/err" || die "the new password was refused right after the reset: $(cat "$tmp/err")"
if sql "$old" -c 'select 1' >/dev/null 2>"$tmp/err"; then die "the old password still connects on $PROXY_PG_PORT"; fi
refused "$tmp/err" || die "unexpected refusal of the old password on $PROXY_PG_PORT: $(cat "$tmp/err")"
[[ "$(http_sql "$new" 'select 1')" == 200 ]] || die "the new password over HTTP: $(cat "$tmp/body")"
http_refused "$old" || die "the old password over HTTP wasn't refused: $(cat "$tmp/body")"
pass "a reset password works at once; the old one is refused ($PROXY_PG_PORT and /sql)"

# 6. Delete: the compute and the tenant are gone.
tenant="$(metadata_sql -v ep="$ep" <<<"SELECT tenant_id FROM neon_endpoints WHERE id = :'ep';")"
[[ "$tenant" =~ ^[0-9a-f]{32}$ ]] || die "no tenant for endpoint $ep in metadata (got '$tenant')"
status="$(api DELETE "/v1/databases/$db_id")"
[[ "$status" == 202 ]] || die "DELETE /v1/databases/$db_id: $status $(cat "$tmp/body")"
for i in $(seq 60); do
  status="$(api GET "/v1/databases/$db_id")"
  [[ "$status" != 404 ]] || break
  [[ $i -lt 60 ]] || die "database $db_id still answers ($status) 60 s after DELETE"
  sleep 1
done
db_id=""
[[ -z "$(docker ps -aq --filter "name=^chelabase-compute-$ep\$")" ]] || die "container chelabase-compute-$ep still exists"
status="$(curl -sS --max-time 30 -o /dev/null -w '%{http_code}' "$STORCON/control/v1/tenant/$tenant")" ||
  die "the storage controller didn't answer"
[[ "$status" == 404 ]] || die "tenant $tenant still exists (HTTP $status)"
rows="$(metadata_sql -v ep="$ep" <<<"SELECT count(*) FROM neon_endpoints WHERE id = :'ep';")"
[[ "$rows" == 0 ]] || die "endpoint $ep still has $rows row(s) in neon_endpoints"
pass "delete removed the compute, tenant $tenant and the endpoint row"
echo "all passed"
