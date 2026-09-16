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

# `macosx` is not in this list: the Mac slice is the host build, and the
# deployment target is set per-slice below.
TARGETS=(aarch64-apple-ios aarch64-apple-ios-sim aarch64-apple-darwin)

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

# One headers directory shared by all three slices. `-create-xcframework`
# copies it per slice rather than referencing it, so there is no aliasing here.
xcodebuild -create-xcframework \
	-library "target/aarch64-apple-ios/release/libapple_iroh.a" -headers include \
	-library "target/aarch64-apple-ios-sim/release/libapple_iroh.a" -headers include \
	-library "target/aarch64-apple-darwin/release/libapple_iroh.a" -headers include \
	-output build/AppleIroh.xcframework >/dev/null

# `-X` drops the extra attribute files that make a zip built on a Mac differ
# from the same bytes zipped anywhere else. The checksum below is what a
# consumer pins, so a zip that changes without its contents changing would be a
# false alarm every time.
(cd build && zip -qry -X AppleIroh.xcframework.zip AppleIroh.xcframework)
shasum -a 256 build/AppleIroh.xcframework.zip | awk '{print $1}' > build/AppleIroh.xcframework.zip.sha256

echo
echo "xcframework: $(du -sh build/AppleIroh.xcframework | awk '{print $1}')"
echo "zip:         $(stat -f%z build/AppleIroh.xcframework.zip) bytes"
echo "sha256:      $(cat build/AppleIroh.xcframework.zip.sha256)"
