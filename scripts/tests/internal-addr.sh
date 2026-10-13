#!/usr/bin/env bash
# Offline tests for scripts/internal-addr.sh: where the host apps' internal listener binds so
# that containers (storage controller, proxy, computes) reach it at `host.docker.internal`.
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT="$HERE/../internal-addr.sh"
FAILS=0

fail() { echo "  FAIL: $*"; FAILS=$((FAILS + 1)); }

# run <env assignments...>: the script's output with only the given variables set (docker is
# never called: the overrides stand in for it).
run() { env -i PATH="$PATH" "$@" bash "$SCRIPT"; }

expect() { # expect <name> <want> <got>
  if [ "$2" != "$3" ]; then fail "$1: want [$2], got [$3]"; fi
}

echo "native Linux Docker: the bridge gateway"
got="$(run CHELABASE_DOCKER_OS='Ubuntu 26.04 LTS' CHELABASE_DOCKER_BRIDGE=172.17.0.1)"
expect native "export CHELABASE_INTERNAL_ADDR=172.17.0.1:8083
export CHELABASE_INTERNAL_URL=http://172.17.0.1:8083" "$got"

echo "another bridge address"
got="$(run CHELABASE_DOCKER_OS='Fedora Linux 44' CHELABASE_DOCKER_BRIDGE=10.200.0.1)"
expect other-bridge "export CHELABASE_INTERNAL_ADDR=10.200.0.1:8083
export CHELABASE_INTERNAL_URL=http://10.200.0.1:8083" "$got"

echo "Docker Desktop (WSL2, macOS): loopback"
got="$(run CHELABASE_DOCKER_OS='Docker Desktop' CHELABASE_DOCKER_BRIDGE=172.17.0.1)"
expect desktop "export CHELABASE_INTERNAL_ADDR=127.0.0.1:8083
export CHELABASE_INTERNAL_URL=http://127.0.0.1:8083" "$got"

echo "an explicit address wins, and the URL follows it"
got="$(run CHELABASE_INTERNAL_ADDR=192.168.5.5:9000 CHELABASE_DOCKER_OS='Ubuntu' CHELABASE_DOCKER_BRIDGE=172.17.0.1)"
expect explicit "export CHELABASE_INTERNAL_ADDR=192.168.5.5:9000
export CHELABASE_INTERNAL_URL=http://192.168.5.5:9000" "$got"

echo "an explicit URL is kept as given"
got="$(run CHELABASE_INTERNAL_ADDR=0.0.0.0:8083 CHELABASE_INTERNAL_URL=http://172.17.0.1:8083 CHELABASE_DOCKER_OS='Ubuntu')"
expect explicit-url "export CHELABASE_INTERNAL_ADDR=0.0.0.0:8083
export CHELABASE_INTERNAL_URL=http://172.17.0.1:8083" "$got"

echo "no Docker answer: loopback (the old default), never an empty address"
got="$(run CHELABASE_DOCKER_OS= CHELABASE_DOCKER_BRIDGE=)"
expect no-docker "export CHELABASE_INTERNAL_ADDR=127.0.0.1:8083
export CHELABASE_INTERNAL_URL=http://127.0.0.1:8083" "$got"

echo "a malformed bridge answer falls back to loopback"
got="$(run CHELABASE_DOCKER_OS='Ubuntu' CHELABASE_DOCKER_BRIDGE='not an ip; rm -rf /')"
expect malformed "export CHELABASE_INTERNAL_ADDR=127.0.0.1:8083
export CHELABASE_INTERNAL_URL=http://127.0.0.1:8083" "$got"

echo "the recipes that run the host apps or live tests evaluate it"
TOP="$(git -C "$HERE" rev-parse --show-toplevel)"
for recipe in dev worker-live; do
  body="$(awk -v r="^$recipe[ :]" '$0 ~ r {on=1; next} on && /^[^ \t#]/ {on=0} on' "$TOP/justfile")"
  case "$body" in
    *scripts/internal-addr.sh*) ;;
    *) fail "recipe $recipe doesn't evaluate scripts/internal-addr.sh" ;;
  esac
done
case "$(cat "$TOP/scripts/live.sh")" in
  *internal-addr.sh*) ;;
  *) fail "scripts/live.sh doesn't evaluate scripts/internal-addr.sh for live runs" ;;
esac

if [ "$FAILS" -gt 0 ]; then
  echo "$FAILS failure(s)"
  exit 1
fi
echo "ok"
