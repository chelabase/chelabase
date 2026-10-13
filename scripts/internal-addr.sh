#!/usr/bin/env bash
# Where the orchestrator's internal listener binds, so that containers (the storage controller,
# the proxy, computes) reach it at `host.docker.internal`. Prints two lines to eval:
#   eval "$(scripts/internal-addr.sh)"
#   export CHELABASE_INTERNAL_ADDR=<host:port>
#   export CHELABASE_INTERNAL_URL=http://<host:port>
# Docker Desktop (WSL2, macOS) reaches the host's loopback: 127.0.0.1:8083. Native Linux Docker
# reaches the host at the bridge gateway (for example 172.17.0.1), so the listener binds there.
# Overrides: CHELABASE_INTERNAL_ADDR (host:port; the URL follows it), CHELABASE_INTERNAL_URL (kept
# as given). Test hooks: CHELABASE_DOCKER_OS and CHELABASE_DOCKER_BRIDGE stand in for docker (set,
# even empty, they are used as the answer).
set -u

PORT=8083
LOOPBACK="127.0.0.1:$PORT"

# Only plain characters ever reach the output (it is eval'd).
safe_addr() { [[ "$1" =~ ^[][A-Za-z0-9._:-]+$ ]]; }
safe_url() { [[ "$1" =~ ^[][A-Za-z0-9._:/-]+$ ]]; }

docker_q() { # docker_q <args...>: a docker answer within 10 s, empty when there is none
  if command -v timeout >/dev/null 2>&1; then
    timeout 10 docker "$@" 2>/dev/null || true
  else
    docker "$@" 2>/dev/null || true
  fi
}

detect() {
  local os bridge
  if [ "${CHELABASE_DOCKER_OS+set}" = set ]; then
    os="$CHELABASE_DOCKER_OS"
  else
    os="$(docker_q info --format '{{.OperatingSystem}}')"
  fi
  case "$os" in
    *"Docker Desktop"*) echo "$LOOPBACK"; return ;;
  esac
  if [ "${CHELABASE_DOCKER_BRIDGE+set}" = set ]; then
    bridge="$CHELABASE_DOCKER_BRIDGE"
  else
    bridge="$(docker_q network inspect bridge --format '{{(index .IPAM.Config 0).Gateway}}')"
  fi
  if [[ "$bridge" =~ ^([0-9]{1,3})\.([0-9]{1,3})\.([0-9]{1,3})\.([0-9]{1,3})$ ]] &&
    [ "${BASH_REMATCH[1]}" -le 255 ] && [ "${BASH_REMATCH[2]}" -le 255 ] &&
    [ "${BASH_REMATCH[3]}" -le 255 ] && [ "${BASH_REMATCH[4]}" -le 255 ]; then
    echo "$bridge:$PORT"
  else
    echo "$LOOPBACK"
  fi
}

addr="${CHELABASE_INTERNAL_ADDR:-}"
if [ -n "$addr" ] && ! safe_addr "$addr"; then
  echo "internal-addr.sh: ignoring CHELABASE_INTERNAL_ADDR=$addr (not a plain host:port)" >&2
  addr=""
fi
[ -n "$addr" ] || addr="$(detect)"

url="${CHELABASE_INTERNAL_URL:-}"
if [ -n "$url" ] && ! safe_url "$url"; then
  echo "internal-addr.sh: ignoring CHELABASE_INTERNAL_URL=$url (not a plain URL)" >&2
  url=""
fi
[ -n "$url" ] || url="http://$addr"

echo "export CHELABASE_INTERNAL_ADDR=$addr"
echo "export CHELABASE_INTERNAL_URL=$url"
