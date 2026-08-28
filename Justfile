# Cross-compile for aarch64 natively on this x86_64 host.
#
#   just build              dynamic release build (fast iteration)
#   just build BUILD=debug  debug build
#   just build-static       fully static release build (nothing needed on the board)
#   just dist               dynamic build + strip + copy into ./dist
#   just dist-static        static build + strip + copy into ./dist (recommended)
#   just clean              remove ./target and ./dist

TARGET := "aarch64-unknown-linux-gnu"

BUILD := "release"
cargo_mode := if BUILD == "debug" { "" } else { "--release" }

# arm64 system libs (freetype/fontconfig/expat for linfb) live in the arm64
# pkg-config dir; allow cross-linking against them
pkg_env := 'PKG_CONFIG_ALLOW_CROSS=1 PKG_CONFIG_PATH=/usr/lib/aarch64-linux-gnu/pkgconfig'

# Dynamic build: links glibc + libfontconfig etc. at runtime; board needs the
# matching shared libraries installed.
build:
    {{pkg_env}} cargo build {{cargo_mode}} --target {{TARGET}} --workspace

# Fully static build: crt-static (static glibc) + pkg-config --static (all font
# libs as .a). The binaries then run on any aarch64 Linux, no installs needed.
build-static:
    {{pkg_env}} PKG_CONFIG_ALL_STATIC=1 CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C target-feature=+crt-static" cargo build {{cargo_mode}} --target {{TARGET}} --workspace

# Dynamic build + strip + copy into ./dist.
dist: build
    mkdir -p dist
    cp target/{{TARGET}}/release/simple-graphics-controller dist/
    cp target/{{TARGET}}/release/sgc-fbdev-client dist/
    aarch64-linux-gnu-strip dist/simple-graphics-controller dist/sgc-fbdev-client
    file dist/simple-graphics-controller dist/sgc-fbdev-client

# Static build + strip + copy into ./dist.
dist-static: build-static
    mkdir -p dist
    cp target/{{TARGET}}/release/simple-graphics-controller dist/
    cp target/{{TARGET}}/release/sgc-fbdev-client dist/
    aarch64-linux-gnu-strip dist/simple-graphics-controller dist/sgc-fbdev-client
    file dist/simple-graphics-controller dist/sgc-fbdev-client

# Remove local build output.
clean:
    rm -rf target dist
