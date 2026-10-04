#!/usr/bin/env bash
# `just stack-reset-slots`: once per machine, after pulling the change that removed the
# per-worktree slots. Removes what the slots left behind and brings the one stack up:
#   1. stops the slot projects chelabase-s0..9 (containers and networks) and removes their NATS
#      and Valkey volumes (any other volume of theirs is listed and kept);
#   2. removes the hook relay's container and image;
#   3. stops the dev apps a slot left running (each .chelabase/dev/s<N>/pids process group, as
#      `just dev-down` does), then deletes the slot files: .chelabase/slots, .chelabase/dev/s<N>, .chelabase/zitadel/studio-s<N>.env,
#      every worktree's generated .env.local (one that sets CHELABASE_SLOT=), the shared
#      .worktrees/.cargo/config.toml and .worktrees/target;
#   4. `just deps-up` with the one Compose file;
#   5. drops the slot metadata databases (chelabase_metadata_s<N>).
# Never touches the stack's own volumes, the chelabase_metadata database or any key. Idempotent:
# a second run finds nothing to do. Prints every step; asks before changing anything unless
# --yes. --dry-run prints the plan and changes nothing.
# CHELABASE_MAIN (tests): the main checkout to clean, instead of this repository's.
set -euo pipefail

yes=0 dry=0
for arg in "$@"; do
  case "$arg" in
    --yes | -y) yes=1 ;;
    --dry-run | -n) dry=1 ;;
    *)
      echo "usage: stack-reset-slots.sh [--yes] [--dry-run]" >&2
      exit 2
      ;;
  esac
done

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# Fails closed: without a main checkout every path below would be relative to wherever we are.
if [[ -n "${CHELABASE_MAIN:-}" ]]; then
  main="$CHELABASE_MAIN"
elif common="$(git -C "$here" rev-parse --path-format=absolute --git-common-dir 2>/dev/null)" &&
  [[ -n "$common" ]]; then
  main="$(dirname "$common")"
else
  echo "stack-reset-slots: can't find the main checkout (git rev-parse --git-common-dir failed in $here)" >&2
  exit 2
