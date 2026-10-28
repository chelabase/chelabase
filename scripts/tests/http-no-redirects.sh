#!/usr/bin/env bash
# Offline invariant (#26 and its follow-up): no HTTP client a service or tool builds follows a
# redirect. Every one of them sends a credential (a PAT, a bearer, an internal token, a NeonJWT,
# a data key) or a forced Host, and a redirect would take it, or the answer it trusts, to
# wherever the upstream points. reqwest follows up to 10 redirects by default.
#
# In the non-test Rust sources (crates/*/src, services/*/src, tools/*/src; a file's inline
# `#[cfg(test)] mod … { }` and test files are skipped):
#   - every `reqwest::Client::builder()` sets `.redirect(reqwest::redirect::Policy::none())`
#     within its next 8 lines;
#   - nothing builds a default client (`reqwest::Client::new()`, `Client::default()`, or a
#     builder's `.unwrap_or_default()`, which falls back to one).
#
#   scripts/tests/http-no-redirects.sh
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"
fail=0
mapfile -t files < <(git ls-files 'crates/*/src/*.rs' 'crates/*/src/**/*.rs' 'services/*/src/*.rs' \
  'services/*/src/**/*.rs' 'tools/*/src/*.rs' 'tools/*/src/**/*.rs' |
  grep -vE '(^|/)(tests?|[a-z0-9_]*_tests?)\.rs$|/tests/|/[a-z0-9_]*_tests/' | sort -u)
for f in "${files[@]}"; do
  out="$(awk -v f="$f" '
    # An inline `#[cfg(test)] mod … {` ends the production part; a `#[cfg(test)] mod x;` does not.
    pending && /^[[:space:]]*(pub(\([a-z]+\))? )?mod [a-z0-9_]+ *\{/ { exit }
    pending && !/^[[:space:]]*#\[/ { pending = 0 }
    /^[[:space:]]*#\[cfg\(test\)\]/ { pending = 1 }
    { line[NR] = $0 }
    /reqwest::Client::new\(\)|reqwest::Client::default\(\)/ { print f ":" NR ": a default client (follows redirects): " $0 }
    /reqwest::Client::builder\(\)/ { start[NR] = 1 }
    END {
      for (n in start) {
        ok = 0; fallback = 0
        for (i = n; i < n + 9 && (i in line); i++) {
          if (line[i] ~ /\.redirect\(reqwest::redirect::Policy::none\(\)\)/) ok = 1
          if (line[i] ~ /unwrap_or_default\(\)/) fallback = 1
        }
        if (!ok) print f ":" n ": a client that follows redirects (no .redirect(reqwest::redirect::Policy::none()))"
        if (fallback) print f ":" n ": unwrap_or_default() falls back to a client that follows redirects"
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
