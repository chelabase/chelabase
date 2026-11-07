#!/usr/bin/env bash
# Offline check of the key-visibility invariant (U10, R-T9a): a service sees exactly the machine
# key files it reads, nothing else under the keys dir, and `.chelabase` inside a checkout mount is
# masked. Renders the base compose file plus the workbench overlay for three stacks and
# compares, per service, the visibility of every key path with a table.
#
# Visibility of a path for a service: the mount with the longest target that is the path or a
# parent of it decides. A tmpfs there, an empty read-only bind masking `<checkout>/.chelabase`, or
# no mount at all means "none"; otherwise the mount's read_only flag gives ro or rw.
#
# The per-service key files are Compose secrets; a service's file secrets are Docker bind mounts
# (read-only, at the secret's target), so the rendered config is first rewritten with
# fixtures/secrets-as-binds.jq to list them among the service's volumes, and every check below
# applies to them unchanged.
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
  "K/realtime-tls/current/client.crt", "K/realtime-tls/current/client.key", "K/realtime-tls/current/ca.crt",
  "K/nats/orchestrator.password", "K/nats/worker.password", "K/nats/realtime.password", "K/nats/runner.password",
  "K/nats/storage.password", "K/nats/functions.password",
  "K/valkey/proxy.password", "K/valkey/control.password", "K/valkey/realtime.password", "K/valkey/runner.password",
  "K/valkey/health.password",
  "K/neon/compute-jwt.pub.pem", "K/neon/storage-tokens/pageserver-api.jwt", "K/neon/storage-tokens/safekeeper-data.jwt",
  "K/neon/storage-tokens/generations-api.jwt", "K/neon/storage-tokens/storcon-admin.jwt",
  "K/neon/storage-tokens/metrics.token",
  "K/realtime/pg.password", "K/realtime/ticket.key", "K/realtime/internal.token",
  "K/tokens/rest-internal.token", "K/tokens/data-keys.token", "K/tokens/app-auth.token",
  "K/tokens/app-sessions.token", "K/tokens/storage-internal.token", "K/tokens/proxy-api.token", "K/tokens/storcon-hook.token",
  "K/tokens/orchestrator-functions.token", "K/tokens/worker-functions.token",
  "K/auth/member-jwt.key", "K/auth/member-jwt.pub", "K/identity/secret", "K/identity/db.password",
  "K/tokens/auth-identity.token", "K/tokens/orchestrator-identity.token", "K/tokens/worker-identity.token",
  "K/tokens/bootstrap-identity.token",
  "K/unlisted/a-future-key", "K/stacks/registry.json", "K/stacks/other/env/stack.env",
  "C/scripts/tests/workbench-compose.sh", "C/.chelabase/anything",
  "S/env/stack.env",
  "S/bootstrap/dev-member.json", "S/bootstrap/dev-token.json", "S/studio.env",
  "S/nats/auth.conf", "S/valkey/users.acl", "S/binaries.json", "S/runner/.curlrc",
  "S/neon/proxy-ca.crt", "S/logs/x", "S/test/x", "S/test/archive.tar.zst", "S/upgrade-e2e.env",
  "S/unlisted/x", "S/setup/setup.code"
]'

