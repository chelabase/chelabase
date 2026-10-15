#!/usr/bin/env bash
# Offline check of the workbench Compose shape (W2 spec section 2): one stack's rendered config,
# from a fixture stack.env (stack feat-x, gateway 8137, proxy 4537).
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
fixture="$root/scripts/tests/fixtures/workbench-stack.env"
fail() {
  echo "workbench-compose: $*" >&2
  exit 1
}
config="$(env -u NEON_IMAGE -u COMPUTE_NODE_IMAGE docker compose -p chelabase-feat-x \
  -f "$root/infra/compose/docker-compose.yml" --env-file "$fixture" \
  --profile sites --profile runner config --no-env-resolution --format json)" || fail "compose config failed"
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
  q '.services.gateway.networks.default.aliases[]?' | grep -qx "$name.feat-x.chelabase.localhost" ||
    fail "the gateway answers $name.feat-x.chelabase.localhost inside the network"
done
[[ "$(q '.services.gateway.environment.CHELABASE_GATEWAY_LISTEN // empty')" == "" ||
  "$(q '.services.gateway.environment.CHELABASE_GATEWAY_LISTEN')" == "0.0.0.0:8137" ]] ||
  fail "the gateway listens on its published port inside"

q '.services["storage-controller"].command[]' | grep -qx -- '--control-plane-url=http://orchestrator:8083/storcon' ||
  fail "storcon's hook calls the orchestrator by service name"
q '.services.proxy.command[]' | grep -qx -- '--auth-endpoint=http://orchestrator:8083/proxy/api/v1' ||
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
  q ".services[\"$s\"].volumes[] | select(.target == \"/var/run/docker.sock\") | .source" | grep -qx /var/run/docker.sock ||
    fail "$s gets the Docker socket"
  q ".services[\"$s\"].group_add[]?" | grep -qx 973 || fail "$s joins the docker group"
done
for s in gateway auth rest studio zitadel mailpit metadata-db; do
  [[ -z "$(q ".services[\"$s\"].volumes[]? | select(.source == \"/var/run/docker.sock\") | .source")" ]] ||
    fail "$s must not get the Docker socket"
done

for s in orchestrator worker gateway auth rest runner; do
  # Which key files each service sees: compose-key-visibility.sh (U10).
  [[ "$(q ".services[\"$s\"].volumes[] | select(.source == \"/m/.chelabase/stacks/feat-x\") | \"\(.target) \(.read_only // false)\"")" == "/m/.chelabase/stacks/feat-x false" ]] ||
    fail "$s mounts the state dir read-write at its own path"
done

[[ "$(q '.services.zitadel.volumes[] | select(.target == "/zitadel-bootstrap") | .source')" == /m/.chelabase/stacks/feat-x/zitadel ]] ||
  fail "zitadel's PATs go to the stack's state dir"
[[ "$(q '.services.zitadel.environment.ZITADEL_EXTERNALDOMAIN')" == auth.feat-x.chelabase.localhost ]] ||
  fail "zitadel's domain is the stack's auth host"
[[ "$(q '.services.zitadel.environment.ZITADEL_EXTERNALPORT')" == 8137 ]] ||
  fail "zitadel's port is the stack's gateway port"
[[ "$(q '.services.zitadel.environment.ZITADEL_OIDC_DEFAULTLOGINURLV2')" == "http://studio.feat-x.chelabase.localhost:8137/login?authRequest=" ]] ||
  fail "zitadel sends sign-ins to the stack's Studio"
for s in zitadel mailpit; do
  [[ -n "$(q ".services[\"$s\"].healthcheck.test // empty")" ]] || fail "$s has a healthcheck"
done
for s in orchestrator worker gateway auth rest; do
  [[ -n "$(q ".services[\"$s\"].healthcheck.test // empty")" ]] || fail "$s has a healthcheck"
done

for s in orchestrator worker gateway auth rest runner; do
  q ".services[\"$s\"].env_file[]? | if type == \"object\" then .path else . end" | grep -qx /m/.chelabase/stacks/feat-x/env/stack.env ||
    fail "$s reads the stack env"
done
q '.services.runner.volumes[] | select(.target == "/usr/local/bin/cargo-nextest") | .source' | grep -qx /usr/local/bin/cargo-nextest ||
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
[[ "$(q '.services.orchestrator.networks | keys | join(" ")')" == "control default" ]] || fail "the orchestrator is on both networks"
[[ "$(q '.services.worker.networks | keys | join(" ")')" == "default" ]] || fail "the worker isn't on the control network"
ca_mounts="$(q '[.services | to_entries[] | select(any(.value.volumes[]?; .source == "/m/.chelabase/compute-ca")) | "\(.key):\(any(.value.volumes[]; .source == "/m/.chelabase/compute-ca" and .read_only == true))"] | sort | join(" ")')"
[[ "$ca_mounts" == "runner:true worker:false" ]] || fail "compute-ca: rw for the worker, ro for the runner (R-T9a), nobody else; got: $ca_mounts"
tls_mounts="$(q '[.services | to_entries[] | select(any(.value.volumes[]?; .source == "/m/.chelabase/orchestrator-tls")) | "\(.key):\(any(.value.volumes[]; .source == "/m/.chelabase/orchestrator-tls" and .read_only == true))"] | sort | join(" ")')"
[[ "$tls_mounts" == "orchestrator:true runner:true worker:false" ]] || fail "orchestrator-tls: rw for the worker, ro for the orchestrator and runner; got: $tls_mounts"
echo "workbench-compose: ok"
