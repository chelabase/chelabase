# shellcheck shell=bash
# Worktree slots: each worktree runs its own app tier (dev services, metadata database, NATS,
# Valkey, proxy) over one shared tier (infra/compose/shared.yml). Sourced, not run.
#
# Slot 0 is the main checkout, and any checkout without a registry entry: today's ports and
# names. `just wt-add` gives a new worktree the lowest free slot 1-9; every app-tier port is
# today's port + 100 × slot. The registry is .cheladb/slots/<N>.env in the main checkout
# (KEY=VALUE lines, world-readable: the hook relay reads them as nobody). An entry whose
# worktree no longer exists is stale: it may still own a database and volumes, so it isn't
# reused until `just stack-free <N>` wipes it. The worktree's .env.local holds the same
# variables, and decides the worktree's identity (slot, database, dev dir, project).
# `just` recipes call slot_load (or slot_load_all) before anything else.

SLOT_MAX=9

# slot_port <base> <slot>: the slot's copy of a port.
slot_port() { echo $(($1 + 100 * $2)); }

# slot_main_dir <dir>: the main checkout of the repository <dir> is in (<dir> itself outside git).
slot_main_dir() {
  local common
  if common="$(git -C "$1" rev-parse --path-format=absolute --git-common-dir 2>/dev/null)"; then
    dirname "$common"
  else
    (cd "$1" && pwd)
  fi
}

# slot_registry <dir>: the slot registry of <dir>'s repository (CHELADB_SLOTS_DIR overrides).
slot_registry() { echo "${CHELADB_SLOTS_DIR:-$(slot_main_dir "$1")/.cheladb/slots}"; }

# slot_env <slot> <worktree>: every variable of the slot, as KEY=VALUE lines. For slot 0 these
# are the values the apps and scripts default to today.
slot_env() {
  local n="$1" wt="$2" db=cheladb_metadata dev=.cheladb/dev
  if ((n > 0)); then
    db="cheladb_metadata_s$n"
    dev=".cheladb/dev/s$n"
  fi
  local gw orch worker internal auth rest rest_int studio nats nats_mon valkey pg http
  gw="$(slot_port 8000 "$n")"
  orch="$(slot_port 8081 "$n")"
  worker="$(slot_port 8082 "$n")"
  internal="$(slot_port 8083 "$n")"
  auth="$(slot_port 8084 "$n")"
  rest="$(slot_port 8085 "$n")"
  rest_int="$(slot_port 8086 "$n")"
  studio="$(slot_port 3000 "$n")"
  nats="$(slot_port 4222 "$n")"
  nats_mon="$(slot_port 8222 "$n")"
  valkey="$(slot_port 6379 "$n")"
  pg="$(slot_port 4432 "$n")"
  http="$(slot_port 4444 "$n")"
  cat <<EOF
CHELADB_SLOT=$n
CHELADB_WORKTREE=$wt
CHELADB_COMPOSE_PROJECT=cheladb-s$n
CHELADB_DEV_DIR=$dev
CHELADB_METADATA_DB=$db
CHELADB_METADATA_DATABASE_URL=postgres://cheladb:cheladb@127.0.0.1:5433/$db
CHELADB_NATS_PORT=$nats
CHELADB_NATS_MONITOR_PORT=$nats_mon
CHELADB_NATS_URL=nats://127.0.0.1:$nats
CHELADB_VALKEY_PORT=$valkey
CHELADB_VALKEY_URL=redis://127.0.0.1:$valkey
CHELADB_PROXY_PG_PORT=$pg
CHELADB_PROXY_HTTP_PORT=$http
CHELADB_GATEWAY_PORT=$gw
CHELADB_GATEWAY_ADDR=127.0.0.1:$gw
CHELADB_ORCHESTRATOR_ADDR=127.0.0.1:$orch
CHELADB_ORCHESTRATOR_URL=http://127.0.0.1:$orch
CHELADB_WORKER_HEALTH_ADDR=127.0.0.1:$worker
CHELADB_INTERNAL_PORT=$internal
CHELADB_INTERNAL_ADDR=127.0.0.1:$internal
CHELADB_INTERNAL_URL=http://127.0.0.1:$internal
CHELADB_CONTROL_PLANE_URI=http://host.docker.internal:$internal
CHELADB_AUTH_ADDR=127.0.0.1:$auth
CHELADB_AUTH_URL=http://127.0.0.1:$auth
CHELADB_REST_ADDR=127.0.0.1:$rest
CHELADB_REST_URL=http://127.0.0.1:$rest
CHELADB_REST_INTERNAL_ADDR=127.0.0.1:$rest_int
CHELADB_REST_INTERNAL_URL=http://127.0.0.1:$rest_int
CHELADB_STUDIO_PORT=$studio
CHELADB_STUDIO_URL=http://localhost:$studio
CHELADB_STUDIO_ORIGINS=http://localhost:$studio
EOF
  # Slot 0 leaves these to where they come from today (the scripts' own defaults, and
  # apps/studio/.env.local written by bootstrap), so it stays exactly as it was.
  if ((n > 0)); then
    echo "GATEWAY_URL=http://127.0.0.1:$gw"
    echo "VITE_API_URL=http://localhost:$gw"
  fi
}

