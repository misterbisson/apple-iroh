#!/usr/bin/env bash
#
# Builds AppleIroh.xcframework — three static slices and a header — and the
# zip plus checksum a consumer pins.
#
# The same script runs on a laptop and in CI. That is the point: an artifact
# whose provenance is "somebody built it on their Mac that day" is not
# auditable, and this repo exists so it is a tagged commit and a public log
# instead.
#
# Needs Rust with the three targets, and Xcode.
set -euo pipefail
cd "$(dirname "$0")"

# **Five Rust targets, three xcframework slices**, because a slice may be
# universal and a Rust build never is.
#
# The consuming Mac app builds `ARCHS = arm64 x86_64` in Release — that is
# Xcode's default for a macOS app and what it ships — so an arm64-only archive
# fails at link on the x86_64 arm. It does not fail in Debug, where
# `ONLY_ACTIVE_ARCH` builds the host architecture alone, which is how an
# arm64-only v0.1.1 passed every local build and failed the first Release one.
#
# The Simulator slice is universal for the same reason one step removed: the
# Simulator runs the host architecture, so an Intel Mac needs `x86_64-apple-ios`
# to run the app at all.
#
# The device slice is not, and cannot be: there has never been an x86_64 iPhone.
TARGETS=(
	aarch64-apple-ios
	aarch64-apple-ios-sim x86_64-apple-ios
	aarch64-apple-darwin x86_64-apple-darwin
)

# Matched to the consuming app's floors. Raising either is a breaking change
# for a consumer that has not raised its own, so it is a version bump here.
IOS_MIN=18.0
MACOS_MIN=15.0

rm -rf build
mkdir -p build

for target in "${TARGETS[@]}"; do
	printf '%-26s' "$target"
	case "$target" in
	*-ios | *-ios-sim) env IPHONEOS_DEPLOYMENT_TARGET="$IOS_MIN" \
		cargo build --release --target "$target" --lib -q ;;
	*) env MACOSX_DEPLOYMENT_TARGET="$MACOS_MIN" \
		cargo build --release --target "$target" --lib -q ;;
	esac
	echo "$(stat -f%z "target/$target/release/libapple_iroh.a") bytes"
done

# **`lipo` the pairs.** `-create-xcframework` will not take two archives for one
# platform — it refuses with "binaries with the same platform and architecture"
# — so the universal slices are made here and handed over as one file each.
fat() {
	local out=$1; shift
	mkdir -p "$(dirname "$out")"
	lipo -create "$@" -output "$out"
	printf '%-26s%s bytes  %s\n' "$(basename "$(dirname "$out")")" \
		"$(stat -f%z "$out")" "$(lipo -archs "$out")"
}

fat build/fat/ios-simulator/libapple_iroh.a \
	target/aarch64-apple-ios-sim/release/libapple_iroh.a \
	target/x86_64-apple-ios/release/libapple_iroh.a
fat build/fat/macos/libapple_iroh.a \
	target/aarch64-apple-darwin/release/libapple_iroh.a \
	target/x86_64-apple-darwin/release/libapple_iroh.a

# One headers directory shared by all three slices. `-create-xcframework`
# copies it per slice rather than referencing it, so there is no aliasing here.
xcodebuild -create-xcframework \
	-library "target/aarch64-apple-ios/release/libapple_iroh.a" -headers include \
	-library "build/fat/ios-simulator/libapple_iroh.a" -headers include \
	-library "build/fat/macos/libapple_iroh.a" -headers include \
	-output build/AppleIroh.xcframework >/dev/null

# **`-create-xcframework` shuffles its own index**, so sort it.
#
# Diagnosed rather than guessed: two CI runs of one commit produced
# byte-identical `libapple_iroh.a` for all three slices and different zips. The
# difference was `Info.plist` — xcodebuild writes `AvailableLibraries` in
# whatever order it happens to finish, so the same three slices came out
# macos/sim/ios one run and sim/macos/ios the next.
#
# The order carries no meaning: Xcode picks a slice by matching platform and
# architecture, never by position. Sorting by `LibraryIdentifier` changes
# nothing a consumer can observe, and makes the artifact a function of its
# inputs.
python3 - build/AppleIroh.xcframework/Info.plist <<'PLIST'
import plistlib, sys
path = sys.argv[1]
with open(path, "rb") as handle:
    plist = plistlib.load(handle)
plist["AvailableLibraries"].sort(key=lambda lib: lib["LibraryIdentifier"])
with open(path, "wb") as handle:
    plistlib.dump(plist, handle, fmt=plistlib.FMT_XML, sort_keys=True)
PLIST

# **Normalise before zipping, or the checksum is a timestamp.**
#
# Measured, not assumed: two CI runs of the same commit with the same pinned
# rustc produced different checksums — 7ffbc109... and 505d4468... — because a
# zip records each file's modification time, and those are when the build ran.
# A checksum that changes when nothing changed cannot be used to verify
# anything.
#
# `touch` to a fixed instant, then `-X` to drop the Mac's extra attributes, and
# `find | sort` so members go in every time in the same order rather than in
# whatever order the filesystem hands them over.
rm -rf build/fat
find build/AppleIroh.xcframework -exec touch -h -t 200001010000 {} +
(cd build && find AppleIroh.xcframework -print | sort | zip -qry -X AppleIroh.xcframework.zip -@)
shasum -a 256 build/AppleIroh.xcframework.zip | awk '{print $1}' > build/AppleIroh.xcframework.zip.sha256

echo
echo "xcframework: $(du -sh build/AppleIroh.xcframework | awk '{print $1}')"
echo "zip:         $(stat -f%z build/AppleIroh.xcframework.zip) bytes"
echo "sha256:      $(cat build/AppleIroh.xcframework.zip.sha256)"
