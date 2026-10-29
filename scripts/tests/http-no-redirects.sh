#!/usr/bin/env bash
# Offline invariant (#26): no HTTP client a service or tool builds follows a redirect. Every one
# of them sends a credential (a PAT, a bearer, an internal token, a NeonJWT, a data key) or a
# forced Host, and a redirect would take it, or the answer it trusts, to wherever the upstream
# points. reqwest follows up to 10 redirects by default.
#
# The compiler does most of this: every client is a `chelabase_http::Client`, which only
# `chelabase_http::client_builder()` (redirect(Policy::none())) can make and which has no
# `Default`, `Deref` or conversion to a `reqwest::Client`. The workspace clippy.toml disallows
# the `reqwest::Client`/`ClientBuilder` types themselves (#32: an inferred `Default::default()`
# of a client field names the type in its struct, so it fails too), every other reqwest
# constructor, `reqwest::get` and `ClientBuilder::redirect`; `just check` runs clippy with
# `-D warnings` on every target. This script covers what clippy can't see. In every Rust file
# under crates/*/src, services/*/src and tools/*/src (inline test modules and test-named files
# included: they use the helper too):
#   - no `allow`/`expect` of clippy::disallowed_methods or clippy::disallowed_types (or the
#     `style`/`all` groups that hold them) outside crates/chelabase-http, so production code
#     can't opt out of the clippy rules;
#   - no `Default` client, a second line behind the type ban: `Client::default()`,
#     `ClientBuilder::default()`, a `Default::default()` on a line that names a client type, a
#     `#[derive(Default)]` struct holding a reqwest client, and an
#     `unwrap_or_default()`/`unwrap_or_else(` fallback on a client build are matched here.
# Integration-test crates (`*/tests/`) are not production code and may allow both lints; their
# files are not in the list below.
#
#   scripts/tests/http-no-redirects.sh
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"
fail=0
mapfile -t files < <(git ls-files 'crates/*/src/*.rs' 'crates/*/src/**/*.rs' 'services/*/src/*.rs' \
  'services/*/src/**/*.rs' 'tools/*/src/*.rs' 'tools/*/src/**/*.rs' | sort -u)
for f in "${files[@]}"; do
  out="$(awk -v f="$f" -v helper="$([[ $f == crates/chelabase-http/* ]] && echo 1 || echo 0)" '
    { line[NR] = $0 }
    !helper && /(allow|expect)\(.*clippy::(disallowed_methods|disallowed_types|style|all)([^a-z_]|$)/ {
      print f ":" NR ": production code may not opt out of the redirect rule (clippy.toml): " $0
    }
    /(Client|ClientBuilder)::default\(\)|reqwest::Client::default|reqwest::ClientBuilder::default/ ||
      (/Default::default\(\)|::default\(\)/ && /(^|[^_[:alnum:]])(Client|ClientBuilder)([^_[:alnum:]]|$)/) {
      print f ":" NR ": a default client (follows redirects): " $0
    }
    /^[[:space:]]*#\[derive\(.*Default/ { derive = NR }
    derive && /^[[:space:]]*(pub(\([a-z]+\))? )?struct / { body = 1 }
    body && /reqwest::(Client|ClientBuilder)|(^|[^:]):[[:space:]]*(Client|ClientBuilder)[,>[:space:]]|[(<](Client|ClientBuilder)[,>)]/ {
      print f ":" derive ": #[derive(Default)] on a struct holding a reqwest client (follows redirects)"
      body = 0; derive = 0
    }
    body && /^[[:space:]]*\}|;[[:space:]]*$/ { body = 0; derive = 0 }
    derive && !body && !/^[[:space:]]*(#\[|\/\/)/ && !/struct / { derive = 0 }
    /client_builder\(\)/ { start[NR] = 1 }
    END {
      for (n in start)
        for (i = n; i < n + 9 && (i in line); i++)
          if (line[i] ~ /unwrap_or_default\(\)|unwrap_or_else\(/) {
            print f ":" n ": a fallback on a failed build is a default client (follows redirects)"
            break
          }
    }' "$f")"
  if [[ -n "$out" ]]; then
    echo "$out" >&2
    fail=1
  fi
done
((${#files[@]} > 0)) || { echo "http-no-redirects: no source files found" >&2; exit 1; }
if ((fail)); then
  echo "http-no-redirects: every client must refuse redirects (see the comment at the top)" >&2
  exit 1
fi
echo "http-no-redirects: ok (${#files[@]} files)"
