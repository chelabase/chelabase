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
  '127\.0\.0\.1:4444' 'live\.sh' 'internal-addr\.sh'; do
  if hits="$(grep -nIE "$pat" "${files[@]}")"; then
    echo "retired-names: $pat" >&2
    echo "$hits" >&2
    bad=1
  fi
done
grep -q 'purge --stack main --yes' CONTRIBUTING.md ||
  { echo "retired-names: CONTRIBUTING.md says how main is purged (--stack main --yes)" >&2; bad=1; }
[[ $bad -eq 0 ]] && echo "retired-names: ok"
exit $bad
