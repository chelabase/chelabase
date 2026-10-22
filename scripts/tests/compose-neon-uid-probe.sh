#!/usr/bin/env bash
# Docker probe (RT1 T1 audit): every Neon service that reads the stack's 0600 secrets reads them
# for any host uid, not only 1000 (the Neon image's own user). The key files are owned by a uid
# other than 1000 here (4321:4322, chowned by a root helper container); each service's user,
# entrypoint and secret mounts come from the real compose file rendered for that uid, with a
# probe in place of its command: it must read every mount, run as the stack's uid, and (with a
# data volume the image created as 1000:1000) write to /data. No stack is started.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
fixture="$root/scripts/tests/fixtures/workbench-stack.env"
fail() {
  echo "compose-neon-uid-probe: $*" >&2
  exit 1
}
if ! command -v docker >/dev/null 2>&1 || ! docker info >/dev/null 2>&1; then
  echo "compose-neon-uid-probe: SKIPPED: docker is not available" >&2
  exit 0
fi
docker image inspect alpine >/dev/null 2>&1 || docker pull -q alpine >/dev/null || fail "no alpine image"
uid=4321 gid=4322

tmp="$(mktemp -d)"
volumes=()
cleanup() {
  # The files belong to $uid now: a root container removes them.
  docker run --rm --pull never -v "$tmp:/t" alpine sh -c 'rm -rf /t/* /t/.[!.]*' >/dev/null 2>&1 || true
  rm -rf "$tmp"
  for v in "${volumes[@]}"; do docker volume rm -f "$v" >/dev/null 2>&1 || true; done
}
trap cleanup EXIT
keys="$tmp/m/.chelabase"
state="$keys/stacks/x"
mkdir -p "$keys"/{neon/storage-tokens,tokens,valkey} "$state"
for f in neon/storage-tokens/pageserver-api.jwt neon/storage-tokens/safekeeper-data.jwt \
  neon/storage-tokens/generations-api.jwt neon/storage-tokens/storcon-admin.jwt \
  tokens/storcon-hook.token tokens/proxy-api.token valkey/proxy.password; do
  echo "secret $f" >"$keys/$f"
  chmod 600 "$keys/$f"
done
echo "public key" >"$keys/neon/compute-jwt.pub.pem"
chmod 644 "$keys/neon/compute-jwt.pub.pem"
docker run --rm --pull never -v "$tmp:/t" alpine chown -R "$uid:$gid" /t/m >/dev/null ||
  fail "could not hand the key files to $uid:$gid"

config="$(env -u NEON_IMAGE -u COMPUTE_NODE_IMAGE CHELABASE_UID="$uid" CHELABASE_GID="$gid" \
  CHELABASE_KEYS_DIR="$keys" CHELABASE_STACK_DIR="$state" docker compose -p chelabase-feat-x \
  -f "${COMPOSE_FILE_UNDER_TEST:-$root/infra/compose/docker-compose.yml}" --env-file "$fixture" \
  config --no-env-resolution --format json |
  jq -f "$root/scripts/tests/fixtures/secrets-as-binds.jq")" || fail "compose config failed"
q() { jq -r "$1" <<<"$config"; }
image="$(q '.services.pageserver.image')"
docker image inspect "$image" >/dev/null 2>&1 || {
  echo "compose-neon-uid-probe: SKIPPED: the Neon image $image is not pulled" >&2
  exit 0
}

# Reads every file given, then writes /data when asked; prints the uid it ran as.
probe='set -e
want_data="$1"; shift
for f in "$@"; do cat "$f" >/dev/null 2>&1 || { echo "UNREADABLE $f"; exit 3; }; done
if [ "$want_data" = yes ]; then touch /data/.probe 2>/dev/null || { echo "UNWRITABLE /data"; exit 4; }; fi
id -u'

for s in storage-controller pageserver safekeeper1 safekeeper2 safekeeper3 register-safekeepers proxy; do
  args=(--rm --pull never --network none)
  user="$(q ".services[\"$s\"].user // empty")"
  [[ -n "$user" ]] && args+=(--user "$user")
  files=()
  while IFS=$'\t' read -r src dst; do
    args+=(--mount "type=bind,source=$src,destination=$dst,readonly")
    files+=("$dst")
  done < <(q ".services[\"$s\"].volumes[]? | select(.type == \"bind\") | \"\(.source)\t\(.target)\"")
  [[ ${#files[@]} -gt 0 ]] || fail "$s mounts no secret (the probe's list is stale)"
  want_data=no
  if [[ -n "$(q ".services[\"$s\"].volumes[]? | select(.type == \"volume\" and .target == \"/data\") | .target")" ]]; then
    v="chelabase-uid-probe-$s-$$"
    volumes+=("$v")
    args+=(-v "$v:/data")
    want_data=yes
  fi
  # A wrapper entrypoint that ends in `exec … "$@"` (the stack-uid prelude) runs in front of the
  # probe as it would in front of the command; any other entrypoint is the command's own script
  # and is replaced by the probe.
  entry="$(q ".services[\"$s\"].entrypoint // [] | if length == 4 and .[0] == \"/bin/sh\" and (.[2] | test(\"exec setpriv\")) then .[2] else empty end" | sed 's/\$\$/$/g')"
  if [[ -n "$entry" ]]; then
    out="$(docker run "${args[@]}" --entrypoint /bin/sh "$image" -c "$entry" neon /bin/sh -c "$probe" sh "$want_data" "${files[@]}" 2>&1)" ||
      fail "$s can't read its secrets or write its data as the stack's uid:
$out"
  else
    out="$(docker run "${args[@]}" --entrypoint /bin/sh "$image" -c "$probe" sh "$want_data" "${files[@]}" 2>&1)" ||
      fail "$s can't read its secrets as the stack's uid:
$out"
  fi
  [[ "$out" == "$uid" ]] || fail "$s runs as uid $out, not the stack's $uid"
done
echo "compose-neon-uid-probe: ok"
