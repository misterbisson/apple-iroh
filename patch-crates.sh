#!/usr/bin/env bash
#
# Puts `patched/netdev` and `patched/netwatch` in place: two crates iroh
# depends on, each as crates.io published it, with the diff in `patches/`
# applied. `Cargo.toml` points `[patch.crates-io]` at them.
#
# **Why two crates are patched at all.** Both choose their Apple code by naming
# the operating system, and both name `macos` and `ios` and stop there. Built
# for tvOS, `netdev::interfaces()` has no body and `netwatch` has no route
# monitor, so iroh does not compile. tvOS has the same routing socket, the same
# `SystemConfiguration` and the same `nw_path_monitor` iOS has, so the patch
# adds `tvos` wherever `ios` is named and changes nothing else. For every other
# target the two crates compile to what they compiled to before.
#
# **Why a diff and not a copy of the crates.** A megabyte of somebody else's
# source in this repository is a megabyte nobody will read. The diffs are 26
# and 36 changed lines, and they are the whole of what this repository adds to
# iroh's dependency graph.
#
# **The checksums are crates.io's own**, the ones `Cargo.lock` held for these
# two versions before they were patched. A crate that does not match is not
# unpacked.
#
# Remove this file, `patches/` and the `[patch.crates-io]` table when both
# crates name tvOS themselves.
set -euo pipefail
cd "$(dirname "$0")"

CRATES=(
	"netdev 0.46.3 c3c52c2584961c68f5a6e3356797344061f818c93b8acc9cd6d7e284795e563e"
	"netwatch 0.19.3 39da9cad5f23aa43401f09497d3b280e51b5feba115d9ffbf38ca35d6ff95e96"
)

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

for crate in "${CRATES[@]}"; do
	read -r name version sha <<<"$crate"
	patch="patches/$name-$version-tvos.patch"
	# The stamp is the crate's checksum and the patch's, so a changed patch
	# or a bumped version rebuilds the directory and nothing else does.
	stamp="$sha $(shasum -a 256 "$patch" | awk '{print $1}')"
	if [ "$(cat "patched/$name/.patched" 2>/dev/null)" = "$stamp" ]; then
		echo "$name $version: already patched"
		continue
	fi

	curl -fsSL --retry 3 --retry-delay 2 -o "$work/$name.crate" \
		"https://static.crates.io/crates/$name/$name-$version.crate"
	got="$(shasum -a 256 "$work/$name.crate" | awk '{print $1}')"
	if [ "$got" != "$sha" ]; then
		echo "$name $version: FAILED, checksum mismatch, refusing to unpack"
		echo "  expected $sha"
		echo "  got      $got"
		exit 1
	fi

	rm -rf "patched/$name" "$work/$name-$version"
	mkdir -p patched
	tar -xzf "$work/$name.crate" -C "$work"
	mv "$work/$name-$version" "patched/$name"
	patch -s -p2 -d "patched/$name" <"$patch"
	echo "$stamp" >"patched/$name/.patched"
	echo "$name $version: patched"
done
