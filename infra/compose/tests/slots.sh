#!/usr/bin/env bash
# Offline checks for worktree slots (scripts/lib/slots.sh) and the Compose split
# (infra/compose/shared.yml + slot.yml). Starts and stops nothing: registries live in temp
# dirs, and Docker is only used for `docker compose config`. Run: just stack-test
set -euo pipefail

root="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$root" # proto's just shim needs the repo's .prototools
failures=0
pass() { echo "ok   $1"; }
fail() {
  echo "FAIL $1" >&2
  failures=$((failures + 1))
}
check() { # check <name> <command...>
  local name="$1"
  shift
  if "$@"; then pass "$name"; else fail "$name"; fi
}
same() { # same <name> <want> <got>: prints a diff on failure
  if [[ "$2" == "$3" ]]; then pass "$1"; else
    fail "$1"
    diff <(printf '%s\n' "$2") <(printf '%s\n' "$3") >&2 || true
  fi
}

# shellcheck source=../../../scripts/lib/slots.sh
. "$root/scripts/lib/slots.sh"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
reg="$tmp/slots"
mkdir -p "$tmp/wt-a" "$tmp/wt-b" "$tmp/wt-c" "$tmp/wt-d"

# --- port arithmetic ------------------------------------------------------------------------
same "slot 0 keeps the base port" 8000 "$(slot_port 8000 0)"
same "slot 3 adds 300" 8383 "$(slot_port 8083 3)"
same "slot 9 adds 900" 3900 "$(slot_port 3000 9)"

# Every port, across all ten slots and the shared tier, is distinct.
ports_of_slot() { # the slot's ports, each once
  slot_env "$1" /x | sed -nE 's/^CHELADB_[A-Z_]*PORT=([0-9]+)$/\1/p; s/^CHELADB_[A-Z_]*ADDR=127\.0\.0\.1:([0-9]+)$/\1/p' | sort -u
}
all_ports() {
  printf '%s\n' 5433 5434 8080 8025 1234 9898 # the shared tier
  for n in $(seq 0 9); do ports_of_slot "$n"; done
}
check "no two slots (or the shared tier) share a port" test -z "$(all_ports | sort | uniq -d)"
check "each slot has 13 ports" test "$(ports_of_slot 4 | wc -l)" -eq 13

# --- the slot's variables -------------------------------------------------------------------
slot0="CHELADB_SLOT=0
CHELADB_WORKTREE=/x/main
CHELADB_COMPOSE_PROJECT=cheladb-s0
CHELADB_DEV_DIR=.cheladb/dev
CHELADB_METADATA_DB=cheladb_metadata
CHELADB_METADATA_DATABASE_URL=postgres://cheladb:cheladb@127.0.0.1:5433/cheladb_metadata
CHELADB_NATS_PORT=4222
CHELADB_NATS_MONITOR_PORT=8222
CHELADB_NATS_URL=nats://127.0.0.1:4222
CHELADB_VALKEY_PORT=6379
CHELADB_VALKEY_URL=redis://127.0.0.1:6379
CHELADB_PROXY_PG_PORT=4432
CHELADB_PROXY_HTTP_PORT=4444
CHELADB_GATEWAY_PORT=8000
CHELADB_GATEWAY_ADDR=127.0.0.1:8000
CHELADB_ORCHESTRATOR_ADDR=127.0.0.1:8081
CHELADB_ORCHESTRATOR_URL=http://127.0.0.1:8081
CHELADB_WORKER_HEALTH_ADDR=127.0.0.1:8082
CHELADB_INTERNAL_PORT=8083
CHELADB_INTERNAL_ADDR=127.0.0.1:8083
CHELADB_INTERNAL_URL=http://127.0.0.1:8083
CHELADB_CONTROL_PLANE_URI=http://host.docker.internal:8083
CHELADB_AUTH_ADDR=127.0.0.1:8084
CHELADB_AUTH_URL=http://127.0.0.1:8084
CHELADB_REST_ADDR=127.0.0.1:8085
CHELADB_REST_URL=http://127.0.0.1:8085
CHELADB_REST_INTERNAL_ADDR=127.0.0.1:8086
CHELADB_REST_INTERNAL_URL=http://127.0.0.1:8086
CHELADB_STUDIO_PORT=3000
CHELADB_STUDIO_URL=http://localhost:3000
CHELADB_STUDIO_ORIGINS=http://localhost:3000"
same "slot 0's variables are today's defaults" "$slot0" "$(slot_env 0 /x/main)"