# slot_key <file> <key>: the value of KEY in a KEY=VALUE file (empty if absent).
slot_key() {
  local line
  [[ -f "$1" ]] || return 0
  while IFS= read -r line || [[ -n "$line" ]]; do
    if [[ "$line" == "$2="* ]]; then
      printf '%s\n' "${line#*=}"
      return 0
    fi
  done <"$1"
}

# slot_of <registry> <worktree>: the slot registered for that worktree; fails if none.
slot_of() {
  local file n
  for n in $(seq 1 "$SLOT_MAX"); do
    file="$1/$n.env"
    if [[ -f "$file" && "$(slot_key "$file" CHELADB_WORKTREE)" == "$2" ]]; then
      echo "$n"
      return 0
    fi
  done
  return 1
}

# slot_is_free <registry> <slot>: no entry.
slot_is_free() { [[ ! -f "$1/$2.env" ]]; }

# slot_stale <registry> <slot>: an entry whose worktree no longer exists.
slot_stale() {
  local file="$1/$2.env" wt
  [[ -f "$file" ]] || return 1
  wt="$(slot_key "$file" CHELADB_WORKTREE)"
  [[ -z "$wt" || ! -d "$wt" ]]
}

# slot_keys: the name of every slot variable (slot N's set, the largest).
slot_keys() { slot_env 1 /x | cut -d= -f1; }

# The variables that say which slot a worktree is: an exported value may not contradict the
# worktree's .env.local.
SLOT_IDENTITY_KEYS=(CHELADB_SLOT CHELADB_COMPOSE_PROJECT CHELADB_DEV_DIR CHELADB_METADATA_DB CHELADB_METADATA_DATABASE_URL)

# slot_write_entry <registry> <slot> <worktree>
slot_write_entry() {
  local tmpfile
  tmpfile="$(mktemp "$1/.$2.env.XXXXXX")"
  slot_env "$2" "$3" >"$tmpfile"
  chmod 644 "$tmpfile"
  mv -f "$tmpfile" "$1/$2.env"
}

# slot_alloc <registry> <worktree>: prints the worktree's slot, registering the lowest free
# one (1-9) if it has none. Serialised with flock when available.
slot_alloc() {
  local reg="$1" wt="$2" n lock
  mkdir -p "$reg"
  chmod 755 "$reg" 2>/dev/null || true
  if command -v flock >/dev/null; then
    exec {lock}>"$reg/.lock"
    flock "$lock"
  fi
  if ! n="$(slot_of "$reg" "$wt")"; then
    n=""
    for candidate in $(seq 1 "$SLOT_MAX"); do
      if slot_is_free "$reg" "$candidate"; then
        n="$candidate"
        break
      fi
    done
  fi
  if [[ -n "$n" ]]; then slot_write_entry "$reg" "$n" "$wt"; fi
  if [[ -n "${lock:-}" ]]; then exec {lock}>&-; fi
  if [[ -z "$n" ]]; then
    local stale="" candidate
    for candidate in $(seq 1 "$SLOT_MAX"); do
      if slot_stale "$reg" "$candidate"; then stale+=" just stack-free $candidate;"; fi
    done
    echo "no free slot (1-$SLOT_MAX): just stack lists them; just wt-remove <branch> frees one${stale:+; stale entries, whose worktree is gone:$stale}" >&2
    return 1
  fi
  echo "$n"
}

