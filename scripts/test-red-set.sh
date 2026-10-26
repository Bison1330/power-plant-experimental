#!/usr/bin/env bash
# Runs the workspace's tests with one feature set and compares what fails
# with ci/expected-red/<feature>.txt. Passes when the two are the same set.
# Fails when a test fails that is not listed (the red set grew) and when a
# listed test passes or no longer runs (it shrank: take it off the list in
# the same commit). Fails on a build error, which leaves no test to compare.
#
#   scripts/test-red-set.sh try-runtime
#   scripts/test-red-set.sh runtime-benchmarks
#
# RED_SET_LOG=<file> compares a saved `cargo test` log instead of running
# the tests, and RED_SET_EXPECTED=<file> another list (to test this script).
set -uo pipefail

feature="${1:?usage: scripts/test-red-set.sh <feature>}"
root="$(cd "$(dirname "$0")/.." && pwd)"
expected="${RED_SET_EXPECTED:-$root/ci/expected-red/$feature.txt}"
[ -f "$expected" ] || { echo "no expected-red list for $feature at $expected" >&2; exit 2; }

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

if [ -n "${RED_SET_LOG:-}" ]; then
    cp "$RED_SET_LOG" "$work/log"
    status=1
else
    cargo test --workspace --locked --no-fail-fast --features "$feature" 2>&1 | tee "$work/log" >/dev/null
    status=${PIPESTATUS[0]}
fi

if grep -q '^error: could not compile' "$work/log"; then
    grep -B2 -A12 '^error' "$work/log" | head -60
    echo "build failed: no red set to compare" >&2
    exit 1
fi

# `<crate> <test>` for every failure: the crate from the binary's name.
awk '/Running unittests/ { c = $NF; sub(/.*deps\//, "", c); sub(/-[0-9a-f]+\)$/, "", c) }
     /\.\.\. FAILED$/   { print c " " $2 }' "$work/log" | sort -u > "$work/actual"
grep -v '^#' "$expected" | grep -v '^$' | sort -u > "$work/expected"

passed=$(grep -c '\.\.\. ok$' "$work/log")
red=$(wc -l < "$work/actual")
want=$(wc -l < "$work/expected")
new=$(comm -13 "$work/expected" "$work/actual")
gone=$(comm -23 "$work/expected" "$work/actual")

echo "$feature: $passed passed, $red red, $want expected red"
if [ -n "$new" ]; then
    echo "Red and not on the list ($(echo "$new" | wc -l)):"
    echo "$new" | sed 's/^/  /'
fi
if [ -n "$gone" ]; then
    echo "On the list and not red: green now, or no longer run ($(echo "$gone" | wc -l)):"
    echo "$gone" | sed 's/^/  /'
fi
if [ -n "$new" ] || [ -n "$gone" ]; then
    exit 1
fi
# Same set, so any failure cargo reported is one of the listed ones.
[ "$status" -eq 0 ] || [ "$red" -gt 0 ] || { echo "cargo test failed with no test failing" >&2; exit 1; }
exit 0
