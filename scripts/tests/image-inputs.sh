#!/usr/bin/env bash
# Offline tests of the image release tooling on a scratch git repo: a small cargo and pnpm
# workspace with all nine images pinned at its first commit, and a stub `docker` that only logs.
#   - scripts/image-inputs.sh changed: which images changed since their pins
#   - scripts/publish-images.sh: --service / --all / no flag, and --push-only on the selection
#   - scripts/pin-images.sh + scripts/tests/compose-image-pins.sh: a partial digests file
#   - scripts/image-inputs.sh copies: a Dockerfile COPY outside the image's inputs
# Part of `just check`.
set -euo pipefail
src="$(cd "$(dirname "$0")/../.." && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
fx="$work/repo"
fail=0
ok() { echo "ok   $*"; }
bad() { echo "FAIL $*" >&2; fail=1; }
expect() { [[ "$2" == "$3" ]] && ok "$1" || bad "$1: want '$2', got '$3'"; }
has() { [[ "$3" == *"$2"* ]] && ok "$1" || bad "$1: no '$2' in: $3"; }
g() { git -C "$fx" "$@"; }

# ---- fixture ---------------------------------------------------------------------------------

mkdir -p "$fx" "$work/bin"
cd "$fx"
crate() { # crate <dir> <name> <bin|lib> [<dep-line>...]
  local d="$1" n="$2" kind="$3"
  shift 3
  mkdir -p "$d/src"
  { printf '[package]\nname = "%s"\nversion = "0.1.0"\nedition = "2024"\n\n[dependencies]\n' "$n"
    for l in "$@"; do echo "$l"; done; } >"$d/Cargo.toml"
  [[ "$kind" == bin ]] && echo 'fn main() {}' >"$d/src/main.rs" || echo '' >"$d/src/lib.rs"
}
cat >Cargo.toml <<'EOF'
[workspace]
resolver = "3"
members = ["services/*", "crates/*", "tools/cli"]
exclude = ["services/identity"]
EOF
cp "$src/rust-toolchain.toml" .
crate crates/http http lib
crate crates/meta meta lib 'http = { path = "../http" }'
crate services/gateway gateway bin 'http = { path = "../../crates/http" }'
crate services/orchestrator orchestrator bin 'meta = { path = "../../crates/meta" }'
crate services/worker worker bin 'meta = { path = "../../crates/meta" }'
crate services/rest rest bin '[dev-dependencies]' 'meta = { path = "../../crates/meta" }'
crate services/auth auth bin
crate services/realtime realtime bin
crate services/init init bin
crate tools/cli cli bin 'meta = { path = "../../crates/meta" }'
cargo generate-lockfile --offline -q

pkg() { # pkg <dir> <name> [<workspace dep>...]
  mkdir -p "$1"
  local deps="" d
  for d in "${@:3}"; do deps+="${deps:+,}\"$d\":\"workspace:*\""; done
  echo "{\"name\":\"$2\",\"dependencies\":{$deps}}" >"$1/package.json"
}
echo '{"name":"root","private":true}' >package.json
printf 'packages:\n  - "apps/*"\n  - "packages/*"\n  - "services/identity"\n' >pnpm-workspace.yaml
echo 'lockfileVersion: 9.0' >pnpm-lock.yaml
pkg apps/studio @x/studio @x/ui
pkg apps/docs @x/docs @x/client
pkg packages/ui @x/ui
pkg packages/client @x/client
pkg services/identity @x/identity
mkdir -p openapi docs
echo '{}' >openapi/management.json
echo 'docs' >docs/readme.md

