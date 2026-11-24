#!/bin/sh
# Studio image entry point (POSIX sh: the Caddy image has no bash). Studio's Content-Security-Policy
# names the API origin in connect-src and media-src (#306), so this derives it from the public
# entry settings the stack already has, then runs the command (Caddy):
#
#   CHELABASE_STUDIO_API_ORIGIN     <scheme>://<API host>        (http://api.example.com:8000)
#   CHELABASE_STUDIO_API_WS_ORIGIN  ws(s)://<API host>           (Studio's realtime socket)
#
# The Caddyfile substitutes both when it is parsed ({$NAME}). The rules are Rust's `Entry`
# (crates/chelabase-types/src/entry.rs): a blank scheme is https, only an explicit http is http; the
# host is trimmed and lowercased, one trailing dot dropped from the name, the port kept, and a
# port that is the scheme's default refused (the gateway refuses it too). A host that isn't a
# hostname, IPv4 address or bracketed IPv6 address, with an optional port, stops the container:
# the policy is never rendered from a value it can't vouch for.
set -eu

fail() {
  echo "studio: $*" >&2
  exit 1
}

trim() { printf '%s' "$1" | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//'; }
lower() { printf '%s' "$1" | tr '[:upper:]' '[:lower:]'; }

scheme="$(lower "$(trim "${CHELABASE_PUBLIC_SCHEME:-}")")"
case "$scheme" in
  "" | https) scheme=https ws=wss default_port=443 ;;
  http) ws=ws default_port=80 ;;
  *) fail "CHELABASE_PUBLIC_SCHEME \"${CHELABASE_PUBLIC_SCHEME:-}\" must be \"http\" or \"https\"" ;;
esac

raw="$(trim "${CHELABASE_API_HOST:-}")"
[ -n "$raw" ] || fail "CHELABASE_API_HOST is required (the API's public host, optionally with :port): Studio's CSP allows connections to it and nowhere else"
host="$(lower "$raw")"

# Split off the port: after the closing bracket of an IPv6 address, else after the last colon.
case "$host" in
  \[*)
    name="${host%%]*}]"
    rest="${host#"$name"}"
    ;;
  *:*)
    name="${host%:*}"
    rest=":${host##*:}"
    ;;
  *)
    name="$host"
    rest=""
    ;;
esac
port="${rest#:}"
[ -z "$rest" ] || [ "$rest" = ":$port" ] || fail "CHELABASE_API_HOST \"$raw\" is not a valid host"

if [ -n "$rest" ]; then
  printf '%s' "$port" | grep -Eq '^[1-9][0-9]{0,4}$' && [ "$port" -le 65535 ] ||
    fail "CHELABASE_API_HOST \"$raw\" has an invalid port"
  [ "$port" -ne "$default_port" ] ||
    fail "CHELABASE_API_HOST \"$raw\" carries port $port, the default for $scheme; drop \":$port\""
fi

label='[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?'
case "$name" in
  \[*)
    printf '%s' "$name" | grep -Eq '^\[[0-9a-f:.]*:[0-9a-f:.]*\]$' ||
      fail "CHELABASE_API_HOST \"$raw\" is not a valid [IPv6]:port"
    ;;
  *)
    name="${name%.}"
    printf '%s' "$name" | grep -Eq "^$label(\\.$label)*\$" ||
      fail "CHELABASE_API_HOST \"$raw\" is not a valid hostname or IPv4 address"
    # An all-digit last label is only valid as part of a dotted-quad IPv4 address.
    case "${name##*.}" in
      *[!0-9]*) ;;
      *)
        printf '%s' "$name" | grep -Eq '^(25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])(\.(25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])){3}$' ||
          fail "CHELABASE_API_HOST \"$raw\" is not a valid hostname or IPv4 address"
        ;;
    esac
    ;;
esac

authority="$name${port:+:$port}"
CHELABASE_STUDIO_API_ORIGIN="$scheme://$authority"
CHELABASE_STUDIO_API_WS_ORIGIN="$ws://$authority"
export CHELABASE_STUDIO_API_ORIGIN CHELABASE_STUDIO_API_WS_ORIGIN

exec "$@"
