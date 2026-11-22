#!/usr/bin/env bash
# Live check of the MCP server with the official TypeScript client (needs the stack with the `mcp`
# profile: just workbench up; run through `just mcp-e2e`). Makes a project with a database, an
# Admin and a Viewer member of it, signs each in with the CLI into its own config directory, then
# runs packages/mcp-e2e: each check spawns `chelabase mcp --url <api>` over stdio with
# @modelcontextprotocol/sdk's client and drives search and execute through it (see
# packages/mcp-e2e/src/checks/). Everything it makes is removed on exit, also on failure.
# KEEP=1 leaves the project and the members.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
LOG_PREFIX=mcp-e2e
# shellcheck source=lib/neon-common.sh
. scripts/lib/neon-common.sh
GATEWAY="${GATEWAY_URL:-$CHELABASE_ENTRY_API_URL}"
GW="$GATEWAY"
STUDIO="$CHELABASE_ENTRY_STUDIO_URL"
tmp="$(mktemp -d)"
project_id=""
slug=""
db_id=""
member_ids=()

cleanup() {
  local rc=$?
  set +e
  if [[ "${KEEP:-}" != 1 ]]; then
    local m
    for m in "${member_ids[@]}"; do
      api DELETE "/v1/members/$m" >/dev/null
    done
    [[ -z "$db_id" ]] || api DELETE "/v1/databases/$db_id" >/dev/null
    if [[ -n "$project_id" ]]; then
      api DELETE "/v1/projects/$project_id" "$(jq -nc --arg s "$slug" '{confirm: $s}')" >/dev/null
    fi
  fi
  rm -rf "$tmp"
  exit "$rc"
}
trap cleanup EXIT

[[ -x "${CHELABASE_CLI:-}" ]] || die "CHELABASE_CLI is unset or not executable (run through: just workbench run scripts/mcp-e2e.sh)"
command -v pnpm >/dev/null || die "pnpm is missing"
# shellcheck source=lib/studio-auth.sh
. scripts/lib/studio-auth.sh
DEMO_TENANT_ID="00000000-0000-4000-8000-000000000001"
suffix="$(date +%s)-$RANDOM"
PASSWORD="E2e-pass-$(openssl rand -hex 4)A!"

# ok <method> <path> <expected status> [body]: the Admin-of-the-tenant call, or die.
ok() {
  local status
  status="$(api "$1" "$2" "${4:-}")"
  [[ "$status" == "$3" ]] || die "$1 $2: $status (wanted $3) $(cat "$tmp/body")"
}

# make_member <label> <platform role>: invites a person by link, accepts, grants them the role
# on the project and signs the CLI in as them under $tmp/config-<label>.
make_member() {
  local label="$1" email="mcp-e2e-$1-$suffix@example.com" url id out
  ok POST /v1/invites 201 "$(jq -nc --arg e "$email" '{email: $e, grants: [], delivery: "link"}')"
  id="$(jq -r '.member.id' "$tmp/body")"
  member_ids+=("$id")
  url="$(jq -r '.invite_url // empty' "$tmp/body")"
  [[ "$url" == "$STUDIO/accept-invite?user="* ]] || die "no invite link: $(cat "$tmp/body")"
  accept_link "$url" "$PASSWORD" >/dev/null
  ok PUT "/v1/projects/$project_id/grants/$id" 200 "$(jq -nc --arg r "$2" '{platform_role: $r, pg_roles: []}')"
  mkdir -p "$tmp/config-$label"
  out="$(printf '%s\n' "$PASSWORD" |
    CHELABASE_CONFIG_DIR="$tmp/config-$label" "$CHELABASE_CLI" login --url "$GATEWAY" --email "$email" 2>&1)" ||
    die "chelabase login as the $label: $out"
}

# sql <sql>: runs it on the branch as the tenant admin through the management API.
sql() {
  local out
  ok POST "$branch/sql" 200 "$(jq -nc --arg s "$1" --arg r "$(cat /proc/sys/kernel/random/uuid)" '{run_id: $r, sql: $s}')"
  out="$(cat "$tmp/body")"
  ! grep -q '"t":"error"' <<<"$out" || die "SQL failed: $1: $out"
}

# 1. A project with a database.
slug="mcp-e2e-$suffix"
ok POST /v1/projects 201 "$(jq -nc --arg n "$slug" '{name: $n}')"
project_id="$(jq -r '.id' "$tmp/body")"
slug="$(jq -r '.slug' "$tmp/body")"
status="$(api POST /v1/databases "$(jq -nc --arg t "$DEMO_TENANT_ID" --arg p "$project_id" --arg n "mcp_e2e_$(hex 3)" \
  '{tenant_id: $t, project_id: $p, name: $n}')")"
[[ "$status" == 201 || "$status" == 202 ]] || die "POST /v1/databases: $status $(cat "$tmp/body")"
db_id="$(jq -r '.database_id' "$tmp/body")"
for _ in $(seq 180); do
  ok GET "/v1/databases/$db_id" 200
  case "$(jq -r '.status' "$tmp/body")" in
    ready) break ;;
    failed) die "database $db_id failed: $(cat "$tmp/body")" ;;
  esac
  sleep 1
done
[[ "$(jq -r '.status' "$tmp/body")" == ready ]] || die "database $db_id isn't ready after 180 s"
branch="/v1/databases/$db_id/branches/$db_id"
say "project $slug, database $db_id"

# 2. What the checks use: a table with a row, a sequence and a function that writes.
table="e2e_items"
sql "create table public.$table (id int primary key, label text not null);
  insert into public.$table values (1, 'seed');
  create table public.e2e_audit (n int);
  create sequence public.e2e_seq;
  create function public.e2e_write() returns int language sql as 'insert into public.e2e_audit values (1) returning n'"

# 3. Two members of the project, each signed in under its own config directory.
make_member admin admin
make_member viewer viewer
say "members signed in"

export MCP_E2E_API="$GATEWAY"
export MCP_E2E_CLI="$CHELABASE_CLI"
export MCP_E2E_ADMIN_CONFIG="$tmp/config-admin"
export MCP_E2E_VIEWER_CONFIG="$tmp/config-viewer"
export MCP_E2E_DATABASE="$db_id"
export MCP_E2E_BRANCH="$db_id"
export MCP_E2E_TABLE="$table"
pnpm --filter @chelabase/mcp-e2e --silent check
echo "all passed"
