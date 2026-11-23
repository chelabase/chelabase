#!/usr/bin/env bash
# mcp-core m3: the MCP sandbox runtime image runs locked down. Starts the local image
# (`docker buildx bake -f infra/images/docker-bake.hcl --load mcp-runtime`, or MCP_RUNTIME_IMAGE)
# with a read-only root, every capability dropped and no-new-privileges, then checks: it serves
# (the loopback probe answers, so no capability or writable path is needed), it runs as a
# non-root user, and its environment holds nothing beyond the base image's PATH and
# SSL_CERT_FILE (no secret, no token). Compose's own limits and networks are m8's check.
#
#   infra/mcp-runtime/tests/hardening.sh
set -euo pipefail
image="${MCP_RUNTIME_IMAGE:-chelabase/mcp-runtime:latest}"
fail=0
err() { echo "hardening: FAIL: $*" >&2; fail=1; }
ok() { echo "hardening: ok   $*"; }

c="$(docker run -d --read-only --cap-drop ALL --security-opt no-new-privileges \
  --memory 256m --pids-limit 64 "$image")"
trap 'docker rm -f "$c" >/dev/null 2>&1 || true' EXIT
j="$(docker inspect "$c")"
q() { jq -r ".[0] | $1" <<<"$j"; }
bb() { docker exec "$c" /usr/local/bin/busybox "$@"; }

user="$(q .Config.User)"
[[ -n "$user" && "${user%%:*}" != 0 && "${user%%:*}" != root ]] && ok "image user $user" || err "image user '$user'"
uid="$(bb id -u)"
[[ "$uid" != 0 ]] && ok "uid inside is $uid" || err "uid inside is 0"

# Serving with a read-only root and no capabilities: the probe loads a worker through the loader.
served=""
for _ in $(seq 40); do
  if bb wget -q -T 1 -O - http://127.0.0.1:9090/ 2>/dev/null | grep -q '^ok$'; then served=1; break; fi
  bb sleep 0.25
done
[[ -n "$served" ]] && ok "serves with a read-only root and no capabilities" || err "probe never answered: $(docker logs "$c" 2>&1 | tail -5)"
bb touch /etc/mcp-runtime/x 2>/dev/null && err "root is writable" || ok "root is read-only"

# workerd runs as that user too (PID 1 is the watchdog, its child workerd).
# Busybox applets inside have no links: each is called through busybox.
inside() { bb sh -c "b=/usr/local/bin/busybox; $1"; }
# The workerd process: the one whose executable is workerd (the watchdog's argv names it too).
wd='for p in /proc/[0-9]*; do [ "$($b readlink "$p/exe" 2>/dev/null)" = /usr/local/bin/workerd ] && echo "$p"; done; true'
wpid="$(inside "$wd")"
[[ -n "$wpid" ]] || err "no workerd process"
wuid="$(bb cat "$wpid/status" | awk '/^Uid:/ { print $2 }')"
[[ -n "$wuid" && "$wuid" != 0 ]] && ok "workerd runs as uid $wuid" || err "workerd uid '$wuid'"
caps="$(bb cat "$wpid/status" | awk '/^CapEff:/ { print $2 }')"
[[ "$caps" =~ ^0+$ ]] && ok "no effective capabilities" || err "CapEff $caps"

env_keys="$(q '.Config.Env // [] | map(split("=")[0]) | sort | join(" ")')"
[[ "$env_keys" == "PATH SSL_CERT_FILE" || "$env_keys" == "PATH" ]] && ok "environment: $env_keys" || err "environment: $env_keys"
live_keys="$(bb cat "$wpid/environ" | tr '\0' '\n' | cut -d= -f1 | sort -u | paste -sd ' ')"
for k in $live_keys; do
  # The image's two, Docker's HOME and HOSTNAME, and the watchdog shell's PWD and SHLVL.
  [[ " PATH SSL_CERT_FILE HOME HOSTNAME PWD SHLVL " == *" $k "* ]] || err "workerd sees env $k"
done
ok "workerd's environment: $live_keys"
[[ "$(q '[.Mounts[]] | length')" == 0 ]] && ok "no mounts (no secrets)" || err "mounts: $(q .Mounts)"
[[ "$(q '.Config.ExposedPorts // {} | keys | join(" ")')" == "8080/tcp" ]] && ok "exposes 8080 only" || err "exposed: $(q .Config.ExposedPorts)"

[[ $fail -eq 0 ]] && echo "hardening: ok"
exit $fail
