#!/usr/bin/env bash
# Offline check that the workbench stack doesn't change (epic selfhost-install, m7 and m8): the
# base file plus docker-compose.workbench.yml renders the same stack as the compose files at REV.
# m7 split the one file in two (REV was 2a4d327e, the last single-file commit); m8 made the base
# file the self-hosted install, and the overlay sets back every key it changed, so REV is now the
# commit m8 started from (ca3b59c4). auth-replacement m5 (#118) changed the workbench stack on
# purpose (the identity service, its secrets and its callers' settings), so REV is now its
# last compose commit (c90f396b, the bootstrap's own identity token). At a REV with only docker-compose.yml, that file alone is the reference.
#
#   scripts/tests/compose-overlay-equiv.sh [--env-file <stack.env>] [--rev <git rev>]
#
# --env-file defaults to the fixture stack (feat-x); pass a live stack's env/stack.env to compare
# with its real values. Both renders use every profile (sites, runner) and the same environment,
# and the check fails on any difference in:
#   - `docker compose config --format json --no-env-resolution`, keys sorted, with the top-level
#     `x-*` extension fields dropped. Those hold only YAML anchors, which can't cross files, so
#     where they live changes; Compose never applies them to a container. `--no-env-resolution`
#     keeps the env_file lists as lists (compared exactly) instead of inlining the files' values,
#     so a live stack's secrets never reach the output.
#   - `docker compose config --hash '*'`: each service's config hash, the value `up` compares to
#     decide whether a running container must be recreated. Equal hashes mean an existing stack's
#     containers stay as they are.
# The reference takes today's Neon image pins (the `${NEON_IMAGE:-…}` and
# `${COMPUTE_NODE_IMAGE:-…}` defaults) from the base file, so a `NEON_REF` bump isn't a diff.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
env_file="$root/scripts/tests/fixtures/workbench-stack.env"
# The last intended change of the workbench shape: auth-replacement m5 (the identity service).
rev=7b9a251f
while (($#)); do
  case "$1" in
    --env-file) env_file="$2"; shift 2 ;;
    --rev) rev="$2"; shift 2 ;;
    *) echo "usage: $0 [--env-file <stack.env>] [--rev <git rev>]" >&2; exit 2 ;;
  esac
done
fail() {
  echo "compose-overlay-equiv: $*" >&2
  exit 1
}
dir="$root/infra/compose"
base="$dir/docker-compose.yml"
overlay="$dir/docker-compose.workbench.yml"
[[ -f "$overlay" ]] || fail "$overlay is missing"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
git -C "$root" cat-file -e "$rev:infra/compose/docker-compose.yml" 2>/dev/null ||
  fail "git has no infra/compose/docker-compose.yml at $rev (a shallow clone? fetch the history)"
git -C "$root" show "$rev:infra/compose/docker-compose.yml" >"$tmp/single.yml"
ref=(-f "$tmp/single.yml")
if git -C "$root" cat-file -e "$rev:infra/compose/docker-compose.workbench.yml" 2>/dev/null; then
  git -C "$root" show "$rev:infra/compose/docker-compose.workbench.yml" >"$tmp/overlay.yml"
  ref+=(-f "$tmp/overlay.yml")
fi
for var in NEON_IMAGE COMPUTE_NODE_IMAGE; do
  pin="$(sed -nE "s/.*\\\$\\{$var:-([^}]+)\\}.*/\\1/p" "$base" | head -n1)"
  [[ -n "$pin" ]] || fail "no \${$var:-…} default in $base"
  for f in "$tmp"/*.yml; do
    sed -i -E "s#(\\\$\\{$var:-)[^}]+\\}#\\1$pin}#g" "$f"
  done
done

project="$(sed -n 's/^CHELABASE_COMPOSE_PROJECT=//p' "$env_file")"
project="${project:-chelabase-feat-x}"
# The project directory is pinned, so the reference file's temp location changes nothing.
compose() {
  env -u NEON_IMAGE -u COMPUTE_NODE_IMAGE docker compose -p "$project" --project-directory "$dir" "$@"
}
render() { # render <-f args…>
  compose "$@" --env-file "$env_file" --profile sites --profile runner config --format json \
    --no-env-resolution | jq -S 'with_entries(select(.key | startswith("x-") | not))'
}
hashes() { # hashes <-f args…>
  compose "$@" --env-file "$env_file" --profile sites --profile runner config --hash '*' | sort
}

render "${ref[@]}" >"$tmp/single.json" || fail "rendering the compose files at $rev failed"
render -f "$base" -f "$overlay" >"$tmp/split.json" || fail "rendering base + overlay failed"
diff -u --label "at $rev" --label "base + overlay" "$tmp/single.json" "$tmp/split.json" >&2 ||
  fail "base + overlay renders a different config than the compose files at $rev"

hashes "${ref[@]}" >"$tmp/single.hash" || fail "hashing the compose files at $rev failed"
hashes -f "$base" -f "$overlay" >"$tmp/split.hash" || fail "hashing base + overlay failed"
[[ "$(wc -l <"$tmp/split.hash")" -gt 20 ]] || fail "expected a hash per service, got: $(cat "$tmp/split.hash")"
diff -u --label "at $rev" --label "base + overlay" "$tmp/single.hash" "$tmp/split.hash" >&2 ||
  fail "a service's config hash differs: up would recreate its container"

echo "compose-overlay-equiv: ok ($(jq '.services | length' "$tmp/split.json") services, $(basename "$env_file"))"
