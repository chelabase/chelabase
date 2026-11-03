#!/usr/bin/env bash
# W1 guard (spec §4, S2): nothing but the gateway addresses Studio, Mailpit, docs or
# landing by their loopback ports; everything else uses the entry hosts (CHELABASE_*_HOST).
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
pattern='(localhost|127\.0\.0\.1|\[::1\]):(3000|3001|3002|8025)([^0-9]|$)'
allowed=(
  ':!crates/chelabase-types/src/entry.rs' # the loopback upstream defaults (one place)
  ':!apps/docs/content/docs/self-hosting/hosts.mdx'  # documents those defaults
  ':!infra/compose/docker-compose.yml'   # loopback-bound ports the host gateway reaches (W2 unpublishes them)
  ':!infra/compose/docker-compose.workbench.yml' # the sites' healthchecks, inside their own containers
  ':!docs/superpowers/**'                # specs, plans and notes: history
  ':!docs/roadmap/**'
  ':!docs/superpowers/landing-highlights.md'
  ':!scripts/tests/entry-urls.sh'
)
if hits=$(git grep -nE "$pattern" -- . "${allowed[@]}"); then
  echo "entry-urls: use the entry hosts (CHELABASE_*_HOST), not loopback ports:" >&2
  echo "$hits" >&2
  exit 1
fi
echo "entry-urls: ok"