slot3="CHELADB_SLOT=3
CHELADB_WORKTREE=/x/wt
CHELADB_COMPOSE_PROJECT=cheladb-s3
CHELADB_DEV_DIR=.cheladb/dev/s3
CHELADB_METADATA_DB=cheladb_metadata_s3
CHELADB_METADATA_DATABASE_URL=postgres://cheladb:cheladb@127.0.0.1:5433/cheladb_metadata_s3
CHELADB_NATS_PORT=4522
CHELADB_NATS_MONITOR_PORT=8522
CHELADB_NATS_URL=nats://127.0.0.1:4522
CHELADB_VALKEY_PORT=6679
CHELADB_VALKEY_URL=redis://127.0.0.1:6679
CHELADB_PROXY_PG_PORT=4732
CHELADB_PROXY_HTTP_PORT=4744
CHELADB_GATEWAY_PORT=8300
CHELADB_GATEWAY_ADDR=127.0.0.1:8300
CHELADB_ORCHESTRATOR_ADDR=127.0.0.1:8381
CHELADB_ORCHESTRATOR_URL=http://127.0.0.1:8381
CHELADB_WORKER_HEALTH_ADDR=127.0.0.1:8382
CHELADB_INTERNAL_PORT=8383
CHELADB_INTERNAL_ADDR=127.0.0.1:8383
CHELADB_INTERNAL_URL=http://127.0.0.1:8383
CHELADB_CONTROL_PLANE_URI=http://host.docker.internal:8383
CHELADB_AUTH_ADDR=127.0.0.1:8384
CHELADB_AUTH_URL=http://127.0.0.1:8384
CHELADB_REST_ADDR=127.0.0.1:8385
CHELADB_REST_URL=http://127.0.0.1:8385
CHELADB_REST_INTERNAL_ADDR=127.0.0.1:8386
CHELADB_REST_INTERNAL_URL=http://127.0.0.1:8386
CHELADB_STUDIO_PORT=3300
CHELADB_STUDIO_URL=http://localhost:3300
CHELADB_STUDIO_ORIGINS=http://localhost:3300
GATEWAY_URL=http://127.0.0.1:8300
VITE_API_URL=http://localhost:8300"
same "slot 3's variables" "$slot3" "$(slot_env 3 /x/wt)"

# --- allocation -----------------------------------------------------------------------------
no_slot() { ! slot_of "$1" "$2" >/dev/null; }
check "an empty registry has no slot for a path" no_slot "$reg" "$tmp/wt-a"
same "the first worktree gets slot 1" 1 "$(slot_alloc "$reg" "$tmp/wt-a")"
same "the next one gets slot 2" 2 "$(slot_alloc "$reg" "$tmp/wt-b")"
same "allocating again keeps the slot" 1 "$(slot_alloc "$reg" "$tmp/wt-a")"
same "slot_of finds it" 2 "$(slot_of "$reg" "$tmp/wt-b")"
check "the registry entry is world-readable (the hook relay runs as nobody)" \
  test "$(stat -c %a "$reg/1.env")" = 644
same "the registry entry holds slot 1's variables" "$(slot_env 1 "$tmp/wt-a")" "$(cat "$reg/1.env")"
check "no 0.env is written (the relay's slot 0 is implicit)" test ! -e "$reg/0.env"

same "removing frees the slot" 1 "$(slot_release "$reg" "$tmp/wt-a")"
check "the entry is gone" test ! -e "$reg/1.env"
same "a freed slot is reused (lowest free)" 1 "$(slot_alloc "$reg" "$tmp/wt-c")"

rm -rf "$tmp/wt-b"
# A stale entry (its worktree is gone) may still own a database and volumes: never reused
# silently; `just stack-free <N>` wipes it.
same "an entry whose worktree is gone is skipped" 3 "$(slot_alloc "$reg" "$tmp/wt-d")"
same "and still names the old worktree" "$tmp/wt-b" "$(slot_key "$reg/2.env" CHELADB_WORKTREE)"
check "slot_stale tells a stale entry" slot_stale "$reg" 2
not_stale() { ! slot_stale "$@"; }
check "slot_stale: a live entry isn't stale" not_stale "$reg" 3

