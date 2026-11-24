#!/usr/bin/env bash
# Smoke checks on the built release images (run `just images` first): each binary's --help, the
# binaries per image, a non-root user, no checkout/.chelabase/target, and Studio's SPA fallback.
# Not part of `just check`: it needs the images, which take a cargo build.
set -euo pipefail

TAG="${TAG:-latest}"
REPO="${REPO:-chelabase}"
apps=(gateway orchestrator worker rest auth realtime storage init)
bin_of() { [[ "$1" == init ]] && echo chelabase-init || echo "$1"; }
fail=0
bad() { echo "FAIL: $*" >&2; fail=1; }
ok() { echo "ok: $*"; }

studio_ctr=""
cleanup() { [[ -n "${studio_ctr}" ]] && docker rm -f "${studio_ctr}" >/dev/null 2>&1 || true; }
trap cleanup EXIT

run() { docker run --rm --entrypoint "" "$@"; }

for app in "${apps[@]}"; do
  img="${REPO}/${app}:${TAG}"
  bin="$(bin_of "${app}")"
  docker image inspect "${img}" >/dev/null 2>&1 || { bad "${img} isn't built"; continue; }
  # AC1: <app> --help exits 0 (the default entrypoint is empty, so the command is run as given).
  if docker run --rm "${img}" "${bin}" --help >/dev/null 2>&1; then ok "${img}: ${bin} --help"; else bad "${img}: ${bin} --help exits non-zero"; fi
  # AC2: only its own binary.
  got="$(run "${img}" sh -c 'ls /usr/local/bin' | tr '\n' ' ')"
  [[ "${got}" == "${bin} " ]] && ok "${img}: binaries '${got}'" || bad "${img}: /usr/local/bin holds '${got}', want '${bin}'"
done

# Object storage (#152 AC 5): Compose runs it with `read_only: true` as the image's user, 10001.
img="${REPO}/storage:${TAG}"
if docker image inspect "${img}" >/dev/null 2>&1; then
  user="$(docker image inspect -f '{{.Config.User}}' "${img}")"
  [[ "${user}" == 10001 || "${user}" == 10001:* ]] && ok "${img}: user ${user}" || bad "${img}: user '${user}', want 10001"
  if docker run --rm --read-only "${img}" storage --help >/dev/null 2>&1; then ok "${img}: storage --help on a read-only root"; else bad "${img}: storage --help fails on a read-only root"; fi
fi

all="${REPO}/server:${TAG}"
if docker image inspect "${all}" >/dev/null 2>&1; then
  want="auth chelabase-init gateway orchestrator realtime rest storage worker "
  got="$(run "${all}" sh -c 'ls /usr/local/bin' | sort | tr '\n' ' ')"
  [[ "${got}" == "${want}" ]] && ok "${all}: all eight binaries" || bad "${all}: /usr/local/bin holds '${got}', want '${want}'"
  for app in "${apps[@]}"; do
    bin="$(bin_of "${app}")"
    docker run --rm "${all}" "${bin}" --help >/dev/null 2>&1 || bad "${all}: ${bin} --help exits non-zero"
  done
else
  bad "${all} isn't built"
fi

