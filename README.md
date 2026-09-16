# apple-iroh

**[iroh](https://github.com/n0-computer/iroh) as a static library for Apple
platforms, behind a small C ABI.**

One tagged release publishes `AppleIroh.xcframework.zip` with three slices —
`ios-arm64`, `ios-arm64-simulator`, `macos-arm64` — and the header to call
them. An app fetches the zip, checks the sha256, and links it.

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
| `libapple_iroh.a`, per slice | ~14.0 MB |
| `AppleIroh.xcframework` | 40 MB |
| `AppleIroh.xcframework.zip` | **12.8 MB** |

What an app *gains* is smaller than the archive, because the linker drops what
nothing references. Measured separately against a trivial `main`, linked and
stripped: **17,347,264 bytes** for `ios-arm64`, 17,350,040 for the simulator
and 17,670,936 for `macos-arm64` — that build being iroh's own default profile
rather than the size-tuned one here.

## Versions

`Cargo.toml` pins iroh **exactly**, and `Cargo.lock` is committed. The artifact
is a binary other people build against, so "whatever resolves today" is not a
dependency specification.

Dependabot is expected and wanted here, unlike in a repository where a bump
would silently move a recorded measurement. A bump means: merge, tag, release,
and then whoever pins it re-pins and re-measures anything that depended on the
old one.

**The build is reproducible for a given Xcode, and that took two fixes.**
`rust-toolchain.toml` pins the compiler — a laptop build and a CI build of one
commit differed on rustc version alone. That was not enough: two CI runs of the
same commit on the same pinned toolchain *still* differed, because a zip records
each file's modification time and those are when the build ran. So
`build-xcframework.sh` normalises timestamps and member order before zipping.
Either fix alone leaves a checksum that changes when nothing changed.

**Xcode is deliberately not pinned.** SDK moves get fixed in code rather than
frozen out, which is a standing rule in the consuming project. So a reproduction
is exact for a given Xcode, and each release records which one built it.

`IOS_MIN` and `MACOS_MIN` in `build-xcframework.sh` are the deployment floors.
Raising either breaks a consumer that has not raised its own, so it is a version
bump here.

## Licence

MIT — see `LICENSE`. The artifact carries compiled third-party code whose
licences travel with it; see `NOTICE`.
