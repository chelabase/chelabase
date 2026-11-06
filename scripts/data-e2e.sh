#!/usr/bin/env bash
# Live check of roadmap step 6 (the data API), through the gateway and the proxy (needs just
# the stack: just workbench up):
# - a public and a secret key; the secret key inserts, the public key reads, and the public key's
#   insert is refused (403); a batch rolls back on error (packages/client/scripts/e2e.mjs);
# - `chelabase gen types` output typechecks against the SDK, and a wrong column doesn't;
# - row-level security hides a row from the public key but not from the secret key's role;
# - a revoked key gets 401 within the gateway's 30 s cache (35 s allowed).
# KEEP=1 leaves the database in place.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
LOG_PREFIX=data-e2e
# shellcheck source=lib/neon-common.sh
. scripts/lib/neon-common.sh
STORCON="${STORCON_URL:-http://storage-controller:1234}"
GATEWAY="${GATEWAY_URL:-$CHELABASE_ENTRY_API_URL}"
PROXY_PG_PORT="${CHELABASE_PROXY_PG_PORT:-4432}"
PROXY_HOSTADDR="${CHELABASE_PROXY_HOSTADDR:?run through just workbench run}"
DEMO_TENANT_ID="00000000-0000-4000-8000-000000000001"
DEMO_PROJECT_ID="00000000-0000-4000-8000-000000000002"
tmp="$(mktemp -d)"
db_id=""
t0=$SECONDS

pass() { echo "ok   $* ($((SECONDS - t0)) s)"; }
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

[[ -x "${CHELABASE_CLI:-}" ]] || die "CHELABASE_CLI is unset or not executable (run through: just workbench run scripts/<name>.sh)"
ca="${CHELABASE_PROXY_CA_FILE:?run through just workbench run}"
[[ -s "$ca" ]] || die "$ca is missing (run just workbench up)"
command -v pnpm >/dev/null || die "pnpm is missing"
trap cleanup EXIT

# ok <method> <path> <expected status> [body]: the call, or die.
ok() {
  local status
  status="$(api "$1" "$2" "${4:-}")"
  [[ "$status" == "$3" ]] || die "$1 $2: $status (wanted $3) $(cat "$tmp/body")"
}

# wait_db: until the database is ready.
wait_db() {
  local s=""
  for _ in $(seq 120); do
    ok GET "/v1/databases/$db_id" 200
    s="$(jq -r '.status' "$tmp/body")"
    case "$s" in
      ready) return ;;
      failed) die "database $db_id failed: $(jq -r '.error' "$tmp/body")" ;;
    esac
    sleep 1
  done
  die "database $db_id is still $s after 120 s"
}

# reveal: sets bhost, bpw and bdb from the main branch's connection details.
reveal() {
  ok GET "/v1/databases/$db_id/branches/$db_id/connection" 200
  bhost="$(jq -r '.host' "$tmp/body")"
  bpw="$(jq -r '.password' "$tmp/body")"
  bdb="$(jq -r '.database' "$tmp/body")"
}
# psql_on <psql args…>: through the proxy, with the branch's current password; retries while the
# compute is still starting.
psql_on() {
  local out
  reveal
  for _ in $(seq 60); do
    if out="$(PGPASSWORD="$bpw" PGCONNECT_TIMEOUT=10 psql \
      "host=$bhost hostaddr=$PROXY_HOSTADDR port=${PROXY_PG_PORT} user=postgres dbname=$bdb sslmode=verify-full sslrootcert=$ca" \
      -v ON_ERROR_STOP=1 -qtA "$@" 2>"$tmp/err")"; then
      printf '%s' "$out"
      return
    fi
    sleep 2
  done
  die "psql: $(cat "$tmp/err")"
}

# 1. A database, its public key and a secret key.
name="e2e_$(hex 2)"
status="$(api POST /v1/databases "$(jq -nc --arg t "$DEMO_TENANT_ID" --arg p "$DEMO_PROJECT_ID" --arg n "$name" \
  '{tenant_id: $t, project_id: $p, name: $n}')")"
[[ "$status" == 202 || "$status" == 201 ]] || die "POST /v1/databases: $status $(cat "$tmp/body")"
db_id="$(jq -r '.database_id' "$tmp/body")"
[[ -n "$db_id" && "$db_id" != null ]] || die "POST /v1/databases returned no database_id: $(cat "$tmp/body")"
keys="/v1/databases/$db_id/branches/$db_id/keys"
wait_db
pass "database $name ($db_id) is ready"
ok GET "$keys" 200
PUBLIC_KEY="$(jq -r 'map(select(.kind == "public")) | .[0].key // empty' "$tmp/body")"
[[ "$PUBLIC_KEY" == chk_pub_* ]] || die "no public key in the key list: $(cat "$tmp/body")"
ok POST "$keys" 201 '{"name":"data-e2e"}'
SECRET_KEY="$(jq -r '.key' "$tmp/body")"
secret_id="$(jq -r '.id' "$tmp/body")"
[[ "$SECRET_KEY" == chk_sec_* && "$(jq -r '.role' "$tmp/body")" == authenticated ]] ||
  die "unexpected secret key: $(jq -c 'del(.key)' "$tmp/body")"