# AC2 (CLI absent), AC4: non-root, and nothing from the checkout.
imgs=("${REPO}/studio:${TAG}" "${all}")
for app in "${apps[@]}"; do imgs+=("${REPO}/${app}:${TAG}"); done
for img in "${imgs[@]}"; do
  docker image inspect "${img}" >/dev/null 2>&1 || continue
  uid="$(docker run --rm --entrypoint id "${img}" -u)"
  [[ "${uid}" != 0 ]] && ok "${img}: runs as uid ${uid}" || bad "${img}: runs as root"
  out="$(run "${img}" sh -c 'for p in /usr/local/bin/chelabase /.chelabase /src /target /workspace /app/.chelabase; do [ -e "$p" ] && echo "$p"; done; find / -xdev \( -name .chelabase -o -name target -o -name Cargo.toml \) -not -path "/proc/*" 2>/dev/null | head -3; true')"
  [[ -z "${out}" ]] && ok "${img}: no checkout, .chelabase or target" || bad "${img}: found ${out//$'\n'/ }"
done

# AC3: Studio's index.html on / and a deep link; hashed assets cached long.
studio="${REPO}/studio:${TAG}"
if docker image inspect "${studio}" >/dev/null 2>&1; then
  # #306: without the API host the entry point refuses to render the CSP, so the container exits.
  if out="$(docker run --rm "${studio}" 2>&1)"; then
    bad "studio: started without CHELABASE_API_HOST"
  else
    [[ "${out}" == *"CHELABASE_API_HOST is required"* ]] && ok "studio: refuses to start without CHELABASE_API_HOST" || bad "studio: no API host: ${out}"
  fi
  studio_ctr="images-smoke-studio-$$"
  docker run -d --name "${studio_ctr}" -e CHELABASE_PUBLIC_SCHEME=http -e CHELABASE_API_HOST=api.smoke.test:8000 "${studio}" >/dev/null
  ip="$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "${studio_ctr}")"
  fetch() { curl -fsS --retry 20 --retry-delay 1 --retry-connrefused "$@"; }
  # The shell carries a fresh CSP nonce per response; compare it with the nonce masked.
  mask() { sed -E 's/nonce="[^"]*"/nonce=""/'; }
  root_resp="$(fetch -D - "http://${ip}:3000/" | tr -d '\r')"
  root="$(mask <<<"${root_resp}" | sed '1,/^$/d')"
  deep="$(fetch "http://${ip}:3000/app/members" | mask)"
  [[ "${root}" == *'<div id="root"'* || "${root}" == *"<!doctype html>"* || "${root}" == *"<!DOCTYPE html>"* ]] && ok "studio: GET /" || bad "studio: GET / isn't index.html"
  [[ "${deep}" == "${root}" ]] && ok "studio: /app/members is index.html" || bad "studio: /app/members differs from index.html"
  csp="$(awk 'tolower($1)=="content-security-policy:"{ $1=""; print substr($0, 2) }' <<<"${root_resp}")"
  [[ "${csp}" == *"connect-src 'self' http://api.smoke.test:8000 ws://api.smoke.test:8000;"* ]] && ok "studio: connect-src names the API origin only" || bad "studio: connect-src in '${csp}'"
  [[ "${csp}" != *unsafe-inline* && "${csp}" != *" 'unsafe-eval'"* ]] && ok "studio: no 'unsafe-inline' or 'unsafe-eval'" || bad "studio: unsafe source in '${csp}'"
  header_nonce="$(grep -o "'nonce-[^']*'" <<<"${csp}" | head -1 | sed -E "s/'nonce-(.*)'/\1/")"
  [[ -n "${header_nonce}" && "${root_resp}" == *"<meta property=\"csp-nonce\" nonce=\"${header_nonce}\""* ]] && ok "studio: the shell carries the header's nonce" || bad "studio: header nonce '${header_nonce}' isn't the shell's"
  asset="$(grep -o '/assets/[^"]*\.js' <<<"${root}" | head -1)"
  if [[ -n "${asset}" ]]; then
    cc="$(fetch -D - -o /dev/null "http://${ip}:3000${asset}" | tr -d '\r' | awk 'tolower($1)=="cache-control:"{ $1=""; print }')"
    [[ "${cc}" == *immutable* && "${cc}" == *max-age=31536000* ]] && ok "studio: ${asset} cache-control${cc}" || bad "studio: ${asset} cache-control '${cc}'"
  else
    bad "studio: index.html names no /assets/*.js"
  fi
else
  bad "${studio} isn't built"
fi

[[ "${fail}" -eq 0 ]] && echo "images-smoke: ok"
exit "${fail}"
