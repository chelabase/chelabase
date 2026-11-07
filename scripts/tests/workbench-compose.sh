#!/usr/bin/env bash
# Offline check of the workbench Compose shape (W2 spec section 2): one stack's rendered config,
# from a fixture stack.env (stack feat-x, gateway 8137, proxy 4537).
set -euo pipefail
# `producer | grep -q` under pipefail races: grep -q exits at its first match, the producer can
# get SIGPIPE/EPIPE and fail the pipeline. gq is grep -q that reads all its input (same status).
gq() { grep "$@" >/dev/null; }
root="$(cd "$(dirname "$0")/../.." && pwd)"
fixture="$root/scripts/tests/fixtures/workbench-stack.env"
fail() {
  echo "workbench-compose: $*" >&2
  exit 1
}
config="$(env -u NEON_IMAGE -u COMPUTE_NODE_IMAGE docker compose -p chelabase-feat-x \
  -f "$root/infra/compose/docker-compose.yml" -f "$root/infra/compose/docker-compose.workbench.yml" \
  --env-file "$fixture" \
  --profile sites --profile runner --profile services config --no-env-resolution --format json |
  jq -f "$root/scripts/tests/fixtures/secrets-as-binds.jq")" || fail "compose config failed"
# (The rewrite lists each service's file secrets, which Docker binds read-only, among its volumes.)
q() { jq -r "$1" <<<"$config"; }

[[ "$(q .name)" == chelabase-feat-x ]] || fail "the project name comes from -p"

published="$(q '[.services | to_entries[] | select(.value.ports) | .key as $s
  | .value.ports[] | "\($s) \(.host_ip) \(.published) \(.target)"] | sort | .[]')"
expected="$(printf '%s\n' "gateway 127.0.0.1 8137 8137" "gateway ::1 8137 8137" \
  "proxy 127.0.0.1 4537 4432" "proxy ::1 4537 4432" | sort)"
[[ "$published" == "$expected" ]] || fail "published ports must be the gateway's and the proxy's Postgres port only; got:
$published"

! grep -q 'host.docker.internal' <<<"$config" || fail "host.docker.internal is gone"
! grep -q 'host-gateway' <<<"$config" || fail "no extra_hosts host-gateway"

for name in studio api auth mail docs landing; do
  q '.services.gateway.networks.default.aliases[]?' | gq -x "$name.feat-x.chelabase.localhost" ||
    fail "the gateway answers $name.feat-x.chelabase.localhost inside the network"
done
[[ "$(q '.services.gateway.environment.CHELABASE_GATEWAY_LISTEN // empty')" == "" ||
  "$(q '.services.gateway.environment.CHELABASE_GATEWAY_LISTEN')" == "0.0.0.0:8137" ]] ||
  fail "the gateway listens on its published port inside"

q '.services["storage-controller"].command[]' | gq -x -- '--control-plane-url=http://orchestrator:8083/storcon' ||
  fail "storcon's hook calls the orchestrator by service name"
q '.services.proxy.command[]' | gq -x -- '--auth-endpoint=http://orchestrator:8083/proxy/api/v1' ||
  fail "the proxy's auth endpoint is the orchestrator by service name"

