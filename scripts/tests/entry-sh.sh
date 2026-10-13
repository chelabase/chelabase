#!/usr/bin/env bash
# Offline tests for scripts/lib/entry.sh (W1 T5 review).
#
# Invariant: the canonical entry settings are CHELABASE_PUBLIC_SCHEME and the
# CHELABASE_*_HOST settings (plus CHELABASE_GATEWAY_LISTEN). Everything else entry.sh exports
# (CHELABASE_AUTH_HOST_NAME, CHELABASE_PUBLIC_PORT, CHELABASE_PUBLIC_SECURE and the
# CHELABASE_ENTRY_*_URL) is derived from them, never configured, so no two values can disagree;
# infra/compose/.env.example therefore holds only canonical settings.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

fail=0
check() { # name expected actual
  if [[ "$2" != "$3" ]]; then
    echo "entry-sh: $1: expected [$2], got [$3]" >&2
    fail=1
  fi
}

# Source entry.sh in a clean environment with the given settings; print the derived values.
derive() {
  env -i PATH="$PATH" HOME="$HOME" "$@" bash -c '
    set -u
    . scripts/lib/entry.sh
    printf "%s|%s|%s|%s|%s|%s\n" "$CHELABASE_AUTH_HOST_NAME" "$CHELABASE_PUBLIC_PORT" \
      "$CHELABASE_PUBLIC_SECURE" "$CHELABASE_ENTRY_STUDIO_URL" "$CHELABASE_ENTRY_API_URL" "$CHELABASE_ENTRY_AUTH_URL"'
}

# Exhaustive over scheme × auth host form (with a port, without one).
for scheme in http https; do
  for auth in auth.example.com auth.example.com:8443; do
    if [[ "$auth" == *:* ]]; then port="${auth##*:}"; elif [[ "$scheme" == https ]]; then port=443; else port=80; fi
    secure=false; [[ "$scheme" == https ]] && secure=true
    want="auth.example.com|$port|$secure|$scheme://studio.example.com|$scheme://api.example.com|$scheme://$auth"
    got="$(derive CHELABASE_PUBLIC_SCHEME="$scheme" CHELABASE_STUDIO_HOST=studio.example.com \
      CHELABASE_API_HOST=api.example.com CHELABASE_AUTH_HOST="$auth")"
    check "$scheme $auth" "$want" "$got"
  done
done

# The workbench defaults come from .env.example alone.
check "workbench defaults" \
  "auth.chelabase.localhost|8000|false|http://studio.chelabase.localhost:8000|http://api.chelabase.localhost:8000|http://auth.chelabase.localhost:8000" \
  "$(derive)"

# A derived value can't be configured: a stale one in the environment is recomputed.
check "stale derived values are ignored" \
  "auth.example.com|443|true|https://studio.example.com|https://api.example.com|https://auth.example.com" \
  "$(derive CHELABASE_PUBLIC_SCHEME=https CHELABASE_STUDIO_HOST=studio.example.com CHELABASE_API_HOST=api.example.com \
    CHELABASE_AUTH_HOST=auth.example.com CHELABASE_AUTH_HOST_NAME=old CHELABASE_PUBLIC_PORT=1 CHELABASE_PUBLIC_SECURE=false)"

# .env.example holds only canonical settings: no derived names, no hand-written entry URLs.
for name in CHELABASE_AUTH_HOST_NAME CHELABASE_PUBLIC_PORT CHELABASE_PUBLIC_SECURE VITE_ZITADEL_ISSUER VITE_API_URL; do
  if grep -qE "^${name}=" infra/compose/.env.example; then
    echo "entry-sh: infra/compose/.env.example configures derived ${name}" >&2
    fail=1
  fi
done

# Compose gets the derived values too: the recipes that start Zitadel load the stack's env
# (stack.env carries the entry values) before they run.
for recipe in zitadel-reset zitadel-bootstrap; do
  body="$(awk -v r="^${recipe}[ :]" '$0 ~ r {f=1;next} f&&/^[^ \t#]/{f=0} f' justfile)"
  if ! grep -qE '\. "\$\(.*workbench env' <<<"$body"; then
    echo "entry-sh: the $recipe recipe doesn't load the stack's env (workbench env)" >&2
    fail=1
  fi
