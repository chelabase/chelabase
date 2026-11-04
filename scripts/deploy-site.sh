#!/usr/bin/env bash
# Builds one static site and deploys it to Cloudflare as a static-only Worker (no Worker script).
#
#   scripts/deploy-site.sh landing|docs            build and deploy (from a clean checkout on origin/main)
#   scripts/deploy-site.sh landing|docs --dry-run  build and check the upload, deploy nothing (any branch)
#
# Each app's wrangler.jsonc names its Worker and its custom domain. Wrangler runs from pnpm dlx at
# the pinned version below, so it isn't a workspace dependency. A deploy signs in with
# `wrangler login`, or with CLOUDFLARE_API_TOKEN and CLOUDFLARE_ACCOUNT_ID (the workflows).
set -euo pipefail

WRANGLER_VERSION=4.156.0
DOCS_URL=https://docs.chelabase.com

app="${1:-}"
mode="${2:-deploy}"
case "${app}" in
  landing | docs) ;;
  *) echo "usage: scripts/deploy-site.sh landing|docs [--dry-run]" >&2; exit 2 ;;
esac
case "${mode}" in
  deploy | --dry-run) ;;
  *) echo "deploy-site: unknown option ${mode}" >&2; exit 2 ;;
esac

cd "$(git rev-parse --show-toplevel)"
sha="$(git rev-parse --short=12 HEAD)"

# A live deploy ships what main holds: a clean tree at a commit on origin/main, so the site never
# carries unmerged or uncommitted changes.
if [[ "${mode}" == deploy ]]; then
  [[ -z "$(git status --porcelain)" ]] ||
    { echo "deploy-site: refusing to deploy: the working tree has changes; deploy from a clean checkout of main" >&2; exit 1; }
  git fetch -q origin main ||
    { echo "deploy-site: refusing to deploy: can't fetch origin/main to check HEAD is on it" >&2; exit 1; }
  git merge-base --is-ancestor HEAD origin/main ||
    { echo "deploy-site: refusing to deploy: HEAD ${sha} is not on origin/main; merge first, then deploy from main" >&2; exit 1; }
fi

if [[ "${app}" == landing ]]; then
  # Docs links point at the live docs site instead of GitHub.
  VITE_DOCS_URL="${DOCS_URL}" pnpm --filter @chelabase/landing build
else
  pnpm --filter @chelabase/docs build
fi

cd "apps/${app}"
if [[ "${mode}" == --dry-run ]]; then
  pnpm dlx "wrangler@${WRANGLER_VERSION}" deploy --dry-run
else
  pnpm dlx "wrangler@${WRANGLER_VERSION}" deploy --message "${app} at ${sha}"
fi