full="$tmp/full"
for n in $(seq 1 9); do mkdir -p "$tmp/f$n" && slot_alloc "$full" "$tmp/f$n" >/dev/null; done
mkdir -p "$tmp/f10"
status=0
slot_alloc "$full" "$tmp/f10" >/dev/null 2>"$tmp/err" || status=$?
check "a full registry refuses a tenth worktree" test "$status" -ne 0
check "and says how to free one" grep -q "just wt-remove" "$tmp/err"
rm -rf "$tmp/f4"
status=0
slot_alloc "$full" "$tmp/f10" >/dev/null 2>"$tmp/err" || status=$?
check "a stale entry in a full registry is not reused" test "$status" -ne 0
check "and the refusal names just stack-free 4" grep -q "just stack-free 4" "$tmp/err"

# wt-add on a worktree that already exists must not touch its slot.
mkdir -p "$tmp/live"
live_slot="$(slot_alloc "$reg" "$tmp/live")"
status=0
slot_alloc_new "$reg" "$tmp/live" >/dev/null 2>"$tmp/err" || status=$?
check "slot_alloc_new refuses a directory that exists" test "$status" -ne 0
same "and the live worktree keeps its slot" "$live_slot" "$(slot_of "$reg" "$tmp/live")"
status=0
slot_alloc_new "$reg" "$tmp/not-yet" >/dev/null 2>&1 || status=$?
check "slot_alloc_new registers a new directory" test "$status" -eq 0
check "slot_alloc_new refuses a path the registry already names" \
  bash -c '! (. "$1/scripts/lib/slots.sh"; slot_alloc_new "$2" "$3") 2>/dev/null' _ "$root" "$reg" "$tmp/not-yet"
slot_release "$reg" "$tmp/not-yet" >/dev/null
slot_release "$reg" "$tmp/live" >/dev/null

# --- .env.local -----------------------------------------------------------------------------
mkdir -p "$tmp/wt3"
slot_write_env_local 3 "$tmp/wt3" "$reg"
check ".env.local starts with a comment naming the slot and its registry entry" \
  grep -qx "# Written by just wt-add: this worktree is slot 3 (registry entry: $reg/3.env)." "$tmp/wt3/.env.local"
same ".env.local holds slot 3's variables" "$(slot_env 3 "$tmp/wt3")" "$(grep -v '^#' "$tmp/wt3/.env.local")"

loaded() { # loaded <dir> [VAR=value...]: the slot variables slot_load exports there, in a clean env
  local dir="$1"
  shift
  env -i PATH="$PATH" HOME="$HOME" "$@" bash -c '. "$1/scripts/lib/slots.sh"; slot_load "$2"; slot_env "$CHELADB_SLOT" "$CHELADB_WORKTREE" | while IFS="=" read -r k _; do echo "$k=${!k-}"; done' _ "$root" "$dir"
}
mkdir -p "$tmp/plain"
same "without .env.local, a checkout is slot 0 with today's values" \
  "$(slot_env 0 "$tmp/plain")" "$(loaded "$tmp/plain")"
same "with slot 3's .env.local, every variable is slot 3's" \
  "$(slot_env 3 "$tmp/wt3")" "$(loaded "$tmp/wt3")"
check "a variable already in the environment wins (as with any dotenv file)" \
  grep -qx "CHELADB_INTERNAL_ADDR=127.0.0.1:9999" <(loaded "$tmp/plain" CHELADB_INTERNAL_ADDR=127.0.0.1:9999)
check "slot_load exports CHELADB_SHARED_DIR" \
  test "$(env -i PATH="$PATH" bash -c '. "$1/scripts/lib/slots.sh"; slot_load "$2"; echo "$CHELADB_SHARED_DIR"' _ "$root" "$tmp/plain")" = "$tmp/plain/.cheladb"