mkdir -p infra/images infra/compose scripts/tests
cp "$src"/infra/images/{docker-bake.hcl,server.Dockerfile,studio.Dockerfile,studio.Caddyfile,identity.Dockerfile} infra/images/
cp "$src/.dockerignore" .
cp "$src"/scripts/{image-inputs.sh,publish-images.sh,pin-images.sh} scripts/ 2>/dev/null || true
cp "$src/scripts/tests/compose-image-pins.sh" scripts/tests/
images=(gateway orchestrator worker rest auth realtime init studio identity)
digest() { printf 'sha256:%064d' "$1"; }
write_pins() { # write_pins <sha12>: every image pinned at <sha12>
  local i n=0
  { echo "services:"
    for i in "${images[@]}"; do
      n=$((n + 1))
      echo "  $i:"
      echo "    image: \${CHELABASE_IMAGE_REPO:-ghcr.io/chelabase}/$i:\${CHELABASE_IMAGE_TAG:-$1@$(digest $n)}"
    done; } >infra/compose/docker-compose.yml
  { echo "# CHELABASE_PUBLIC_PORT=8000"
    echo "# BEGIN image pins"
    n=0
    for i in "${images[@]}"; do n=$((n + 1)); echo "# ghcr.io/chelabase/$i:$1@$(digest $n)"; done
    echo "# END image pins"; } >infra/compose/.env.example
}
write_pins 000000000000
git init -q -b main .
git config user.email t@example.com
git config user.name t
git add -A && git commit -qm built
built="$(git rev-parse --short=12 HEAD)"
# The release commit writes the pins of the images built at the commit before it.
write_pins "$built"
git commit -qam pinned
base="$(git rev-parse HEAD)"
# A local origin holding main at the pin commit, so the publish tests can tell main from a branch.
git init -q --bare "$work/origin.git"
git remote add origin "$work/origin.git"
git push -q origin HEAD:main

# docker stub: logs its arguments; `image inspect` answers from STUB_REVISION and a fake digest.
cat >"$work/bin/docker" <<'EOF'
#!/usr/bin/env bash
echo "$*" >>"$STUB_LOG"
if [[ "$1 $2" == "image inspect" ]]; then
  ref="${!#}"
  case "$*" in
    *revision*) echo "$STUB_REVISION" ;;
    *RepoDigests*) printf '%s@sha256:%064d\n' "${ref%:*}" 7 ;;
  esac
fi
EOF
chmod +x "$work/bin/docker"
export PATH="$work/bin:$PATH" STUB_LOG="$work/docker.log" STUB_REVISION=""

# case <message> <edit command...>: reset to the pin commit, run the edit, commit it
case_() { g reset -q --hard "$base"; "$@"; g add -A; g commit -qm "$1" --allow-empty; }
changed() { scripts/image-inputs.sh changed 2>"$work/err" | tr '\n' ' ' | sed 's/ $//'; }
edit() { case "$1" in *.toml) echo "# $RANDOM" ;; *) echo "// $RANDOM" ;; esac >>"$1"; }

# ---- changed set -----------------------------------------------------------------------------

case_ true
expect "nothing changed since the pins (the pin commit itself counts for nothing)" "" "$(changed)"
case_ edit apps/studio/src.ts
expect "apps/studio only -> studio" "studio" "$(changed)"
case_ edit crates/meta/src/lib.rs
expect "crates/meta -> its dependents, not rest (dev only)" "orchestrator worker" "$(changed)"
case_ edit crates/http/src/lib.rs
expect "crates/http -> direct and transitive dependents" "gateway orchestrator worker" "$(changed)"
case_ edit services/auth/src/main.rs
expect "a service's own crate -> that service" "auth" "$(changed)"
case_ edit tools/cli/src/main.rs
expect "a crate no image builds -> nothing" "" "$(changed)"
case_ edit crates/meta/Cargo.toml
expect "a manifest in the build (features unify) -> every Rust image" "gateway orchestrator worker rest auth realtime init" "$(changed)"
for f in Cargo.lock Cargo.toml rust-toolchain.toml; do
  case_ edit "$f"
  expect "$f -> every Rust image" "gateway orchestrator worker rest auth realtime init" "$(changed)"
done
case_ edit packages/ui/index.ts
expect "a workspace package studio uses -> studio" "studio" "$(changed)"
for f in apps/docs/x.ts packages/client/x.ts docs/readme.md; do
  case_ edit "$f"
  expect "$f -> nothing" "" "$(changed)"
done
for f in infra/images/studio.Dockerfile infra/images/studio.Caddyfile openapi/management.json; do
  case_ edit "$f"
  expect "$f -> studio" "studio" "$(changed)"
