#!/usr/bin/env bash
# W2 spec sections 4 and 5: recipes and scripts run against the current checkout's stack through
# `chelabase workbench`; the one-stack machinery is gone.
set -euo pipefail
# `producer | grep -q` under pipefail races: grep -q exits at its first match, the producer can
# get SIGPIPE/EPIPE and fail the pipeline. gq is grep -q that reads all its input (same status).
gq() { grep "$@" >/dev/null; }
cd "$(git rev-parse --show-toplevel)"
fail() {
  echo "workbench-recipes: $*" >&2
  exit 1
}
recipes=" $(just --summary) "
for gone in dev dev-down stop deps-up deps-down deps-ps neon-ps orchestrator worker gateway rest \
  auth studio cleanup _cleanup live live-status live-test _live-check _live-swap-dev \
  stack-reset-slots neon-test-live worker-live docs landing; do
  [[ "$recipes" != *" $gone "* ]] || fail "recipe $gone is retired"
done
[[ "$recipes" == *" workbench "* ]] || fail "there is a workbench recipe"
just --show workbench | gq -- '-p chelabase -- workbench' || fail "just workbench forwards to the CLI"
for e2e in neon-e2e branch-e2e storage-leak-e2e app-users-e2e data-e2e console-e2e sql-editor-e2e \
  merge-e2e role-e2e members-e2e personal-logins-e2e teams-e2e auth-e2e neon-smoke; do
  just --show "$e2e" | gq "workbench run scripts/$e2e.sh" || fail "$e2e runs in the stack's runner"
done
just --show functions-e2e | gq 'workbench test -p chelabase-functions --test e2e -- --run-ignored only' ||
  fail "functions-e2e runs the functions crate's e2e test in the stack's runner"
just --show bench-data-api | gq 'workbench run --env CHELABASE_BENCH_TEST_BIN=' ||
  fail "the benchmark passes its prebuilt test binary"
just --show sweep-dbs | gq 'workbench run scripts/sweep-dbs.sh' || fail "sweep-dbs runs in the runner"
just --show sweep-dbs-test | gq 'workbench run scripts/tests/sweep-dbs.sh' || fail "sweep-dbs-test runs in the runner"
just --show wt-remove | gq 'workbench purge' || fail "wt-remove purges the worktree's stack"
! just --show wt-remove | gq -iE 'sha256|sha256sum|cut -c' || fail "wt-remove lets the CLI resolve the worktree's stack (run from the worktree), never recomputes the slug"
just --show wt-remove | gq -- '--show-toplevel' || fail "wt-remove checks the directory is still that worktree (git toplevel) before it purges (T5 review I4)"
! just --show wt-add | gq '\.env\.local' || fail "wt-add no longer links Studio's env"
just --show wt-add | gq 'workbench slug-of' ||
  fail "wt-add asks the CLI for the base's stack, never recomputes the slug"
just --show wt-add | gq '\.workbench-stack' || fail "wt-add records the base's stack in the task worktree"
grep -qx '/.workbench-stack' .gitignore || fail ".workbench-stack is git-ignored"
# End-of-step I1: a test that recreates the stack's apps interpolates the stack's own checkout
# (CHELABASE_STACK_CHECKOUT), never the task checkout the runner works in.
while IFS= read -r f; do
  grep -q 'CHELABASE_STACK_CHECKOUT' "$f" || fail "$f recreates apps without CHELABASE_STACK_CHECKOUT"
done < <(git grep -l -e '--force-recreate' -- 'services/*/tests/*' 'crates/*/tests/*' 'crates/*/src/*' 'services/*/src/*')
just --show wt-add | gq 'origin/main' || fail "wt-add treats origin/main as a step base"
for r in create-db-demo auth-token; do
  just --show "$r" | gq 'workbench env' || fail "$r loads the stack's env"
done
for f in scripts/live.sh scripts/internal-addr.sh scripts/stack-reset-slots.sh \
  scripts/tests/live.sh scripts/tests/internal-addr.sh scripts/tests/stack-reset-slots.sh \
  scripts/tests/dev-recipes.sh "scripts/zit""adel-reset.sh"; do
  [[ ! -e "$f" ]] || fail "$f is retired"
