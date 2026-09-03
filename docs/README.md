# simple-graphics-controller

Graphics resource controller for Linux. A daemon owns graphics resources and
hands them out to clients over an abstract Unix socket (`@sgc`); a client
library plus demo clients draw via the granted fd (fbdev mmap or a DRM lease).

> Paths in this document are relative to the repository root.

## Workspace layout

| crate                          | description                                        |
| ------------------------------ | -------------------------------------------------- |
| `simple-graphics-controller`   | daemon: owns resources, serves clients on `@sgc`   |
| `simple-graphics-protocol`     | shared protocol: messages, serialization (msgpack) |
| `libsgc-rs`                    | client library: connect, acquire, revoke/regrant   |
| `libsgc-c`                     | C ABI shim over the core (libsgc.h / sgc.hpp)      |
| `rust-samples/sgc-drm-client`  | demo client: acquires a DRM card, modesets on it   |
| `rust-samples/sgc-fbdev-client`| demo client: acquires the fbdev resource, draws    |

The Rust demo clients live under `rust-samples/` (cargo workspace members —
built by `just build` / `just dist-*` like any crate). The C and C++ sample
clients live under `c-samples/` (meson, see below).

The wire format is specified in [PROTOCOL.md](PROTOCOL.md) — the reference for
implementing clients in other languages (e.g. kmscube's C lease client).

Design documents:

- [resource-manager.md](resource-manager.md) — backend features
  (fbdev/drm/input), registries, DRM lease state machine
- [policy-engine.md](policy-engine.md) — windowing policy engine:
  preemption, waiter queues, policies
- [libsgc.md](libsgc.md) — client library: one Rust core, Rust + C ABI +
  C++ faces (pump-based)

## Backends are Cargo features

The server is built with a chosen set of backends (see
[resource-manager.md](resource-manager.md)):

- `drm` — DRM cards as lease factories (**default**)
- `input` — evdev devices (**default**)
- `fbdev` — `/dev/fb0` (opt-in, legacy)

```sh
cargo build -p simple-graphics-controller                          # drm + input
cargo build -p simple-graphics-controller --all-features           # + fbdev
```

## Development environment

Builds target **aarch64-unknown-linux-gnu** (64-bit ARM Linux, e.g. the H618
boards) and run natively on an x86_64 host. The host cross-toolchain does the
whole job — no qemu, no container needed.

### Host packages (Debian trixie)

```sh
sudo dpkg --add-architecture arm64
sudo apt update
sudo apt install -y \
    gcc-aarch64-linux-gnu \
    binutils-aarch64-linux-gnu \
    pkg-config \
    libfreetype-dev:arm64 \
    libfontconfig-dev:arm64 \
    libexpat1-dev:arm64
```

The `:arm64` dev packages are needed by the fbdev demo clients: `linfb` →
`font-loader` links against arm64 libfreetype/libfontconfig/libexpat. They are
picked up via pkg-config, so the build must point at the arm64 pkg-config dir
(see below). The `simple-graphics-controller` daemon itself has no C library
dependencies and builds without them.

### Rust toolchain

```sh
curl https://sh.rustup.rs -sSf | sh          # stable toolchain
rustup target add aarch64-unknown-linux-gnu
```

The repo's `.cargo/config.toml` already sets the linker
(`aarch64-linux-gnu-gcc`) for the aarch64 target.

## Build

Use the `just` recipes:

```sh
just build                 # host dynamic release build (workspace)
just build-gnu-aarch64     # dynamic aarch64 (gnu) build
just build-musl-aarch64    # fully static aarch64 musl build
just dist-gnu-aarch64      # aarch64 build + strip + copy into ./dist
just clean                 # remove ./target and ./dist
```

Linkage convention: musl targets are fully static; gnu targets are
dynamically linked against the platform libc (the board runs glibc).
`just build-gnu-aarch64` needs the arm64 -dev packages on the build host
only for the fbdev demo's font libs, which are linked statically via
pkg-config (`libfreetype-dev:arm64`, `libfontconfig-dev:arm64`,
`libexpat1-dev:arm64`, `libpng-dev:arm64`, `zlib1g-dev:arm64`,
`libbrotli-dev:arm64`, `libbz2-dev:arm64`); the daemon itself links nothing
but glibc.

### Cross toolchains

All three targets add their Rust std first (`rustup target add
aarch64-unknown-linux-gnu aarch64-unknown-linux-musl x86_64-unknown-linux-musl`).
`.cargo/config.toml` names the C linkers it expects on `PATH`
(`aarch64-unknown-linux-gnu-gcc`, `aarch64-unknown-linux-musl-gcc`);
symlink them into `~/.cargo/bin`.

- **aarch64 gnu (dynamic)**: Debian packages `gcc-aarch64-linux-gnu`,
  `binutils-aarch64-linux-gnu`, plus the `:arm64` -dev font packages above.
  Symlink `~/.cargo/bin/aarch64-unknown-linux-gnu-gcc` ->
  `/usr/bin/aarch64-linux-gnu-gcc`.
- **aarch64 musl (fully static)**: the cross gcc comes from musl.cc
  (<https://musl.cc/aarch64-linux-musl-cross.tgz>): extract it (creates
  `~/aarch64-linux-musl-cross/`), then symlink
  `~/.cargo/bin/aarch64-unknown-linux-musl-gcc` ->
  `~/aarch64-linux-musl-cross/bin/aarch64-linux-musl-gcc`. The font -sys
  crates build their vendored sources for musl, so no system font libs are
  needed. (musl.cc toolchains are self-contained — no root required.)
- **x86_64 musl (fully static)**: no cross gcc needed — `crt-static` (from
  `.cargo/config.toml`) links against the musl objects rustup installed, via
  the host `cc`.
- **strip**: the dist recipes use `x86_64-linux-gnu-strip` and
  `aarch64-linux-gnu-strip` (binutils / binutils-aarch64-linux-gnu).

Equivalent plain cargo for the daemon alone (no font deps):

```sh
cargo build --release -p simple-graphics-controller \
    --target aarch64-unknown-linux-gnu
```

### Output

```
target/aarch64-unknown-linux-gnu/release/simple-graphics-controller   # daemon
target/aarch64-unknown-linux-gnu/release/sgc-drm-client               # DRM demo
target/aarch64-unknown-linux-gnu/release/sgc-fbdev-client             # fbdev demo
dist/                                                                 # stripped copies
```

## Packaging (.deb)

`cargo-deb` (install once: `cargo install cargo-deb`) builds two installable
packages per architecture from the release artifacts:

| package                   | contents                                                            | Multi-Arch |
| ------------------------- | ------------------------------------------------------------------- | ---------- |
| simple-graphics-controller| daemon (`/usr/bin`) + runtime `libsgc.so` (`/usr/lib`)               | no — ships a bin |
| libsgc-dev                | `libsgc.h` + `sgc.hpp` (`/usr/include`) + static `libsgc.a` (multiarch lib dir); Depends on the server package | same |

```sh
just deb                # amd64 packages into target/debian/
just deb-gnu-aarch64    # arm64 packages into target/debian/
```

The demo/sample clients (`rust-samples/`, `c-samples/`) are deliberately NOT
packaged — build them from source if you want to try one.

Debian policy forbids a Multi-Arch: same package from mixing a `/usr/bin`
binary with a library, so only `libsgc-dev` is built with `--multiarch`
(its `.a` lands in `/usr/lib/<triple>/`); the server package installs the
`.so` at plain `/usr/lib` and is not multiarch. Both recipes run with
`--no-build` — they package whatever the matching
`build`/`build-gnu-aarch64` produced, resolved through the magic
`target/release` asset prefix (see the manifests' comments). The arm64 pair
is install-tested on the dev board (single `dpkg -i`, daemon runs from
`/usr/bin`).

## Runtime dependencies on the target board

- **Server (musl build)**: none — fully static. The gnu/dynamic builds
  need glibc (present on Armbian).
- **Demo clients (musl)**: none. The aarch64 gnu builds embed the font
  libs statically (pkg-config `--static`); the host's `just build` fbdev
  demo links them dynamically and needs `libfontconfig.so.1` etc. (see the
  dist `file` output).
- **DRM lease clients** (e.g. kmscube built with `-L`): need
  libdrm/gbm/EGL/GLES on the board; they talk to `@sgc`, not the card.

## Usage

1. Start the daemon on the board: `./simple-graphics-controller` (built with
   `drm` + `input` by default).
2. Run a client: `./sgc-drm-client` (acquires the first advertised DRM card),
   `./sgc-fbdev-client` (needs a server built with `--all-features` and a
   working framebuffer), an external lease client like `kmscube -L -A -N`,
   or one of the C/C++ sample clients below.

## C/C++ sample clients (c-samples/)

`c-samples/` holds two minimal sample clients over the libsgc C ABI — one in
C (`sgc-drm-c`, libsgc.h) and one in C++ (`sgc-drm-cpp`, sgc.hpp) — built
with meson, linking `libsgc.a` statically. Each connects, acquires the first
advertised DRM card, and draws a simple animated pattern on the lease fd
(dumb buffer + SETCRTC via raw ioctls using the kernel DRM UAPI headers —
no GBM/EGL, no libdrm linked). The patterns differ
(cycling gradient + orange square vs phase-shifting checkerboard + cyan
stripe) so the two are distinguishable on the display. Both survive the
revoke/requeue/re-grant cycle like the Rust demo client.

```sh
# host build (debug profile picks up libsgc.a from ../target/debug —
# build it first with `cargo build -p libsgc-c`; use -Dbuildtype=release
# and `just build` for a release lib)
cd c-samples && meson setup build && meson compile -C build

# board build (static libsgc.a from the aarch64 workspace build,
# i.e. `just build-gnu-aarch64` -> target/aarch64-unknown-linux-gnu/release)
cd c-samples
meson setup build-aarch64 --cross-file=aarch64-cross.txt -Dbuildtype=release \
  -Dsgc_dir=/abs/path/to/simple-graphics-controller
meson compile -C build-aarch64
```

Logging is controlled by `RUST_LOG` (default: `info`):

```sh
./simple-graphics-controller                              # info: lifecycle events
RUST_LOG=debug ./simple-graphics-controller               # + message details, fds, peer creds
RUST_LOG=trace ./simple-graphics-controller               # + raw byte counts
```

Windowing policy is selected by `SGC_POLICY` (default `fair-queue`):

```sh
SGC_POLICY=fair-queue ./simple-graphics-controller        # default: preempt + FIFO waiters
SGC_POLICY=latest-owner ./simple-graphics-controller      # newest opener wins (LIFO)
SGC_POLICY=first-owner ./simple-graphics-controller       # first holder keeps it; others denied
```

## Development board

Test/deploy target: `root@10.21.50.53` (aarch64). Copy binaries with `scp`
(to `~` for tests), run the server, then exercise grant/revoke/release with
the demo clients or kmscube `-L`. DRM cards are registered in priority order
(connected first); not every card can lease (see resource-manager.md).
