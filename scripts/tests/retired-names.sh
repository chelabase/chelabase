#!/usr/bin/env bash
# W2 end-of-step review: nothing outside history, the migration and the guards still names the
# retired one-stack machinery (the host app tier, the live queue, Studio's
# symlinked env file) or a port the workbench no longer publishes.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
mapfile -t files < <(git ls-files ':!docs/superpowers/**' ':!docs/roadmap/**' \
  ':!docs/superpowers/landing-highlights.md' ':!scripts/tests/**' ':!apps/landing/**' ':!.gitignore' ':!.cursor/**' \
  ':!tools/cli/src/workbench/migrate.rs' ':!tools/cli/src/workbench/lifecycle_tests.rs' \
  ':!tools/cli/src/workbench/commands.rs' ':!tools/cli/src/paths.rs' ':!**/*.png' ':!**/*.lock' \
  ':!pnpm-lock.yaml')
bad=0
for pat in 'just (dev|live|deps-up|deps-down|deps-ps|dev-down|stop|worker|orchestrator)\b' \
  '\.chelabase/dev' '\.chelabase/live' 'apps/studio/\.env\.local' \
  '127\.0\.0\.1:4444' 'live\.sh' 'internal-addr\.sh' 'Engine 26\.0'; do
  if hits="$(grep -nIE "$pat" "${files[@]}")"; then
    echo "retired-names: $pat" >&2
    echo "$hits" >&2
    bad=1
  fi
done
# compute-network-reach: the control network's settings were renamed (CHELABASE_CONTROL_INTERNAL_*,
# CHELABASE_*_CONTROL_INTERNAL_IP), and nothing reads the old names, so a file that still sets or
# interpolates one is silently ignored. Only the base release's .env (the production upgrade test)
# and the workbench's env from before the rename may.
old_control='CHELABASE_(CONTROL_SUBNET|CONTROL_COMPUTE_RANGE|(ORCHESTRATOR|REALTIME|RUNNER)_CONTROL_IP)[=:"]'
mapfile -t control_files < <(printf '%s\n' "${files[@]}" | grep -vxE \
  'scripts/upgrade-prod-e2e\.sh|tools/cli/src/workbench/fixtures/stack-env-before-internal-networks\.env')
if hits="$(grep -nIE "$old_control" "${control_files[@]}" scripts/tests/fixtures/*)"; then
  echo "retired-names: $old_control" >&2
  echo "$hits" >&2
  bad=1
fi
grep -q 'purge --stack main --yes' CONTRIBUTING.md ||
  { echo "retired-names: CONTRIBUTING.md says how main is purged (--stack main --yes)" >&2; bad=1; }
[[ $bad -eq 0 ]] && echo "retired-names: ok"
exit $bad