done
[[ -x scripts/sweep-dbs.sh ]] || fail "scripts/sweep-dbs.sh exists"

mapfile -t scripts < <(git ls-files 'scripts/*.sh' 'scripts/lib/*.sh' ':!scripts/tests/**')
# The self-host e2e scripts test the Compose install on their own throwaway projects, so raw Compose,
# in-container loopback and their own published ports are expected. Only these patterns exempt them.
selfhost_e2e=' scripts/selfhost-e2e.sh scripts/selfhost-tls-e2e.sh '
loopback='127\.0\.0\.1:(5433|1234|6379|8081|8083|4222|7676|9898)'
hostaddr_loopback='hostaddr=127\.0\.0\.1'
for pat in 'compose exec' 'cargo run' 'cargo test' \
  "$loopback" 'host\.docker\.internal' \
  'internal-addr\.sh' 'name=\^chelabase-compute-' '\.chelabase/neon/proxy-ca\.crt' \
  "$hostaddr_loopback" 'just live' 'just dev\b' 'just deps-up'; do
  targets=("${scripts[@]}")
  if [[ "$pat" == 'compose exec' || "$pat" == "$loopback" || "$pat" == "$hostaddr_loopback" ]]; then
    targets=()
    for s in "${scripts[@]}"; do
      [[ "$selfhost_e2e" == *" $s "* ]] || targets+=("$s")
    done
  fi
  hits="$(grep -nE "$pat" "${targets[@]}" || true)"
  [[ -z "$hits" ]] || fail "scripts still use $pat:
$hits"
done
for f in scripts/neon-e2e.sh scripts/branch-e2e.sh; do
  grep -q 'chelabase.dev/stack' "$f" || fail "$f selects computes by the stack's label"
done
grep -q 'CHELABASE_PROXY_HOSTADDR' scripts/neon-e2e.sh || fail "neon-e2e dials the proxy's address"
for f in packages/neon-e2e/check.mjs packages/client/scripts/e2e.mjs; do
  grep -q 'CHELABASE_PROXY_HOSTADDR' "$f" || fail "$f dials the proxy's address"
done
just --show check-scripts | gq 'scripts/tests/workbench-recipes.sh' || fail "just check-scripts runs this file"

# T6 review fix round.
# C1: a curl to the proxy's serverless endpoint (<ep>.localtest.me:4444) must be routed to the
# proxy's address; inside the runner the name resolves to loopback.
for f in $(grep -lE 'PROXY_HTTP_PORT|:4444' "${scripts[@]}" || true); do
  if grep -qE '^\s*[^#]*curl ' "$f"; then
    if [[ "$selfhost_e2e" == *" $f "* ]]; then
      # The self-host e2e scripts have no CHELABASE_PROXY_HOSTADDR: a script that calls the proxy's
      # SQL-over-HTTP URL (http_url) pins the published loopback address; the TLS script's curls go to the gateway.
      if grep -qF 'http_url' "$f"; then
        grep -qF -- '--resolve "$pg_host:$PROXY_HTTP_PORT:127.0.0.1"' "$f" ||
          fail "$f curls the proxy's HTTP endpoint without --resolve \"\$pg_host:\$PROXY_HTTP_PORT:127.0.0.1\""
      fi
    else
      grep -qE -- '--(connect-to|resolve)[ =][^ ]*PROXY_HOSTADDR' "$f" ||
        fail "$f curls the proxy's HTTP endpoint without --connect-to/--resolve to \$CHELABASE_PROXY_HOSTADDR"
    fi
  fi
done
# I2: recipe arguments are passed as arguments, never re-split by the shell.
grep -q '^set positional-arguments' justfile || fail "the justfile sets positional-arguments"
for r in workbench bench-data-api stack-bootstrap; do
  just --show "$r" | gq -E '"\$(@|1)"' || fail "$r passes its arguments quoted (\"\$@\" or \"\$1\")"
  ! just --show "$r" | gq -F '{{' || fail "$r interpolates its arguments into the shell line"
