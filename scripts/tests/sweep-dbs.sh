#!/usr/bin/env bash
# `just sweep-dbs` (scripts/lib/sweep-dbs.sql) against the schema the migrations build, in a scratch
# database on the metadata server (needs CHELABASE_METADATA_DATABASE_URL and CREATE DATABASE;
# `just sweep-dbs-test` runs it in the stack's runner). Never touches the stack's own data.
#   1. repro: a database with a member grant, app users, a data key, a role, a merge, ... then the
#      sweep: it must exit 0, leave members/teams/tenants/projects alone, and leave no row that
#      references a database or branch, and no value sealed with the secrets key
#      (so ensure_key_check adopts a new key: KeyCheck::Replaced).
#   2. guard: every foreign key to databases/branches, directly or through a table the sweep
#      deletes, is either deleted by the sweep or reached by ON DELETE CASCADE / SET NULL; and the
#      guard fails, naming the constraint, when the list is made incomplete.
# SWEEP_SQL=<file> runs another statement list (for showing the repro fail on the old one-statement delete).
set -euo pipefail
cd "$(dirname "$0")/../.."
: "${CHELABASE_METADATA_DATABASE_URL:?run through: just sweep-dbs-test}"
sweep_sql="${SWEEP_SQL:-scripts/lib/sweep-dbs.sql}"
failed=0
fail() {
  echo "sweep-dbs test: FAIL: $*" >&2
  failed=1
}

base="${CHELABASE_METADATA_DATABASE_URL%%\?*}"
query=""
[[ "$CHELABASE_METADATA_DATABASE_URL" == *\?* ]] && query="?${CHELABASE_METADATA_DATABASE_URL#*\?}"
scratch="sweep_test_$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')"
admin() { psql "$CHELABASE_METADATA_DATABASE_URL" -v ON_ERROR_STOP=1 -qtA "$@"; }
sdb() { psql "${base%/*}/$scratch$query" -v ON_ERROR_STOP=1 -qtA "$@"; }
admin -c "CREATE DATABASE $scratch"
trap 'admin -c "DROP DATABASE IF EXISTS $scratch WITH (FORCE)" >/dev/null 2>&1 || true' EXIT
cat crates/chelabase-metadata/migrations/*.sql | sdb -f - >/dev/null

# Tables whose columns hold values sealed with the secrets key (SECRETS_KEY_SEALED).
mapfile -t sealed_tables < <(sed -n '/^pub const SECRETS_KEY_SEALED/,/^];/p' crates/chelabase-metadata/src/lib.rs |
  sed -n 's/^ *("\([a-z_]*\)", *"[a-z_]*", *[A-Za-z(")_ >0-9.]*),$/\1/p' | sort -u)
((${#sealed_tables[@]} >= 5)) || { echo "sweep-dbs test: found only ${#sealed_tables[@]} sealed tables in lib.rs" >&2; exit 1; }

# deleted_closure <sql file>: the tables a sweep clears: databases, branches, every table the file
# deletes from, and every table reached from those by ON DELETE CASCADE.
deleted_closure() {
  local tables
  tables="{$(sed -n 's/^DELETE FROM \([a-z_]*\);.*/\1/p' "$1" | paste -sd, -)}"
  sdb -v tables="$tables" <<'SQL'
WITH RECURSIVE del(t) AS (
  SELECT unnest(:'tables'::text[]) UNION SELECT 'databases' UNION SELECT 'branches'
  UNION
  SELECT c.conrelid::regclass::text FROM pg_constraint c JOIN del ON c.confrelid::regclass::text = del.t
  WHERE c.contype = 'f' AND c.confdeltype = 'c')
SELECT t FROM del ORDER BY t;
SQL
}

# guard <sql file>: prints one line per problem; empty when the file covers the schema.
guard() {
  local closure sealed tables
  closure="{$(deleted_closure "$1" | paste -sd, -)}"
  sealed="{$(printf '%s,' "${sealed_tables[@]}" | sed 's/,$//')}"
  sdb -v del="$closure" -v sealed="$sealed" <<'SQL'
SELECT 'foreign key ' || c.conname || ' (' || c.conrelid::regclass || ' -> ' || c.confrelid::regclass
       || ') has no ON DELETE CASCADE and ' || c.conrelid::regclass || ' is not deleted by the sweep'
FROM pg_constraint c
WHERE c.contype = 'f' AND c.confdeltype IN ('a', 'r', 'd')
  AND c.confrelid::regclass::text = ANY (:'del'::text[])
  AND NOT c.conrelid::regclass::text = ANY (:'del'::text[])
UNION ALL
SELECT 'table ' || t || ' holds values sealed with the secrets key but the sweep does not clear it'
FROM unnest(:'sealed'::text[]) t WHERE NOT t = ANY (:'del'::text[])
UNION ALL
SELECT 'table ' || t || ' is not the sweep''s to clear'
FROM unnest(ARRAY['members', 'teams', 'team_members', 'team_grants', 'project_grants', 'projects', 'tenants', 'invites']) t
WHERE t = ANY (:'del'::text[]);
SQL
}

