#!/usr/bin/env bash
# W2 spec sections 4 and 5: recipes and scripts run against the current checkout's stack through
# `chelabase workbench`; the one-stack machinery is gone.
set -euo pipefail
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
just --show workbench | grep -q -- '-p chelabase -- workbench' || fail "just workbench forwards to the CLI"
for e2e in neon-e2e branch-e2e storage-leak-e2e app-users-e2e data-e2e console-e2e sql-editor-e2e \
  merge-e2e role-e2e members-e2e auth-e2e neon-smoke; do
  just --show "$e2e" | grep -q "workbench run scripts/$e2e.sh" || fail "$e2e runs in the stack's runner"
done
just --show bench-data-api | grep -q 'workbench run --env CHELABASE_BENCH_TEST_BIN=' ||
  fail "the benchmark passes its prebuilt test binary"
just --show sweep-dbs | grep -q 'workbench run scripts/sweep-dbs.sh' || fail "sweep-dbs runs in the runner"
just --show wt-remove | grep -q 'workbench purge' || fail "wt-remove purges the worktree's stack"
! just --show wt-remove | grep -qiE 'sha256|sha256sum|cut -c' || fail "wt-remove lets the CLI resolve the worktree's stack (run from the worktree), never recomputes the slug"
just --show wt-remove | grep -q -- '--show-toplevel' || fail "wt-remove checks the directory is still that worktree (git toplevel) before it purges (T5 review I4)"
! just --show wt-add | grep -q '\.env\.local' || fail "wt-add no longer links Studio's env"
for r in create-db-demo zitadel-reset; do
  just --show "$r" | grep -q 'workbench env' || fail "$r loads the stack's env"
done
for f in scripts/live.sh scripts/internal-addr.sh scripts/stack-reset-slots.sh \
  scripts/tests/live.sh scripts/tests/internal-addr.sh scripts/tests/stack-reset-slots.sh \
  scripts/tests/dev-recipes.sh; do
  [[ ! -e "$f" ]] || fail "$f is retired"
done
[[ -x scripts/sweep-dbs.sh ]] || fail "scripts/sweep-dbs.sh exists"

mapfile -t scripts < <(git ls-files 'scripts/*.sh' 'scripts/lib/*.sh' ':!scripts/tests/**')
for pat in 'compose exec' 'cargo run' 'cargo test' \
  '127\.0\.0\.1:(5433|1234|6379|8081|8083|4222|7676|9898)' 'host\.docker\.internal' \
  'internal-addr\.sh' 'name=\^chelabase-compute-' '\.chelabase/neon/proxy-ca\.crt' \
  '\.chelabase/zitadel' 'hostaddr=127\.0\.0\.1' 'just live' 'just dev\b' 'just deps-up'; do
  hits="$(grep -nE "$pat" "${scripts[@]}" || true)"
  [[ -z "$hits" ]] || fail "scripts still use $pat:
$hits"
done
for f in scripts/zitadel-reset.sh; do
  grep -q -- '-p "\$CHELABASE_COMPOSE_PROJECT"' "$f" || fail "$f acts on the stack's project"
done
for f in scripts/neon-e2e.sh scripts/branch-e2e.sh; do
  grep -q 'chelabase.dev/stack' "$f" || fail "$f selects computes by the stack's label"
done
grep -q 'CHELABASE_PROXY_HOSTADDR' scripts/neon-e2e.sh || fail "neon-e2e dials the proxy's address"
for f in packages/neon-e2e/check.mjs packages/client/scripts/e2e.mjs; do
  grep -q 'CHELABASE_PROXY_HOSTADDR' "$f" || fail "$f dials the proxy's address"
done
just --show check | grep -q 'scripts/tests/workbench-recipes.sh' || fail "just check runs this file"

