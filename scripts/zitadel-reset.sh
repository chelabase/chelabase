#!/usr/bin/env bash
# Wipe only this stack's Zitadel Postgres volume so FirstInstance can rewrite admin.pat.
# Does not touch metadata or NATS volumes. Host script: the justfile loads the stack's env first
# (`just zitadel-reset`): CHELABASE_COMPOSE_PROJECT, CHELABASE_STACK_DIR and CHELABASE_ZITADEL_DIR.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROJECT="${CHELABASE_COMPOSE_PROJECT:?run through: just zitadel-reset}"
STACK_DIR="${CHELABASE_STACK_DIR:?run through: just zitadel-reset}"
ZDIR="${CHELABASE_ZITADEL_DIR:?run through: just zitadel-reset}"
# The base file and the workbench overlay, as every workbench compose call passes them.
COMPOSE_FILE="${COMPOSE_FILE:-$ROOT/infra/compose/docker-compose.yml}"
COMPOSE_OVERLAY="${COMPOSE_OVERLAY:-$ROOT/infra/compose/docker-compose.workbench.yml}"
# The entry values Compose interpolates (Zitadel's external domain, port, TLS mode): the stack's
# env already holds them, entry.sh derives any that are missing.
# shellcheck source=lib/entry.sh
. "$ROOT/scripts/lib/entry.sh"
compose=(docker compose -p "$CHELABASE_COMPOSE_PROJECT" -f "$COMPOSE_FILE" -f "$COMPOSE_OVERLAY" \
  --env-file "$STACK_DIR/env/stack.env")

echo "[zitadel-reset] stopping Zitadel services and removing ${PROJECT}_zitadel_pg_data…"
"${compose[@]}" stop zitadel zitadel-db >/dev/null 2>&1 || true
"${compose[@]}" rm -f zitadel zitadel-db >/dev/null 2>&1 || true

# Volume name is <project>_zitadel_pg_data, matched exactly: another stack's volume is never touched.
VOL="${PROJECT}_zitadel_pg_data"
if docker volume inspect "$VOL" >/dev/null 2>&1; then
  docker volume rm "$VOL"
  echo "[zitadel-reset] removed volume ${VOL}"
else
  echo "[zitadel-reset] no ${VOL} volume found"
fi

rm -f "${ZDIR}/admin.pat" "${ZDIR}/login-client.pat" "${ZDIR}/demo.env" "${ZDIR}/demo-sa.json"
mkdir -p "$ZDIR"
chmod 700 "$ZDIR" 2>/dev/null || true

echo "[zitadel-reset] starting Zitadel fresh…"
"${compose[@]}" up -d zitadel-db zitadel

# Both PATs: the orchestrator, the worker and auth mount them as Compose secrets, so an app
# created or restarted while one is missing fails to start. The running apps keep the old PATs
# (read once, at startup) until they are recreated.
echo "[zitadel-reset] waiting for admin.pat and login-client.pat…"
for _ in $(seq 1 60); do
  if [[ -s "${ZDIR}/admin.pat" ]] && [[ -s "${ZDIR}/login-client.pat" ]]; then
    chmod 600 "${ZDIR}/admin.pat"
    chmod 600 "${ZDIR}/login-client.pat"
    echo "[zitadel-reset] PATs ready — run: just zitadel-bootstrap, then just workbench restart"
    exit 0
  fi
  sleep 2
done

echo "[zitadel-reset] ERROR: admin.pat or login-client.pat not written. Check: ${compose[*]} logs zitadel" >&2
exit 1