done


# --- End-of-step review: entry.sh derives what Entry derives -------------------------------
# Rust's Entry normalizes hosts (lowercase, no trailing dot), treats a blank scheme as https,
# and keeps IPv6 brackets; Zitadel's TLS mode follows the scheme (external for https, else
# disabled), so its issuer's scheme matches the services'.
derive2() {
  env -i PATH="$PATH" HOME="$HOME" "$@" bash -c '
    set -u
    . scripts/lib/entry.sh
    printf "%s|%s|%s|%s|%s\n" "$CHELABASE_PUBLIC_SCHEME" "$CHELABASE_AUTH_HOST_NAME" "$CHELABASE_PUBLIC_PORT" \
      "$CHELABASE_ZITADEL_TLS_MODE" "$CHELABASE_ENTRY_AUTH_URL"'
}
base=(CHELABASE_STUDIO_HOST=studio.example.com CHELABASE_API_HOST=api.example.com)
check "blank scheme is https" "https|auth.example.com|443|external|https://auth.example.com" \
  "$(derive2 "${base[@]}" CHELABASE_PUBLIC_SCHEME= CHELABASE_AUTH_HOST=auth.example.com)"
check "hosts are normalized" "https|auth.example.com|8443|external|https://auth.example.com:8443" \
  "$(derive2 "${base[@]}" CHELABASE_PUBLIC_SCHEME=HTTPS CHELABASE_AUTH_HOST=Auth.Example.COM.:8443)"
check "http means TLS disabled" "http|auth.example.com|80|disabled|http://auth.example.com" \
  "$(derive2 "${base[@]}" CHELABASE_PUBLIC_SCHEME=http CHELABASE_AUTH_HOST=auth.example.com)"
check "bracketed IPv6 with a port" "http|[::1]|8443|disabled|http://[::1]:8443" \
  "$(derive2 "${base[@]}" CHELABASE_PUBLIC_SCHEME=http CHELABASE_AUTH_HOST=[::1]:8443)"
check "bracketed IPv6 without a port" "https|[::1]|443|external|https://[::1]" \
  "$(derive2 "${base[@]}" CHELABASE_PUBLIC_SCHEME=https CHELABASE_AUTH_HOST=[::1])"

# The landing page is a site like the others (spec §1; user, 2026-10-13).
check "landing default" "landing.chelabase.localhost:8000" \
  "$(env -i PATH="$PATH" HOME="$HOME" bash -c '. scripts/lib/entry.sh; printf %s "$CHELABASE_LANDING_HOST"')"

# Compose takes the entry values from entry.sh only: no second copy as defaults.
for name in CHELABASE_AUTH_HOST_NAME CHELABASE_PUBLIC_PORT CHELABASE_PUBLIC_SECURE CHELABASE_PUBLIC_SCHEME \
  CHELABASE_STUDIO_HOST CHELABASE_ZITADEL_TLS_MODE; do
  if grep -qE "\\$\\{${name}:-" infra/compose/docker-compose.yml; then
    echo "entry-sh: docker-compose.yml defaults ${name} (a second copy; require it instead)" >&2
    fail=1
  fi
done
if ! grep -qE -- '--tlsMode \$\{CHELABASE_ZITADEL_TLS_MODE' infra/compose/docker-compose.yml; then
  echo "entry-sh: Zitadel's --tlsMode doesn't follow CHELABASE_ZITADEL_TLS_MODE" >&2
  fail=1
fi
# Every script that starts Zitadel sources entry.sh.
if ! grep -q "scripts/lib/entry.sh\|lib/entry.sh" scripts/zitadel-reset.sh; then
  echo "entry-sh: scripts/zitadel-reset.sh starts Zitadel without sourcing entry.sh" >&2
  fail=1
fi

[[ $fail -eq 0 ]] && echo "entry-sh: ok"
exit $fail