# service -> { path: mode }; every other (service, path) must be none.
# RT1 T1 (S1, S27): each service sees its own NATS/Valkey password and the bearers it sends or
# checks; the runner (the test harness, which also has the Docker socket) sees all of them.
# Fork round 4 (C5): realtime sees the realtime client pair (realtime-tls, ro: it logs in to computes
# with it), the worker renews it (rw), the runner reads it and the metrics token (ro). Nobody else
# sees either.
# RT1 T7 (S1): realtime sees the gateway's verify key (ticket requests), the platform CA's
# certificate (T8's compute TLS), its NATS and Valkey passwords and its three realtime secrets.
# The stack's state dir (least privilege): an app sees only the state files it reads itself, ro.
# Its env files (stack.env, studio.env) are read by Compose on the host, at create time, so they
# need no mount; nothing in an app writes under the state dir (logs, test archives, the proxy CA
# copy, binaries.json and the runner's .curlrc are the host CLI's), and no app opens a state file but
# one: the orchestrator reads the stack's setup code (setup/setup.code, ro; #126).
# CHELABASE_PROXY_CA_FILE is a path the orchestrator hands to clients, never opens. The runner
# (the test harness, which has the Docker socket anyway) keeps the whole dir, rw: it runs nextest
# archives from test/, curl with CURL_HOME=runner/, `docker compose` with stack.env and the env
# files to recreate apps (roles_live), `dev token` (bootstrap/dev-member.json, and the session it
# keeps in bootstrap/dev-token.json), reads proxy-ca.crt and studio.env, and writes
# upgrade-e2e.env at the dir's root.
expected='{
  "orchestrator": {
    "C/scripts/tests/workbench-compose.sh": "ro",
    "K/secrets/secrets.key": "ro", "K/gateway/internal-verify.pem": "ro",
    "K/neon/compute-jwt.pem": "ro", "K/neon/compute-jwt.pub": "ro",
    "K/orchestrator-tls/current/client.crt": "ro", "K/orchestrator-tls/current/client.key": "ro", "K/orchestrator-tls/current/ca.crt": "ro",
    "K/nats/orchestrator.password": "ro", "K/valkey/control.password": "ro", "K/realtime/internal.token": "ro",
    "K/tokens/rest-internal.token": "ro", "K/tokens/data-keys.token": "ro", "K/tokens/app-auth.token": "ro",
    "K/tokens/app-sessions.token": "ro", "K/tokens/storage-internal.token": "ro", "K/tokens/proxy-api.token": "ro", "K/tokens/storcon-hook.token": "ro",
    "K/tokens/orchestrator-identity.token": "ro", "K/auth/member-jwt.pub": "ro",
    "K/tokens/orchestrator-functions.token": "ro",
    "S/setup/setup.code": "ro"
  },
  "worker": {
    "C/scripts/tests/workbench-compose.sh": "ro",
    "K/secrets/secrets.key": "ro",
    "K/neon/compute-jwt.pem": "ro", "K/neon/compute-jwt.pub": "ro",
    "K/compute-ca/ca.crt": "rw", "K/compute-ca/ca.key": "rw",
    "K/orchestrator-tls/current/client.crt": "rw", "K/orchestrator-tls/current/client.key": "rw", "K/orchestrator-tls/current/ca.crt": "rw",
    "K/realtime-tls/current/client.crt": "rw", "K/realtime-tls/current/client.key": "rw", "K/realtime-tls/current/ca.crt": "rw",
    "K/nats/worker.password": "ro", "K/valkey/control.password": "ro", "K/realtime/pg.password": "ro",
    "K/tokens/worker-identity.token": "ro", "K/tokens/worker-functions.token": "ro"
  },
  "gateway": {
    "C/scripts/tests/workbench-compose.sh": "ro",
    "K/gateway/internal-signing.pem": "ro", "K/auth/app-jwt.pub": "ro", "K/auth/member-jwt.pub": "ro",
    "K/tokens/data-keys.token": "ro", "K/tokens/app-sessions.token": "ro"
  },
  "auth": {
    "C/scripts/tests/workbench-compose.sh": "ro",
    "K/auth/cookie.key": "ro", "K/auth/app-jwt.key": "ro", "K/auth/app-jwt.pub": "ro",
    "K/auth/member-jwt.key": "ro", "K/auth/member-jwt.pub": "ro",
    "K/gateway/internal-verify.pem": "ro",
    "K/compute-ca/ca.crt": "ro",
    "K/tokens/app-auth.token": "ro", "K/tokens/auth-identity.token": "ro"
  },
  "rest": {
    "C/scripts/tests/workbench-compose.sh": "ro",
    "K/gateway/internal-verify.pem": "ro",
    "K/compute-ca/ca.crt": "ro",
    "K/tokens/rest-internal.token": "ro"
  },
  "storage": {
    "C/scripts/tests/workbench-compose.sh": "ro",
    "K/gateway/internal-verify.pem": "ro",
    "K/compute-ca/ca.crt": "ro",
    "K/tokens/storage-internal.token": "ro",
    "K/nats/storage.password": "ro"
  },
  "functions": {
    "C/scripts/tests/workbench-compose.sh": "ro",
    "K/secrets/secrets.key": "ro", "K/gateway/internal-verify.pem": "ro",
    "K/nats/functions.password": "ro",
    "K/tokens/orchestrator-functions.token": "ro", "K/tokens/worker-functions.token": "ro"
  },
  "realtime": {
    "C/scripts/tests/workbench-compose.sh": "ro",
    "K/gateway/internal-verify.pem": "ro",
    "K/compute-ca/ca.crt": "ro",
    "K/realtime-tls/current/client.crt": "ro", "K/realtime-tls/current/client.key": "ro", "K/realtime-tls/current/ca.crt": "ro",
    "K/nats/realtime.password": "ro", "K/valkey/realtime.password": "ro",
    "K/realtime/pg.password": "ro", "K/realtime/ticket.key": "ro", "K/realtime/internal.token": "ro",
    "K/auth/member-jwt.pub": "ro"
  },
  "identity": {
    "C/scripts/tests/workbench-compose.sh": "ro",
    "K/identity/secret": "ro", "K/identity/db.password": "ro",
    "K/tokens/auth-identity.token": "ro", "K/tokens/orchestrator-identity.token": "ro",
    "K/tokens/worker-identity.token": "ro", "K/tokens/bootstrap-identity.token": "ro"
  },
  "studio": { "C/scripts/tests/workbench-compose.sh": "rw" },
  "docs": { "C/scripts/tests/workbench-compose.sh": "rw" },
  "landing": { "C/scripts/tests/workbench-compose.sh": "rw" },
  "runner": {
    "C/scripts/tests/workbench-compose.sh": "rw",
    "S/env/stack.env": "rw",
    "S/bootstrap/dev-member.json": "rw", "S/bootstrap/dev-token.json": "rw", "S/studio.env": "rw",
    "S/nats/auth.conf": "rw", "S/valkey/users.acl": "rw", "S/binaries.json": "rw", "S/runner/.curlrc": "rw",
    "S/neon/proxy-ca.crt": "rw", "S/logs/x": "rw", "S/test/x": "rw", "S/test/archive.tar.zst": "rw",
    "S/upgrade-e2e.env": "rw", "S/unlisted/x": "rw", "S/setup/setup.code": "rw",
    "K/secrets/secrets.key": "ro",
    "K/neon/compute-jwt.pem": "ro", "K/neon/compute-jwt.pub": "ro",
    "K/auth/app-jwt.key": "ro", "K/auth/app-jwt.pub": "ro",
    "K/compute-ca/ca.crt": "ro", "K/compute-ca/ca.key": "ro",
    "K/orchestrator-tls/current/client.crt": "ro", "K/orchestrator-tls/current/client.key": "ro", "K/orchestrator-tls/current/ca.crt": "ro",
    "K/realtime-tls/current/client.crt": "ro", "K/realtime-tls/current/client.key": "ro", "K/realtime-tls/current/ca.crt": "ro",
    "K/nats/orchestrator.password": "ro", "K/nats/worker.password": "ro", "K/nats/realtime.password": "ro", "K/nats/runner.password": "ro",
    "K/valkey/proxy.password": "ro", "K/valkey/control.password": "ro", "K/valkey/realtime.password": "ro", "K/valkey/runner.password": "ro",
    "K/valkey/health.password": "ro",
    "K/neon/compute-jwt.pub.pem": "ro", "K/neon/storage-tokens/pageserver-api.jwt": "ro", "K/neon/storage-tokens/safekeeper-data.jwt": "ro",
    "K/neon/storage-tokens/generations-api.jwt": "ro", "K/neon/storage-tokens/storcon-admin.jwt": "ro",
    "K/neon/storage-tokens/metrics.token": "ro",
    "K/realtime/pg.password": "ro", "K/realtime/ticket.key": "ro", "K/realtime/internal.token": "ro",
    "K/tokens/rest-internal.token": "ro", "K/tokens/data-keys.token": "ro", "K/tokens/app-auth.token": "ro",
    "K/tokens/app-sessions.token": "ro", "K/tokens/storage-internal.token": "ro", "K/tokens/proxy-api.token": "ro", "K/tokens/storcon-hook.token": "ro",
    "K/auth/member-jwt.key": "ro", "K/auth/member-jwt.pub": "ro", "K/identity/secret": "ro", "K/identity/db.password": "ro",
    "K/tokens/auth-identity.token": "ro", "K/tokens/orchestrator-identity.token": "ro", "K/tokens/worker-identity.token": "ro",
    "K/tokens/orchestrator-functions.token": "ro"
  }
}'

