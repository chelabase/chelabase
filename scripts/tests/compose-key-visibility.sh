#!/usr/bin/env bash
# Offline check of the key-visibility invariant (U10, R-T9a): a service sees exactly the machine
# key files it reads, nothing else under the keys dir, and `.chelabase` inside a checkout mount is
# masked. Renders the one compose file (there is no other file or overlay) for three stacks and
# compares, per service, the visibility of every key path with a table.
#
# Visibility of a path for a service: the mount with the longest target that is the path or a
# parent of it decides. A tmpfs there, an empty read-only bind masking `<checkout>/.chelabase`, or
# no mount at all means "none"; otherwise the mount's read_only flag gives ro or rw.
#
# Stacks: feat-x (a worktree: checkout outside the keys dir), main (checkout /m, keys dir
# /m/.chelabase inside it), and a keys dir overridden to /alt/keys.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
fixture="$root/scripts/tests/fixtures/workbench-stack.env"
fail() {
  echo "compose-key-visibility: $*" >&2
  exit 1
}

# key paths, as K/<under the keys dir>, C/<under the checkout>, S/<under the stack's state dir>
universe='[
  "K/secrets/secrets.key",
  "K/gateway/internal-signing.pem", "K/gateway/internal-verify.pem",
  "K/auth/cookie.key", "K/auth/app-jwt.key", "K/auth/app-jwt.pub",
  "K/neon/compute-jwt.pem", "K/neon/compute-jwt.pub",
  "K/compute-ca/ca.crt", "K/compute-ca/ca.key",
  "K/orchestrator-tls/current/client.crt", "K/orchestrator-tls/current/client.key", "K/orchestrator-tls/current/ca.crt",
  "K/unlisted/a-future-key", "K/stacks/registry.json", "K/stacks/other/env/stack.env",
  "C/scripts/tests/workbench-compose.sh", "C/.chelabase/anything",
  "S/env/stack.env"
]'

# service -> { path: mode }; every other (service, path) must be none.
expected='{
  "orchestrator": {
    "C/scripts/tests/workbench-compose.sh": "ro", "S/env/stack.env": "rw",
    "K/secrets/secrets.key": "ro", "K/gateway/internal-verify.pem": "ro",
    "K/neon/compute-jwt.pem": "ro", "K/neon/compute-jwt.pub": "ro",
    "K/orchestrator-tls/current/client.crt": "ro", "K/orchestrator-tls/current/client.key": "ro", "K/orchestrator-tls/current/ca.crt": "ro"
  },
  "worker": {
    "C/scripts/tests/workbench-compose.sh": "ro", "S/env/stack.env": "rw",
    "K/secrets/secrets.key": "ro",
    "K/neon/compute-jwt.pem": "ro", "K/neon/compute-jwt.pub": "ro",
    "K/compute-ca/ca.crt": "rw", "K/compute-ca/ca.key": "rw",
    "K/orchestrator-tls/current/client.crt": "rw", "K/orchestrator-tls/current/client.key": "rw", "K/orchestrator-tls/current/ca.crt": "rw"
  },
  "gateway": {
    "C/scripts/tests/workbench-compose.sh": "ro", "S/env/stack.env": "rw",
    "K/gateway/internal-signing.pem": "ro", "K/auth/app-jwt.pub": "ro"
  },
  "auth": {
    "C/scripts/tests/workbench-compose.sh": "ro", "S/env/stack.env": "rw",
    "K/auth/cookie.key": "ro", "K/auth/app-jwt.key": "ro", "K/auth/app-jwt.pub": "ro",
    "K/gateway/internal-verify.pem": "ro",
    "K/compute-ca/ca.crt": "ro"
  },
  "rest": {
    "C/scripts/tests/workbench-compose.sh": "ro", "S/env/stack.env": "rw",
    "K/gateway/internal-verify.pem": "ro",
    "K/compute-ca/ca.crt": "ro"
  },
  "studio": { "C/scripts/tests/workbench-compose.sh": "rw" },
  "docs": { "C/scripts/tests/workbench-compose.sh": "rw" },
  "landing": { "C/scripts/tests/workbench-compose.sh": "rw" },
  "runner": {
    "C/scripts/tests/workbench-compose.sh": "rw", "S/env/stack.env": "rw",
    "K/secrets/secrets.key": "ro",
    "K/neon/compute-jwt.pem": "ro", "K/neon/compute-jwt.pub": "ro",
    "K/auth/app-jwt.key": "ro", "K/auth/app-jwt.pub": "ro",
    "K/compute-ca/ca.crt": "ro", "K/compute-ca/ca.key": "ro",
    "K/orchestrator-tls/current/client.crt": "ro", "K/orchestrator-tls/current/client.key": "ro", "K/orchestrator-tls/current/ca.crt": "ro"
  }
}'

