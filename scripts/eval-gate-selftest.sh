#!/usr/bin/env bash
# ENT-1 self-test: the golden gate must pass on the unpatched tree and must fail on every
# golden/seeded-regressions/*.patch. Works on a copy of the working tree (target/ and .git excluded),
# so the real sources are never touched. Builds go to a private target dir ($repo/target/selftest by default,
# kept between runs so third-party crates are compiled once); sharing the main target dir can leave a stale sf-eval binary.
set -euo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
export CARGO_TARGET_DIR="${SELFTEST_TARGET_DIR:-$repo/target/selftest}"
tar -C "$repo" --exclude=./target --exclude=./.git -cf - . | tar -C "$work" -xf -
cd "$work"
# Cargo identifies workspace crates by relative path, so artifacts from an earlier (possibly aborted, patched) run are
# reused whenever they look newer than the sources. Make every source newer than any old artifact.
find . -type f -exec touch {} +

gate() {
  cargo build -q --workspace --manifest-path "$work/Cargo.toml" >"$work/build.log" 2>&1 || { cat "$work/build.log"; fail "build failed"; }
  "$CARGO_TARGET_DIR/debug/sf-eval" gate --golden "$work/golden" --baseline "$work/golden/baseline.json" 2>&1
}

fail() { echo "SELFTEST FAIL: $1"; exit 1; }

echo "== unpatched: gate must pass"
gate >"$work/base.log" || { cat "$work/base.log"; fail "gate fails on the unpatched tree"; }
tail -n 1 "$work/base.log"

shopt -s nullglob
patches=("$repo"/golden/seeded-regressions/*.patch)
[ ${#patches[@]} -ge 2 ] || fail "need at least two seeded regressions, found ${#patches[@]}"
for p in "${patches[@]}"; do
  name=$(basename "$p")
  echo "== $name: gate must fail"
  patch -s -p1 <"$p" || fail "$name does not apply"
  if gate >"$work/out.log"; then
    cat "$work/out.log"
    fail "gate PASSED with $name applied: the seeded regression was not caught"
  fi
  grep -E '^REGRESSION' "$work/out.log" | head -n 5
  patch -s -R -p1 <"$p" || fail "$name does not revert"
done
echo "SELFTEST PASS: gate passes clean and blocks all ${#patches[@]} seeded regressions"