# Services that don't mount the checkout read their secret at a fixed path inside (an entrypoint
# wrapper, S6): every bind whose source is a service secret, as service:source->target, for all
# services. NATS and Valkey get none (they read hashes rendered into the stack dir).
expected_secret_binds='[
  "proxy:K/valkey/proxy.password->/run/valkey-proxy.password",
  "proxy:K/tokens/proxy-api.token->/run/proxy-api.token",
  "storage-controller:K/tokens/storcon-hook.token->/run/storcon-hook.token",
  "storage-controller:K/neon/compute-jwt.pub.pem->/run/storage-auth.pub.pem",
  "storage-controller:K/neon/storage-tokens/pageserver-api.jwt->/run/pageserver-api.jwt",
  "storage-controller:K/neon/storage-tokens/safekeeper-data.jwt->/run/safekeeper-data.jwt",
  "pageserver:K/neon/compute-jwt.pub.pem->/run/storage-auth.pub.pem",
  "pageserver:K/neon/storage-tokens/generations-api.jwt->/run/generations-api.jwt",
  "pageserver:K/neon/storage-tokens/safekeeper-data.jwt->/run/safekeeper-data.jwt",
  "safekeeper1:K/neon/compute-jwt.pub.pem->/run/storage-auth.pub.pem",
  "safekeeper1:K/neon/storage-tokens/safekeeper-data.jwt->/run/safekeeper-data.jwt",
  "safekeeper2:K/neon/compute-jwt.pub.pem->/run/storage-auth.pub.pem",
  "safekeeper2:K/neon/storage-tokens/safekeeper-data.jwt->/run/safekeeper-data.jwt",
  "safekeeper3:K/neon/compute-jwt.pub.pem->/run/storage-auth.pub.pem",
  "safekeeper3:K/neon/storage-tokens/safekeeper-data.jwt->/run/safekeeper-data.jwt",
  "register-safekeepers:K/neon/storage-tokens/storcon-admin.jwt->/run/storcon-admin.jwt",
  "valkey:K/valkey/health.password->/run/valkey-health.password"
]'

