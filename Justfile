# Build recipes. Host builds dynamic, target builds fully static.
#
#   just build               dynamic release build for the current host (fast iteration)
#   just build-gnu-aarch64   fully static aarch64 (gnu) build
#   just build-musl-x86_64   fully static x86_64 musl build
#   just dist-gnu-aarch64    aarch64 build + strip + copy into ./dist
#   just dist-musl-x86_64    musl build + strip + copy into ./dist
#   just clean               remove ./target and ./dist

TARGET := "aarch64-unknown-linux-gnu"

# Dynamic host build (static glibc would drag in png/z/brotli deps and breaks
# proc-macros; the static builds live in build-musl-x86_64 / build-gnu-aarch64).
build:
    cargo build --release --workspace

# arm64 system libs (freetype/fontconfig/expat for linfb) live in the arm64
# pkg-config dir; allow cross-linking against them. crt-static comes from
# .cargo/config.toml, so both targets build fully static.
pkg_env := 'PKG_CONFIG_ALLOW_CROSS=1 PKG_CONFIG_PATH=/usr/lib/aarch64-linux-gnu/pkgconfig'

# Fully static aarch64 build: crt-static (from .cargo/config.toml) + pkg-config
# --static (all font libs as .a). Runs on any aarch64 Linux, no installs needed.
build-gnu-aarch64:
    {{pkg_env}} PKG_CONFIG_ALL_STATIC=1 cargo build --release --target {{TARGET}} --workspace

# Fully static x86_64 musl build. crt-static comes from .cargo/config.toml.
# Runs on any Alpine x86_64, zero deps.
build-musl-x86_64:
    cargo build --release --target x86_64-unknown-linux-musl --workspace

# aarch64 build + strip + copy into ./dist.
dist-gnu-aarch64: build-gnu-aarch64
    mkdir -p dist
    cp target/{{TARGET}}/release/simple-graphics-controller dist/
    cp target/{{TARGET}}/release/sgc-fbdev-client dist/
    aarch64-linux-gnu-strip dist/simple-graphics-controller dist/sgc-fbdev-client
    file dist/simple-graphics-controller dist/sgc-fbdev-client

# musl build + strip + copy into ./dist.
dist-musl-x86_64: build-musl-x86_64
    mkdir -p dist
    cp target/x86_64-unknown-linux-musl/release/simple-graphics-controller dist/
    cp target/x86_64-unknown-linux-musl/release/sgc-fbdev-client dist/
    x86_64-linux-gnu-strip dist/simple-graphics-controller dist/sgc-fbdev-client
    file dist/simple-graphics-controller dist/sgc-fbdev-client

# Remove local build output.
clean:
    rm -rf target dist
