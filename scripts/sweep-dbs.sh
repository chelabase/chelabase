#!/usr/bin/env bash
# Removes this stack's Neon computes (label chelabase.dev/stack=$CHELABASE_STACK), deletes their
# storage-controller tenants, and clears the `databases`, `branches` and `neon_endpoints` rows.
# Does not touch the stack's infrastructure or any other stack. Run it through
# `just sweep-dbs` (the stack's runner).
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
LOG_PREFIX=sweep-dbs
# shellcheck source=lib/neon-common.sh
. scripts/lib/neon-common.sh
STORCON="${STORCON_URL:-http://storage-controller:1234}"
: "${CHELABASE_STACK:?run through: just sweep-dbs}"

filter=(--filter "label=chelabase.dev/stack=$CHELABASE_STACK" --filter 'label=chelabase.dev/endpoint-id')
ids="$(docker ps -aq "${filter[@]}" 2>/dev/null || true)"
if [[ -z "${ids}" ]]; then
  echo "no Neon compute containers to remove"
else
  echo "removing Neon compute containers:"
  docker ps -a "${filter[@]}" --format '  {{.Names}}\t{{.Status}}'
  # shellcheck disable=SC2086
  docker rm -f ${ids} >/dev/null
  echo "removed $(echo "${ids}" | wc -w | tr -d ' ') compute container(s)"
fi

if psql "$CHELABASE_METADATA_DATABASE_URL" -qtA -c 'SELECT 1' >/dev/null 2>&1; then
  tenants="$(metadata_sql <<<"SELECT DISTINCT tenant_id FROM neon_endpoints;")"
  for tenant in ${tenants}; do
    delete_tenant "${tenant}"
  done
  echo "deleted $(echo "${tenants}" | wc -w | tr -d ' ') storage-controller tenant(s)"
  counts="$(metadata_sql <<<"WITH e AS (DELETE FROM neon_endpoints RETURNING 1), b AS (DELETE FROM branches RETURNING 1), d AS (DELETE FROM databases RETURNING 1) SELECT (SELECT count(*) FROM e) || ' ' || (SELECT count(*) FROM d);")"
  echo "cleared ${counts% *} Neon endpoint row(s) and ${counts#* } metadata database row(s)"
else
  echo "metadata Postgres isn't answering — skipped clearing the databases table (just workbench up, then just sweep-dbs)"
fi