for s in orchestrator worker gateway auth rest studio docs landing runner; do
  [[ "$(q ".services[\"$s\"].image // empty")" == chelabase-workbench:fixture000000 ]] || fail "$s runs the workbench image"
  [[ "$(q ".services[\"$s\"].user // empty")" == 1000:1000 ]] || fail "$s runs as the host user"
  [[ "$(q ".services[\"$s\"].working_dir // empty")" == /w/feat-x ]] || fail "$s works in the checkout"
done
for s in docs landing; do
  [[ "$(q ".services[\"$s\"].profiles // [] | index(\"sites\")")" != null ]] || fail "$s is in the sites profile"
done
[[ "$(q '.services.runner.profiles // [] | index("runner")')" != null ]] || fail "the runner is in the runner profile"
for s in orchestrator worker gateway auth rest studio; do
  [[ "$(q ".services[\"$s\"].profiles // [] | length")" == 0 ]] || fail "$s starts by default"
done

for s in orchestrator worker gateway auth rest; do
  [[ "$(q ".services[\"$s\"].command | if type == \"array\" then .[0] else . end")" == "/w/feat-x/target/debug/$s" ]] ||
    fail "$s runs the checkout's debug binary"
  [[ "$(q ".services[\"$s\"].volumes[] | select(.source == \"/w/feat-x\") | .read_only // false")" == true ]] ||
    fail "$s mounts the checkout read-only"
done
[[ "$(q '.services.studio.volumes[] | select(.source == "/w/feat-x") | .read_only // false')" == false ]] ||
  fail "studio mounts the checkout read-write (Vite's cache)"

for s in orchestrator worker runner; do
  q ".services[\"$s\"].volumes[] | select(.target == \"/var/run/docker.sock\") | .source" | gq -x /var/run/docker.sock ||
    fail "$s gets the Docker socket"
  q ".services[\"$s\"].group_add[]?" | gq -x 973 || fail "$s joins the docker group"
done
for s in gateway auth rest studio mailpit metadata-db; do
  [[ -z "$(q ".services[\"$s\"].volumes[]? | select(.source == \"/var/run/docker.sock\") | .source")" ]] ||
    fail "$s must not get the Docker socket"
done

# Which key and state files each service sees: compose-key-visibility.sh (U10). Only the runner
# mounts the whole state dir (read-write, at its own path); the apps get none of it.
for s in orchestrator worker gateway auth rest realtime studio; do
  [[ -z "$(q ".services[\"$s\"].volumes[]? | select(.source == \"/m/.chelabase/stacks/feat-x\") | .target")" ]] ||
    fail "$s must not mount the whole state dir"
done
[[ "$(q '.services.runner.volumes[] | select(.source == "/m/.chelabase/stacks/feat-x") | "\(.target) \(.read_only // false)"')" == "/m/.chelabase/stacks/feat-x false" ]] ||
  fail "runner mounts the state dir read-write at its own path"

# The exceptions: the orchestrator reads the stack's setup code (#126), checked below, and the
# worker the `storage` identity's two S3 key files for the blob sweep (object-storage m8) and
# the `functions` identity's for the bundle sweep (edge-functions m7).
for s in orchestrator worker gateway auth rest realtime studio; do
  [[ -z "$(q ".services[\"$s\"].volumes[]? | select((.source // \"\") | startswith(\"/m/.chelabase/stacks/feat-x/\"))
    | select(\"$s:\" + .source != \"orchestrator:/m/.chelabase/stacks/feat-x/setup/setup.code\")
    | select((\"$s:\" + .source | test(\"^worker:/m/[.]chelabase/stacks/feat-x/(storage|functions)-s3/(access|secret)-key$\")) | not) | .source")" ]] ||
    fail "$s mounts nothing from the state dir (it opens no state file)"
done
for s in mailpit identity orchestrator worker gateway auth rest functions service-runner; do
  [[ -n "$(q ".services[\"$s\"].healthcheck.test // empty")" ]] || fail "$s has a healthcheck"
done

for s in orchestrator worker gateway auth rest functions service-runner runner; do
  q ".services[\"$s\"].env_file[]? | if type == \"object\" then .path else . end" | gq -x /m/.chelabase/stacks/feat-x/env/stack.env ||
    fail "$s reads the stack env"
done
q '.services.runner.volumes[] | select(.target == "/usr/local/bin/cargo-nextest") | .source' | gq -x /usr/local/bin/cargo-nextest ||
  fail "the runner gets the host's cargo-nextest"
# W2 live: a live test's own input (docker_backend_live's endpoint id) reaches the runner from
# the caller's env, as it reached `cargo test` on the host.
[[ "$(q '.services.runner.environment // {} | has("CHELABASE_LIVE_ENDPOINT")')" == true ]] ||
  fail "the runner passes CHELABASE_LIVE_ENDPOINT through from the caller"

df="$root/infra/workbench/Dockerfile"
[[ -f "$df" ]] || fail "infra/workbench/Dockerfile exists"
grep -Eq '^FROM ubuntu:26\.04@sha256:[0-9a-f]{64}' "$df" || fail "the image starts from ubuntu:26.04 by digest"
grep -q '24\.21\.0' "$df" || fail "node 24.21.0"
grep -q 'SHASUMS256' "$df" || fail "node's tarball is verified"
# T4 review M4/M5: the checksum list is itself verified against the Node release keys, Docker's
# apt key by its published fingerprint, and the node tarball follows the build architecture.
grep -q 'SHASUMS256.txt.asc' "$df" || fail "node's SHASUMS256.txt is signature-checked (SHASUMS256.txt.asc)"
grep -Eq 'gpg[^\n]*--verify' "$df" || fail "gpg --verify runs on the node checksums"
grep -q '9DC858229FC7DD38854AE2D88D81803C0EBFCD88' "$df" || fail "Docker's apt key is checked by its fingerprint"
! grep -q 'linux-x64' "$df" || fail "the node architecture isn't hard-coded (map dpkg --print-architecture)"
[[ "$(grep -c 'dpkg --print-architecture' "$df")" -ge 2 ]] || fail "both the Docker repo and node follow dpkg --print-architecture"
grep -q 'pnpm@12\.6\.0' "$df" || fail "pnpm 12.6.0 through corepack"
for tool in ca-certificates curl jq postgresql-client docker-ce-cli; do
  grep -q "$tool" "$df" || fail "the image has $tool"
done
[[ "$(q '.networks.control.ipam.config[0].subnet')" == 10.213.4.0/22 ]] || fail "the control network has the stack's subnet"
[[ "$(q '.networks.control.ipam.config[0].ip_range')" == 10.213.6.0/23 ]] || fail "computes get addresses from the upper /23"
[[ "$(q '.services.orchestrator.networks.control.ipv4_address')" == 10.213.4.10 ]] || fail "the orchestrator's static control address"
[[ "$(q '.services.runner.networks.control.ipv4_address')" == 10.213.4.11 ]] || fail "the runner's static control address"
[[ "$(q '.services.orchestrator.networks | keys | sort | join(" ")')" == "compute control default functions-control" ]] || fail "the orchestrator is on the default, control, compute and functions-control networks"
[[ "$(q '.services.worker.networks | keys | sort | join(" ")')" == "compute default functions-control services-control" ]] || fail "the worker isn't on the control network (it is on compute, default, functions-control and services-control)"
ca_mounts="$(q '[.services | to_entries[] | select(any(.value.volumes[]?; .source == "/m/.chelabase/compute-ca")) | "\(.key):\(any(.value.volumes[]; .source == "/m/.chelabase/compute-ca" and .read_only == true))"] | sort | join(" ")')"
[[ "$ca_mounts" == "runner:true worker:false" ]] || fail "compute-ca: rw for the worker, ro for the runner (R-T9a), nobody else; got: $ca_mounts"
tls_mounts="$(q '[.services | to_entries[] | select(any(.value.volumes[]?; .source == "/m/.chelabase/orchestrator-tls")) | "\(.key):\(any(.value.volumes[]; .source == "/m/.chelabase/orchestrator-tls" and .read_only == true))"] | sort | join(" ")')"
[[ "$tls_mounts" == "orchestrator:true runner:true worker:false" ]] || fail "orchestrator-tls: rw for the worker, ro for the orchestrator and runner; got: $tls_mounts"

# --- RT1 T1 (R11): the compute network (P4, W14) ---------------------------------------------
# Computes join `<project>_compute` (+ control). On it: what a compute needs (pageserver,
# safekeepers, the storage broker, the orchestrator's hooks listener) and the clients that dial
# computes. Never NATS, Valkey, a database, identity, Mailpit, SeaweedFS, the gateway or Studio.
[[ "$(q '.networks | has("compute")')" == true ]] || fail "the stack defines a compute network"
on_compute="$(q '[.services | to_entries[] | select(.value.networks // {} | has("compute")) | .key] | sort | join(" ")')"
# Ruling (d): the storage broker isn't needed by computes (unauthenticated gRPC): not on it.
[[ "$on_compute" == "auth orchestrator pageserver proxy realtime rest runner safekeeper1 safekeeper2 safekeeper3 storage worker" ]] ||
  fail "exactly the compute dependencies and the clients that dial computes are on the compute network; got: $on_compute"
for s in auth orchestrator pageserver proxy realtime rest runner safekeeper1 safekeeper2 safekeeper3 worker; do
  [[ "$(q ".services[\"$s\"].networks | has(\"default\")")" == true ]] || fail "$s stays on the default network too"
done
[[ "$(q '.services.runner.networks | keys | sort | join(" ")')" == "compute control default functions-control" ]] || fail "the runner is on default, control, compute and functions-control (the functions e2e)"
for s in nats valkey metadata-db storcon-db identity mailpit seaweedfs gateway storage-controller storage-broker studio docs landing; do
  [[ "$(q ".services[\"$s\"].networks // {} | has(\"compute\")")" == false ]] || fail "$s must not be on the compute network"
done
# The orchestrator answers computes as `orchestrator` there (CHELABASE_CONTROL_PLANE_URI).
[[ "$(q '.services.orchestrator.networks.compute.aliases // [] | map(select(. != "orchestrator")) | length')" == 0 ]] ||
  fail "the orchestrator has no other name on the compute network"
grep -qx 'CHELABASE_COMPUTE_NETWORK=chelabase-feat-x_compute' "$fixture" || fail "the fixture's computes use the compute network"

# --- RT1 T1 (R11, S1): per-service credentials, never in stack.env ---------------------------
env_of() { q ".services[\"$1\"].environment // {} | .[\"$2\"] // empty"; }
want_env() { # want_env <service> <key> <value>
  [[ "$(env_of "$1" "$2")" == "$3" ]] || fail "$1: $2 must be $3, got '$(env_of "$1" "$2")'"
}
K=/m/.chelabase
for pair in orchestrator:orchestrator worker:worker runner:runner; do
  s="${pair%%:*}" u="${pair#*:}"
  want_env "$s" CHELABASE_NATS_USER "$u"
  want_env "$s" CHELABASE_NATS_PASSWORD_FILE "$K/nats/$u.password"
  want_env "$s" CHELABASE_NATS_INBOX_PREFIX "_INBOX_$u"
done
for pair in orchestrator:control worker:control runner:runner; do
  s="${pair%%:*}" u="${pair#*:}"
  want_env "$s" CHELABASE_VALKEY_USER "$u"
  want_env "$s" CHELABASE_VALKEY_PASSWORD_FILE "$K/valkey/$u.password"
done
want_env worker CHELABASE_REALTIME_PG_PASSWORD_FILE "$K/realtime/pg.password"
want_env orchestrator CHELABASE_REALTIME_INTERNAL_TOKEN_FILE "$K/realtime/internal.token"
for f in PG_PASSWORD:pg.password TICKET_KEY:ticket.key INTERNAL_TOKEN:internal.token; do
  want_env runner "CHELABASE_REALTIME_${f%%:*}_FILE" "$K/realtime/${f#*:}"
done
# S27: every internal bearer is a generated file, read through <NAME>_FILE by the services that use it.
for t in REST_INTERNAL:rest-internal DATA_KEYS:data-keys APP_AUTH_INTERNAL:app-auth APP_SESSIONS_INTERNAL:app-sessions PROXY_API:proxy-api STORCON_HOOK:storcon-hook; do
  for s in orchestrator runner; do
    want_env "$s" "CHELABASE_${t%%:*}_TOKEN_FILE" "$K/tokens/${t#*:}.token"
  done
done
want_env orchestrator CHELABASE_STORAGE_INTERNAL_TOKEN_FILE "$K/tokens/storage-internal.token"
want_env gateway CHELABASE_DATA_KEYS_TOKEN_FILE "$K/tokens/data-keys.token"
want_env gateway CHELABASE_APP_SESSIONS_INTERNAL_TOKEN_FILE "$K/tokens/app-sessions.token"
want_env auth CHELABASE_APP_AUTH_INTERNAL_TOKEN_FILE "$K/tokens/app-auth.token"
want_env rest CHELABASE_REST_INTERNAL_TOKEN_FILE "$K/tokens/rest-internal.token"
# No plaintext form and no public development bearer anywhere in the rendered config.
plain="$(q '[.services | to_entries[] | .key as $s | (.value.environment // {}) | to_entries[]
  | select(.key | IN("CHELABASE_NATS_PASSWORD", "CHELABASE_REST_INTERNAL_TOKEN", "CHELABASE_DATA_KEYS_TOKEN",
      "CHELABASE_APP_AUTH_INTERNAL_TOKEN", "CHELABASE_APP_SESSIONS_INTERNAL_TOKEN", "CHELABASE_PROXY_API_TOKEN",
      "CHELABASE_STORCON_HOOK_TOKEN", "CHELABASE_REALTIME_INTERNAL_TOKEN", "CHELABASE_STORAGE_INTERNAL_TOKEN",
      "NEON_PROXY_TO_CONTROLPLANE_TOKEN"))
  | "\($s): \(.key)"] | join(", ")')"
[[ -z "$plain" ]] || fail "a credential is set in plain in a service's environment: $plain"
! grep -Eq 'chelabase-dev-(rest-internal|data-keys|app-auth|app-sessions|proxy-api|storcon-hook|realtime|storage-internal)' <<<"$config" ||
  fail "a public development bearer (chelabase-dev-*) is in the rendered config"
for k in CHELABASE_NATS_USER CHELABASE_NATS_PASSWORD CHELABASE_VALKEY_USER CHELABASE_VALKEY_PASSWORD_FILE \
  CHELABASE_REST_INTERNAL_TOKEN CHELABASE_DATA_KEYS_TOKEN CHELABASE_APP_AUTH_INTERNAL_TOKEN; do
  ! grep -q "^$k" "$fixture" || fail "the stack.env fixture sets $k (S1: never in stack.env)"
done

# --- RT1 T1: NATS and Valkey auth (S1, S6) --------------------------------------------------
nats_conf="$root/infra/compose/nats/nats.conf"
[[ -f "$nats_conf" ]] || fail "infra/compose/nats/nats.conf exists"
! grep -q 'no_auth_user' "$nats_conf" || fail "nats.conf has no anonymous user"
grep -q 'auth.conf' "$nats_conf" || fail "nats.conf includes the rendered users (auth.conf)"
[[ "$(q '.services.nats.volumes[] | select(.source == "/w/feat-x/infra/compose/nats/nats.conf") | .read_only // false')" == true ]] ||
  fail "nats mounts the checkout's nats.conf read-only"
[[ "$(q '.services.nats.volumes[] | select(.source == "/m/.chelabase/stacks/feat-x/nats/auth.conf") | .read_only // false')" == true ]] ||
  fail "nats mounts the stack's rendered auth.conf read-only"
q '.services.nats.command[]' | gq -Ex -- '-c|--config|--config=.*|-c=.*' || fail "nats starts from its config file"
q '.services.nats.command[]' | gq -x -- '--jetstream' || fail "nats keeps JetStream"
valkey_cmd="$(q '.services.valkey.command // [] | if type == "array" then join(" ") else . end')"
acl_target="$(q '.services.valkey.volumes[] | select(.source == "/m/.chelabase/stacks/feat-x/valkey/users.acl") | .target')"
[[ -n "$acl_target" && "$(q '.services.valkey.volumes[] | select(.source == "/m/.chelabase/stacks/feat-x/valkey/users.acl") | .read_only // false')" == true ]] ||
  fail "valkey mounts the stack's rendered users.acl read-only"
grep -q -- "--aclfile $acl_target" <<<"$valkey_cmd" || fail "valkey loads its ACL file ($acl_target); got: $valkey_cmd"
# Ruling (f): `user default off`, so the healthcheck logs in as the PING-only `health` user, its
# password from a narrow mount; the container runs as the stack's uid so it reads the 0600 files.
valkey_hc="$(q '.services.valkey.healthcheck.test | join(" ")')"
grep -q -- '--user health' <<<"$valkey_hc" && grep -q '/run/valkey-health.password' <<<"$valkey_hc" ||
  fail "valkey's healthcheck authenticates as health with /run/valkey-health.password; got: $valkey_hc"
[[ "$(q '.services.valkey.user // empty')" == 1000:1000 ]] || fail "valkey runs as the stack's uid:gid (reads its 0600 ACL file)"
# S6: the proxy's Valkey password and its control-plane token come from narrow mounts through an
# entrypoint wrapper; its flags stay in `command`.
proxy_entry="$(q '.services.proxy.entrypoint // [] | join(" ")')"
# Fork round 4 (K8, C5): the Valkey password is read by the proxy itself from its mount
# (--redis-plain-password-file), so it is in no argument and no shell substitution.
proxy_flags="$(q '.services.proxy | [(.entrypoint // [])[], (.command // [])[]] | join(" ")')"
grep -q -- '--redis-plain=redis://proxy@valkey:6379' <<<"$proxy_flags" || fail "the proxy logs in to Valkey as proxy, dialing valkey:6379, with no password in the URL; got: $proxy_flags"
grep -q -- '--redis-plain-password-file=/run/valkey-proxy.password' <<<"$proxy_flags" || fail "the proxy reads its Valkey password from /run/valkey-proxy.password (--redis-plain-password-file)"
! grep -q 'cat /run/valkey-proxy.password' <<<"$proxy_entry" || fail "the proxy's entrypoint no longer cats its Valkey password"
grep -q -- '--control-plane-token-file=/run/proxy-api.token' <<<"$proxy_entry" && ! grep -q 'NEON_PROXY_TO_CONTROLPLANE_TOKEN' <<<"$proxy_entry" ||
  fail "the proxy's control-plane token comes from /run/proxy-api.token"
! grep -q -- '--redis-plain' <<<"$(q '.services.proxy.command[]')" || fail "no unauthenticated --redis-plain left in the proxy's command"
q '.services.proxy.command[]' | gq -x -- '--redis-auth-type=plain' || fail "the proxy keeps --redis-auth-type=plain"
storcon_entry="$(q '.services["storage-controller"].entrypoint // [] | join(" ")')"
storcon_flags="$(q '.services["storage-controller"] | [(.entrypoint // [])[], (.command // [])[]] | join(" ")')"
# Fork round 4 (F6): the storage controller reads its hook token from its mount itself
# (--control-plane-jwt-token-file), never through CONTROL_PLANE_JWT_TOKEN in its environment
# and never as `--control-plane-jwt-token=` on its command line (compose-secret-argv.sh).
grep -q -- '--control-plane-jwt-token-file=/run/storcon-hook.token' <<<"$storcon_flags" ||
  fail "the storage controller's hook token comes from /run/storcon-hook.token (--control-plane-jwt-token-file); got: $storcon_flags"
! grep -q 'CONTROL_PLANE_JWT_TOKEN' <<<"$storcon_flags" ||
  fail "the storage controller's hook token is no longer in CONTROL_PLANE_JWT_TOKEN"
! grep -qE -- '--control-plane-jwt-token([= ]|$)' <<<"$storcon_flags" ||
  fail "the storage controller's hook token is on its command line (--control-plane-jwt-token)"

# --- RT1 T1 (user, storage auth): NeonJWT on the pageserver and safekeeper, no --dev ----------
# Tokens are signed with the compute-JWT key pair; the Neon binaries verify them against its SPKI
# PEM (<keys>/neon/compute-jwt.pub.pem) and present the scoped tokens the workbench renders
# into <keys>/neon/storage-tokens/ (storage_controller/src/main.rs Secrets, pageserver
# --auth-token-file and control_plane_api_token, safekeeper --auth-token-path).
storcon_all="$(q '.services["storage-controller"] | [(.entrypoint // [])[], (.command // [])[]] | join(" ")')"
! grep -qw -- '--dev' <<<"$storcon_all" || fail "the storage controller runs without --dev (strict mode)"
# The public key (no secret) may stay in PUBLIC_KEY; the tokens are read by the controller
# itself from their mounts (F6), never through its environment.
grep -q PUBLIC_KEY <<<"$storcon_entry" && grep -q "cat /run/storage-auth.pub.pem" <<<"$storcon_entry" ||
  fail "the storage controller's PUBLIC_KEY comes from /run/storage-auth.pub.pem"
for pair in pageserver-jwt-token-file:/run/pageserver-api.jwt safekeeper-jwt-token-file:/run/safekeeper-data.jwt \
  control-plane-jwt-token-file:/run/storcon-hook.token; do
  grep -qF -- "--${pair%%:*}=${pair#*:}" <<<"$storcon_all" ||
    fail "the storage controller reads --${pair%%:*}=${pair#*:}"
done
for gone in PAGESERVER_JWT_TOKEN SAFEKEEPER_JWT_TOKEN; do
  ! grep -q "$gone" <<<"$storcon_all" || fail "the storage controller no longer gets $gone in its environment"
done
ps_all="$(q '.services.pageserver | [(.entrypoint // [])[], (.command // [])[]] | join(" ")')"
for want in "http_auth_type='NeonJWT'" "pg_auth_type='NeonJWT'" "auth_validation_public_key_path='/run/storage-auth.pub.pem'" \
  "control_plane_api_token" "/run/generations-api.jwt" "--auth-token-file=/run/safekeeper-data.jwt"; do
  grep -qF -- "$want" <<<"$ps_all" || fail "the pageserver's config has $want"
done
! grep -q NEON_AUTH_TOKEN <<<"$ps_all" || fail "the pageserver no longer gets NEON_AUTH_TOKEN in its environment"
# User, 2026-10-16: three safekeepers, so the storage controller's strict mode (no --dev) accepts
# --timeline-safekeeper-count=3 (storage_controller/src/main.rs:397). No fork change.
grep -q -- '--timeline-safekeeper-count=3' <<<"$storcon_all" || fail "the storage controller places every timeline on 3 safekeepers"
sk_volumes=""
for n in 1 2 3; do
  sk_args="$(q ".services.safekeeper$n | [(.entrypoint // [])[], (.command // [])[]] | join(\" \")")"
  for want in "--id=$n" "--advertise-pg=safekeeper$n:5454" "--availability-zone=az-$n" \
    --pg-auth-public-key-path=/run/storage-auth.pub.pem --http-auth-public-key-path=/run/storage-auth.pub.pem \
    --auth-token-path=/run/safekeeper-data.jwt; do
    grep -qF -- "$want" <<<"$sk_args" || fail "safekeeper$n runs with $want"
  done
  sk_volumes+="$(q ".services.safekeeper$n.volumes // [] | map(select(.type == \"volume\") | .source) | join(\",\")") "
done
[[ "$(tr ' ' '\n' <<<"$sk_volumes" | grep -c .)" == 3 && "$(tr ' ' '\n' <<<"$sk_volumes" | grep . | sort -u | wc -l)" == 3 ]] ||
  fail "each safekeeper keeps its WAL on its own volume; got: $sk_volumes"
reg="$(q '.services["register-safekeepers"] | [(.entrypoint // [])[], (.command // [])[]] | join(" ")')"
grep -q 'Authorization: Bearer' <<<"$reg" && grep -q '/run/storcon-admin.jwt' <<<"$reg" ||
  fail "register-safekeepers calls the storage controller with the admin token from /run/storcon-admin.jwt"
grep -Eq 'for ID in 1 2 3;' <<<"$reg" || fail "register-safekeepers registers safekeepers 1, 2 and 3 (computes get them from the storage controller)"
for n in 1 2 3; do
  [[ "$(q ".services[\"register-safekeepers\"].depends_on.safekeeper$n.condition // empty")" == service_healthy ]] ||
    fail "register-safekeepers waits for a healthy safekeeper$n"
done

# --- RT1 T7: the realtime service, wired like rest (pre-flight §4, S1, P4) --------------------
[[ "$(q '.services | has("realtime")')" == true ]] || fail "the stack runs realtime"
[[ "$(q '.services.realtime.image // empty')" == chelabase-workbench:fixture000000 ]] || fail "realtime runs the workbench image"
[[ "$(q '.services.realtime.user // empty')" == 1000:1000 ]] || fail "realtime runs as the host user"
[[ "$(q '.services.realtime.working_dir // empty')" == /w/feat-x ]] || fail "realtime works in the checkout"
[[ "$(q '.services.realtime.profiles // [] | length')" == 0 ]] || fail "realtime starts by default"
[[ "$(q '.services.realtime.command | if type == "array" then .[0] else . end')" == /w/feat-x/target/debug/realtime ]] ||
  fail "realtime runs the checkout's debug binary"
[[ "$(q '.services.realtime.volumes[] | select(.source == "/w/feat-x") | .read_only // false')" == true ]] ||
  fail "realtime mounts the checkout read-only"
[[ -z "$(q '.services.realtime.volumes[]? | select((.source // "") | . == "/m/.chelabase/stacks/feat-x" or startswith("/m/.chelabase/stacks/feat-x/")) | .source')" ]] ||
  fail "realtime mounts nothing from the state dir (it opens no state file)"
[[ -z "$(q '.services.realtime.volumes[]? | select(.source == "/var/run/docker.sock") | .source')" ]] ||
  fail "realtime must not get the Docker socket"
[[ -z "$(q '.services.realtime.ports // [] | .[]?')" ]] || fail "realtime publishes no port (the gateway is the way in)"
q '.services.realtime.env_file[]? | if type == "object" then .path else . end' |
  gq -x /m/.chelabase/stacks/feat-x/env/stack.env || fail "realtime reads the stack env"
grep -q '8088/health' <<<"$(q '.services.realtime.healthcheck.test // [] | join(" ")')" ||
  fail "realtime's healthcheck asks its internal listener's /health (8088)"
for dep in nats valkey; do
  [[ "$(q ".services.realtime.depends_on.$dep.condition // empty")" == service_healthy ]] || fail "realtime waits for a healthy $dep"
done
# P4: realtime dials computes (T8's replication and check connections) on the compute network.
# Fork round 4 (C4, C5, SP10): realtime logs in to a compute as chelabase_realtime over the control
# network, from a static address outside the computes' ip_range (so no compute can take it).
[[ "$(q '.services.realtime.networks | keys | sort | join(" ")')" == "compute control default" ]] ||
  fail "realtime is on the default, control and compute networks only"
[[ "$(q '.services.realtime.networks.control.ipv4_address')" == 10.213.4.12 ]] ||
  fail "realtime's static control address is \${CHELABASE_REALTIME_CONTROL_IP} (10.213.4.12)"
[[ "$(q '.services.realtime.networks.control.ipv4_address')" == "$(sed -n 's/^CHELABASE_REALTIME_CONTROL_IP=//p' "$fixture")" ]] ||
  fail "realtime's control address comes from CHELABASE_REALTIME_CONTROL_IP"
# S1: realtime's own credentials and secrets, as files, in its own environment.
want_env realtime CHELABASE_NATS_USER realtime
want_env realtime CHELABASE_NATS_PASSWORD_FILE "$K/nats/realtime.password"
want_env realtime CHELABASE_NATS_INBOX_PREFIX _INBOX_realtime
want_env realtime CHELABASE_VALKEY_USER realtime
want_env realtime CHELABASE_VALKEY_PASSWORD_FILE "$K/valkey/realtime.password"
for f in PG_PASSWORD:pg.password TICKET_KEY:ticket.key INTERNAL_TOKEN:internal.token; do
  want_env realtime "CHELABASE_REALTIME_${f%%:*}_FILE" "$K/realtime/${f#*:}"
done
# Fork round 4: the realtime client pair (realtime-tls/current), the CIDR list and the storage
# token refresh interval, per service. A setting of stack.env reaches a service through its
# env_file; eff_env is a service's environment, else stack.env's value when it reads stack.env.
eff_env() { # eff_env <service> <key>
  local v
  v="$(env_of "$1" "$2")"
  if [[ -z "$v" ]] && q ".services[\"$1\"].env_file[]? | if type == \"object\" then .path else . end" |
    gq -x /m/.chelabase/stacks/feat-x/env/stack.env; then
    v="$(sed -n "s/^$2=//p" "$fixture")"
  fi
  printf '%s' "$v"
}
want_eff() { # want_eff <service> <key> <value>
  [[ "$(eff_env "$1" "$2")" == "$3" ]] || fail "$1: $2 must be $3, got '$(eff_env "$1" "$2")'"
}
want_eff orchestrator CHELABASE_REALTIME_LOGIN_CIDRS "10.213.4.12/32,10.213.4.11/32"
want_eff worker CHELABASE_REALTIME_LOGIN_CIDRS "10.213.4.12/32,10.213.4.11/32"
want_eff runner CHELABASE_REALTIME_LOGIN_CIDRS "10.213.4.12/32,10.213.4.11/32"
for s in realtime worker runner; do
  want_eff "$s" CHELABASE_REALTIME_TLS_DIR "$K/realtime-tls"
done
[[ -n "$(env_of worker CHELABASE_STORAGE_TOKEN_REFRESH_SECS)" ]] ||
  fail "worker: CHELABASE_STORAGE_TOKEN_REFRESH_SECS is set in its environment"
rt_mounts="$(q '[.services | to_entries[] | select(any(.value.volumes[]?; .source == "/m/.chelabase/realtime-tls")) | "\(.key):\(any(.value.volumes[]; .source == "/m/.chelabase/realtime-tls" and .read_only == true))"] | sort | join(" ")')"
[[ "$rt_mounts" == "realtime:true runner:true worker:false" ]] ||
  fail "realtime-tls: rw for the worker (it renews the pair), ro for realtime and the runner, nobody else; got: $rt_mounts"
[[ "$(q '[.services | to_entries[] | select(any(.value.volumes[]?; .target == "/m/.chelabase/realtime-tls" and .type == "bind" and .source == .target)) | .key] | length')" == 3 ]] ||
  fail "realtime-tls is mounted at its own path"
mt_mounts="$(q '[.services | to_entries[] | select(any(.value.volumes[]?; .source == "/m/.chelabase/neon/storage-tokens/metrics.token")) | "\(.key):\(any(.value.volumes[]; .source == "/m/.chelabase/neon/storage-tokens/metrics.token" and .read_only == true))"] | sort | join(" ")')"
[[ "$mt_mounts" == "runner:true" ]] || fail "metrics.token: read-only for the runner, nobody else; got: $mt_mounts"
# S7: Valkey TLS is off in the stack (platform mode turns it on).
[[ "$(env_of realtime CHELABASE_VALKEY_TLS)" == off ]] || fail "realtime: CHELABASE_VALKEY_TLS=off in the stack"
# --- Object storage (epic object-storage m2): one SeaweedFS, two buckets, two identities -------
# The identities, bucket-scoped actions, gRPC mTLS and healthcheck are the base file's (checked in
# compose-secret-boundary.sh); the workbench runs them as is, with the key pairs and the gRPC TLS
# files as Compose secrets from the stack's state dir (so none is in `docker inspect`). No admin
# key exists, and create-bucket holds nothing.
seaweed_binds() { # "target<-<dir>/<file>" of seaweedfs's key and TLS file binds, sorted
  q '[.services.seaweedfs.volumes[]? | select(.type == "bind" and (.target | test("/secrets/(s3|storage-s3|grpc-tls)/")))
    | "\(.target | sub("^/run/chelabase/secrets/"; ""))<-\(.source | sub("^.*/(?<d>(s3|storage-s3|grpc-tls)/[a-z.-]+)$"; "\(.d)"))"] | sort | join(" ")'
}
sw_want="grpc-tls/ca.crt<-grpc-tls/ca.crt grpc-tls/client.crt<-grpc-tls/client.crt grpc-tls/client.key<-grpc-tls/client.key"
sw_want+=" grpc-tls/server.crt<-grpc-tls/server.crt grpc-tls/server.key<-grpc-tls/server.key"
sw_want+=" s3/access-key<-s3/access-key s3/secret-key<-s3/secret-key"
sw_want+=" storage-s3/access-key<-storage-s3/access-key storage-s3/secret-key<-storage-s3/secret-key"
[[ "$(seaweed_binds)" == "$sw_want" ]] || fail "seaweedfs mounts the neon and storage key pairs and the gRPC TLS files; got: $(seaweed_binds)"
others="$(q '[.services | to_entries[] | select(.key != "seaweedfs")
  | select(any(.value.volumes[]?; .type == "bind" and (.source | test("/(grpc-tls|s3-admin)/")))) | .key] | join(" ")')"
[[ -z "$others" ]] || fail "only seaweedfs mounts the gRPC TLS files; got: $others"
# m5: the `storage` identity's keys: seaweedfs, the storage service and the worker (its blob
# sweep, m8), read-only, nobody else.
others="$(q '[.services | to_entries[] | select(any(.value.volumes[]?; .type == "bind" and (.source | test("/storage-s3/"))))
  | .key] | sort | join(" ")')"
[[ "$others" == "seaweedfs storage worker" ]] || fail "only seaweedfs, storage and the worker mount the storage S3 keys; got: $others"
storage_binds="$(q '[.services.storage.volumes[] | select(.type == "bind" and (.source | test("/storage-s3/")))
  | "\(.target)\(if (.read_only // false) then "" else ":rw" end)"] | sort | join(" ")')"
[[ "$storage_binds" == "/run/chelabase/secrets/storage-s3/access-key /run/chelabase/secrets/storage-s3/secret-key" ]] ||
  fail "storage mounts its two key files read-only; got: $storage_binds"
[[ "$(q '.services["create-bucket"] | [.volumes // [], .environment // {}] | tojson')" == '[[],{}]' ]] ||
  fail "create-bucket holds no key, volume or credential"
! grep -qE 's3-admin|chelabase-(storage|admin)-dev' <<<"$config" || fail "no admin key file and no dev S3 key of the storage identity is in the rendered config (inspect)"
for s in pageserver safekeeper1 safekeeper2 safekeeper3; do
  want_env "$s" AWS_ACCESS_KEY_ID chelabase-dev
done
[[ "$(q '[.services | to_entries[] | select(any(.value.volumes[]?; .source == "neon_seaweedfs_data")) | .key] | join(" ")')" == seaweedfs ]] ||
  fail "only seaweedfs mounts its data volume (S3 is reached by URL only)"
# Edge functions (m4b): the `functions` identity's pair, generated per stack in its state dir
# (never a fixed value), mounted read-only by seaweedfs, the functions service and the worker
# (the bundle sweep, m7) only.
fn_binds() { # fn_binds <service>: its functions-s3 binds, "target<-source", sorted
  q "[.services[\"$1\"].volumes[]? | select(.type == \"bind\" and (.source | test(\"/functions-s3/\")))
    | \"\\(.target)<-\\(.source)\\(if (.read_only // false) then \"\" else \":rw\" end)\"] | sort | join(\" \")"
}
fn_want="/run/chelabase/secrets/functions-s3/access-key<-/m/.chelabase/stacks/feat-x/functions-s3/access-key"
fn_want+=" /run/chelabase/secrets/functions-s3/secret-key<-/m/.chelabase/stacks/feat-x/functions-s3/secret-key"
for s in seaweedfs functions worker; do
  [[ "$(fn_binds "$s")" == "$fn_want" ]] || fail "$s mounts the stack's functions S3 pair read-only; got: $(fn_binds "$s")"
done
others="$(q '[.services | to_entries[] | select(any(.value.volumes[]?; (.source // "") | test("/functions-s3/"))) | .key] | sort | join(" ")')"
[[ "$others" == "functions seaweedfs worker" ]] || fail "only seaweedfs, functions and the worker mount the functions S3 keys; got: $others"
! grep -qE 'chelabase-functions-dev' <<<"$config" || fail "no fixed dev key of the functions identity is in the rendered config"
[[ "$(q '.configs // {} | keys | join(" ")')" == "" ]] || fail "the overlay holds no fixed-value configs"
[[ "$(q '.services.functions.command | join(" ")')" == "$(sed -n 's/^CHELABASE_CHECKOUT=//p' "$fixture")/target/debug/functions" ]] ||
  fail "functions runs the checkout's debug binary"
[[ "$(q '.services["functions-runtime"].image')" == chelabase/functions-runtime:* && "$(q '.services["functions-runtime"].pull_policy')" == never ]] ||
  fail "functions-runtime runs the locally built image, never pulled"
# Edge functions (m10): the service runner, in profile services (off by default), runs the
# checkout's debug binary in the Docker group, with the socket and its two caller bearers only.
[[ "$(q '.services["service-runner"].profiles | join(" ")')" == services ]] || fail "service-runner is in the services profile only"
[[ "$(q '.services["service-runner"].command | join(" ")')" == "$(sed -n 's/^CHELABASE_CHECKOUT=//p' "$fixture")/target/debug/service-runner" ]] ||
  fail "service-runner runs the checkout's debug binary"
[[ "$(q '.services["service-runner"].group_add | join(" ")')" == "$(sed -n 's/^CHELABASE_DOCKER_GID=//p' "$fixture")" ]] ||
  fail "service-runner joins the Docker group"
[[ "$(q '[.services["service-runner"].volumes[] | select(.type == "bind" and (.source | test("docker.sock|service-runner.token$"))) | .source | split("/") | last] | sort | join(" ")')" == "docker.sock functions-service-runner.token worker-service-runner.token" ]] ||
  fail "service-runner mounts the Docker socket and its two caller bearers"

# Neon's S3 endpoint is the base file's one setting, default http://seaweedfs:8333, for the
# safekeepers and the pageserver under the workbench too.
neon_endpoints() { # neon_endpoints <json>
  local svc
  for svc in pageserver safekeeper1 safekeeper2 safekeeper3; do
    jq -r --arg s "$svc" '.services[$s].command | map(tostring) | join(" ")' <<<"$1" |
      grep -oP "(?<![_a-z])endpoint='[^']*'" | sed "s|^endpoint='|$svc=|; s|'\$||"
  done | sort | tr '\n' ' ' | sed 's/ $//'
}
[[ "$(neon_endpoints "$config")" == "pageserver=http://seaweedfs:8333 safekeeper1=http://seaweedfs:8333 safekeeper2=http://seaweedfs:8333 safekeeper3=http://seaweedfs:8333" ]] ||
  fail "Neon's S3 endpoint defaults to http://seaweedfs:8333 under the workbench; got: $(neon_endpoints "$config")"
over="$(CHELABASE_NEON_S3_ENDPOINT=http://other.example:9000 env -u NEON_IMAGE -u COMPUTE_NODE_IMAGE docker compose -p chelabase-feat-x \
  -f "$root/infra/compose/docker-compose.yml" -f "$root/infra/compose/docker-compose.workbench.yml" \
  --env-file "$fixture" --profile sites --profile runner config --no-env-resolution --format json)"
[[ "$(neon_endpoints "$over")" == "pageserver=http://other.example:9000 safekeeper1=http://other.example:9000 safekeeper2=http://other.example:9000 safekeeper3=http://other.example:9000" ]] ||
  fail "CHELABASE_NEON_S3_ENDPOINT changes Neon's S3 endpoint under the workbench; got: $(neon_endpoints "$over")"

# auth-replacement m5 (#118): the identity service runs from the checkout's sources on the
# workbench image, on the default network only, with no host port and no gateway route.
[[ "$(q '.services.identity // empty | length')" -gt 0 ]] || fail "the stack runs identity"
[[ "$(q '.services.identity.ports // [] | length')" == 0 ]] || fail "identity publishes no host port"
[[ "$(q '.services.identity.networks // {} | keys | join(" ")')" == default ]] ||
  fail "identity is on the default network only, got: $(q '.services.identity.networks // {} | keys | join(" ")')"
[[ "$(q '.services.identity.image')" == chelabase-workbench:fixture000000 ]] || fail "identity runs the workbench image"
[[ "$(q '.services.identity.user')" == 1000:1000 ]] || fail "identity runs as the host user"
[[ "$(q '.services.identity.profiles // [] | length')" == 0 ]] || fail "identity starts by default"
q '.services.identity.command | if type == "array" then join(" ") else . end' | gq -F 'services/identity/src/main.ts' ||
  fail "identity runs the checkout's identity sources"
[[ "$(q '.services.identity.volumes[] | select(.source == "/w/feat-x") | .read_only // false')" == true ]] ||
  fail "identity mounts the checkout read-only"
[[ -n "$(q '.services.identity.healthcheck.test // empty')" ]] || fail "identity has a healthcheck"
# Its entrypoint writes the database URL into /run/identity as the service user: the tmpfs must
# be that user's own, in both files (a root-owned tmpfs crash-loops the service).
q '.services.identity.tmpfs // [] | .[]' | gq -Fx '/run/identity:uid=1000,gid=1000,mode=0700' ||
  fail "identity's /run/identity tmpfs is the host user's own"
[[ -z "$(q '.services.identity.volumes[]? | select(.source == "/var/run/docker.sock") | .source')" ]] ||
  fail "identity must not get the Docker socket"
q '.services.identity.depends_on | keys[]' | gq -x metadata-db || fail "identity waits for metadata-db"
want_env identity CHELABASE_IDENTITY_SECRET_FILE "$K/identity/secret"
want_env identity CHELABASE_IDENTITY_DB_PASSWORD_FILE "$K/identity/db.password"
for c in AUTH ORCHESTRATOR WORKER; do
  lc="$(tr '[:upper:]' '[:lower:]' <<<"$c")"
  want_env identity "CHELABASE_IDENTITY_${c}_TOKEN_FILE" "$K/tokens/$lc-identity.token"
done
for s in auth orchestrator worker; do
  want_env "$s" CHELABASE_IDENTITY_TOKEN_FILE "$K/tokens/$s-identity.token"
  want_eff "$s" CHELABASE_IDENTITY_URL http://identity:8080
done
# Workbench mail goes to Mailpit, in plain SMTP.
want_env identity CHELABASE_SMTP_HOST mailpit
want_env identity CHELABASE_SMTP_TLS none
# The gateway has no upstream for it.
! q '.services.gateway.environment // {} | to_entries[] | .value' | gq -E '//identity(:|/|$)' ||
  fail "the gateway must have no identity upstream"

# The base file: the identity image, no host port, the internal network only.
base="$(env -u NEON_IMAGE -u COMPUTE_NODE_IMAGE docker compose -p chelabase-base-x \
  -f "$root/infra/compose/docker-compose.yml" --env-file "$root/infra/compose/.env.example" \
  config --no-env-resolution --format json)" || fail "base compose config failed"
bq() { jq -r "$1" <<<"$base"; }
[[ "$(bq '.services.identity.ports // [] | length')" == 0 ]] || fail "base: identity publishes no host port"
[[ "$(bq '.services.identity.networks // {} | keys | join(" ")')" == default ]] || fail "base: identity is on the default network only"
bq '.services.identity.image' | gq -E '/identity:' || fail "base: identity runs the identity image"
[[ "$(bq '.services.identity.user')" == 10001:10001 ]] || fail "base: identity runs as the app user"
[[ -n "$(bq '.services.identity.healthcheck.test // empty')" ]] || fail "base: identity has a healthcheck"
for d in metadata-db init-secrets; do
  bq '.services.identity.depends_on | keys[]' | gq -x "$d" || fail "base: identity waits for $d"
done
bq '.services["init-bootstrap"].depends_on | keys[]' | gq -x identity || fail "base: the bootstrap waits for identity"
[[ "$(bq '.services.identity.read_only // false')" == true ]] || fail "base: identity's root filesystem is read-only"
! bq '.services.gateway.environment // {} | to_entries[] | .value' | gq -E '//identity(:|/|$)' ||
  fail "base: the gateway must have no identity upstream"
published_base="$(bq '[.services | to_entries[] | select(.value.ports) | .key] | unique | join(" ")')"
[[ "$published_base" == "gateway proxy" ]] || fail "base: only the gateway and the proxy publish ports, got: $published_base"

# auth-replacement m5 (#118): the base identity service owns its /run/identity tmpfs.
bq '.services.identity.tmpfs // [] | .[]' | grep -Fqx '/run/identity:uid=10001,gid=10001,mode=0700' ||
  fail "base identity's /run/identity tmpfs is uid 10001's own"
# m13 fix (#126): the dev member claims the stack's tenant with the setup code, and only the
# orchestrator checks it. Without the code it answers every first sign-up `sign_up_closed`, so a
# fresh workbench stack's bootstrap failed. It reads the stack's own code file, mounted read-only.
code=/m/.chelabase/stacks/feat-x/setup/setup.code
want_env orchestrator CHELABASE_SETUP_CODE_FILE "$code"
[[ "$(q ".services.orchestrator.volumes[] | select(.target == \"$code\") | \"\(.source) \(.read_only // false)\"")" == "$code true" ]] ||
  fail "the orchestrator mounts the stack's setup code read-only at $code"
# The invariant behind it: an app's workbench `environment: !override` drops the base file's keys.
# Every *_FILE key a base app service sets is set again in the workbench (its environment or
# stack.env), or is named here with why the workbench doesn't need it.
not_in_workbench=(
  orchestrator:CHELABASE_METADATA_DATABASE_PASSWORD_FILE # stack.env's URL carries the password
  worker:CHELABASE_METADATA_DATABASE_PASSWORD_FILE       # likewise
  functions:CHELABASE_METADATA_DATABASE_PASSWORD_FILE    # likewise
  orchestrator:CHELABASE_PROXY_CA_FILE                   # a file name, defaulted by the service
  identity:CHELABASE_SMTP_PASSWORD_FILE                  # Mailpit takes no SMTP login
)
for s in $(bq '.services | keys[]'); do
  [[ "$(q ".services[\"$s\"].image // empty")" == chelabase-workbench:fixture000000 || $s == identity ]] || continue
  for k in $(bq ".services[\"$s\"].environment // {} | keys[] | select(endswith(\"_FILE\"))"); do
    [[ " ${not_in_workbench[*]} " == *" $s:$k "* ]] && continue
    [[ -n "$(eff_env "$s" "$k")" ]] ||
      fail "$s: the base sets $k, but the workbench drops it (set it in the overlay, or list it in not_in_workbench with why)"
  done
done
echo "workbench-compose: ok"