printf 'CHELADB_SLOT=12\n' >"$tmp/plain/.env.local"
status=0
env -i PATH="$PATH" bash -c '. "$1/scripts/lib/slots.sh"; slot_load "$2"' _ "$root" "$tmp/plain" 2>/dev/null || status=$?
check "a slot outside 0-9 is refused" test "$status" -ne 0
rm "$tmp/plain/.env.local"
# demo.env (written by bootstrap) names slot 0's URLs; it must not override the slot's.
mkdir -p "$tmp/wt3/.cheladb/zitadel"
printf 'CHELADB_INTERNAL_URL=http://127.0.0.1:8083\nCHELADB_ZITADEL_ORG_ID=o1\n' >"$tmp/wt3/.cheladb/zitadel/demo.env"
got="$(env -i PATH="$PATH" bash -c '. "$1/scripts/lib/slots.sh"; slot_load_all "$2"; echo "$CHELADB_INTERNAL_URL $CHELADB_ZITADEL_ORG_ID"' _ "$root" "$tmp/wt3")"
same "slot_load_all: demo.env adds its variables but not over the slot's" "http://127.0.0.1:8383 o1" "$got"
# W7: a slot's Studio uses its own Zitadel app (bootstrap writes studio-s<N>.env).
printf 'VITE_ZITADEL_CLIENT_ID=c-s3\n' >"$tmp/wt3/.cheladb/zitadel/studio-s3.env"
got="$(env -i PATH="$PATH" bash -c '. "$1/scripts/lib/slots.sh"; slot_load_all "$2"; echo "${VITE_ZITADEL_CLIENT_ID-unset}"' _ "$root" "$tmp/wt3")"
same "slot_load_all exports the slot's Studio client id" c-s3 "$got"
mkdir -p "$tmp/plain/.cheladb/zitadel"
printf 'VITE_ZITADEL_CLIENT_ID=c-s0\n' >"$tmp/plain/.cheladb/zitadel/studio-s0.env"
got="$(env -i PATH="$PATH" bash -c '. "$1/scripts/lib/slots.sh"; slot_load_all "$2"; echo "${VITE_ZITADEL_CLIENT_ID-unset}"' _ "$root" "$tmp/plain")"
same "slot 0 keeps Studio's own .env.local client id" unset "$got"

# --- identity: the worktree's .env.local decides which slot it is ----------------------------
load_status() { # load_status <dir> [VAR=value...]: slot_load's exit status in a clean env
  local dir="$1"
  shift
  env -i PATH="$PATH" "$@" bash -c '. "$1/scripts/lib/slots.sh"; slot_load "$2"' _ "$root" "$dir" >/dev/null 2>"$tmp/err" && echo 0 || echo 1
}
same "an exported CHELADB_SLOT that contradicts .env.local is refused" 1 "$(load_status "$tmp/wt3" CHELADB_SLOT=0)"
check "and the refusal says so" grep -q "CHELADB_SLOT" "$tmp/err"
same "an exported metadata database that contradicts .env.local is refused" 1 \
  "$(load_status "$tmp/wt3" CHELADB_METADATA_DB=cheladb_metadata)"
same "so is another slot's dev dir" 1 "$(load_status "$tmp/wt3" CHELADB_DEV_DIR=.cheladb/dev)"
same "an exported identity that agrees is fine" 0 "$(load_status "$tmp/wt3" CHELADB_SLOT=3 CHELADB_METADATA_DB=cheladb_metadata_s3)"
check "slot 0 without .env.local keeps a scratch metadata database" \
  grep -qx "CHELADB_METADATA_DB=cheladb_metadata_scratch" <(loaded "$tmp/plain" CHELADB_METADATA_DB=cheladb_metadata_scratch)

same "slot 0 without .env.local refuses an exported other slot" 1 "$(load_status "$tmp/plain" CHELADB_SLOT=3)"
same "so does another slot's compose project" 1 "$(load_status "$tmp/plain" CHELADB_COMPOSE_PROJECT=cheladb-s3)"
same "and another slot's dev dir" 1 "$(load_status "$tmp/plain" CHELADB_DEV_DIR=.cheladb/dev/s3)"
same "slot 0's own identity exported is fine" 0 \
  "$(load_status "$tmp/plain" CHELADB_SLOT=0 CHELADB_COMPOSE_PROJECT=cheladb-s0 CHELADB_DEV_DIR=.cheladb/dev)"
same "a scratch metadata database stays allowed without .env.local" 0 \
  "$(load_status "$tmp/plain" CHELADB_METADATA_DB=cheladb_metadata_scratch CHELADB_METADATA_DATABASE_URL=postgres://x/y)"