# slot_alloc_new <registry> <worktree>: slot_alloc for a worktree about to be created. Refuses
# a directory that exists or a path the registry already names, so a failed `wt-add` can
# release what it registered without touching a live worktree's slot.
slot_alloc_new() {
  local n
  if [[ -e "$2" ]]; then
    echo "$2 already exists: wt-add only creates new worktrees (just stack-claim gives an existing one a slot)" >&2
    return 1
  fi
  if n="$(slot_of "$1" "$2")"; then
    echo "slot $n already names $2 (just stack-free $n, or remove $1/$n.env if that is a leftover)" >&2
    return 1
  fi
  slot_alloc "$1" "$2"
}

# slot_release <registry> <worktree>: removes the worktree's entry; prints the freed slot.
slot_release() {
  local n
  n="$(slot_of "$1" "$2")" || return 1
  rm -f "$1/$n.env"
  echo "$n"
}

# slot_write_env_local <slot> <worktree> <registry>: the worktree's .env.local.
slot_write_env_local() {
  {
    echo "# Written by just wt-add: this worktree is slot $1 (registry entry: $3/$1.env)."
    echo "# Every just recipe loads it; just stack lists the slots. Regenerate with just stack-claim."
    slot_env "$1" "$2"
  } >"$2/.env.local"
}

# slot_load <worktree>: exports the worktree's slot variables. A variable already in the
# environment wins, then the worktree's .env.local, then the values derived from CHELADB_SLOT
# (0 when unset). The identity variables (SLOT_IDENTITY_KEYS) are the exception: when the
# worktree has a .env.local, an exported value that contradicts it is refused. Without one
# (slot 0) only the metadata database stays overridable (a scratch one, say); a slot-N
# CHELADB_SLOT, project or dev dir is refused. Also sets CHELADB_SHARED_DIR (the main checkout's .cheladb: the shared tier's
# bind mounts must name the same paths from every worktree), SLOT_ROOT and SLOT_KEYS.
slot_load() {
  local wt line k v n
  wt="$(cd "$1" && pwd)"
  local -A file=()
  if [[ -f "$wt/.env.local" ]]; then
    while IFS= read -r line || [[ -n "$line" ]]; do
      [[ "$line" =~ ^([A-Za-z_][A-Za-z0-9_]*)=(.*)$ ]] || continue
      file[${BASH_REMATCH[1]}]="${BASH_REMATCH[2]}"
    done <"$wt/.env.local"
  fi
  for k in "${SLOT_IDENTITY_KEYS[@]}"; do
    if [[ -n "${file[$k]+set}" && -n "${!k+set}" && "${!k}" != "${file[$k]}" ]]; then
      echo "$k=${!k} is exported, but $wt/.env.local says $k=${file[$k]}: unset it (a shell left from another worktree?)" >&2
      return 1
    fi
  done
  if [[ ! -f "$wt/.env.local" ]]; then
    # No .env.local: this checkout is slot 0. Only the metadata database may be redirected
    # (scratch runs); a slot-N identity here would run slot N's stack from the wrong checkout.
    local want
    for want in CHELADB_SLOT=0 CHELADB_COMPOSE_PROJECT=cheladb-s0 CHELADB_DEV_DIR=.cheladb/dev; do
      k="${want%%=*}"
      if [[ -n "${!k+set}" && "${!k}" != "${want#*=}" ]]; then
        echo "$k=${!k} is exported, but $wt has no .env.local (it is slot 0, $k=${want#*=}): unset it (a shell left from another worktree? just stack-claim gives a worktree its slot)" >&2
        return 1
      fi
    done
  fi
  n="${CHELADB_SLOT:-${file[CHELADB_SLOT]:-0}}"
  if ! [[ "$n" =~ ^[0-9]$ ]]; then
    echo "CHELADB_SLOT=$n is not a slot (0-$SLOT_MAX); see $wt/.env.local" >&2
    return 1
  fi
  SLOT_KEYS=()
  while IFS= read -r line; do
    k="${line%%=*}"
    v="${line#*=}"
    SLOT_KEYS+=("$k")
    [[ -z "${!k+set}" ]] || continue
    [[ -z "${file[$k]+set}" ]] || v="${file[$k]}"
    export "$k=$v"
  done < <(slot_env "$n" "$wt")
  export CHELADB_SHARED_DIR="${CHELADB_SHARED_DIR:-$(slot_main_dir "$wt")/.cheladb}"
  SLOT_KEYS+=(CHELADB_SHARED_DIR)
  SLOT_ROOT="$wt"
}

