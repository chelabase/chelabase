#!/usr/bin/env bash
# Wipe only the Zitadel Postgres volume so FirstInstance can rewrite admin.pat.
# Does not touch metadata or NATS volumes. Zitadel is in the shared tier: every worktree
# slot signs in against it.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# CHELADB_SHARED_DIR: the shared tier's bind mounts name the main checkout's .cheladb from
# every worktree, so a later `just deps-up` doesn't recreate Zitadel.
# shellcheck source=lib/slots.sh
. "$ROOT/scripts/lib/slots.sh"
slot_load "$ROOT"
COMPOSE_FILE="${COMPOSE_FILE:-$ROOT/infra/compose/shared.yml}"
COMPOSE_ENV="${COMPOSE_ENV:-$ROOT/infra/compose/.env.example}"
ZDIR="${CHELADB_ZITADEL_DIR:-$ROOT/.cheladb/zitadel}"

echo "[zitadel-reset] stopping Zitadel services and removing zitadel_pg_data…"
docker compose -f "$COMPOSE_FILE" --env-file "$COMPOSE_ENV" stop zitadel zitadel-db >/dev/null 2>&1 || true
docker compose -f "$COMPOSE_FILE" --env-file "$COMPOSE_ENV" rm -f zitadel zitadel-db >/dev/null 2>&1 || true

# Volume name is project_volume → cheladb_zitadel_pg_data
VOL="$(docker volume ls -q --filter name=zitadel_pg_data | head -n1 || true)"
if [[ -n "$VOL" ]]; then
  docker volume rm "$VOL"
  echo "[zitadel-reset] removed volume ${VOL}"
else
  echo "[zitadel-reset] no zitadel_pg_data volume found"
fi

rm -f "${ZDIR}/admin.pat" "${ZDIR}/login-client.pat" "${ZDIR}/demo.env" "${ZDIR}/demo-sa.json" "${ZDIR}"/studio-s*.env
mkdir -p "$ZDIR"
chmod 777 "$ZDIR" 2>/dev/null || true

echo "[zitadel-reset] starting Zitadel fresh…"
docker compose -f "$COMPOSE_FILE" --env-file "$COMPOSE_ENV" up -d zitadel-db zitadel

echo "[zitadel-reset] waiting for admin.pat…"
for _ in $(seq 1 60); do
  if [[ -f "${ZDIR}/admin.pat" ]] && [[ -s "${ZDIR}/admin.pat" ]]; then
    echo "[zitadel-reset] admin.pat ready — run: just zitadel-bootstrap"
    exit 0
  fi
  sleep 2
done

echo "[zitadel-reset] ERROR: admin.pat not written. Check: docker compose -f ${COMPOSE_FILE} logs zitadel" >&2
exit 1
