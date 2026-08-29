# simple-graphics-controller

Graphics resource controller for Linux. A daemon owns graphics resources and
hands them out to clients over an abstract Unix socket (`@sgc`); a client
library plus a framebuffer demo client draw via `linfb`.

> Paths in this document are relative to the repository root.

## Workspace layout

| crate                      | description                                        |
| -------------------------- | -------------------------------------------------- |
| `simple-graphics-controller` | daemon: owns resources, serves clients on `@sgc`   |
| `simple-graphics-protocol`   | shared protocol: messages, serialization (msgpack) |
| `sgc-fbdev-client`           | demo client: acquires the fbdev resource, draws    |

The wire format is specified in [PROTOCOL.md](PROTOCOL.md) — the reference for
implementing clients in other languages (e.g. the planned C library).

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

The `:arm64` dev packages are needed by `sgc-fbdev-client`: `linfb` →
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
(`aarch64-linux-gnu-gcc`) for the aarch64 target, and `zig` for the optional
`x86_64-unknown-linux-musl` target (zig is only needed if you build that one).

## Build

Two build modes. `dist-static` is the recommended one for deployment.

### Static build (recommended — nothing needed on the board)

Fully static binaries: static glibc (`crt-static`) + all font libraries linked
statically via `pkg-config --static`. They run on any aarch64 Linux with no
packages installed.

```sh
just dist-static     # build + strip + copy into ./dist
```

Equivalent plain cargo:

```sh
PKG_CONFIG_ALLOW_CROSS=1 PKG_CONFIG_ALL_STATIC=1 \
PKG_CONFIG_PATH=/usr/lib/aarch64-linux-gnu/pkgconfig \
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C target-feature=+crt-static" \
    cargo build --release --target aarch64-unknown-linux-gnu --workspace
```

Requires the static archive variants of the font libs on the build host
(`libfreetype-dev:arm64`, `libfontconfig-dev:arm64`, `libexpat1-dev:arm64`,
`libpng-dev:arm64`, `zlib1g-dev:arm64`, `libbrotli-dev:arm64`,
`libbz2-dev:arm64`, `libc6-dev:arm64`).

### Dynamic build (fast iteration)

Two env vars are required — without them `servo-fontconfig-sys` can't find the
arm64 fontconfig and falls back to building it from source, which fails when
cross-compiling:

```sh
export PKG_CONFIG_ALLOW_CROSS=1
export PKG_CONFIG_PATH=/usr/lib/aarch64-linux-gnu/pkgconfig
```

```sh
just build               # release build -> target/aarch64-unknown-linux-gnu/release
just build BUILD=debug   # debug build
just dist                # build + strip + copy into ./dist
```

### Plain cargo (dynamic)

```sh
PKG_CONFIG_ALLOW_CROSS=1 PKG_CONFIG_PATH=/usr/lib/aarch64-linux-gnu/pkgconfig \
    cargo build --release --target aarch64-unknown-linux-gnu --workspace
```

### Output

```
target/aarch64-unknown-linux-gnu/release/simple-graphics-controller   # daemon
target/aarch64-unknown-linux-gnu/release/sgc-fbdev-client             # demo client
```

## Runtime dependencies on the target board

- **Static build**: none. The binaries are self-contained.
- **Dynamic build**: `sgc-fbdev-client` links `libfontconfig.so.1`, so the
  board needs `libfontconfig1 libfreetype6 libexpat1`. The daemon only needs
  the base glibc.

## Usage

1. Start the daemon on the board: `./simple-graphics-controller`
2. Run the client: `./sgc-fbdev-client` (needs a working framebuffer device)

Logging is controlled by `RUST_LOG` (default: `info`):

```sh
./simple-graphics-controller                              # info: lifecycle events
RUST_LOG=debug ./simple-graphics-controller               # + message details, fds, peer creds
RUST_LOG=trace ./simple-graphics-controller               # + raw byte counts
```
