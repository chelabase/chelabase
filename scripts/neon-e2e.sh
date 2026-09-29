#!/usr/bin/env bash
# Live check of roadmap step 3 (needs just neon-up and just dev). A hand-made compute:
# - answers through Neon's proxy over Postgres wire, SQL over HTTP and the serverless driver;
# - refuses a wrong password;
# - cancels a running query;
# - takes a changed password at once (metadata, then notify-attach, then /password_updated).
# Replaces a demo compute left by just neon-compute-demo. KEEP=1 leaves the compute running
# on failure.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
LOG_PREFIX=neon-e2e
# shellcheck source=lib/neon-common.sh
. scripts/lib/neon-common.sh
[[ -z "${CHELADB_INTERNAL_ADDR:-}" ]] || check_internal_addr "$CHELADB_INTERNAL_ADDR"
INTERNAL="${CHELADB_INTERNAL_URL:-http://${CHELADB_INTERNAL_ADDR:-127.0.0.1:8083}}"
STORCON="${STORCON_URL:-http://127.0.0.1:1234}"
HOOK_TOKEN="${CHELADB_STORCON_HOOK_TOKEN:-cheladb-dev-storcon-hook}"
tmp="$(mktemp -d)"

pass() { echo "ok   $*"; }
# This script's own format (overrides the shared die).
die() {
  echo "FAIL $*" >&2
  exit 1
}
cleanup() {
  if [[ "${KEEP:-}" != 1 ]]; then scripts/neon-compute-demo.sh --down >/dev/null || true; fi
  rm -rf "$tmp"
}
# refused <file>: the proxy's own auth failure, not a connection error or a downed proxy.
refused() { grep -q "password authentication failed for user 'postgres'" "$1"; }

# Built up front: a cold compile between the first connect (step 1) and the cache check
# (step 7) could outlast the proxy's 1-minute cache TTL.
cargo build -q -p cheladb || die "cargo build -p cheladb"
# The new password for step 7, and its verifier, made before any connect for the same reason.
new="$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')"
verifier="$(printf '%s' "$new" | cargo run -q -p cheladb -- neon verifier)" || die "cheladb neon verifier"

# neon-compute-demo refuses to create over an existing demo: replace it.
if [[ -f .cheladb/neon/demo-compute.env ]]; then
  scripts/neon-compute-demo.sh --down >/dev/null || die "removing the previous demo compute"
fi
scripts/neon-compute-demo.sh >/dev/null || die "neon-compute-demo failed"
trap cleanup EXIT
# shellcheck disable=SC1091
source .cheladb/neon/demo-compute.env
ca="$root/.cheladb/neon/proxy-ca.crt"
host="$DEMO_ENDPOINT.localtest.me"

# sql <password> <psql args…>
sql() {
  local pw="$1"
  shift
  PGPASSWORD="$pw" PGCONNECT_TIMEOUT=10 psql \
    "host=$host hostaddr=127.0.0.1 port=4432 user=postgres dbname=postgres sslmode=verify-full sslrootcert=$ca" \
    -v ON_ERROR_STOP=1 -qtA "$@"
}
# http_sql <password> <query>: prints the HTTP status; the body goes to $tmp/body.
http_sql() {
  curl -sS --max-time 30 --cacert "$ca" -o "$tmp/body" -w '%{http_code}' -X POST "https://$host:4444/sql" \
    -H "Neon-Connection-String: postgresql://postgres:$1@$host/postgres" \
    -H 'content-type: application/json' \
    -d "$(jq -nc --arg q "$2" '{query: $q, params: []}')" || true
}
# http_refused <password>: the proxy answers /sql with its auth failure (400 and the message).
http_refused() {
  local status
  status="$(http_sql "$1" 'select 1')"
  [[ "$status" == 400 ]] && jq -e '.message | test("password authentication failed")' "$tmp/body" >/dev/null
}

# 1. The compute pulls its spec and answers through the proxy ("the database system is
# starting up" until then).
for i in $(seq 90); do
  if sql "$DEMO_PASSWORD" -c 'select 1' >/dev/null 2>"$tmp/err"; then break; fi
  [[ $i -lt 90 ]] || die "the compute never answered through the proxy: $(cat "$tmp/err")"
  sleep 2
done
pass "the compute answers through the proxy on 4432"

# 2. Postgres wire.
sql "$DEMO_PASSWORD" -c 'CREATE TABLE e2e (n int); INSERT INTO e2e VALUES (1), (2), (3);'
pass "psql created a table and inserted 3 rows"