# --- 1. the repro ---
sdb <<'SQL'
INSERT INTO tenants (id, slug) VALUES ('00000000-0000-0000-0000-0000000000a1', 'sweep-t');
INSERT INTO projects (id, tenant_id, slug, name) VALUES
  ('00000000-0000-0000-0000-0000000000a2', '00000000-0000-0000-0000-0000000000a1', 'sweep-p', 'sweep-p');
INSERT INTO members (id, tenant_id, zitadel_user_id, status) VALUES
  ('00000000-0000-0000-0000-0000000000a3', '00000000-0000-0000-0000-0000000000a1', 'z-1', 'active');
INSERT INTO teams (id, tenant_id, name) VALUES
  ('00000000-0000-0000-0000-0000000000a4', '00000000-0000-0000-0000-0000000000a1', 'sweep-team');
-- A database with a main branch (same id) and a child branch.
INSERT INTO neon_endpoints (id, tenant_id, timeline_id, compute_token_sha256) VALUES
  ('ep-aaaaaaaaaaaa', repeat('a', 32), repeat('b', 32), repeat('c', 64));
INSERT INTO databases (id, project_id, name, status) VALUES
  ('00000000-0000-0000-0000-0000000000b1', '00000000-0000-0000-0000-0000000000a2', 'sweep-db', 'ready');
INSERT INTO branches (id, database_id, name, parent_branch_id, status, endpoint_id, owner_password_enc, authenticator_password_enc) VALUES
  ('00000000-0000-0000-0000-0000000000b1', '00000000-0000-0000-0000-0000000000b1', 'main', NULL, 'ready', 'ep-aaaaaaaaaaaa', 'sealed-1', 'sealed-2');
INSERT INTO branches (id, database_id, name, parent_branch_id, status) VALUES
  ('00000000-0000-0000-0000-0000000000b2', '00000000-0000-0000-0000-0000000000b1', 'dev', '00000000-0000-0000-0000-0000000000b1', 'ready');
-- A role, a data key, a member role, a merge, a saved query.
INSERT INTO neon_roles (endpoint_id, name, origin, scram_verifier, password_enc) VALUES
  ('ep-aaaaaaaaaaaa', 'app', 'api', 'SCRAM-SHA-256$x', 'sealed-3');
INSERT INTO neon_databases (endpoint_id, name, owner) VALUES ('ep-aaaaaaaaaaaa', 'app', 'app');
INSERT INTO data_api_keys (id, branch_id, kind, role, name, prefix, sha256, sealed) VALUES
  (gen_random_uuid(), '00000000-0000-0000-0000-0000000000b1', 'public', 'anon', 'k', 'pk_', decode(repeat('ab', 32), 'hex'), 'sealed-4');
INSERT INTO member_branch_roles (member_id, branch_id, role_name, password_enc) VALUES
  ('00000000-0000-0000-0000-0000000000a3', '00000000-0000-0000-0000-0000000000b1', 'm_role', 'sealed-5');
INSERT INTO database_grant_overrides (member_id, database_id, platform_role) VALUES
  ('00000000-0000-0000-0000-0000000000a3', '00000000-0000-0000-0000-0000000000b1', 'viewer');
INSERT INTO personal_logins (member_id, database_id, role_name, scram_verifier, valid_until) VALUES
  ('00000000-0000-0000-0000-0000000000a3', '00000000-0000-0000-0000-0000000000b1', 'p_login', 'SCRAM-SHA-256$y', now() + interval '1 day');
INSERT INTO neon_roles (endpoint_id, name, origin, scram_verifier, member_id, personal_database_id) VALUES
  ('ep-aaaaaaaaaaaa', 'p_login', 'personal', 'SCRAM-SHA-256$y',
   '00000000-0000-0000-0000-0000000000a3', '00000000-0000-0000-0000-0000000000b1');
