#!/usr/bin/env bash
# Live check of roadmap step 4, through the public API (needs the stack: just workbench up):
# - creates a database and waits until it is ready;
# - psql, SQL over HTTP and the serverless driver answer through Neon's proxy;
# - each of those sessions reaches the compute over TLS (pg_stat_ssl), and a side proxy trusting
#   another CA is refused at the compute (needs a proxy recreated with compute-hop-tls);
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
STORCON="${STORCON_URL:-http://storage-controller:1234}"
GATEWAY="${GATEWAY_URL:-$CHELABASE_ENTRY_API_URL}"
# The runner dials the proxy inside the stack network (CHELABASE_PROXY_PG_PORT); connection
# strings the orchestrator hands out name the stack's port on the host (CHELABASE_PROXY_PORT).
PROXY_PG_PORT="${CHELABASE_PROXY_PG_PORT:-4432}"
URI_PG_PORT="${CHELABASE_PROXY_PORT:?run through just workbench run}"
PROXY_HOSTADDR="${CHELABASE_PROXY_HOSTADDR:?run through just workbench run}"
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
[[ -x "${CHELABASE_CLI:-}" ]] || die "CHELABASE_CLI is unset or not executable (run through: just workbench run scripts/<name>.sh)"

ca="${CHELABASE_PROXY_CA_FILE:?run through just workbench run}"
[[ -s "$ca" ]] || die "$ca is missing (run just workbench up)"

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
[[ "$uri" == "postgresql://postgres:$old@$host:${URI_PG_PORT}/$dbname?sslmode=verify-full" ]] ||
  die "unexpected uri in /connection (host $host, database $dbname)"
pass "/connection reveals $host, database $dbname"

