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
# constructor, `reqwest::get`, `ClientBuilder::redirect`, and `RequestBuilder::from_parts` and
# `build_split` (which take and hand out a bare client); `just check` runs clippy with
# `-D warnings` on every target. This script covers what clippy can't see. In every Rust file
# under crates/*/src, services/*/src and tools/*/src (inline test modules and test-named files
# included: they use the helper too):
#   - no `allow`/`expect` of clippy::disallowed_methods or clippy::disallowed_types (their old
#     singular names, the `style`/`all` groups that hold them, or rustc's `warnings`) outside
#     crates/chelabase-http, so production code can't opt out of the clippy rules. Each
#     attribute is joined up to its closing `]` first, so a multi-line one counts too;
#   - no `Default` client, a second line behind the type ban: `Client::default()`,
#     `ClientBuilder::default()`, a `Default::default()` on a line that names a client type, a
#     `#[derive(Default)]` struct holding a reqwest client, and an
#     `unwrap_or_default()`/`unwrap_or_else(` fallback on a client build are matched here.
# Integration-test crates (`*/tests/`) are not production code and may allow both lints; their
# files are not in the list below. Every Cargo.toml is checked for a `[lints]` table, and every
# cargo config for `rustflags`, that allows the same lints.
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
    # Each attribute is read whole, from `#[`/`#![` to its closing `]`, however many lines it
    # spans, with whitespace and raw-identifier prefixes dropped before it is matched.
    !helper {
      rest = $0
      while (rest != "") {
        if (!inattr) {
          p = match(rest, /#!?\[/)
          if (!p) break
          inattr = 1; depth = 0; attr = ""; astart = NR
          rest = substr(rest, p)
        }
        n = length(rest)
        for (i = 1; i <= n; i++) {
          c = substr(rest, i, 1)
          if (c == "[") depth++
          else if (c == "]" && --depth == 0) break
        }
        attr = attr substr(rest, 1, i)
        if (i > n) {
          # Still open at the end of the line; give up on a runaway (an unbalanced `[` in text).
          if (NR - astart > 40) inattr = 0
          break
        }
        inattr = 0
        rest = substr(rest, i + 1)
        a = attr
        gsub(/[[:space:]]/, "", a)
        gsub(/r#/, "", a)
        if (a ~ /(allow|expect)\(/ &&
            (a ~ /clippy::(disallowed_methods?|disallowed_types?|style|all)([^_[:alnum:]]|$)/ ||
             a ~ /[(,]warnings([^_[:alnum:]]|$)/))
          print f ":" astart ": production code may not opt out of the redirect rule (clippy.toml): " a
      }
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
# Cargo-level opt-outs: a `[lints]` table (crate or workspace) that allows either lint, a group
# holding it or `warnings`, and a `rustflags` entry in a cargo config that does the same.
mapfile -t manifests < <(git ls-files 'Cargo.toml' '*/Cargo.toml' | sort -u)
for f in "${manifests[@]}"; do
  out="$(awk -v f="$f" '
    /^[[:space:]]*\[/ { section = $0; gsub(/[][[:space:]"]/, "", section) }
    /=/ {
      key = $0; sub(/=.*/, "", key); gsub(/[[:space:]"]/, "", key)
      val = $0; sub(/^[^=]*=/, "", val)
      full = section "." key
      sub(/^workspace\./, "", full)
      if (full ~ /^lints\.(clippy\.(disallowed_methods?|disallowed_types?|style|all)|rust\.warnings)$/ &&
          val ~ /allow|expect/)
        print f ":" NR ": a [lints] table may not opt out of the redirect rule (clippy.toml): " $0
    }' "$f")"
  if [[ -n "$out" ]]; then
    echo "$out" >&2
    fail=1
  fi
done
while IFS= read -r f; do
  if grep -nE -- '(-A|--allow)[[:space:]=",]*(clippy::(disallowed_methods?|disallowed_types?|style|all)|warnings)|--cap-lints[[:space:]=",]*(allow|warn)' "$f" >&2; then
    echo "$f: a cargo config may not opt out of the redirect rule (clippy.toml)" >&2
    fail=1
  fi
done < <(git ls-files '.cargo/config' '.cargo/config.toml' '*/.cargo/config' '*/.cargo/config.toml')
if ((fail)); then
  echo "http-no-redirects: every client must refuse redirects (see the comment at the top)" >&2
  exit 1
fi
echo "http-no-redirects: ok (${#files[@]} files)"
