#!/usr/bin/env bash
# Docker probe of the key mounts (U10 audit): in a real worktree layout (<main>/.worktrees/x with
# .chelabase an absolute symlink to <main>/.chelabase) Docker follows the symlink and orders mounts
# by the unresolved target depth, so a badly placed mask hides the key mounts. This renders every
# checkout-mounting service's mounts from the real compose file, starts a plain alpine container
# with exactly those mounts, and asserts that no other stack's state dir (`stacks/other`) is
# visible and that this stack's own is visible only as far as the service reads it (#110). For the
# orchestrator it also asserts every key file it reads is readable and that nothing else under the
# keys dir is visible. Two layouts: a worktree (checkout = <main>/.worktrees/x, `.chelabase` a
# symlink) and the main stack (checkout = <main>, the keys dir is `<main>/.chelabase` itself, no
# symlink; the layout #110 describes). The Docker socket is left out of every run: it bypasses
# mount isolation by design. No stack is started.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
fixture="$root/scripts/tests/fixtures/workbench-stack.env"
fail() {
  echo "compose-mask-probe: $*" >&2
  exit 1
}
if ! command -v docker >/dev/null 2>&1 || ! docker info >/dev/null 2>&1; then
  echo "compose-mask-probe: SKIPPED: docker is not available" >&2
  exit 0
fi
docker image inspect alpine >/dev/null 2>&1 || docker pull -q alpine >/dev/null || fail "no alpine image"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
main="$tmp/m"
keys="$main/.chelabase"
checkout="$main/.worktrees/x"
state="$keys/stacks/x"
mkdir -p "$keys"/{secrets,gateway,auth,neon/storage-tokens,compute-ca,orchestrator-tls/v1,realtime-tls,nats,valkey,realtime,tokens,identity} \
  "$state"/{env,nats,valkey,bootstrap,neon,logs,test,runner,setup} "$keys/stacks/other" "$checkout"
# RT1 T1: the service secrets (S1, S27) and the storage auth files exist too; the orchestrator
# reads its own NATS and Valkey passwords, realtime's internal token and the internal bearers.
for f in secrets/secrets.key gateway/internal-signing.pem gateway/internal-verify.pem auth/cookie.key \
  auth/app-jwt.key auth/app-jwt.pub neon/compute-jwt.pem neon/compute-jwt.pub compute-ca/ca.crt \
  compute-ca/ca.key orchestrator-tls/v1/client.crt orchestrator-tls/v1/client.key orchestrator-tls/v1/ca.crt \
  nats/orchestrator.password nats/worker.password nats/realtime.password nats/runner.password nats/storage.password nats/functions.password \
  valkey/proxy.password valkey/control.password valkey/realtime.password valkey/runner.password valkey/health.password \
  realtime/pg.password realtime/ticket.key realtime/internal.token \
  tokens/rest-internal.token tokens/data-keys.token tokens/app-auth.token tokens/app-sessions.token \
  tokens/storage-internal.token tokens/proxy-api.token tokens/storcon-hook.token \
  tokens/orchestrator-functions.token tokens/worker-functions.token \
  auth/member-jwt.key auth/member-jwt.pub identity/secret identity/db.password \
  tokens/auth-identity.token tokens/orchestrator-identity.token tokens/worker-identity.token \
  tokens/bootstrap-identity.token tokens/functions-service-runner.token tokens/worker-service-runner.token \
  neon/storage-tokens/metrics.token realtime-tls/client.crt realtime-tls/client.key realtime-tls/ca.crt \
  neon/compute-jwt.pub.pem neon/storage-tokens/pageserver-api.jwt neon/storage-tokens/safekeeper-data.jwt \
  neon/storage-tokens/generations-api.jwt neon/storage-tokens/storcon-admin.jwt \
  stacks/other/secret; do
  echo "content of $f" >"$keys/$f"
done
ln -s v1 "$keys/orchestrator-tls/current"
# The stack's state dir lies under the masked keys dir too; the orchestrator sees only its setup
# code (#126: the dev member's claim needs it).
for f in setup/setup.code env/stack.env bootstrap/dev-member.json bootstrap/dev-token.json studio.env nats/auth.conf valkey/users.acl binaries.json neon/proxy-ca.crt \
  runner/.curlrc logs/x test/archive.tar.zst storage-s3/access-key storage-s3/secret-key \
  functions-s3/access-key functions-s3/secret-key; do
  mkdir -p "$(dirname "$state/$f")"
  echo "content of $f" >"$state/$f"
done
ln -s "$keys" "$checkout/.chelabase"
echo script >"$checkout/script.sh"


# Renders the overlay for one checkout path and sets $config.
render() {
  config="$(env -u NEON_IMAGE -u COMPUTE_NODE_IMAGE CHELABASE_CHECKOUT="$1" CHELABASE_KEYS_DIR="$keys" \
    CHELABASE_STACK_DIR="$state" docker compose -p chelabase-feat-x -f "${COMPOSE_FILE_UNDER_TEST:-$root/infra/compose/docker-compose.yml}" \
    -f "${COMPOSE_OVERLAY_UNDER_TEST:-$root/infra/compose/docker-compose.workbench.yml}" \
    --env-file "$fixture" --profile sites --profile runner --profile services config --no-env-resolution --format json |
    jq -f "$root/scripts/tests/fixtures/secrets-as-binds.jq")" ||
    fail "compose config failed"
}