# sql <password> <psql args…>
sql() {
  local pw="$1"
  shift
  PGPASSWORD="$pw" PGCONNECT_TIMEOUT=10 psql \
    "host=$host hostaddr=$PROXY_HOSTADDR port=${PROXY_PG_PORT} user=postgres dbname=$dbname sslmode=verify-full sslrootcert=$ca" \
    -v ON_ERROR_STOP=1 -qtA "$@"
}
# http_sql <password> <query>: prints the HTTP status; the body goes to $tmp/body.
http_sql() {
  # <ep>.localtest.me resolves to loopback in the runner: dial the proxy's address instead.
  curl -sS --max-time 30 --cacert "$ca" --connect-to "$host:$PROXY_HTTP_PORT:$PROXY_HOSTADDR:$PROXY_HTTP_PORT" -o "$tmp/body" -w '%{http_code}' -X POST "$http_url" \
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

# compute-hop-tls m1: the proxy's session to the compute runs over TLS, on the Postgres port
# and on /sql (the WebSocket session is checked by the driver above).
got="$(sql "$old" -c 'SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()')"
[[ "$got" == t ]] || die "the proxy's Postgres session to the compute is not TLS (pg_stat_ssl.ssl = '$got')"
status="$(http_sql "$old" 'SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()')"
[[ "$status" == 200 && "$(jq -r '.rows[0].ssl' "$tmp/body")" == true ]] ||
  die "the proxy's /sql session to the compute is not TLS: $status $(cat "$tmp/body")"
pass "the proxy reaches the compute over TLS ($PROXY_PG_PORT and /sql)"

# A proxy that trusts another CA is refused by every compute, and doesn't fall back to
# plaintext. A side proxy is the stack's own (its image, user, mounts, entrypoint and command)
# with NEON_INTERNAL_CA_FILE pointing at a file written at start. With the compute CA it logs in
# (the harness works); with a throwaway CA the login fails at the compute's certificate.
project="${CHELABASE_COMPOSE_PROJECT:?run through just workbench run}"
proxy_c="$(docker ps -q --filter "label=com.docker.compose.project=$project" --filter "label=com.docker.compose.service=proxy" | head -n1)"
[[ -n "$proxy_c" ]] || die "no running proxy container in $project"
side_proxies=()
# side_proxy <name> <CA PEM>: starts it; prints its address on the default network. It runs in
# a command substitution: the caller adds <name> to side_proxies first, for the cleanup.
side_proxy() {
  local name="$1" pem="$2" image user argv=()
  image="$(docker inspect -f '{{.Config.Image}}' "$proxy_c")"
  user="$(docker inspect -f '{{.Config.User}}' "$proxy_c")"
  mapfile -t argv < <(docker inspect "$proxy_c" | jq -r '.[0].Config | (.Entrypoint // []) + (.Cmd // []) | .[]')
  [[ "${argv[0]:-}" == /bin/sh ]] || die "the proxy's entrypoint isn't /bin/sh: ${argv[*]}"
  docker create -q --name "$name" --user "$user" --volumes-from "$proxy_c" --network "${project}_default" \
    -e SIDE_CA_PEM="$pem" -e NEON_INTERNAL_CA_FILE=/tmp/side-ca.crt --entrypoint /bin/sh "$image" \
    -c 'printf "%s\n" "$SIDE_CA_PEM" >/tmp/side-ca.crt && exec "$@"' side "${argv[@]}" >/dev/null ||
    die "could not create the side proxy $name"
  docker network connect "${project}_compute" "$name"
  docker start "$name" >/dev/null
  local ip
  ip="$(docker inspect -f "{{with index .NetworkSettings.Networks \"${project}_default\"}}{{.IPAddress}}{{end}}" "$name")"
  for i in $(seq 30); do
    timeout 2 bash -c "</dev/tcp/$ip/4432" 2>/dev/null && break
    [[ $i -lt 30 ]] || die "the side proxy $name never listened: $(docker logs --tail 20 "$name" 2>&1)"
    sleep 1
  done
  echo "$ip"
}
side_cleanup() {
  for c in "${side_proxies[@]}"; do docker rm -f "$c" >/dev/null 2>&1 || true; done
}
trap 'side_cleanup; cleanup' EXIT
# side_sql <address> <psql args…>: the owner's login through that proxy.
side_sql() {
  local addr="$1"
  shift
  PGPASSWORD="$old" PGCONNECT_TIMEOUT=30 psql \
    "host=$host hostaddr=$addr port=4432 user=postgres dbname=$dbname sslmode=verify-full sslrootcert=$ca" \
    -v ON_ERROR_STOP=1 -qtA "$@"
}
suffix="$(hex 3)"
compute_ca_pem="$(docker exec "$proxy_c" cat /run/chelabase/secrets/compute-ca/ca.crt)" ||
  die "the proxy has no compute CA at /run/chelabase/secrets/compute-ca/ca.crt (recreate it: the stack predates compute-hop-tls)"
good="neon-e2e-ca-good-$suffix"
side_proxies+=("$good")
good_ip="$(side_proxy "$good" "$compute_ca_pem")"
got="$(side_sql "$good_ip" -c 'SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()' 2>"$tmp/err")" ||
  die "a side proxy with the compute CA could not log in: $(cat "$tmp/err")"
[[ "$got" == t ]] || die "a side proxy with the compute CA reached the compute without TLS"
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout "$tmp/other-ca.key" \
  -out "$tmp/other-ca.crt" -days 1 -subj "/CN=neon-e2e throwaway CA" 2>/dev/null || die "openssl could not make a CA"
bad="neon-e2e-ca-bad-$suffix"
side_proxies+=("$bad")
bad_ip="$(side_proxy "$bad" "$(cat "$tmp/other-ca.crt")")"
if side_sql "$bad_ip" -c 'select 1' >/dev/null 2>"$tmp/err"; then
  die "a proxy trusting another CA logged in to the compute"
fi
grep -q "Couldn't connect to compute node" "$tmp/err" || die "unexpected refusal through a proxy with another CA: $(cat "$tmp/err")"
docker logs "$bad" 2>&1 | grep -qiE 'certificate|UnknownIssuer' ||
  die "the proxy with another CA failed, but not at the compute's certificate: $(docker logs --tail 20 "$bad" 2>&1)"
side_cleanup
side_proxies=()
pass "a proxy trusting another CA can't log in (the compute's certificate is refused, no plaintext fallback)"

# A wrong password is refused, over Postgres wire and over HTTP.
if sql wrong-password -c 'select 1' >/dev/null 2>"$tmp/err"; then die "a wrong password connected"; fi
refused "$tmp/err" || die "unexpected refusal on $PROXY_PG_PORT: $(cat "$tmp/err")"
http_refused wrong-password || die "unexpected answer to a wrong password on /sql: $(cat "$tmp/body")"
pass "a wrong password is refused ($PROXY_PG_PORT and /sql)"

# Query cancel goes through the proxy (it keeps cancel keys in Valkey).
started=$SECONDS
PGPASSWORD="$old" timeout -s INT 2 psql \
  "host=$host hostaddr=$PROXY_HOSTADDR port=${PROXY_PG_PORT} user=postgres dbname=$dbname sslmode=verify-full sslrootcert=$ca" \
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
[[ -z "$(docker ps -aq --filter "label=chelabase.dev/stack=$CHELABASE_STACK" --filter "label=chelabase.dev/endpoint-id=$ep")" ]] || die "the compute container of endpoint $ep still exists"
status="$(curl -sS --max-time 30 -o /dev/null -w '%{http_code}' "${STORAGE_AUTH[@]}" "$STORCON/control/v1/tenant/$tenant")" ||
  die "the storage controller didn't answer"
[[ "$status" == 404 ]] || die "tenant $tenant still exists (HTTP $status)"
rows="$(metadata_sql -v ep="$ep" <<<"SELECT count(*) FROM neon_endpoints WHERE id = :'ep';")"
[[ "$rows" == 0 ]] || die "endpoint $ep still has $rows row(s) in neon_endpoints"
pass "delete removed the compute, tenant $tenant and the endpoint row"
echo "all passed"