fi
if [[ "$main" != /* || ! -e "$main/.git" ]]; then
  echo "stack-reset-slots: $main is not the main checkout (no .git there); nothing changed" >&2
  exit 2
fi
compose_dir="$here/infra/compose"
compose=(docker compose -f "$compose_dir/docker-compose.yml" --env-file "$compose_dir/.env.example")
say_apps=()
say() { echo "[stack-reset-slots] $*"; }

# --- what there is to remove -------------------------------------------------------------------
declare -A containers=() networks=() volumes=() kept_volumes=()
for n in $(seq 0 9); do
  project="chelabase-s$n"
  label="label=com.docker.compose.project=$project"
  ids="$(docker ps -aq --filter "$label" 2>/dev/null | tr '\n' ' ' || true)"
  nets="$(docker network ls -q --filter "$label" 2>/dev/null | tr '\n' ' ' || true)"
  vols="" other=""
  for v in $(docker volume ls -q --filter "$label" 2>/dev/null || true); do
    if [[ "$v" == "${project}_nats_data" || "$v" == "${project}_valkey_data" ]]; then
      vols+="$v "
    else
      other+="$v "
    fi
  done
  [[ -z "${ids// /}" ]] || containers[$project]="${ids% }"
  [[ -z "${nets// /}" ]] || networks[$project]="${nets% }"
  [[ -z "${vols// /}" ]] || volumes[$project]="${vols% }"
  [[ -z "${other// /}" ]] || kept_volumes[$project]="${other% }"
done
relay="$(docker ps -aq --filter 'name=^chelabase-hook-relay-1$' 2>/dev/null | tr '\n' ' ' || true)"
relay="${relay% }"
relay_image=""
if docker image inspect chelabase/hook-relay:dev >/dev/null 2>&1; then relay_image=chelabase/hook-relay:dev; fi

files=()
[[ ! -e "$main/.chelabase/slots" ]] || files+=("$main/.chelabase/slots")
for f in "$main"/.chelabase/dev/s[0-9] "$main"/.chelabase/zitadel/studio-s[0-9].env; do
  [[ ! -e "$f" ]] || files+=("$f")
done
for f in "$main/.env.local" "$main"/.worktrees/*/.env.local; do
  # Only the files wt-add wrote for a slot; anyone's own .env.local stays.
  if [[ -f "$f" ]] && grep -q '^CHELABASE_SLOT=' "$f"; then files+=("$f"); fi
done
[[ ! -e "$main/.worktrees/.cargo/config.toml" ]] || files+=("$main/.worktrees/.cargo/config.toml")
[[ ! -e "$main/.worktrees/target" ]] || files+=("$main/.worktrees/target")

# Dev apps a slot left running: pids from .chelabase/dev/s<N>/pids that are still our cargo/pnpm
# apps (a stale file can name a pid the OS has reused: same guard as `just dev-down`).
app_re='(cargo|pnpm|target/debug/|vite)'
app_pids=()
for pf in "$main"/.chelabase/dev/s[0-9]/pids; do
  [[ -f "$pf" ]] || continue
  while read -r pid; do
    [[ "$pid" =~ ^[0-9]+$ ]] || continue
    cmd="$(tr '\0' ' ' 2>/dev/null <"/proc/$pid/cmdline" || true)"
    if [[ "$cmd" =~ $app_re ]]; then
      app_pids+=("$pid")
      say_apps+=("  stop slot app pid $pid (${pf#"$main"/}): $cmd")
    else
      say_apps+=("  skipping pid $pid in ${pf#"$main"/} (${cmd:-not running})")
    fi
  done <"$pf"
done

# The metadata databases are listed only when the metadata Postgres is up (step 5 runs after
# deps-up, so it finds them then).
slot_dbs() {
  "${compose[@]}" exec -T metadata-db psql -U chelabase -d chelabase_metadata -qtA \
    -c "SELECT datname FROM pg_database WHERE datname ~ '^chelabase_metadata_s[0-9]\$' ORDER BY 1" 2>/dev/null |
    grep -E '^chelabase_metadata_s[0-9]$' || true
}
dbs="$(slot_dbs | tr '\n' ' ')"

# --- the plan ----------------------------------------------------------------------------------
say "plan:"
if ((${#containers[@]} + ${#networks[@]} + ${#volumes[@]} + ${#kept_volumes[@]} == 0)); then
  say "  no slot projects (chelabase-s0..9)"
fi
for project in $(printf '%s\n' "${!containers[@]}" "${!networks[@]}" "${!volumes[@]}" "${!kept_volumes[@]}" | sort -u); do
  say "  $project: remove containers [${containers[$project]:-none}], networks [${networks[$project]:-none}], volumes [${volumes[$project]:-none}]"
  [[ -z "${kept_volumes[$project]:-}" ]] || say "  $project: keep unknown volumes [${kept_volumes[$project]}] (docker volume rm them yourself)"
done
say "  hook relay: container [${relay:-none}], image [${relay_image:-none}]"
for line in "${say_apps[@]}"; do say "$line"; done
if ((${#files[@]})); then
  for f in "${files[@]}"; do say "  delete $f"; done
else
  say "  no slot files"
fi
say "  then: just deps-up (the one Compose file)"
say "  then: drop the slot metadata databases [${dbs:-none found now; checked again after deps-up}]"

if ((dry)); then
  say "dry run: nothing changed"
  exit 0
fi
if ((!yes)); then
  answer=""
  read -r -p "[stack-reset-slots] proceed? [y/N] " answer || true
  if [[ "$answer" != [yY] && "$answer" != [yY][eE][sS] ]]; then
    say "nothing changed"
    exit 1
  fi
fi

# --- 1. the slot projects ------------------------------------------------------------------------
for project in "${!containers[@]}"; do
  say "$project: removing containers ${containers[$project]}"
  # shellcheck disable=SC2086
  docker rm -f ${containers[$project]} >/dev/null
done
for project in "${!networks[@]}"; do
  say "$project: removing networks ${networks[$project]}"
  # shellcheck disable=SC2086
  docker network rm ${networks[$project]} >/dev/null
done
for project in "${!volumes[@]}"; do
  say "$project: removing volumes ${volumes[$project]}"
  # shellcheck disable=SC2086
  docker volume rm ${volumes[$project]} >/dev/null
done

# --- 2. the hook relay ---------------------------------------------------------------------------
if [[ -n "$relay" ]]; then
  say "removing the hook relay container"
  # shellcheck disable=SC2086
  docker rm -f $relay >/dev/null
fi
if [[ -n "$relay_image" ]]; then
  say "removing the image $relay_image"
  docker image rm "$relay_image" >/dev/null
fi

# --- 3. the slot apps and files ------------------------------------------------------------------
for pid in "${app_pids[@]}"; do
  say "stopping slot app pid $pid"
  # `just dev` starts each app under setsid: its pid is its process group id.
  kill -- -"$pid" 2>/dev/null || kill "$pid" 2>/dev/null || true
done
for f in "${files[@]}"; do
  say "deleting $f"
  rm -rf -- "$f"
done
rmdir "$main/.worktrees/.cargo" 2>/dev/null || true

# --- 4. the one stack ----------------------------------------------------------------------------
say "starting the stack: just deps-up"
just --justfile "$here/justfile" --working-directory "$here" deps-up

# --- 5. the slot metadata databases ----------------------------------------------------------------
dbs="$(slot_dbs)"
for db in $dbs; do
  say "dropping database $db"
  "${compose[@]}" exec -T metadata-db psql -U chelabase -d chelabase_metadata -v ON_ERROR_STOP=1 -qtA \
    -c "DROP DATABASE IF EXISTS \"$db\"" >/dev/null
done
[[ -n "$dbs" ]] || say "no slot metadata databases"
say "done"