pass "public key listed, secret key created (role authenticated)"
export GATEWAY_URL="$GATEWAY" PUBLIC_KEY SECRET_KEY

# 2. A table, through the proxy (the bootstrap's default privileges cover it).
psql_on -c "CREATE TABLE notes (id bigint generated always as identity primary key, body text not null, created_at timestamptz default now())" >/dev/null
pass "table notes created"

# 3. The SDK against the gateway.
(cd packages/client && node scripts/e2e.mjs queries) || die "scripts/e2e.mjs queries"
pass "secret insert, public select, public insert refused, batch rollback"

# 4. Generated types against the SDK: a valid call compiles, a wrong column doesn't.
# The key goes through the environment, never argv (other users can read a process's argv).
CHELABASE_DATA_KEY="$SECRET_KEY" "$CHELABASE_CLI" gen types --url "$GATEWAY" --out "$tmp/db.ts" ||
  die "chelabase gen types"
grep -q 'notes' "$tmp/db.ts" || die "the generated types don't mention notes"
cat >"$tmp/sample.ts" <<EOF
import { createClient } from "@chelabase/client";
import { type Database, types } from "./db.ts";

const db = createClient<Database>({ url: "$GATEWAY", key: "chk_pub_x", types });

export async function ok() {
  const rows = await db.from("notes").select("id", "body").where({ body: "first" });
  const id: number = rows[0].id;
  await db.from("notes").insert({ body: "typed" });
  await db.from("notes").update({ body: "edited" }).where({ id });
  await db.from("notes").delete().all();
  return id;
}

export async function wrong() {
  // @ts-expect-error there is no such column
  await db.from("notes").select("nope");
  // @ts-expect-error an update needs .where() or .all()
  await db.from("notes").update({ body: "x" }).then();
}
EOF
cat >"$tmp/tsconfig.json" <<EOF
{
  "compilerOptions": {
    "target": "ES2022", "module": "ESNext", "moduleResolution": "Bundler", "lib": ["ES2022", "DOM"],
    "types": [], "strict": true, "skipLibCheck": true, "noEmit": true, "allowImportingTsExtensions": true,
    "verbatimModuleSyntax": true, "isolatedModules": true,
    "paths": { "@chelabase/client": ["$root/packages/client/src/index.ts"] }
  },
  "files": ["sample.ts"]
}
EOF
pnpm --filter @chelabase/client exec tsc -p "$tmp/tsconfig.json" >"$tmp/tsc.out" 2>&1 ||
  die "the generated types don't typecheck: $(cat "$tmp/tsc.out")"
pass "chelabase gen types output typechecks (and the wrong calls are rejected)"

# 5. Row-level security: the secret key's role (authenticated) sees every row, the public key's
# (anon) doesn't see 'hidden'.
psql_on -c "ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
  CREATE POLICY notes_visible ON notes FOR SELECT USING (body <> 'hidden');
  CREATE POLICY notes_staff ON notes FOR SELECT TO authenticated USING (true);
  CREATE POLICY notes_insert ON notes FOR INSERT TO authenticated WITH CHECK (true);" >/dev/null
(cd packages/client && node scripts/e2e.mjs rls) || die "scripts/e2e.mjs rls"
pass "row-level security hides the row from the public key only"

# 6. Revoke the secret key: 401 within the gateway's 30 s cache (35 s allowed).
# probe: the schema call's status (000 when curl fails). The header is read from stdin, so the
# key never shows in argv.
probe() {
  printf 'Authorization: Bearer %s\n' "$SECRET_KEY" |
    curl -sS --max-time 10 -o /dev/null -w '%{http_code}' -H @- "$GATEWAY/data/v1/schema" || true
}
code="$(probe)"
[[ "$code" == 200 ]] || die "the secret key doesn't read the schema before the revoke ($code)"
ok DELETE "$keys/$secret_id" 204
r0=$SECONDS
code=""
while :; do
  code="$(probe)"
  [[ "$code" == 401 ]] && break
  ((SECONDS - r0 <= 35)) || die "the revoked key still answers $code after $((SECONDS - r0)) s"
  sleep 1
done
pass "revoked key got 401 after $((SECONDS - r0)) s"
echo "all passed ($((SECONDS - t0)) s)"
