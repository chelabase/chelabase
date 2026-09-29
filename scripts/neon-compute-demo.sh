#!/usr/bin/env bash
# A hand-made Neon compute behind the proxy (roadmap step 3):
# - a tenant and a timeline in the storage controller;
# - an endpoint with a `postgres` role and database in metadata;
# - a compute container that pulls its spec from the orchestrator's internal listener.
#
#   scripts/neon-compute-demo.sh          create (needs just neon-up and a running orchestrator)
#   scripts/neon-compute-demo.sh --down   remove the container, the metadata rows and the tenant
#
# Writes .cheladb/neon/demo-compute.env (the password is in it) and .cheladb/neon/proxy-ca.crt.
# Local dev only.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
STORCON="${STORCON_URL:-http://127.0.0.1:1234}"
LOG_PREFIX=neon-compute-demo
# shellcheck source=lib/neon-common.sh
. scripts/lib/neon-common.sh
[[ -z "${CHELADB_INTERNAL_ADDR:-}" ]] || check_internal_addr "$CHELADB_INTERNAL_ADDR"
INTERNAL="${CHELADB_INTERNAL_URL:-http://${CHELADB_INTERNAL_ADDR:-127.0.0.1:8083}}"
NETWORK="${CHELADB_COMPUTE_NETWORK:-cheladb_default}"
state_dir=.cheladb/neon
env_file="$state_dir/demo-compute.env"
# The token's temp dir (see create) goes away on any exit, including SIGINT and SIGTERM.
token_dir=""
trap '[[ -z "$token_dir" ]] || rm -rf "$token_dir"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

down() {
  if [[ ! -f "$env_file" ]]; then
    say "nothing to remove ($env_file doesn't exist)"
    return 0
  fi
  # shellcheck disable=SC1090
  source "$env_file"
  docker rm -f "cheladb-compute-$DEMO_ENDPOINT" >/dev/null 2>&1 || true
  # If the metadata delete fails, still remove the tenant, but keep the env file so a
  # re-run of --down can finish the job once the database is back.
  local metadata_failed=0
  metadata_sql -v ep="$DEMO_ENDPOINT" <<<"DELETE FROM neon_endpoints WHERE id = :'ep';" || metadata_failed=1
  local status=""
  for _ in $(seq 120); do
    status="$(curl -sS --max-time 60 -o /dev/null -w '%{http_code}' -X DELETE "$STORCON/v1/tenant/$DEMO_TENANT")" ||
      die "DELETE tenant $DEMO_TENANT: no answer"
    case "$status" in
      202) sleep 0.5 ;;
      2* | 404) break ;;
      *) die "DELETE tenant $DEMO_TENANT: HTTP $status" ;;
    esac
  done
  [[ "$status" != 202 ]] || die "tenant $DEMO_TENANT is still being deleted after 60 s"
  if [[ "$metadata_failed" -eq 1 ]]; then
    die "removed the container and tenant $DEMO_TENANT, but couldn't delete endpoint $DEMO_ENDPOINT from metadata (is the metadata database up?). Kept $env_file: re-run $0 --down"
  fi
  rm -f "$env_file"
  say "removed $DEMO_ENDPOINT and tenant $DEMO_TENANT"
}

create() {
  [[ ! -f "$env_file" ]] || die "$env_file exists: run $0 --down first"
  curl -fsS --max-time 5 "$INTERNAL/health" >/dev/null ||
    die "the orchestrator's internal listener isn't answering at $INTERNAL (run just dev)"
  local images
  images="$(scripts/neon-images.sh)" || die "scripts/neon-images.sh failed"
  eval "$images"

  local tenant timeline ep password token
  tenant="$(hex 16)"
  timeline="$(hex 16)"
  ep="ep-$(hex 6)"
  password="$(hex 16)"
  token="$(hex 32)"
  # Written first, so --down can clean up after a failure anywhere below.
  mkdir -p "$state_dir"
  (
    umask 077
    printf 'DEMO_ENDPOINT=%s\nDEMO_PASSWORD=%s\nDEMO_TENANT=%s\nDEMO_TIMELINE=%s\n' \
      "$ep" "$password" "$tenant" "$timeline" >"$env_file"
  )

  call POST "$STORCON/v1/tenant" "{\"new_tenant_id\":\"$tenant\",\"placement_policy\":{\"Attached\":0}}" >/dev/null
  call POST "$STORCON/v1/tenant/$tenant/timeline" "{\"new_timeline_id\":\"$timeline\",\"pg_version\":17}" >/dev/null
  say "tenant $tenant, timeline $timeline"

  local verifier token_sha
  verifier="$(printf '%s' "$password" | cargo run -q -p cheladb -- neon verifier)"
  token_sha="$(printf '%s' "$token" | sha256sum | cut -d' ' -f1)"
  metadata_sql -v ep="$ep" -v tenant="$tenant" -v timeline="$timeline" -v sha="$token_sha" -v verifier="$verifier" <<'SQL'
INSERT INTO neon_endpoints (id, tenant_id, timeline_id, compute_token_sha256)
  VALUES (:'ep', :'tenant', :'timeline', :'sha');
INSERT INTO neon_roles (endpoint_id, name, scram_verifier) VALUES (:'ep', 'postgres', :'verifier');
INSERT INTO neon_databases (endpoint_id, name, owner) VALUES (:'ep', 'postgres', 'postgres');
SQL
  say "endpoint $ep in metadata"

  local args compute_args
  compute_args="$(cargo run -q -p cheladb -- neon compute-args --endpoint "$ep" \
    --control-plane-uri http://host.docker.internal:8083)" || die "cheladb neon compute-args failed"
  mapfile -t args <<<"$compute_args"
  [[ -n "${args[0]:-}" ]] || die "cheladb neon compute-args printed nothing"
  # compute_ctl tries its spec three times, then exits; Docker restarts it.
  # The token goes in through an env file, not argv (which `ps` and `docker inspect` show).
  local run_status=0
  token_dir="$(umask 077 && mktemp -d)"
  (umask 077 && printf 'NEON_CONTROL_PLANE_TOKEN=%s\n' "$token" >"$token_dir/token.env")
  docker run -d --name "cheladb-compute-$ep" --network "$NETWORK" --restart unless-stopped \
    -p 127.0.0.1::3080 --add-host host.docker.internal:host-gateway \
    --label "cheladb.dev/endpoint-id=$ep" --env-file "$token_dir/token.env" \
    --entrypoint /usr/local/bin/compute_ctl "$COMPUTE_NODE_IMAGE" "${args[@]}" >/dev/null || run_status=$?
  rm -rf "$token_dir"
  token_dir=""
  [[ "$run_status" -eq 0 ]] || die "docker run failed (exit $run_status)"
  # `compose cp` writes progress to stderr: show it only if the copy fails.
  local cp_err
  cp_err="$("${compose[@]}" --profile neon cp proxy:/tls/ca.crt "$state_dir/proxy-ca.crt" 2>&1 >/dev/null)" ||
    die "copying the proxy's CA certificate failed: $cp_err"
  say "compute cheladb-compute-$ep started"
  echo ""
  echo "psql (password in $env_file):"
  echo "  psql \"host=$ep.localtest.me port=4432 user=postgres dbname=postgres sslmode=verify-full sslrootcert=$state_dir/proxy-ca.crt\""
  echo "SQL over HTTP and WebSocket: https://$ep.localtest.me:4444/sql, wss://$ep.localtest.me:4444/v2"
  echo "Remove: just neon-compute-demo --down"
}

case "${1:-}" in
  --down) down ;;
  "") create ;;
  *) die "usage: $0 [--down]" ;;
esac
