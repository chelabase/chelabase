#!/usr/bin/env bash
# edge-functions m1: the functions runtime container is locked down as compose runs it.
# shared-state: starts (or recreates) `functions-runtime` on this checkout's workbench stack
# (profile `functions`) from the local image `chelabase/functions-runtime:latest`
# (`docker buildx bake -f infra/images/docker-bake.hcl functions-runtime`), then checks
# `docker inspect`: non-root, read-only root, every capability dropped, no-new-privileges,
# memory and pids limits, restart always, no published port, no secret mounted, and only the
# `functions` (internal) and `functions-egress` networks.
#
#   infra/functions-runtime/tests/hardening.sh     # or: just workbench run <this file>
set -euo pipefail
root="$(cd "$(dirname "$0")/../../.." && pwd)"
if [[ -z "${CHELABASE_COMPOSE_PROJECT:-}" ]]; then
  stack="$(cat "$root/.workbench-stack")"
  main="$(git -C "$root" worktree list --porcelain | awk 'NR == 1 { print $2 }')"
  set -a
  # shellcheck disable=SC1090
  . "$main/.chelabase/stacks/$stack/env/stack.env"
  set +a
fi
project="$CHELABASE_COMPOSE_PROJECT"
env_file="$CHELABASE_STACK_DIR/env/stack.env"
fail=0
err() { echo "hardening: FAIL: $*" >&2; fail=1; }
ok() { echo "hardening: ok   $*"; }

docker compose -p "$project" --project-directory "$root/infra/compose" \
  -f "$root/infra/compose/docker-compose.yml" -f "$root/infra/compose/docker-compose.workbench.yml" \
  --env-file "$env_file" --profile functions up -d --no-deps --wait --wait-timeout 60 functions-runtime

c="$(docker ps -q --filter "label=com.docker.compose.project=$project" \
  --filter "label=com.docker.compose.service=functions-runtime" | head -n1)"
[[ -n "$c" ]] || { echo "hardening: FAIL: functions-runtime isn't running" >&2; exit 1; }
j="$(docker inspect "$c")"
q() { jq -r ".[0] | $1" <<<"$j"; }

user="$(q .Config.User)"
[[ -n "$user" && "${user%%:*}" != 0 && "${user%%:*}" != root ]] && ok "runs as $user" || err "runs as '$user'"
[[ "$(docker exec "$c" /usr/local/bin/busybox id -u)" != 0 ]] && ok "uid inside is not 0" || err "uid inside is 0"
[[ "$(q .HostConfig.ReadonlyRootfs)" == true ]] && ok "read-only root" || err "root is writable"
[[ "$(q '.HostConfig.CapDrop | join(",")')" =~ ^(ALL|all)$ ]] && ok "cap_drop ALL" || err "cap_drop is $(q .HostConfig.CapDrop)"
[[ "$(q '.HostConfig.CapAdd // [] | length')" == 0 ]] && ok "no cap_add" || err "cap_add is $(q .HostConfig.CapAdd)"
q '.HostConfig.SecurityOpt // [] | .[]' | grep -q '^no-new-privileges' && ok "no-new-privileges" || err "no no-new-privileges"
[[ "$(q .HostConfig.Privileged)" == false ]] && ok "not privileged" || err "privileged"
[[ "$(q .HostConfig.Memory)" -gt 0 ]] && ok "memory limit $(q .HostConfig.Memory)" || err "no memory limit"
[[ "$(q '.HostConfig.PidsLimit // 0')" -gt 0 ]] && ok "pids limit $(q .HostConfig.PidsLimit)" || err "no pids limit"
[[ "$(q .HostConfig.RestartPolicy.Name)" == always ]] && ok "restart always" || err "restart is $(q .HostConfig.RestartPolicy.Name)"
[[ "$(q '[.HostConfig.PortBindings // {} | .[] | .[]?] | length')" == 0 && "$(q '[.NetworkSettings.Ports // {} | .[] | select(. != null) | .[]] | length')" == 0 ]] &&
  ok "no published port" || err "published ports: $(q .NetworkSettings.Ports)"
[[ "$(q .HostConfig.NetworkMode)" != host ]] && ok "not on the host network" || err "host network"
[[ "$(q '[.Mounts[] | select(.Type != "tmpfs")] | length')" == 0 ]] && ok "no volume or bind mount (no secrets)" ||
  err "mounts: $(q '[.Mounts[] | "\(.Type):\(.Name // .Source)->\(.Destination)"] | join(" ")')"
nets="$(q '.NetworkSettings.Networks | keys | sort | join(" ")')"
[[ "$nets" == "${project}_functions ${project}_functions-egress" ]] && ok "networks: $nets" || err "networks: $nets"
[[ "$(docker network inspect -f '{{.Internal}}' "${project}_functions")" == true ]] &&
  ok "${project}_functions is internal" || err "${project}_functions is not internal"
# Only functions-runtime (and later the functions service) is on these two networks.
for n in functions functions-egress; do
  members="$(docker network inspect -f '{{range .Containers}}{{.Name}} {{end}}' "${project}_$n")"
  for m in $members; do
    [[ "$m" == *functions-runtime* || "$m" == *-functions-[0-9]* ]] || err "$m is on ${project}_$n"
  done
done
[[ "$(docker inspect -f '{{.State.Health.Status}}' "$c")" == healthy ]] && ok "healthy (probe bundle answers)" || err "not healthy"

[[ $fail -eq 0 ]] && echo "hardening: ok"
exit $fail