# Every bind whose source is the stack's state dir or under it, for all services, as
# service:S/<source>->target (":rw" when writable; a target in the state dir also as S/…): the
# table above goes by target, so it can't see a state file mounted at another path. NATS and
# Valkey read the auth files the workbench renders (hashes only); the runner gets the whole dir.
expected_state_binds='[
  "nats:S/nats/auth.conf->/etc/nats/auth.conf",
  "valkey:S/valkey/users.acl->/etc/valkey/users.acl",
  "orchestrator:S/setup/setup.code->S/setup/setup.code",
  "runner:S->S:rw",
  "seaweedfs:S/s3/access-key->/run/chelabase/secrets/s3/access-key",
  "seaweedfs:S/s3/secret-key->/run/chelabase/secrets/s3/secret-key",
  "seaweedfs:S/storage-s3/access-key->/run/chelabase/secrets/storage-s3/access-key",
  "seaweedfs:S/storage-s3/secret-key->/run/chelabase/secrets/storage-s3/secret-key",
  "seaweedfs:S/grpc-tls/ca.crt->/run/chelabase/secrets/grpc-tls/ca.crt",
  "seaweedfs:S/grpc-tls/server.crt->/run/chelabase/secrets/grpc-tls/server.crt",
  "seaweedfs:S/grpc-tls/server.key->/run/chelabase/secrets/grpc-tls/server.key",
  "seaweedfs:S/grpc-tls/client.crt->/run/chelabase/secrets/grpc-tls/client.crt",
  "seaweedfs:S/grpc-tls/client.key->/run/chelabase/secrets/grpc-tls/client.key",
  "storage:S/storage-s3/access-key->/run/chelabase/secrets/storage-s3/access-key",
  "storage:S/storage-s3/secret-key->/run/chelabase/secrets/storage-s3/secret-key",
  "worker:S/storage-s3/access-key->/run/chelabase/secrets/storage-s3/access-key",
  "worker:S/storage-s3/secret-key->/run/chelabase/secrets/storage-s3/secret-key",
  "seaweedfs:S/functions-s3/access-key->/run/chelabase/secrets/functions-s3/access-key",
  "seaweedfs:S/functions-s3/secret-key->/run/chelabase/secrets/functions-s3/secret-key",
  "functions:S/functions-s3/access-key->/run/chelabase/secrets/functions-s3/access-key",
  "functions:S/functions-s3/secret-key->/run/chelabase/secrets/functions-s3/secret-key",
  "worker:S/functions-s3/access-key->/run/chelabase/secrets/functions-s3/access-key",
  "worker:S/functions-s3/secret-key->/run/chelabase/secrets/functions-s3/secret-key"
]'

