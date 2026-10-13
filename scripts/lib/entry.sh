# The public entry (W1), for scripts and recipes. Sourced (by bash), not run.
#
# Exports the host settings (CHELABASE_PUBLIC_SCHEME, CHELABASE_{STUDIO,API,AUTH,DOCS,LANDING,MAIL}_HOST,
# CHELABASE_GATEWAY_LISTEN): a value already in the environment wins (even a blank one), otherwise
# the workbench value from infra/compose/.env.example (the one place the defaults live; values there
# are read literally). They are normalized as Rust's `Entry` does: the scheme is lowercased and a
# blank one is https (only an explicit http is http); hosts are lowercased with one trailing dot
# removed from the name, and a bracketed IPv6 address keeps its brackets.
#
# Then derives, never reading them from the environment: CHELABASE_AUTH_HOST_NAME (the auth host
# without its port), CHELABASE_PUBLIC_PORT (its port, else 443 for https / 80 for http),
# CHELABASE_PUBLIC_SECURE (true iff https), CHELABASE_ZITADEL_TLS_MODE (external for https, else
# disabled) and CHELABASE_ENTRY_{STUDIO,API,AUTH,DOCS,LANDING,MAIL}_URL (empty for an optional host
# that isn't set).

_entry_example="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../infra/compose" && pwd)/.env.example"
for _entry_var in CHELABASE_PUBLIC_SCHEME CHELABASE_STUDIO_HOST CHELABASE_API_HOST CHELABASE_AUTH_HOST \
  CHELABASE_DOCS_HOST CHELABASE_LANDING_HOST CHELABASE_MAIL_HOST CHELABASE_GATEWAY_LISTEN; do
  if [[ -z "${!_entry_var+x}" ]]; then
    _entry_line="$(grep -E "^${_entry_var}=" "$_entry_example" | head -n1 || true)"
    [[ -z "$_entry_line" ]] || export "${_entry_var}=${_entry_line#*=}"
  fi
done
unset _entry_var _entry_line _entry_example

# One host setting, normalized: trimmed, lowercased, one trailing dot dropped from the name, the
# port kept. Sets _entry_name (with IPv6 brackets) and _entry_port (empty without one).
_entry_split() {
  local raw="${1,,}"
  raw="${raw#"${raw%%[![:space:]]*}"}"
  raw="${raw%"${raw##*[![:space:]]}"}"
  if [[ "$raw" == \[* ]]; then
    _entry_name="${raw%%]*}]"
    _entry_port="${raw#"$_entry_name"}"
    _entry_port="${_entry_port#:}"
  elif [[ "$raw" == *:* ]]; then
    _entry_name="${raw%:*}"
    _entry_port="${raw##*:}"
  else
    _entry_name="$raw"
    _entry_port=""
  fi
  _entry_name="${_entry_name%.}"
}

_entry_scheme="${CHELABASE_PUBLIC_SCHEME:-}"
_entry_scheme="${_entry_scheme,,}"
_entry_scheme="${_entry_scheme//[[:space:]]/}"
case "$_entry_scheme" in
  "" | https) CHELABASE_PUBLIC_SCHEME=https ;;
  http) CHELABASE_PUBLIC_SCHEME=http ;;
  *)
    echo "entry.sh: CHELABASE_PUBLIC_SCHEME \"${CHELABASE_PUBLIC_SCHEME}\" must be \"http\" or \"https\"" >&2
    unset _entry_scheme
    return 1
    ;;
esac
unset _entry_scheme
export CHELABASE_PUBLIC_SCHEME

for _entry_var in CHELABASE_STUDIO_HOST CHELABASE_API_HOST CHELABASE_AUTH_HOST CHELABASE_DOCS_HOST \
  CHELABASE_LANDING_HOST CHELABASE_MAIL_HOST; do
  if [[ -n "${!_entry_var:-}" ]]; then
    _entry_split "${!_entry_var}"
    export "${_entry_var}=${_entry_name}${_entry_port:+:${_entry_port}}"
  fi
done

_entry_split "${CHELABASE_AUTH_HOST:-}"
CHELABASE_AUTH_HOST_NAME="$_entry_name"
if [[ -n "$_entry_port" ]]; then
  CHELABASE_PUBLIC_PORT="$_entry_port"
elif [[ "$CHELABASE_PUBLIC_SCHEME" == https ]]; then
  CHELABASE_PUBLIC_PORT=443
else
  CHELABASE_PUBLIC_PORT=80
fi
if [[ "$CHELABASE_PUBLIC_SCHEME" == https ]]; then
  CHELABASE_PUBLIC_SECURE=true
  CHELABASE_ZITADEL_TLS_MODE=external
else
  CHELABASE_PUBLIC_SECURE=false
  CHELABASE_ZITADEL_TLS_MODE=disabled
fi
export CHELABASE_AUTH_HOST_NAME CHELABASE_PUBLIC_PORT CHELABASE_PUBLIC_SECURE CHELABASE_ZITADEL_TLS_MODE
unset -f _entry_split
unset _entry_var _entry_name _entry_port

for _entry_site in STUDIO API AUTH DOCS LANDING MAIL; do
  _entry_var="CHELABASE_${_entry_site}_HOST"
  _entry_url=""
  [[ -z "${!_entry_var:-}" ]] || _entry_url="${CHELABASE_PUBLIC_SCHEME}://${!_entry_var}"
  export "CHELABASE_ENTRY_${_entry_site}_URL=${_entry_url}"
done
unset _entry_site _entry_var _entry_url