# --- tearing a slot down: the registry entry alone decides ------------------------------------
entry_env() { # entry_env <slot> [VAR=value...]: slot_load_entry's identity, in a polluted env
  local n="$1"
  shift
  env -i PATH="$PATH" "$@" bash -c '. "$1/scripts/lib/slots.sh"; slot_load_entry "$2" "$3" "$1" &&
    echo "$CHELADB_SLOT $CHELADB_METADATA_DB $CHELADB_COMPOSE_PROJECT $CHELADB_DEV_DIR $CHELADB_METADATA_DATABASE_URL"' \
    _ "$root" "$reg" "$n" 2>"$tmp/err"
}
same "slot_load_entry ignores an inherited slot 0" \
  "3 cheladb_metadata_s3 cheladb-s3 .cheladb/dev/s3 postgres://cheladb:cheladb@127.0.0.1:5433/cheladb_metadata_s3" \
  "$(entry_env 3 CHELADB_SLOT=0 CHELADB_METADATA_DB=cheladb_metadata CHELADB_COMPOSE_PROJECT=cheladb-s0 \
    CHELADB_DEV_DIR=.cheladb/dev CHELADB_METADATA_DATABASE_URL=postgres://x/cheladb_metadata)"
refuses_entry() { ! entry_env "$1" >/dev/null; }
check "slot_load_entry refuses slot 0" refuses_entry 0
cp "$reg/3.env" "$tmp/3.env.bak"
sed -i 's/^CHELADB_METADATA_DB=.*/CHELADB_METADATA_DB=cheladb_metadata/' "$reg/3.env"
same "slot_load_entry refuses an entry naming another database" "" "$(entry_env 3)"
check "and says why" grep -q "cheladb_metadata_s3" "$tmp/err"
cp "$tmp/3.env.bak" "$reg/3.env"

# --- Compose: shared tier and slot tier -----------------------------------------------------
shared_yml="$root/infra/compose/shared.yml"
slot_yml="$root/infra/compose/slot.yml"
check "the old single Compose file is gone" test ! -e "$root/infra/compose/docker-compose.yml"
pin() { sed -nE "s/.*\\\$\\{$1:-([^}]+)\\}.*/\\1/p" "$2" | head -n1; }
check "shared.yml and slot.yml pin the same Neon image" \
  bash -c 'test -n "$1" && test "$1" = "$2"' _ "$(pin NEON_IMAGE "$shared_yml")" "$(pin NEON_IMAGE "$slot_yml")"