# $1 label, $2 checkout, $3 keys dir, $4 state dir, $5 link (1: <checkout>/.chelabase is a symlink
# to the keys dir, as in a real worktree; Docker follows it, so a mount at the link is a mount at
# the keys dir, but it is ordered by the unresolved path); the rest are env overrides for the render.
check() {
  local label="$1" checkout="$2" keys="$3" state="$4" link="$5"
  shift 5
  local config
  config="$(env -u NEON_IMAGE -u COMPUTE_NODE_IMAGE "$@" docker compose -p chelabase-feat-x \
    -f "${COMPOSE_FILE_UNDER_TEST:-$root/infra/compose/docker-compose.yml}" --env-file "$fixture" \
    --profile sites --profile runner config --no-env-resolution --format json)" ||
    fail "$label: compose config failed"
  local got want
  got="$(jq -S --arg k "$keys" --arg c "$checkout" --arg s "$state" --argjson link "$link" --argjson universe "$universe" '
    def depth: split("/") | map(select(. != "")) | length;
    def resolve: if $link == 1 and . == ($c + "/.chelabase") then $k else . end;
    def expand: if startswith("K/") then $k + "/" + .[2:]
      elif startswith("C/.chelabase/") and $link == 1 then $k + "/" + .[13:]
      elif startswith("C/") then $c + "/" + .[2:]
      else $s + "/" + .[2:] end;
    def masked($m): $m.type == "tmpfs"
      or ($m.target == ($c + "/.chelabase") and ($m.read_only // false) and ($m.source // "") != $k);
    # Docker orders mounts by the depth of the unresolved target; at equal depth the tmpfs
    # (listed later in the engine order) wins, so a tie hides the key mounts.
    def visible($vols; $p):
      [$vols[] | select((.target | resolve) as $t | $p == $t or ($p | startswith($t + "/")))]
      | if length == 0 then "none"
        else max_by([(.target | depth), (if .type == "tmpfs" then 1 else 0 end)]) as $m
          | if masked($m) then "none" elif ($m.read_only // false) then "ro" else "rw" end
        end;
    [.services | to_entries[] | .key as $svc | (.value.volumes // []) as $vols
      | {key: $svc, value: ([$universe[] | {key: ., value: visible($vols; expand)}]
        | map(select(.value != "none")) | from_entries)}]
    | from_entries' <<<"$config")"
  want="$(jq -S --argjson services "$(jq -c '.services | keys' <<<"$config")" \
    '. as $e | [$services[] | {key: ., value: ($e[.] // {})}] | from_entries' <<<"$expected")"
  [[ "$got" == "$want" ]] || {
    diff <(echo "$want") <(echo "$got") >&2 || true
    fail "$label: per-service key visibility differs from the table (expected < > got)"
  }

  # Every mount under the keys dir (outside stacks/) is the real path at its own path, a key
  # file or the CA / client-pair directory; none is the keys dir, a parent of it or a whole
  # key directory such as secrets/ or auth/.
  local bad
  bad="$(jq -r --arg k "$keys" --arg c "$checkout" '
    .services | to_entries[] | .key as $svc | (.value.volumes // [])[]
    | select(.target as $t | $t == $k or ($k | startswith($t + "/")) or ($t | startswith($k + "/")))
    | select((.target | startswith($k + "/stacks/")) | not)
    | select(.type == "bind")
    | select(.target != ($c + "/.chelabase"))
    | select(.source != .target)
    | "\($svc): \(.source) -> \(.target)"' <<<"$config")"
  [[ -z "$bad" ]] || fail "$label: key mounts must be narrow and at their own path; got:
$bad"
  # No whole-directory mount of a key directory (secrets, gateway, auth, neon): files only.
  bad="$(jq -r --arg k "$keys" '
    .services | to_entries[] | .key as $svc | (.value.volumes // [])[]
    | select(.target | IN($k + "/secrets", $k + "/gateway", $k + "/auth", $k + "/neon"))
    | "\($svc): \(.target)"' <<<"$config")"
  [[ -z "$bad" ]] || fail "$label: a whole key directory is mounted; got:
$bad"
  # Every checkout-mounting service masks the keys dir itself with a read-only tmpfs (the real
  # directory, which a symlinked <checkout>/.chelabase leads to), and also <checkout>/.chelabase
  # when that is a different directory (no symlink to the keys dir).
  local unmasked
  unmasked="$(jq -r --arg c "$checkout" --arg k "$keys" --argjson link "$link" '
    def ro_tmpfs($t): any(.value.volumes[]?; .target == $t and .type == "tmpfs" and (.read_only // false));
    .services | to_entries[] | select(any(.value.volumes[]?; .target == $c)) | .key as $svc
    | select((ro_tmpfs($k)
        and (($link == 1 or $k == ($c + "/.chelabase")) or ro_tmpfs($c + "/.chelabase"))) | not) | $svc' <<<"$config")"
  [[ -z "$unmasked" ]] || fail "$label: these services lack a read-only tmpfs mask on the keys dir $keys (and on <checkout>/.chelabase when it is a different directory): $unmasked"
  # Mount order: every mask is strictly shallower (by unresolved path) than every bind under its
  # resolved target (the keys dir; a symlinked <checkout>/.chelabase resolves to it), so no tie lets the mask hide them.
  local tied
  tied="$(jq -r --arg k "$keys" --arg c "$checkout" --argjson link "$link" '
    def depth: split("/") | map(select(. != "")) | length;
    def resolve: if $link == 1 and . == ($c + "/.chelabase") then $k else . end;
    .services | to_entries[] | .key as $svc | (.value.volumes // []) as $v
    | ($v | map(select(.type == "tmpfs" and (.target | endswith("/.chelabase") or . == $k)))) as $masks
    | $masks[] | .target as $m | ($m | resolve) as $r
    | $v[] | select(.type == "bind" and (.target | startswith($r + "/")))
    | select((.target | depth) <= ($m | depth))
    | "\($svc): mask \($m) (covering \($r)) is not shallower than \(.target)"' <<<"$config")"
  [[ -z "$tied" ]] || fail "$label: a mask ties or is deeper than a mount it would hide:
$tied"
  # The set of checkout-mounting services is the one the table knows.
  local mounting
  mounting="$(jq -r --arg c "$checkout" '[.services | to_entries[] | select(any(.value.volumes[]?; .target == $c)) | .key] | sort | join(" ")' <<<"$config")"
  [[ "$mounting" == "auth docs gateway landing orchestrator rest runner studio worker" ]] ||
    fail "$label: checkout-mounting services changed (update the table); got: $mounting"
}

check "checkout apart from the keys dir" /w/feat-x /m/.chelabase /m/.chelabase/stacks/feat-x 0 \
  CHELABASE_CHECKOUT_MASK=/w/feat-x/.chelabase
check "real worktree (symlinked .chelabase)" /m/.worktrees/x /m/.chelabase /m/.chelabase/stacks/x 1 \
  CHELABASE_CHECKOUT=/m/.worktrees/x CHELABASE_STACK_DIR=/m/.chelabase/stacks/x
check "main stack (keys dir inside the checkout)" /m /m/.chelabase /m/.chelabase/stacks/main 0 \
  CHELABASE_CHECKOUT=/m CHELABASE_STACK_DIR=/m/.chelabase/stacks/main \
  CHELABASE_COMPUTE_CA_DIR=/m/.chelabase/compute-ca
check "overridden keys dir" /w/feat-x /alt/keys /alt/keys/stacks/feat-x 0 \
  CHELABASE_CHECKOUT_MASK=/w/feat-x/.chelabase CHELABASE_KEYS_DIR=/alt/keys CHELABASE_STACK_DIR=/alt/keys/stacks/feat-x
echo "compose-key-visibility: ok"