# T6 review fix round.
# C1: a curl to the proxy's serverless endpoint (<ep>.localtest.me:4444) must be routed to the
# proxy's address; inside the runner the name resolves to loopback.
for f in $(grep -lE 'PROXY_HTTP_PORT|:4444' "${scripts[@]}" || true); do
  if grep -qE '^\s*[^#]*curl ' "$f"; then
    grep -qE -- '--(connect-to|resolve)[ =][^ ]*PROXY_HOSTADDR' "$f" ||
      fail "$f curls the proxy's HTTP endpoint without --connect-to/--resolve to \$CHELABASE_PROXY_HOSTADDR"
  fi
done
# I2: recipe arguments are passed as arguments, never re-split by the shell.
grep -q '^set positional-arguments' justfile || fail "the justfile sets positional-arguments"
for r in workbench bench-data-api zitadel-bootstrap; do
  just --show "$r" | grep -qE '"\$(@|1)"' || fail "$r passes its arguments quoted (\"\$@\" or \"\$1\")"
  ! just --show "$r" | grep -qF '{{' || fail "$r interpolates its arguments into the shell line"
done
# Minor: Studio's env must exist before a script reads the client id from it.
for f in $(grep -lF 'studio.env' "${scripts[@]}" || true); do
  grep -qE -- '-f "\$CHELABASE_STACK_DIR/studio\.env"' "$f" ||
    fail "$f reads studio.env without checking it exists (say: run just workbench up)"
done
# Minor: the benchmark's binary comes from the recipe, not a hard-coded target path.
just --show bench-data-api | grep -q 'CHELABASE_BENCH_BIN=' || fail "bench-data-api passes CHELABASE_BENCH_BIN"
! grep -q 'target/release/bench-data-api' scripts/bench-data-api.sh || fail "bench-data-api.sh takes its binary from CHELABASE_BENCH_BIN"
# Minor: the client e2e defaults to the stack's API URL.
grep -q 'CHELABASE_ENTRY_API_URL' packages/client/scripts/e2e.mjs || fail "client e2e defaults to CHELABASE_ENTRY_API_URL"

# End-of-step review I1: connection strings the orchestrator hands out name the stack's host
# proxy port (CHELABASE_PROXY_PORT); the runner dials 4432 inside the network
# (CHELABASE_PROXY_PG_PORT). A script that checks a handed-out URI compares with the host port.
grep -q 'CHELABASE_PROXY_PORT' scripts/neon-e2e.sh ||
  fail "neon-e2e checks handed-out URIs against the stack's host proxy port (CHELABASE_PROXY_PORT)"
# End-of-step review I2: the host can't resolve `zitadel`; host-side bootstrap goes through the
# stack's gateway, as `up` does (P6).
just --show zitadel-bootstrap | grep -q 'CHELABASE_ZITADEL_UPSTREAM=http://127.0.0.1:' ||
  fail "zitadel-bootstrap reaches Zitadel through the stack's gateway (CHELABASE_ZITADEL_UPSTREAM)"
just --show check | grep -q 'scripts/tests/retired-names.sh' || fail "just check runs retired-names.sh"
# W2 live: the worker's live tests share the stack (branch_live restarts its pageserver, roles_live
# recreates its orchestrator, caps count every database), so they run one at a time, as the
# retired worker-live recipe's --test-threads=1 did; a nextest test group holds them to one.
nextest=.config/nextest.toml
[[ -f "$nextest" ]] || fail "$nextest exists (the worker's live tests run one at a time)"
grep -qE '^\[test-groups\.stack-serial\]' "$nextest" || fail "$nextest defines test group stack-serial"
grep -A3 -E '^\[test-groups\.stack-serial\]' "$nextest" | grep -qE '^max-threads = 1$' ||
  fail "stack-serial runs one test at a time (max-threads = 1)"
grep -B3 -A3 'test-group = "stack-serial"' "$nextest" | grep -qF 'package(worker) & binary(/_live$/)' ||
  fail "every worker live test binary is in stack-serial"
# catalog_live creates cluster-wide roles (the reserved ones on a fresh stack) and reads them all.
grep -B3 -A3 'test-group = "stack-serial"' "$nextest" | grep -qF 'package(chelabase-schema) & binary(catalog_live)' ||
  fail "chelabase-schema's catalog_live is in stack-serial"
echo "workbench-recipes: ok"