done
# Minor: Studio's env must exist before a script reads the client id from it.
for f in $(grep -lF 'studio.env' "${scripts[@]}" || true); do
  grep -qE -- '-f "\$CHELABASE_STACK_DIR/studio\.env"' "$f" ||
    fail "$f reads studio.env without checking it exists (say: run just workbench up)"
done
# Minor: the benchmark's binary comes from the recipe, not a hard-coded target path.
just --show bench-data-api | gq 'CHELABASE_BENCH_BIN=' || fail "bench-data-api passes CHELABASE_BENCH_BIN"
! grep -q 'target/release/bench-data-api' scripts/bench-data-api.sh || fail "bench-data-api.sh takes its binary from CHELABASE_BENCH_BIN"
# Minor: the client e2e defaults to the stack's API URL.
grep -q 'CHELABASE_ENTRY_API_URL' packages/client/scripts/e2e.mjs || fail "client e2e defaults to CHELABASE_ENTRY_API_URL"

# End-of-step review I1: connection strings the orchestrator hands out name the stack's host
# proxy port (CHELABASE_PROXY_PORT); the runner dials 4432 inside the network
# (CHELABASE_PROXY_PG_PORT). A script that checks a handed-out URI compares with the host port.
grep -q 'CHELABASE_PROXY_PORT' scripts/neon-e2e.sh ||
  fail "neon-e2e checks handed-out URIs against the stack's host proxy port (CHELABASE_PROXY_PORT)"
# m4 (#11): the bootstrap runs `chelabase-init bootstrap` in a container on the stack's compose
# network (the CLI's `dev bootstrap`), so the host needs no route into the stack and the recipe
# only builds the tool and calls the CLI; the CLI loads the stack env.
just --show stack-bootstrap | gq 'cargo build -q -p init' || fail "stack-bootstrap builds chelabase-init first"
just --show stack-bootstrap | gq -- '-p chelabase -- dev bootstrap' ||
  fail "stack-bootstrap runs the CLI's dev bootstrap"
just --show check-scripts | gq 'scripts/tests/retired-names.sh' || fail "just check-scripts runs retired-names.sh"
# W2 live: the worker's live tests share the stack (branch_live restarts its pageserver, roles_live
# recreates its orchestrator, caps count every database), so they run one at a time, as the
# retired worker-live recipe's --test-threads=1 did; a nextest test group holds them to one.
nextest=.config/nextest.toml
[[ -f "$nextest" ]] || fail "$nextest exists (the worker's live tests run one at a time)"
grep -qE '^\[test-groups\.stack-serial\]' "$nextest" || fail "$nextest defines test group stack-serial"
grep -A3 -E '^\[test-groups\.stack-serial\]' "$nextest" | gq -E '^max-threads = 1$' ||
  fail "stack-serial runs one test at a time (max-threads = 1)"
grep -B3 -A3 'test-group = "stack-serial"' "$nextest" | gq -F 'package(worker) & binary(/_live$/)' ||
  fail "every worker live test binary is in stack-serial"
# catalog_live creates cluster-wide roles (the reserved ones on a fresh stack) and reads them all.
grep -B3 -A3 'test-group = "stack-serial"' "$nextest" | gq -F 'package(chelabase-schema) & binary(catalog_live)' ||
  fail "chelabase-schema's catalog_live is in stack-serial"
# RT1 final fix round (T1 audit): a script reads the stack's generated bearer (`<NAME>` or
# `<NAME>_FILE`, secret_value) and stops with a message when it has none; it never falls back to a
# public development bearer (`chelabase-dev-*`), which the stack refuses. compute-network-e2e.sh
# lists them only to prove every port refuses them.
dev_hits="$(grep -nE 'chelabase-dev-[a-z]' "${scripts[@]}" | grep -v '^scripts/compute-network-e2e\.sh:' || true)"
[[ -z "$dev_hits" ]] || fail "scripts fall back to a public development bearer:
$dev_hits"
just --show check-scripts | gq 'scripts/tests/compose-secret-argv.sh' || fail "just check-scripts runs compose-secret-argv.sh"
echo "workbench-recipes: ok"
