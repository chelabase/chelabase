#!/usr/bin/env bash
# W2 spec section 5: Rust live tests reach their stack by service name (they run in the stack's
# runner), and every test that dials the proxy by its domain can be pointed at the proxy's address.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
mapfile -t files < <(git ls-files 'services/*/tests/*.rs' 'services/*/tests/**/*.rs' \
  'crates/*/tests/*.rs' 'crates/*/tests/**/*.rs' 'tools/*/tests/*.rs' 'tools/*/tests/**/*.rs' \
  services/rest/src/conn.rs)
bad=0
for pat in '127\.0\.0\.1:5433' '127\.0\.0\.1:1234' '127\.0\.0\.1:6379' '127\.0\.0\.1:8081' \
  '127\.0\.0\.1:8083' '127\.0\.0\.1:4222' '127\.0\.0\.1:7676' 'host\.docker\.internal' \
  'just dev\b' 'just live\b' 'just deps-up' 'internal-addr\.sh'; do
  if hits="$(grep -nE "$pat" "${files[@]}")"; then
    echo "stack-urls: $pat" >&2
    echo "$hits" >&2
    bad=1
  fi
done
while IFS= read -r f; do
  grep -q 'CHELABASE_PROXY_HOSTADDR' "$f" ||
    { echo "stack-urls: $f reads CHELABASE_PROXY_DOMAIN but not CHELABASE_PROXY_HOSTADDR" >&2; bad=1; }
done < <(grep -lE 'CHELABASE_PROXY_DOMAIN' "${files[@]}" || true)
grep -qE 'metadata-db:5432' "${files[@]}" ||
  { echo "stack-urls: no live test falls back to metadata-db:5432" >&2; bad=1; }

# T2 review (second finding in the area, so an invariant): a live test depends on nothing that
# only exists on the host. Every runtime path, address and container name comes from the stack's
# env (with a fallback), because the tests run in the stack's runner: no cargo, no dev pids, no
# fixed project, and `0.0.0.0` bind addresses are never dialled.
mapfile -t tests < <(printf '%s\n' "${files[@]}" | grep -v '^services/rest/src/conn\.rs$')
# Loopback that is a test's own fixture, not a host-only dependency: realtime's c4_* tests run a
# fake compute in the test process and dial it, and the orchestrator's route tests assert the
# fake locator's address in a response body.
# The exemption covers only the `"127.0.0.1"` rule, and the c4_* files only while none of them is
# a live test (no `#[ignore`): a live test dialling loopback is still caught.
in_process='^(services/realtime/tests/c4_[a-z_]*\.rs:[0-9]+:|services/orchestrator/tests/internal\.rs:[0-9]+:.*body\["host"\], "127\.0\.0\.1")'
if grep -nE '#\[ignore' services/realtime/tests/c4_*.rs >&2; then
  echo "stack-urls: a c4_* test is live, so its loopback lines are no longer exempt" >&2
  bad=1
fi
for pat in '\.chelabase/dev' 'cargo run' '"chelabase-[a-z0-9-]+-1"' 'CHELABASE_INTERNAL_ADDR' \
  '"127\.0\.0\.1"' '5433'; do
  exempt='^$'
  [[ "$pat" == '"127\.0\.0\.1"' ]] && exempt="$in_process"
  if hits="$(grep -nE "$pat" "${tests[@]}" | grep -vE "$exempt")"; then
    echo "stack-urls: host-only state ($pat)" >&2
    echo "$hits" >&2
    bad=1
  fi
done
needs() { # needs <pattern> <env var regex> <what>: every file matching the pattern also reads the var
  while IFS= read -r f; do
    grep -qE "$2" "$f" || { echo "stack-urls: $f $3 without $2" >&2; bad=1; }
  done < <(grep -lE "$1" "${tests[@]}" || true)
}
needs 'join\("\.chelabase/neon/proxy-ca\.crt"\)' CHELABASE_PROXY_CA_FILE "reads the proxy CA"
needs 'target/debug|fn chelabase_cli' CHELABASE_CLI "finds the chelabase CLI"
# The shared helpers in services/worker/tests/common/mod.rs read CHELABASE_COMPOSE_PROJECT
# (`compose_project`, and the container names built from it), so a file that gets its container
# names from them honours the rule without naming the variable itself.
needs '"docker"' 'CHELABASE_COMPOSE_PROJECT|compose_project\(|pageserver_container\(|storage_broker_container\(' "runs docker on a stack's containers"
needs 'fn internal_addr|INTERNAL_PORT' CHELABASE_INTERNAL_URL "dials the internal listener"
grep -q 'docker' services/worker/tests/roles_live.rs && grep -q -- '--force-recreate' services/worker/tests/roles_live.rs ||
  { echo "stack-urls: roles_live restarts the orchestrator by recreating its container (compose --force-recreate)" >&2; bad=1; }

# The runner can recreate the orchestrator with a test's settings: Compose is in the image, and
# the two settings the restart tests change pass through from the caller's env (unset otherwise).
grep -q 'docker-compose-plugin' infra/workbench/Dockerfile ||
  { echo "stack-urls: the runner image has the Compose plugin" >&2; bad=1; }
fixture=scripts/tests/fixtures/workbench-stack.env
render() {
  docker compose -p chelabase-feat-x -f infra/compose/docker-compose.yml -f infra/compose/docker-compose.workbench.yml \
    --env-file "$fixture" config --format json
}
for knob in CHELABASE_PASSWORD_STRENGTH CHELABASE_BRANCH_DATABASE_CAP; do
  [[ "$(env "$knob=on" bash -c "$(declare -f render); fixture=$fixture; render" | jq -r ".services.orchestrator.environment.$knob // empty")" == on ]] ||
    { echo "stack-urls: the orchestrator takes $knob from the caller's env" >&2; bad=1; }
  [[ -z "$(env -u "$knob" bash -c "$(declare -f render); fixture=$fixture; render" | jq -r ".services.orchestrator.environment.$knob // empty")" ]] ||
    { echo "stack-urls: $knob is unset on the orchestrator by default" >&2; bad=1; }
done
[[ $bad -eq 0 ]] && echo "stack-urls: ok"
exit $bad
