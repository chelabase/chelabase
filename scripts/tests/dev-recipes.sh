#!/usr/bin/env bash
# Offline checks of the workbench recipes and scripts (W1 end-of-step review): URLs come from
# scripts/lib/entry.sh (one source), and nothing reaches the gateway after it was stopped.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

fail=0
complain() { echo "dev-recipes: $*" >&2; fail=1; }

# The body of a justfile recipe (its indented lines).
recipe() { awk -v r="$1" '$0 ~ "^"r"( |:)" {f=1; next} f && /^[^ \t#]/ {f=0} f' justfile; }

# _cleanup empties Mailpit before it stops the apps (the gateway), and tolerates a stack that
# isn't up.
cleanup="$(recipe _cleanup)"
mail_line="$(grep -n 'CHELABASE_ENTRY_MAIL_URL' <<<"$cleanup" | head -n1 | cut -d: -f1)"
down_line="$(grep -nE 'dev-down|kill' <<<"$cleanup" | head -n1 | cut -d: -f1)"
if [[ -z "$mail_line" || -z "$down_line" || "$mail_line" -gt "$down_line" ]]; then
  complain "_cleanup must empty Mailpit (CHELABASE_ENTRY_MAIL_URL) before stopping the apps"
fi
if [[ -n "$mail_line" ]] && ! sed -n "${mail_line}p" <<<"$cleanup" | grep -q '||'; then
  complain "_cleanup's Mailpit call must tolerate a stack that isn't up (|| …)"
fi

# Recipes and scripts don't spell out entry hosts: they use CHELABASE_ENTRY_*_URL.
for r in _live-swap-dev _cleanup create-db-demo dev; do
  if recipe "$r" | grep -q 'chelabase\.localhost'; then
    complain "recipe $r spells out an entry host; use CHELABASE_ENTRY_*_URL"
  fi
done
for f in scripts/*.sh scripts/lib/neon-common.sh; do
  if grep -q 'chelabase\.localhost' "$f"; then complain "$f spells out an entry host"; fi
done

# The issuer comes from the entry, not from a demo.env copy that can be stale.
for f in scripts/auth-e2e.sh scripts/members-e2e.sh tools/cli/src/dev/token.rs; do
  if grep -q 'CHELABASE_ZITADEL_ISSUER' "$f"; then complain "$f reads CHELABASE_ZITADEL_ISSUER; use the entry"; fi
done

# The Vite configs don't carry their own copy of the workbench hosts.
for f in apps/studio/vite.config.ts apps/docs/vite.config.ts apps/landing/vite.config.ts; do
  if grep -q 'chelabase\.localhost' "$f"; then complain "$f has its own copy of a workbench host"; fi
done

[[ $fail -eq 0 ]] && echo "dev-recipes: ok"
exit $fail