# 3. SQL over HTTP.
status="$(http_sql "$DEMO_PASSWORD" 'SELECT count(*)::int AS n FROM e2e')"
[[ "$status" == 200 && "$(jq -r '.rows[0].n' "$tmp/body")" == 3 ]] ||
  die "SQL over HTTP: $status $(cat "$tmp/body")"
pass "SQL over HTTP on 4444 read the rows back"

# 4. The serverless driver, over WebSocket and HTTP.
DATABASE_URL="postgresql://postgres:$DEMO_PASSWORD@$host/postgres" NODE_EXTRA_CA_CERTS="$ca" EXPECT=3 \
  pnpm --filter @cheladb/neon-e2e --silent check || die "@neondatabase/serverless"

# 5. A wrong password is refused, over Postgres wire and over HTTP.
if sql wrong-password -c 'select 1' >/dev/null 2>"$tmp/err"; then die "a wrong password connected"; fi
refused "$tmp/err" || die "unexpected refusal on 4432: $(cat "$tmp/err")"
http_refused wrong-password || die "unexpected answer to a wrong password on /sql: $(cat "$tmp/body")"
pass "a wrong password is refused (4432 and /sql)"

# 6. Query cancel goes through the proxy (it keeps cancel keys in Valkey).
started=$SECONDS
PGPASSWORD="$DEMO_PASSWORD" timeout -s INT 2 psql \
  "host=$host hostaddr=127.0.0.1 port=4432 user=postgres dbname=postgres sslmode=verify-full sslrootcert=$ca" \
  -c 'SELECT pg_sleep(30)' >/dev/null 2>"$tmp/err" || true
[[ $((SECONDS - started)) -lt 10 ]] || die "cancel took $((SECONDS - started)) s"
grep -q 'canceling statement due to user request' "$tmp/err" || die "query cancel: $(cat "$tmp/err")"
pass "query cancel works through the proxy"

# 7. A changed password works at once. Order: metadata, computes, then the proxy's cache.
metadata_sql -v ep="$DEMO_ENDPOINT" -v v="$verifier" \
  <<<"UPDATE neon_roles SET scram_verifier = :'v' WHERE endpoint_id = :'ep' AND name = 'postgres';"
# The proxy still has the old secret cached: this proves the cache is on.
sql "$DEMO_PASSWORD" -c 'select 1' >/dev/null 2>"$tmp/err" ||
  die "the old password stopped working before the cache was cleared (is the cache on?): $(cat "$tmp/err")"
pass "the proxy serves the cached secret until it's told otherwise"
status="$(curl -sS --max-time 60 -o "$tmp/body" -w '%{http_code}' -X PUT "$INTERNAL/storcon/notify-attach" \
  -H "Authorization: Bearer $HOOK_TOKEN" -H 'content-type: application/json' \
  -d "{\"tenant_id\":\"$DEMO_TENANT\",\"preferred_az\":null,\"stripe_size\":null,\"shards\":[]}")" || true
[[ "$status" == 200 ]] || die "notify-attach: $status $(cat "$tmp/body")"
receivers="$(cargo run -q -p cheladb -- neon password-updated --endpoint "$DEMO_ENDPOINT" --role postgres)" ||
  die "password-updated"
[[ "$receivers" =~ ^[0-9]+$ && "$receivers" -ge 1 ]] || die "password-updated reached $receivers proxies"
sql "$new" -c 'select 1' >/dev/null 2>"$tmp/err" || die "the new password was refused right after the change: $(cat "$tmp/err")"
if sql "$DEMO_PASSWORD" -c 'select 1' >/dev/null 2>"$tmp/err"; then die "the old password still connects on 4432"; fi
refused "$tmp/err" || die "unexpected refusal of the old password on 4432: $(cat "$tmp/err")"
[[ "$(http_sql "$new" 'select 1')" == 200 ]] || die "the new password over HTTP: $(cat "$tmp/body")"
http_refused "$DEMO_PASSWORD" || die "the old password over HTTP wasn't refused: $(cat "$tmp/body")"
pass "a changed password works at once ($receivers proxy notified); the old one is refused (4432 and /sql)"

# 8. Clean up; the tenant is gone.
trap 'rm -rf "$tmp"' EXIT
scripts/neon-compute-demo.sh --down >/dev/null || die "neon-compute-demo --down failed"
status="$(curl -sS --max-time 30 -o /dev/null -w '%{http_code}' "$STORCON/control/v1/tenant/$DEMO_TENANT")" ||
  die "the storage controller didn't answer"
[[ "$status" == 404 ]] || die "tenant $DEMO_TENANT still exists (HTTP $status)"
pass "neon-compute-demo --down removed the compute and the tenant"
echo "all passed"
