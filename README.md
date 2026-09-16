# apple-iroh

**[iroh](https://github.com/n0-computer/iroh) as a static library for Apple
platforms, behind a small C ABI.**

One tagged release publishes `AppleIroh.xcframework.zip` with three slices —
`ios-arm64`, `ios-arm64_x86_64-simulator`, `macos-arm64_x86_64` — and the header
to call them. An app fetches the zip, checks the sha256, and links it.

Two of the three are universal, from v0.2.0. Xcode builds a Mac app
`ARCHS = arm64 x86_64` in Release by default, and the Simulator runs the host's
architecture, so an arm64-only archive fails at link the first time somebody
builds Release or opens the Simulator on an Intel Mac. v0.1.1 was arm64-only and
did exactly that.

## Why this is its own repository

Not for tidiness. For a macOS runner.

Building this needs Xcode: `xcodebuild -create-xcframework`, and the iOS SDKs.
On a **private** repository GitHub bills macOS minutes at roughly ten times the
Linux rate, which is why the app repository that consumes this runs Linux-only
CI and does its Mac builds on a desk. On a **public** repository the macOS
runner is free.

Which means the artifact can be built by CI from a tagged commit, rather than by
somebody on their laptop. That is the part that matters. A binary in a release
with no record of what produced it is not auditable; this one has a public log,
and the log prints the compiler, the Xcode version and the OS that made it.

It is also a repository that changes a few times a year — when iroh moves, or
when the surface below needs something new. Nothing about it is specific to the
app that uses it.

## What it exposes

The smallest surface that can answer one question: **is this connection
direct, or is a relay carrying it?**

Two endpoints behind NAT either hole-punched their way to a direct path or are
being relayed, and for anything that moves real volume the difference decides
whether it is a product or a disappointment. Plex meters relayed video below SD.
Tailscale calls relaying-when-direct-was-possible one of the most common causes
of performance issues, and shipped peer relays to get traffic off its own relay
fleet. Both treat the relay as a correctness floor and never as the path the
bytes should take.

```c
int32_t apple_iroh_start(void);
int32_t apple_iroh_endpoint_id(char *buf, int32_t cap);
int32_t apple_iroh_connect(const char *id_hex);   /* blocks */
int32_t apple_iroh_path(const char *id_hex);      /* RELAY | DIRECT */
int32_t apple_iroh_relay(const char *id_hex, char *buf, int32_t cap);
void    apple_iroh_stop(void);
```

`apple_iroh_path` returns a **bitmask, not an enum**, because both bits can be
set at once — and that state is the interesting one. These stacks normally come
up on the relay and upgrade once hole punching lands, so a sample showing both
is the upgrade in progress. Poll it on a clock and you get *time to direct*,
which is a more useful number than whether it ever got there: a session relayed
for its first eight seconds and direct afterwards is a different thing from one
relayed for eight seconds every time it reconnects, and both answer "direct:
yes".

Conventions across the boundary are in `include/apple_iroh.h`. In short: every
call returns `int32_t`, negative is an error code, nothing panics across the
line, ids are 64 hex characters, and string-out functions refuse to truncate.

## Consuming it

```bash
VERSION=v0.1.0
curl -fsSL -O "https://github.com/misterbisson/apple-iroh/releases/download/$VERSION/AppleIroh.xcframework.zip"
curl -fsSL -O "https://github.com/misterbisson/apple-iroh/releases/download/$VERSION/AppleIroh.xcframework.zip.sha256"
shasum -a 256 -c <(echo "$(cat AppleIroh.xcframework.zip.sha256)  AppleIroh.xcframework.zip")
unzip -q AppleIroh.xcframework.zip
```

**Pin the checksum, not only the tag.** A tag can be moved and a release asset
can be replaced; a checksum cannot. A fetch step that verifies nothing will
happily install something else.

Link against `CoreFoundation`, `Security`, `SystemConfiguration`, `Network` and
`c++`. `Network` is not optional: iroh carries an iOS-specific path monitor
calling `nw_path_monitor_*`, which is also what notices a device moving from
Wi-Fi to cellular.

## Building it yourself

```bash
rustup target add aarch64-apple-ios aarch64-apple-ios-sim aarch64-apple-darwin
./build-xcframework.sh
```

The same script CI runs. It writes `build/AppleIroh.xcframework`, the zip, and
the checksum.

## Sizes, measured

Release profile is `opt-level = "z"`, LTO, one codegen unit, `panic = "abort"` —
the consumer is an app binary and size is what is being traded for.

| | |
|---|---|
| `libapple_iroh.a`, one architecture | ~14.0 MB |
| `libapple_iroh.a`, universal slice | ~28 MB |
| `AppleIroh.xcframework` | 67 MB |
| `AppleIroh.xcframework.zip` | **21.5 MB** |

The zip was 12.8 MB at v0.1.1, when every slice was arm64 alone. Carrying
x86\_64 for macOS and the Simulator is what the other 8.7 MB is.

What an app *gains* is smaller than the archive, because the linker drops what
nothing references. Measured separately against a trivial `main`, linked and
stripped: **17,347,264 bytes** for `ios-arm64`, 17,350,040 for the simulator
and 17,670,936 for `macos-arm64` — that build being iroh's own default profile
rather than the size-tuned one here, and measured at v0.1.1 against the
arm64-only slices. An app that ships universal pays roughly twice that.

## Versions

`Cargo.toml` pins iroh **exactly**, and `Cargo.lock` is committed. The artifact
is a binary other people build against, so "whatever resolves today" is not a
dependency specification.

Dependabot is expected and wanted here, unlike in a repository where a bump
would silently move a recorded measurement. A bump means: merge, tag, release,
and then whoever pins it re-pins and re-measures anything that depended on the
old one.

**The build is reproducible for a given Xcode, and getting there took three
fixes, each found by measuring rather than reasoning.**

1. `rust-toolchain.toml` pins the compiler. A laptop build and a CI build of one
   commit differed on rustc version alone.
2. The zip records each file's modification time, and those are when the build
   ran. `build-xcframework.sh` normalises them and adds members in sorted order.
3. **`xcodebuild -create-xcframework` writes its own index in a
   nondeterministic order.** Two CI runs of one commit produced *byte-identical*
   `libapple_iroh.a` for all three slices and different zips; the whole
   difference was `Info.plist` listing the same three slices as macos/sim/ios
   one run and sim/macos/ios the next. The order carries no meaning — Xcode
   matches a slice on platform and architecture, never position — so the script
   sorts `AvailableLibraries` by `LibraryIdentifier`.

Each fix alone leaves a checksum that changes when nothing changed, which cannot
verify anything.

**Xcode is deliberately not pinned.** SDK moves get fixed in code rather than
frozen out, which is a standing rule in the consuming project. So a reproduction
is exact for a given Xcode, and each release records which one built it.

`IOS_MIN` and `MACOS_MIN` in `build-xcframework.sh` are the deployment floors.
Raising either breaks a consumer that has not raised its own, so it is a version
bump here.

## Licence

MIT — see `LICENSE`. The artifact carries compiled third-party code whose
licences travel with it; see `NOTICE`.