if command -v docker >/dev/null && command -v jq >/dev/null; then
  render() { # render <file> [VAR=value...]: `docker compose config` as JSON, nothing else set
    local file="$1"
    shift
    env -i PATH="$PATH" HOME="$HOME" DOCKER_CONFIG="${DOCKER_CONFIG:-$HOME/.docker}" "$@" \
      docker compose -f "$file" --env-file "$root/infra/compose/.env.example" config --format json
  }
  ports_of() { jq -r '[.services[] | .ports[]? | "\(.host_ip):\(.published)"] | sort | join(" ")'; }

  shared="$(render "$shared_yml")"
  same "the shared project keeps the name cheladb" cheladb "$(jq -r .name <<<"$shared")"
  same "the shared tier's services" \
    "create-bucket hook-relay mailpit metadata-db pageserver proxy-tls register-safekeepers safekeeper1 seaweedfs storage-broker storage-controller storcon-db zitadel zitadel-db" \
    "$(jq -r '.services | keys | join(" ")' <<<"$shared")"
  same "the shared tier publishes today's ports, on loopback" \
    "127.0.0.1:1234 127.0.0.1:5433 127.0.0.1:5434 127.0.0.1:8025 127.0.0.1:8080 127.0.0.1:9898" \
    "$(ports_of <<<"$shared")"
  same "the shared tier keeps today's volume names" \
    "metadata_pg_data neon_pageserver_data neon_proxy_tls neon_safekeeper1_data neon_seaweedfs_data neon_storcon_pg_data zitadel_pg_data" \
    "$(jq -r '.volumes | keys | join(" ")' <<<"$shared")"
  relay="$(jq -c '.services["hook-relay"]' <<<"$shared")"
  check "the storage controller calls the hook relay" \
    grep -qx -- '--control-plane-url=http://hook-relay:8099/storcon' <(jq -r '.services["storage-controller"].command[]' <<<"$shared")
  check "the hook relay is not published on the host" test "$(jq -r '.ports // [] | length' <<<"$relay")" = 0
  check "the hook relay reads the slot registry, read-only" \
    test "$(jq -r '.volumes[] | select(.target == "/slots") | "\(.source) \(.read_only)"' <<<"$relay")" = "$root/.cheladb/slots true"
  check "the hook relay reaches the slots' internal listeners on the host" \
    grep -q 'host-gateway' <<<"$(jq -c '.extra_hosts' <<<"$relay")"
  check "the hook relay image is the local build" \
    test "$(jq -r '.image' <<<"$relay")" = "cheladb/hook-relay:dev"
  check "shared mounts follow CHELADB_SHARED_DIR (the same paths from every worktree)" \
    test "$(render "$shared_yml" CHELADB_SHARED_DIR=/main/.cheladb |
      jq -r '[.services[] | .volumes[]? | select(.type == "bind") | .source] | sort | join(" ")')" = "/main/.cheladb/slots /main/.cheladb/zitadel"

  slot_render() { # slot_render <n>: slot.yml rendered with slot n's variables
    local vars
    mapfile -t vars < <(slot_env "$1" /x)
    render "$slot_yml" "${vars[@]}"
  }
  s0="$(slot_render 0)"
  same "slot 0's app tier is project cheladb-s0" cheladb-s0 "$(jq -r .name <<<"$s0")"
  same "the slot tier's services" "nats proxy valkey" "$(jq -r '.services | keys | join(" ")' <<<"$s0")"
  same "slot 0 publishes today's ports" \
    "127.0.0.1:4222 127.0.0.1:4432 127.0.0.1:4444 127.0.0.1:6379 127.0.0.1:8222" "$(ports_of <<<"$s0")"
  same "slot.yml with nothing set is slot 0" "$s0" "$(render "$slot_yml")"
  check "slot 0's proxy asks the internal listener on 8083" \
    grep -qx -- '--auth-endpoint=http://host.docker.internal:8083/proxy/api/v1' <(jq -r '.services.proxy.command[]' <<<"$s0")
  s3="$(slot_render 3)"
  same "slot 3's app tier is project cheladb-s3" cheladb-s3 "$(jq -r .name <<<"$s3")"
  same "slot 3 publishes its own ports" \
    "127.0.0.1:4522 127.0.0.1:4732 127.0.0.1:4744 127.0.0.1:6679 127.0.0.1:8522" "$(ports_of <<<"$s3")"
  proxy3="$(jq -c '.services.proxy' <<<"$s3")"
  check "slot 3's proxy asks slot 3's internal listener" \
    grep -qx -- '--auth-endpoint=http://host.docker.internal:8383/proxy/api/v1' <(jq -r '.command[]' <<<"$proxy3")
  check "the proxy joins the shared network (it reaches computes there)" \
    test "$(jq -r '.networks | keys | sort | join(" ")' <<<"$proxy3")" = "default shared"
  same "the shared network is the shared project's" "true cheladb_default" \
    "$(jq -r '.networks.shared | "\(.external) \(.name)"' <<<"$s3")"
  same "the proxy certificate volume is the shared one" "true cheladb_neon_proxy_tls" \
    "$(jq -r '.volumes.neon_proxy_tls | "\(.external) \(.name)"' <<<"$s3")"
  check "the proxy uses its slot's Valkey" \
    grep -qx -- '--redis-plain=redis://valkey:6379' <(jq -r '.command[]' <<<"$proxy3")
  check "valkey is not on the shared network (each slot's proxy finds its own)" \
    test "$(jq -r '.services.valkey.networks | keys | join(" ")' <<<"$s3")" = default
else
  pass "no docker or jq: skipped the compose config checks"
fi

# --- one-shot shared services ---------------------------------------------------------------
# `docker compose wait` says "no containers" once a one-shot has already exited, so
# slot_shared_oneshot reads `ps -a` instead. The stub answers from a script of states.
oneshot() { # oneshot <states, one per call...>: exit status of slot_shared_oneshot
  printf '%s\n' "$@" >"$tmp/oneshot-states"
  (
    docker() { # pops the next state; the last one repeats
      local first
      first="$(head -n1 "$tmp/oneshot-states")"
      if [[ "$(wc -l <"$tmp/oneshot-states")" -gt 1 ]]; then sed -i 1d "$tmp/oneshot-states"; fi
      [[ -n "$first" ]] && echo "$first"
      return 0
    }
    SLOT_COMPOSE_DIR="$tmp" SLOT_ONESHOT_POLL=0 SLOT_ONESHOT_TRIES=5 slot_shared_oneshot register-safekeepers 2>/dev/null
  )
}
fails() { ! "$@"; }
check "a one-shot that already exited 0 passes" oneshot "exited 0"
check "a one-shot that is still running is waited for" oneshot "running 0" "running 0" "exited 0"
check "a one-shot that exited non-zero fails" fails oneshot "exited 1"
check "a missing one-shot fails" fails oneshot ""
check "a one-shot that never finishes fails" fails oneshot "running 0"