# slot_load_entry <registry> <slot> <checkout>: before tearing slot <slot> down. Every slot
# variable is cleared, then filled from the registry entry alone (never from the environment
# or a worktree's .env.local), and the entry must name slot <slot>'s own database, project and
# dev dir. Refuses slot 0. <checkout> provides the compose files and the shared .cheladb.
slot_load_entry() {
  local reg="$1" n="$2" here="$3" file line k
  if ! [[ "$n" =~ ^[1-9]$ ]]; then
    echo "refusing to tear down slot '$n': only slots 1-$SLOT_MAX are torn down" >&2
    return 1
  fi
  file="$reg/$n.env"
  [[ -f "$file" ]] || {
    echo "no registry entry $file" >&2
    return 1
  }
  for k in $(slot_keys) CHELADB_SHARED_DIR SLOT_COMPOSE_DIR; do unset "$k"; done
  while IFS= read -r line || [[ -n "$line" ]]; do
    [[ "$line" =~ ^([A-Za-z_][A-Za-z0-9_]*)=(.*)$ ]] || continue
    export "${BASH_REMATCH[1]}=${BASH_REMATCH[2]}"
  done <"$file"
  local want
  for want in "CHELADB_SLOT=$n" "CHELADB_METADATA_DB=cheladb_metadata_s$n" \
    "CHELADB_COMPOSE_PROJECT=cheladb-s$n" "CHELADB_DEV_DIR=.cheladb/dev/s$n"; do
    k="${want%%=*}"
    if [[ "${!k-}" != "${want#*=}" ]]; then
      echo "refusing to tear down slot $n: $file says $k=${!k-} (expected ${want#*=})" >&2
      return 1
    fi
  done
  export CHELADB_SHARED_DIR
  CHELADB_SHARED_DIR="$(slot_main_dir "$here")/.cheladb"
  SLOT_ROOT="$(cd "$here" && pwd)"
}

# slot_load_all <worktree>: slot_load, then demo.env (bootstrap's identity and key paths) as
# `just` sourced it before slots, without letting its slot-0 URLs override the slot's.
slot_load_all() {
  slot_load "$1" || return 1
  local demo="$SLOT_ROOT/.cheladb/zitadel/demo.env" saved="" k
  if [[ ! -f "$demo" ]]; then
    slot_load_studio_client
    return 0
  fi
  for k in "${SLOT_KEYS[@]}"; do saved+="export $k=$(printf '%q' "${!k}");"; done
  set -a
  # shellcheck disable=SC1090
  source "$demo"
  set +a
  eval "$saved"
  slot_load_studio_client
}

# slot_load_studio_client: slot N's Studio signs in with its own Zitadel app (W7), whose client
# id `cheladb dev bootstrap` writes to .cheladb/zitadel/studio-s<N>.env. It wins over the
# Studio .env.local all worktrees share (slot 0's), as VITE_API_URL does.
slot_load_studio_client() {
  local file="$SLOT_ROOT/.cheladb/zitadel/studio-s$CHELADB_SLOT.env" id
  ((CHELADB_SLOT > 0)) && [[ -f "$file" ]] || return 0
  id="$(slot_key "$file" VITE_ZITADEL_CLIENT_ID)"
  if [[ -n "$id" && -z "${VITE_ZITADEL_CLIENT_ID+set}" ]]; then export VITE_ZITADEL_CLIENT_ID="$id"; fi
}

# --- Compose and Docker (after slot_load) ----------------------------------------------------
# SLOT_COMPOSE_DIR (default: the worktree's infra/compose) lets `just wt-remove` tear a slot
# down with the current checkout's files.

