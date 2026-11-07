#!/usr/bin/env bash
# Offline check (object-storage m16, #107): every `CHELABASE_STORAGE_*` and `CHELABASE_WEBHOOKS_*`
# variable the code or the Compose files name appears, in backticks, on the docs' storage settings
# page; and the page names no variable the code doesn't (a stale row).
#
# Sources: the Rust under services/, crates/ and tools/, the Compose files and .env.example.
# Not variables, so not required:
#   CHELABASE_STORAGE_EVENTS   the NATS stream that holds webhook events (the page says so)
#   names ending in `_`        prefixes, such as CHELABASE_STORAGE_S3_
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"
page=apps/docs/content/docs/self-hosting/storage-settings.mdx
fail=0
err() {
  echo "storage-docs-vars: $*" >&2
  fail=1
}
[[ -f "$page" ]] || {
  err "$page is missing"
  exit 1
}

# The whole word (greedy), then prefixes (a trailing `_`) dropped.
pattern='CHELABASE_(STORAGE|WEBHOOKS)_[A-Z0-9_]+'
mapfile -t in_code < <(
  {
    git grep -hoE "$pattern" -- 'services/*.rs' 'crates/*.rs' 'tools/*.rs'
    git grep -hoE "$pattern" -- 'infra/compose/*.yml' 'infra/compose/.env.example'
  } | grep -v '_$' | grep -vx 'CHELABASE_STORAGE_EVENTS' | sort -u
)
((${#in_code[@]} > 10)) || err "found only ${#in_code[@]} variables in the code: the search is broken"

for v in "${in_code[@]}"; do
  grep -qF "\`$v\`" "$page" || err "$v is in the code but not in $page"
done

mapfile -t on_page < <(grep -oE "\`$pattern\`" "$page" | tr -d '`' | sort -u)
for v in "${on_page[@]}"; do
  [[ "$v" == CHELABASE_STORAGE_EVENTS ]] && continue
  printf '%s\n' "${in_code[@]}" | grep -qx "$v" || err "$page lists $v, which no code or Compose file names"
done

((fail == 0)) || exit 1
echo "storage-docs-vars: ok (${#in_code[@]} variables)"