# A service's volumes as `docker run --mount` arguments (the Docker socket stays out); sets
# `args`. Fails when the service is not in the rendered config.
mount_args() {
  local m src
  [[ "$(jq -r --arg s "$1" '.services[$s] | type' <<<"$config")" == object ]] || fail "service $1 is not rendered"
  mapfile -t mounts < <(jq -r --arg s "$1" '.services[$s].volumes[] | select(.target != "/var/run/docker.sock")
    | if .type == "tmpfs" then "type=tmpfs,destination=\(.target)" + (if .read_only // false then ",readonly" else "" end)
      else "type=bind,source=\(.source),destination=\(.target)" + (if .read_only // false then ",readonly" else "" end) end' <<<"$config")
  args=()
  for m in "${mounts[@]}"; do
    # A host tool the runner mounts (cargo-nextest) may not exist here; it is outside the fixture.
    if [[ "$m" == type=bind,source=* ]]; then
      src="${m#type=bind,source=}"
      src="${src%%,destination=*}"
      [[ "$src" == "$tmp"/* || -e "$src" ]] || continue
    fi
    args+=(--mount "$m")
  done
}

# Every checkout-mounting service (docs and landing use the same anchors), and the files under
# the stack's own state dir each is expected to see (relative to $state): the runner mounts the
# whole dir, the orchestrator reads the setup code (the worker's S3 key files land at other targets).
services=(auth functions gateway identity orchestrator realtime rest runner service-runner storage studio worker docs landing)
state_expected() {
  case "$1" in
  runner) (cd "$state" && find . \( -type f -o -type l \) | sed 's|^\./||' | sort) ;;
  orchestrator) echo setup/setup.code ;;
  esac
}

# Runs each service's mounts: no stacks/other file visible, and under stacks/x only what it reads.
probe_services() { # $1 = layout name, the rest = services
  local layout="$1" s out want
  shift
  for s in "$@"; do
    mount_args "$s"
    out="$(docker run --rm --pull never "${args[@]}" alpine sh -c "[ -d '$keys/stacks' ] && find '$keys/stacks' \( -type f -o -type l \) | sort || true" 2>&1)" ||
      fail "$layout: $s: the container failed:
$out"
    ! grep -q "$keys/stacks/other" <<<"$out" || fail "$layout: $s sees another stack's state dir:
$out"
    want="$(state_expected "$s" | sed "s|^|$state/|" | sort)"
    [[ "$out" == "$want" ]] || {
      diff <(echo "$want") <(echo "$out") >&2 || true
      fail "$layout: $s sees a different part of its own state dir than it reads (expected < > got)"
    }
  done
}

# The orchestrator's exact key list, and the checkout and mask checks, for one layout.
probe_orchestrator() { # $1 = layout name, $2 = checkout
  mount_args orchestrator
  # What the orchestrator reads (U10 table), as paths under the keys dir.
  readable=(secrets/secrets.key gateway/internal-verify.pem neon/compute-jwt.pem neon/compute-jwt.pub
    orchestrator-tls/current/client.crt orchestrator-tls/current/client.key orchestrator-tls/current/ca.crt
    nats/orchestrator.password valkey/control.password realtime/internal.token
    tokens/rest-internal.token tokens/data-keys.token tokens/app-auth.token tokens/app-sessions.token
    tokens/storage-internal.token tokens/proxy-api.token tokens/storcon-hook.token tokens/orchestrator-identity.token auth/member-jwt.pub
    tokens/orchestrator-functions.token stacks/x/setup/setup.code)
  expected="$(printf '%s\n' "$keys/secrets/secrets.key" "$keys/gateway/internal-verify.pem" \
    "$keys/neon/compute-jwt.pem" "$keys/neon/compute-jwt.pub" "$keys/orchestrator-tls/current" \
    "$keys/orchestrator-tls/v1/client.crt" "$keys/orchestrator-tls/v1/client.key" "$keys/orchestrator-tls/v1/ca.crt" \
    "$keys/nats/orchestrator.password" "$keys/valkey/control.password" "$keys/realtime/internal.token" \
    "$keys/tokens/rest-internal.token" "$keys/tokens/data-keys.token" "$keys/tokens/app-auth.token" \
    "$keys/tokens/app-sessions.token" "$keys/tokens/storage-internal.token" "$keys/tokens/proxy-api.token" "$keys/tokens/storcon-hook.token" \
    "$keys/tokens/orchestrator-identity.token" "$keys/auth/member-jwt.pub" "$keys/tokens/orchestrator-functions.token" \
    "$state/setup/setup.code" | sort)"

  script='set -e
  for f in "$@"; do cat "$f" >/dev/null || { echo "UNREADABLE $f"; exit 3; }; done
  find "$K" \( -type f -o -type l \) | sort'
  out="$(docker run --rm --pull never "${args[@]}" -e "K=$keys" alpine sh -c "$script" sh "${readable[@]/#/$keys/}" 2>&1)" ||
    fail "$1: the orchestrator's mounts hide a key it reads, or the container failed:
  $out"
  [[ "$out" == "$expected" ]] || {
    diff <(echo "$expected") <(echo "$out") >&2 || true
    fail "$1: files visible under the keys dir differ from what the orchestrator reads (expected < > got)"
  }
  # The checkout's own files stay visible, and a write into the mask fails instead of landing in memory.
  docker run --rm --pull never "${args[@]}" alpine sh -c "test -f '$2/script.sh'" ||
    fail "$1: the checkout's files must stay visible"
  docker run --rm --pull never "${args[@]}" alpine sh -c "! touch '$keys/new-file' 2>/dev/null && ! touch '$2/.chelabase/new-file' 2>/dev/null" ||
    fail "$1: a write into the masked keys dir must fail"
}

render "$checkout"
probe_services worktree "${services[@]}"
probe_orchestrator worktree "$checkout"
# The main stack: the checkout is the main checkout itself and the keys dir lies inside it.
rm "$checkout/.chelabase"
echo script >"$main/script.sh"
render "$main"
probe_services main "${services[@]}"
probe_orchestrator main "$main"
echo "compose-mask-probe: ok"