SLOT_LIB="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# compose_shared <args...>: docker compose on the shared tier (project cheladb).
compose_shared() {
  local dir="${SLOT_COMPOSE_DIR:-$SLOT_ROOT/infra/compose}"
  docker compose -p cheladb -f "$dir/shared.yml" --env-file "$dir/.env.example" "$@"
}

# slot_shared_oneshot <service>: waits for a shared one-shot service to finish and succeeds
# only if it exited 0. (`docker compose wait` says "no containers" once it has already exited.)
slot_shared_oneshot() {
  local service="$1" state i
  for ((i = 0; i < ${SLOT_ONESHOT_TRIES:-120}; i++)); do
    state="$(compose_shared ps -a --format '{{.State}} {{.ExitCode}}' "$service" 2>/dev/null | head -n1)"
    case "$state" in
      "exited 0") return 0 ;;
      exited*) echo "$service exited with code ${state#exited }" >&2; return 1 ;;
      "") echo "$service has no container" >&2; return 1 ;;
    esac
    sleep "${SLOT_ONESHOT_POLL:-0.5}"
  done
  echo "$service still not finished ($state)" >&2
  return 1
}

# compose_slot <args...>: docker compose on this slot's app tier (project cheladb-s<N>).
compose_slot() {
  local dir="${SLOT_COMPOSE_DIR:-$SLOT_ROOT/infra/compose}"
  docker compose -p "$CHELADB_COMPOSE_PROJECT" -f "$dir/slot.yml" --env-file "$dir/.env.example" "$@"
}

# slot_describe <container ids...>: one "  name<TAB>status" line each.
slot_describe() {
  local args=() id
  for id in "$@"; do args+=(--filter "id=$id"); done
  docker ps -a "${args[@]}" --format '  {{.Names}}	{{.Status}}'
}

# slot_compute_ids: ids of this slot's compute containers (label cheladb.dev/slot). Slot 0
# also owns computes created before slots existed (no slot label).
slot_compute_ids() {
  docker ps -a --filter label=cheladb.dev/endpoint-id \
    --format '{{.ID}} {{.Label "cheladb.dev/slot"}}' |
    awk -v slot="$CHELADB_SLOT" '$2 == slot || (slot == 0 && $2 == "") { print $1 }'
}

# slot_old_layout: the pre-slot NATS, Valkey and proxy containers still in project cheladb.
slot_old_layout() {
  local svc
  for svc in nats valkey proxy; do
    docker ps -aq --filter label=com.docker.compose.project=cheladb \
      --filter "label=com.docker.compose.service=$svc"
  done
}

# slot_running_projects: the slot projects (cheladb-s<N>) with a running container.
slot_running_projects() {
  docker ps --format '{{.Label "com.docker.compose.project"}}' | grep -E '^cheladb-s[0-9]$' | sort -u || true
}

# slot_wait_metadata: waits up to 60 s for the shared metadata Postgres.
slot_wait_metadata() {
  for _ in $(seq 60); do
    if compose_shared exec -T metadata-db pg_isready -U cheladb -d cheladb_metadata >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  echo "the metadata Postgres did not become ready (compose logs metadata-db)" >&2
  return 1
}

# slot_ensure_db: creates this slot's metadata database if it's missing (slot 0's is created
# with the volume; CHELADB_METADATA_DB may also name a scratch one). `just dev` migrates it.
slot_ensure_db() {
  local db="$CHELADB_METADATA_DB" exists
  [[ "$db" != cheladb_metadata ]] || return 0
  [[ "$db" =~ ^[a-z_][a-z0-9_]{0,62}$ ]] || {
    echo "CHELADB_METADATA_DB=$db is not a plain database name" >&2
    return 1
  }
  exists="$(compose_shared exec -T metadata-db psql -U cheladb -d cheladb_metadata -tAc \
    "SELECT 1 FROM pg_database WHERE datname = '$db'")"
  if [[ "$exists" != 1 ]]; then
    compose_shared exec -T metadata-db psql -U cheladb -d cheladb_metadata -v ON_ERROR_STOP=1 -qc \
      "CREATE DATABASE $db OWNER cheladb"
    echo "created the metadata database $db"
  fi
}

