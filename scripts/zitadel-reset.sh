#!/usr/bin/env bash
# Wipe only the Zitadel Postgres volume so FirstInstance can rewrite admin.pat.
# Does not touch metadata or NATS volumes.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMPOSE_FILE="${COMPOSE_FILE:-$ROOT/infra/compose/docker-compose.yml}"
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

rm -f "${ZDIR}/admin.pat" "${ZDIR}/login-client.pat" "${ZDIR}/demo.env" "${ZDIR}/demo-sa.json"
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