# $1 label, $2 checkout, $3 keys dir, $4 state dir, $5 link (1: <checkout>/.chelabase is a symlink
# to the keys dir, as in a real worktree; Docker follows it, so a mount at the link is a mount at
# the keys dir, but it is ordered by the unresolved path); the rest are env overrides for the render.
check() {
  local label="$1" checkout="$2" keys="$3" state="$4" link="$5"
  shift 5
  local config
  config="$(env -u NEON_IMAGE -u COMPUTE_NODE_IMAGE "$@" docker compose -p chelabase-feat-x \
    -f "${COMPOSE_FILE_UNDER_TEST:-$root/infra/compose/docker-compose.yml}" \
    -f "${COMPOSE_OVERLAY_UNDER_TEST:-$root/infra/compose/docker-compose.workbench.yml}" --env-file "$fixture" \
    --profile sites --profile runner config --no-env-resolution --format json |
    jq -f "$root/scripts/tests/fixtures/secrets-as-binds.jq")" ||
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
    | select(.target | IN($k + "/secrets", $k + "/gateway", $k + "/auth", $k + "/neon",
        $k + "/nats", $k + "/valkey", $k + "/realtime", $k + "/tokens", $k + "/neon/storage-tokens"))
    | "\($svc): \(.target)"' <<<"$config")"
  [[ -z "$bad" ]] || fail "$label: a whole key directory is mounted; got:
$bad"
  # RT1 T1: secrets mounted at another path (the proxy's and the storage controller's), read-only.
  local binds want_binds
  binds="$(jq -S -c --arg k "$keys" '
    [.services | to_entries[] | .key as $svc | (.value.volumes // [])[]
      | select(.type == "bind" and .source != .target)
      | select(.source as $src | any(("/nats/", "/valkey/", "/realtime/", "/tokens/", "/neon/storage-tokens/", "/neon/compute-jwt.pub.pem") as $d | ($src | startswith($k + $d)); .))
      | "\($svc):K/\(.source | ltrimstr($k + "/"))->\(.target)\(if (.read_only // false) then "" else ":rw" end)"]
    | sort' <<<"$config")"
  want_binds="$(jq -S -c 'sort' <<<"$expected_secret_binds")"
  [[ "$binds" == "$want_binds" ]] || fail "$label: secrets mounted at another path differ; want $want_binds, got $binds"
  # Least privilege on the state dir, by source: exactly the binds listed above.
  binds="$(jq -S -c --arg s "$state" '
    def rel: if . == $s then "S" elif startswith($s + "/") then "S/" + .[($s | length) + 1:] else . end;
    [.services | to_entries[] | .key as $svc | (.value.volumes // [])[]
      | select(.type == "bind" and ((.source // "") | rel | . == "S" or startswith("S/")))
      | "\($svc):\(.source | rel)->\(.target | rel)\(if (.read_only // false) then "" else ":rw" end)"]
    | sort' <<<"$config")"
  want_binds="$(jq -S -c 'sort' <<<"$expected_state_binds")"
  [[ "$binds" == "$want_binds" ]] || fail "$label: binds of the state dir differ; want $want_binds, got $binds"
  # NATS and Valkey see no machine key or service secret at all (Valkey: its healthcheck's own
  # password only, listed above).
  bad="$(jq -r --arg k "$keys" '
    .services | to_entries[] | select(.key | IN("nats", "valkey")) | .key as $svc | (.value.volumes // [])[]
    | select((.source // "") | startswith($k + "/")) | select((.source | startswith($k + "/stacks/")) | not)
    | select(.source != ($k + "/valkey/health.password"))
    | "\($svc): \(.source)"' <<<"$config")"
  [[ -z "$bad" ]] || fail "$label: nats or valkey mounts a key file; got:
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
  [[ "$mounting" == "auth docs functions gateway identity landing orchestrator realtime rest runner storage studio worker" ]] ||
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