# slot_metadata_up: whether this slot's metadata database exists and answers (pg_isready
# answers for a server whose database was never created).
slot_metadata_up() {
  compose_shared exec -T metadata-db psql -U cheladb -d "$CHELADB_METADATA_DB" -tAc 'SELECT 1' >/dev/null 2>&1
}

# --- Recipes' bodies (after slot_load) -------------------------------------------------------

# slot_dev_down: stops the host processes `just dev` started for this slot.
slot_dev_down() {
  local pids="$SLOT_ROOT/$CHELADB_DEV_DIR/pids" pid cmd
  if [[ ! -f "$pids" ]]; then
    echo "no $CHELADB_DEV_DIR/pids — nothing to stop"
    return 0
  fi
  while read -r pid; do
    [[ -n "$pid" ]] || continue
    # A stale pids file (reboot, crash) can name a pid the OS has since
    # reused: only touch processes that are still our cargo/pnpm apps.
    cmd="$(tr '\0' ' ' 2>/dev/null <"/proc/$pid/cmdline" || true)"
    if [[ ! "$cmd" =~ (cargo|pnpm|target/debug/|vite) ]]; then
      echo "skipping pid $pid (${cmd:-not running})"
      continue
    fi
    # `just dev` starts each app under setsid, so its pid is also its process
    # group id: killing the group also stops grandchildren (cargo → binary,
    # pnpm → vite) that a plain `kill` would orphan.
    kill -- -"$pid" 2>/dev/null || kill "$pid" 2>/dev/null || true
  done <"$pids"
  rm -f "$pids"
  echo "dev host processes stopped"
}

# slot_sweep: removes this slot's computes (and, in slot 0, the old managed containers and
# volumes), deletes the storage-controller tenants its metadata database names, and clears its
# `databases`, `branches` and `neon_endpoints` rows. Other slots are left alone.
slot_sweep() {
  local ids volumes tenants tenant counts
  if ((CHELADB_SLOT == 0)); then
    ids="$(docker ps -aq --filter 'label=cheladb.managed=true' 2>/dev/null || true)"
    if [[ -z "${ids}" ]]; then
      echo "no managed tenant containers to remove"
    else
      echo "removing managed tenant containers:"
      # shellcheck disable=SC2086
      slot_describe ${ids}
      # shellcheck disable=SC2086
      docker stop ${ids} >/dev/null
      # -v also drops anonymous volumes of databases created before named volumes.
      # shellcheck disable=SC2086
      docker rm -f -v ${ids} >/dev/null
      echo "removed $(echo "${ids}" | wc -w | tr -d ' ') container(s)"
    fi
    volumes="$(docker volume ls -q --filter 'label=cheladb.managed=true' 2>/dev/null || true)"
    if [[ -z "${volumes}" ]]; then
      echo "no managed data volumes to remove"
    else
      # shellcheck disable=SC2086
      docker volume rm -f ${volumes} >/dev/null
      echo "removed $(echo "${volumes}" | wc -w | tr -d ' ') data volume(s)"
    fi
  fi

  ids="$(slot_compute_ids)"
  if [[ -z "${ids}" ]]; then
    echo "no Neon compute containers to remove (slot $CHELADB_SLOT)"
  else
    echo "removing slot $CHELADB_SLOT's Neon compute containers:"
    # shellcheck disable=SC2086
    slot_describe ${ids}
    # shellcheck disable=SC2086
    docker rm -f ${ids} >/dev/null
    echo "removed $(echo "${ids}" | wc -w | tr -d ' ') compute container(s)"
  fi

  if slot_metadata_up; then
    LOG_PREFIX=sweep-dbs
    # shellcheck source=neon-common.sh
    . "$SLOT_LIB/neon-common.sh"
    tenants="$(metadata_sql <<<"SELECT DISTINCT tenant_id FROM neon_endpoints;")"
    for tenant in ${tenants}; do
      delete_tenant "${tenant}"
    done
    echo "deleted $(echo "${tenants}" | wc -w | tr -d ' ') storage-controller tenant(s)"
    counts="$(metadata_sql <<<"WITH e AS (DELETE FROM neon_endpoints RETURNING 1), b AS (DELETE FROM branches RETURNING 1), d AS (DELETE FROM databases RETURNING 1) SELECT (SELECT count(*) FROM e) || ' ' || (SELECT count(*) FROM d);")"
    echo "cleared ${counts% *} Neon endpoint row(s) and ${counts#* } metadata database row(s) in $CHELADB_METADATA_DB"
  else
    echo "metadata Postgres ($CHELADB_METADATA_DB) not running — skipped clearing databases table (just up && just sweep-dbs)"
  fi
}