# --- justfile -------------------------------------------------------------------------------
has_recipe() { just --justfile "$root/justfile" --show "$1" >/dev/null 2>&1; }
for recipe in stack stack-migrate stack-claim stack-free stack-test hook-relay-build deps-up deps-down dev dev-down sweep-dbs wt-add wt-remove; do
  check "recipe $recipe exists" has_recipe "$recipe"
done
check "dev keeps pids and logs in the slot's CHELADB_DEV_DIR" \
  bash -c "just --justfile '$root/justfile' --show dev | grep -q 'CHELADB_DEV_DIR/pids'"
check "dev-down reads the slot's pids" \
  bash -c "just --justfile '$root/justfile' --show dev-down | grep -q 'CHELADB_DEV_DIR/pids'"

check "wt-remove tears down through slot_load_entry (the registry entry, not the environment)" \
  bash -c "just --justfile '$root/justfile' --show wt-remove | grep -q 'slot_load_entry'"
check "wt-add registers a new slot (slot_alloc_new) before creating the worktree" \
  bash -c "just --justfile '$root/justfile' --show wt-add | awk '/slot_alloc_new/ { a = NR } /worktree add/ { w = NR } END { exit !(a && w && a < w) }'"
check "wt-add takes a slot argument: auto (default) or none" \
  bash -c "just --justfile '$root/justfile' --show wt-add | grep -q '^wt-add .*slot=\"auto\"'"
check "wt-add with slot none registers nothing and writes no .env.local" \
  bash -c "just --justfile '$root/justfile' --show wt-add | awk '/\"\\\$slot\" == \"none\"/ { n = NR } /^ *slot_write_env_local/ { w = NR } END { exit !(n && w && n < w) }'"
check "a full registry under slot=auto suggests slot=none or stack-free" \
  bash -c "just --justfile '$root/justfile' --show wt-add | grep -q 'base }} none' && just --justfile '$root/justfile' --show wt-add | grep -q 'just stack-free'"
check "stack-migrate always rebuilds the hook relay" \
  bash -c "just --justfile '$root/justfile' --show stack-migrate | grep -q 'hook-relay-build' && ! just --justfile '$root/justfile' --show stack-migrate | grep -q 'image inspect'"
check "zitadel-reset removes the per-slot Studio env files" \
  grep -q 'studio-s\*.env' "$root/scripts/zitadel-reset.sh"
check "cleanup refuses to reset the shared Zitadel while other slots run" \
  bash -c "just --justfile '$root/justfile' --show cleanup | grep -q 'slot_running_projects'"
check "deps-up never recreates the shared tier under other slots" \
  bash -c "just --justfile '$root/justfile' --show deps-up | grep -q 'up -d --no-recreate'"
check "deps-up checks its one-shots with slot_shared_oneshot (not compose wait, which misses an exited one)" \
  bash -c "just --justfile '$root/justfile' --show deps-up | grep -q 'slot_shared_oneshot register-safekeepers' && ! just --justfile '$root/justfile' --show deps-up | grep -q ' wait '"
check "the serverless driver check takes the slot's proxy HTTP port (CHELADB_PROXY_HTTP_PORT)" \
  bash -c "grep -q 'CHELADB_PROXY_HTTP_PORT' '$root/packages/neon-e2e/check.mjs' && ! grep -q ':4444/' '$root/packages/neon-e2e/check.mjs'"
check "moon's tasks go through just (which sets CHELADB_SHARED_DIR)" \
  bash -c "grep -q 'deps-up' '$root/infra/compose/moon.yml' && ! grep -q 'shared.yml' '$root/infra/compose/moon.yml'"

if [[ "$failures" -gt 0 ]]; then
  echo "$failures failure(s)" >&2
  exit 1
fi
echo "all passed"