done
for f in pnpm-lock.yaml package.json pnpm-workspace.yaml; do
  case_ edit "$f"
  expect "$f -> studio and identity" "studio identity" "$(changed)"
done
for f in services/identity/src.ts infra/images/identity.Dockerfile; do
  case_ edit "$f"
  expect "$f -> identity" "identity" "$(changed)"
done
case_ sed -i 's/8000/9000/' infra/compose/.env.example
expect ".env.example outside the pin block -> studio" "studio" "$(changed)"
all="gateway orchestrator worker rest auth realtime init studio identity"
for f in infra/images/server.Dockerfile infra/images/docker-bake.hcl .dockerignore; do
  case_ edit "$f"
  expect "$f -> every image" "$all" "$(changed)"
done
for f in scripts/publish-images.sh scripts/pin-images.sh scripts/image-inputs.sh; do
  case_ edit "$f"
  expect "$f (release tooling, not image content) -> nothing" "" "$(changed)"
done
case_ sed -i 's#/worker:${CHELABASE_IMAGE_TAG:-[0-9a-f]*#/worker:${CHELABASE_IMAGE_TAG:-deadbeef0000#' infra/compose/docker-compose.yml
expect "a pin commit missing from history -> changed" "worker" "$(changed)"
has "... with a warning" "worker: pin commit deadbeef0000 is not in this repository" "$(cat "$work/err")"

# ---- publish-images.sh selection (docker is the stub) ------------------------------------------

publish() { : >"$STUB_LOG"; set +e; out="$(scripts/publish-images.sh "$@" 2>&1)"; rc=$?; set -e; }
case_ edit apps/studio/src.ts
publish
expect "no flag -> usage error" 2 "$rc"
has "no flag -> the changed set as a hint" "changed since their pins: studio" "$out"
expect "no flag -> docker untouched" "" "$(cat "$STUB_LOG")"
publish --all --service gateway
expect "--all with --service -> usage error" 2 "$rc"
publish --service nosuch
expect "unknown image -> usage error" 2 "$rc"
has "unknown image -> named" "nosuch" "$out"
publish --all
expect "--all builds only the changed image" 0 "$rc"
expect "--all -> bake studio" "buildx bake -f infra/images/docker-bake.hcl --load studio" "$(cat "$STUB_LOG")"
publish --service gateway --service rest
expect "--service builds exactly the named images" "buildx bake -f infra/images/docker-bake.hcl --load gateway rest" "$(cat "$STUB_LOG")"
has "--service on an unchanged image -> a note" "gateway is unchanged since its pin" "$out"
# A public push must be from a commit on origin/main: a squash merge drops branch commits, and
# the pins would name a revision that's in no history.
STUB_REVISION="$(g rev-parse HEAD)" publish --push-only --service studio --service init
expect "--push-only off origin/main -> refused" 1 "$rc"
has "--push-only off origin/main -> says why" "not on origin/main" "$out"
expect "--push-only off origin/main -> nothing pushed" "" "$(grep '^push' "$STUB_LOG" || true)"
STUB_REVISION="$(g rev-parse HEAD)" PUBLISH_IMAGES_REPO=localhost:5000/test publish --push-only --service studio
expect "--push-only to a test registry -> allowed off main" 0 "$rc"
g push -q origin HEAD:main
STUB_REVISION="$(g rev-parse HEAD)" publish --push-only --service studio --service init
expect "--push-only pushes the selection" 0 "$rc"
names() { awk "/$1/"' {print $NF}' "$STUB_LOG" | sed -E 's#.*/([a-z]+):.*#\1#' | sort -u | tr '\n' ' ' | sed 's/ $//'; }
expect "--push-only pushes only the selection" "init studio" "$(names '^push')"
expect "--push-only inspects only the selection" "init studio" "$(names '^image inspect')"
has "--push-only prints pins for the selection" "pin studio $(g rev-parse --short=12 HEAD) sha256:" "$out"
[[ "$out" != *"pin gateway"* ]] && ok "--push-only prints no other pins" || bad "--push-only printed a pin outside the selection: $out"
case_ true
publish --all
expect "--all with nothing changed -> exit 0" 0 "$rc"
has "--all with nothing changed -> says so" "nothing changed" "$out"
expect "--all with nothing changed -> builds nothing" "" "$(cat "$STUB_LOG")"