# slot_pids_alive <pids file>: whether any process it names still runs.
slot_pids_alive() {
  local pid
  [[ -f "$1" ]] || return 1
  while read -r pid; do
    if [[ -n "$pid" && -d "/proc/$pid" ]]; then return 0; fi
  done <"$1"
  return 1
}

# slot_stack <main checkout> <registry>: the shared tier and every slot, one line each.
slot_stack() {
  local main="$1" reg="$2" n wt state tier dev pids running
  running="$(docker ps --format '{{.Label "com.docker.compose.project"}}' 2>/dev/null | sort | uniq -c)"
  tier_of() { awk -v p="$1" '$2 == p { print $1 " running"; f = 1 } END { if (!f) print "down" }' <<<"$running"; }
  echo "shared tier (project cheladb): $(tier_of cheladb)"
  printf '%-4s %-9s %-11s %-9s %-7s %-7s %-6s %s\n' slot entry "app tier" dev gateway studio proxy worktree
  for n in $(seq 0 "$SLOT_MAX"); do
    if ((n == 0)); then
      wt="$main (and any worktree without a slot)"
      state=implicit
    elif [[ -f "$reg/$n.env" ]]; then
      wt="$(slot_key "$reg/$n.env" CHELADB_WORKTREE)"
      if [[ -d "$wt" ]]; then state=taken; else state=stale; fi
    else
      wt="-"
      state=free
    fi
    tier="$(tier_of "cheladb-s$n")"
    pids="$main/.cheladb/dev/pids"
    ((n == 0)) || pids="$main/.cheladb/dev/s$n/pids"
    dev=down
    if slot_pids_alive "$pids"; then dev=running; fi
    [[ "$state" != free || "$tier" != down || "$dev" != down ]] || continue
    printf '%-4s %-9s %-11s %-9s %-7s %-7s %-6s %s\n' "$n" "$state" "$tier" "$dev" \
      "$(slot_port 8000 "$n")" "$(slot_port 3000 "$n")" "$(slot_port 4432 "$n")" "$wt"
  done
}

# slot_wipe: after slot_load_entry. Stops the slot's dev apps, deletes its databases (tenants,
# computes, rows) and drops its metadata database, then removes its app tier and volumes.
slot_wipe() {
  local db="$CHELADB_METADATA_DB"
  [[ "$db" =~ ^cheladb_metadata_s[1-9]$ ]] || {
    echo "refusing to drop $db: not a slot's metadata database" >&2
    return 1
  }
  slot_dev_down
  if slot_metadata_up; then
    slot_sweep
    compose_shared exec -T metadata-db psql -U cheladb -d cheladb_metadata -v ON_ERROR_STOP=1 -qc \
      "DROP DATABASE IF EXISTS $db WITH (FORCE)"
    echo "dropped $db"
  else
    slot_compute_ids | xargs -r docker rm -f
    echo "no $db (or the metadata Postgres is down): nothing to sweep or drop"
  fi
  compose_slot down -v
}

# slot_shared_drift: the shared services whose running container was made from another
# configuration than shared.yml renders now (images exported first, as for `up`).
slot_shared_drift() {
  local svc want have
  while read -r svc want; do
    [[ -n "$svc" ]] || continue
    have="$(docker ps -a --filter label=com.docker.compose.project=cheladb \
      --filter "label=com.docker.compose.service=$svc" \
      --format '{{.Label "com.docker.compose.config-hash"}}' | head -n1)"
    if [[ -n "$have" && "$have" != "$want" ]]; then echo "$svc"; fi
  done < <(compose_shared config --hash '*')
}