INSERT INTO saved_queries (id, database_id, user_id, name, sql) VALUES
  (gen_random_uuid(), '00000000-0000-0000-0000-0000000000b1', 'u', 'q', 'select 1');
INSERT INTO merges (id, database_id, source_branch_id, target_branch_id, direction, database_name, status, created_by) VALUES
  ('00000000-0000-0000-0000-0000000000c1', '00000000-0000-0000-0000-0000000000b1',
   '00000000-0000-0000-0000-0000000000b2', '00000000-0000-0000-0000-0000000000b1',
   'into_parent', 'app', 'done', 'u');
INSERT INTO merge_events (merge_id, kind) VALUES ('00000000-0000-0000-0000-0000000000c1', 'note');
INSERT INTO branch_databases (branch_id, name, owner, origin) VALUES
  ('00000000-0000-0000-0000-0000000000b1', 'app', 'app', 'main');
INSERT INTO branch_guard_exemptions (branch_id, role_name) VALUES ('00000000-0000-0000-0000-0000000000b1', 'g');
INSERT INTO branch_restores (branch_id, timeline_id, from_timeline_id, at_lsn) VALUES
  ('00000000-0000-0000-0000-0000000000b1', repeat('d', 32), repeat('e', 32), '0/16B3748');
-- App users enabled on the project, with a session holding a sealed grace value.
INSERT INTO project_auth (project_id, status, database_id, pg_database) VALUES
  ('00000000-0000-0000-0000-0000000000a2', 'enabled', '00000000-0000-0000-0000-0000000000b1', 'app');
INSERT INTO branch_auth_sync (branch_id, pg_database, password_sealed) VALUES
  ('00000000-0000-0000-0000-0000000000b1', 'app', 'sealed-6');
INSERT INTO app_sessions (id, project_id, user_id, zitadel_session_id, refresh_hash, grace_refresh_sealed, grace_until, expires_at) VALUES
  (gen_random_uuid(), '00000000-0000-0000-0000-0000000000a2', gen_random_uuid(), 'zs', '\x00',
   'sealed-7', now() + interval '1 hour', now() + interval '1 day');
SQL
keep_before="$(sdb -c "SELECT (SELECT count(*) FROM members) || ',' || (SELECT count(*) FROM teams) || ',' || (SELECT count(*) FROM projects) || ',' || (SELECT count(*) FROM tenants)")"
sealed_sql="SELECT $(printf '(SELECT count(*) FROM %s) + ' "${sealed_tables[@]}")0"

if sdb -1 -f "$sweep_sql" >/dev/null; then
  keep_after="$(sdb -c "SELECT (SELECT count(*) FROM members) || ',' || (SELECT count(*) FROM teams) || ',' || (SELECT count(*) FROM projects) || ',' || (SELECT count(*) FROM tenants)")"
  [[ "$keep_before" == "$keep_after" ]] || fail "the sweep changed members/teams/projects/tenants: $keep_before -> $keep_after"
  # Every table that references databases/branches (directly or through a cascade) is empty.
  for t in $(deleted_closure scripts/lib/sweep-dbs.sql); do
    n="$(sdb -c "SELECT count(*) FROM $t")"
    [[ "$n" == 0 ]] || fail "$t still has $n row(s) after the sweep"
  done
  n="$(sdb -c "$sealed_sql")"
  [[ "$n" == 0 ]] || fail "$n sealed value(s) survive the sweep (ensure_key_check would answer Mismatch, not Replaced)"
else
  fail "the sweep failed on a populated schema ($sweep_sql)"
fi

# --- 2. the guard ---
problems="$(guard scripts/lib/sweep-dbs.sql)"
[[ -z "$problems" ]] || fail "scripts/lib/sweep-dbs.sql misses:"$'\n'"$problems"
# It must notice an incomplete list, and name the constraint or table.
check_catches() { # <table whose DELETE is dropped> <text the report must contain>
  local incomplete out
  incomplete="$(mktemp)"
  grep -v "^DELETE FROM $1;" scripts/lib/sweep-dbs.sql >"$incomplete"
  out="$(guard "$incomplete")"
  rm -f "$incomplete"
  [[ "$out" == *"$2"* ]] || fail "the guard did not name '$2' when the sweep skips $1 (it said: ${out:-nothing})"
}
check_catches database_grant_overrides database_grant_overrides_database_id_fkey
check_catches project_auth project_auth_database_id_fkey
check_catches app_sessions 'table app_sessions holds values sealed'

if [[ $failed -eq 0 ]]; then echo "sweep-dbs test: ok"; fi
exit $failed