# ---- partial digests: pin-images.sh and compose-image-pins.sh ---------------------------------

case_ true
sha="$(g rev-parse --short=12 HEAD)"
printf 'pushed; pins:\npin studio %s %s\n' "$sha" "$(digest 99)" >"$work/digests"
if scripts/pin-images.sh "$work/digests" >/dev/null; then
  expect "partial digests -> only studio's lines change" \
    "$(printf '%s\n' "+# ghcr.io/chelabase/studio:$sha@$(digest 99)" "+    image: \${CHELABASE_IMAGE_REPO:-ghcr.io/chelabase}/studio:\${CHELABASE_IMAGE_TAG:-$sha@$(digest 99)}")" \
    "$(g diff -U0 | grep '^+[^+]')"
  scripts/tests/compose-image-pins.sh "$work/digests" >/dev/null && ok "compose-image-pins.sh passes on the partial digests" ||
    bad "compose-image-pins.sh fails on the partial digests"
else
  bad "pin-images.sh refused a partial digests file"
fi
case_ true
printf 'pin identity %s %s\n' "$sha" "$(digest 97)" >"$work/digests"
if scripts/pin-images.sh "$work/digests" >/dev/null; then
  expect "identity digests -> only identity's lines change" \
    "$(printf '%s\n' "+# ghcr.io/chelabase/identity:$sha@$(digest 97)" "+    image: \${CHELABASE_IMAGE_REPO:-ghcr.io/chelabase}/identity:\${CHELABASE_IMAGE_TAG:-$sha@$(digest 97)}")" \
    "$(g diff -U0 | grep '^+[^+]')"
  scripts/tests/compose-image-pins.sh "$work/digests" >/dev/null && ok "compose-image-pins.sh passes on the identity digests" ||
    bad "compose-image-pins.sh fails on the identity digests"
else
  bad "pin-images.sh refused an identity digests file"
fi
case_ true
printf 'pin studio %s %s\npin nosuch %s %s\n' "$sha" "$(digest 99)" "$sha" "$(digest 98)" >"$work/digests"
if scripts/pin-images.sh "$work/digests" >"$work/out" 2>&1; then bad "pin-images.sh accepted an unknown image"; else
  has "pin-images.sh rejects an unknown image, by name" "nosuch" "$(cat "$work/out")"
  expect "... and changes nothing" "" "$(g status --porcelain)"
fi
if scripts/tests/compose-image-pins.sh "$work/digests" >"$work/out" 2>&1; then bad "compose-image-pins.sh accepted an unknown image"; else
  has "compose-image-pins.sh rejects an unknown image, by name" "nosuch" "$(cat "$work/out")"
fi

# ---- COPY coverage ----------------------------------------------------------------------------

case_ true
printf 'FROM scratch\nCOPY . .\nCOPY --from=builder /out /x\n' >"$work/ok.Dockerfile"
scripts/image-inputs.sh copies "$work/ok.Dockerfile" gateway >/dev/null 2>&1 && ok "COPY . . and --from are covered" ||
  bad "a covered Dockerfile fails the COPY check"
printf 'FROM scratch\nCOPY --chown=1:1 Cargo.toml \\\n  docs/readme.md /src/\n' >"$work/bad.Dockerfile"
if scripts/image-inputs.sh copies "$work/bad.Dockerfile" gateway >"$work/out" 2>&1; then
  bad "a COPY outside gateway's inputs passes the check"
else
  has "a COPY outside the inputs fails, naming the path" "docs/readme.md" "$(cat "$work/out")"
fi
scripts/image-inputs.sh copies >/dev/null 2>&1 && ok "the fixture's own Dockerfiles are covered" || bad "the fixture's own Dockerfiles fail the COPY check"

[[ $fail -eq 0 ]] && echo "image-inputs: ok"
exit $fail
