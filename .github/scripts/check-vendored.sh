#!/usr/bin/env bash
# Checks that vendor/pallet-asset-conversion is upstream's crate plus the
# reviewed patch and nothing else:
#   1. the tag in UPSTREAM still points at the commit recorded beside it;
#   2. src/ and README.md equal that commit's, with the genesis patch applied;
#   3. Cargo.toml names the same dependencies, in the same sections, as
#      upstream's manifest at that commit.
# Cargo.toml, rustfmt.toml and UPSTREAM are ours and are not compared.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
vendored=$root/vendor/pallet-asset-conversion
patch=$root/vendor/pallet-asset-conversion.genesis.patch

field() { awk -v k="$1" '$1 == k { print $2 }' "$vendored/UPSTREAM"; }
repo=$(field repository)
tag=$(field tag)
commit=$(field commit)
path=$(field path)

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# A tag can be moved. For an annotated tag the peeled `^{}` line is the commit.
refs=$(git ls-remote "$repo" "refs/tags/$tag" "refs/tags/$tag^{}")
resolved=$(awk '$2 ~ /\^\{\}$/ { print $1 }' <<<"$refs")
[ -n "$resolved" ] || resolved=$(awk 'NR == 1 { print $1 }' <<<"$refs")
if [ "$resolved" != "$commit" ]; then
	echo "error: $tag resolves to '${resolved:-nothing}', UPSTREAM records $commit" >&2
	exit 1
fi

git -c init.defaultBranch=upstream init -q "$work/sdk"
git -C "$work/sdk" sparse-checkout set --no-cone "/$path/src/" "/$path/README.md" "/$path/Cargo.toml"
git -C "$work/sdk" fetch -q --depth 1 --filter=blob:none "$repo" "$commit"
git -C "$work/sdk" checkout -q FETCH_HEAD

# Outside any repository, so `git apply` takes the patch's paths as they are.
expected=$work/expected
mkdir "$expected"
cp -R "$work/sdk/$path/src" "$work/sdk/$path/README.md" "$expected/"
(cd "$expected" && git apply --whitespace=nowarn "$patch")

status=0
for entry in src README.md; do
	if ! diff -r "$expected/$entry" "$vendored/$entry"; then
		echo "error: vendored $entry differs from $tag plus the genesis patch" >&2
		status=1
	fi
done

# The dependency names per section ([dependencies], [dev-dependencies]),
# which is what a manifest written by hand could drift on unreviewed.
deps() {
	awk '
		/^\[/ { section = $0; next }
		section ~ /^\[(dev-|build-)?dependencies\]$/ && /^[A-Za-z0-9_-]+[ .=]/ {
			name = $1; sub(/[ .=].*/, "", name); print section, name
		}' "$1" | sort
}
if ! diff <(deps "$work/sdk/$path/Cargo.toml") <(deps "$vendored/Cargo.toml"); then
	echo "error: vendored Cargo.toml dependencies differ from upstream's" >&2
	status=1
fi

[ "$status" -eq 0 ] && echo "vendor/pallet-asset-conversion matches $tag ($commit) plus the genesis patch"
exit "$status"
