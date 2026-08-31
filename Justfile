# Build & deploy recipes. Host builds dynamic, target builds fully static.
#
#   just                     build (dynamic release for the current host)
#   just build-gnu-aarch64   fully static aarch64 (gnu) build
#   just build-musl-x86_64   fully static x86_64 musl build
#   just dist-gnu-aarch64    aarch64 build + strip + copy into ./dist
#   just dist-musl-x86_64    musl build + strip + copy into ./dist
#   just clean               remove ./target and ./dist

# All shipped binaries; the dist recipes copy/strip/file exactly these.
BINS := "simple-graphics-controller sgc-fbdev-client sgc-fbdev-client-2"

TARGET_GNU_AARCH64 := "aarch64-unknown-linux-gnu"
TARGET_MUSL_AMD64 := "x86_64-unknown-linux-musl"
STRIP_GNU_AARCH64 := "aarch64-linux-gnu-strip"
STRIP_MUSL_AMD64 := "x86_64-linux-gnu-strip"

# Dynamic host build (static glibc would drag in png/z/brotli deps and breaks
# proc-macros; the static builds live in build-musl-x86_64 / build-gnu-aarch64).
default: build

build:
    cargo build --release --workspace

# arm64 system libs (freetype/fontconfig/expat for linfb) live in the arm64
# pkg-config dir; allow cross-linking against them. crt-static comes from
# .cargo/config.toml, so both targets build fully static.
pkg_env := 'PKG_CONFIG_ALLOW_CROSS=1 PKG_CONFIG_PATH=/usr/lib/aarch64-linux-gnu/pkgconfig'

# Fully static aarch64 build: crt-static (from .cargo/config.toml) + pkg-config
# --static (all font libs as .a). Runs on any aarch64 Linux, no installs needed.
build-gnu-aarch64:
    {{pkg_env}} PKG_CONFIG_ALL_STATIC=1 cargo build --release --target {{TARGET_GNU_AARCH64}} --workspace

# Fully static x86_64 musl build. crt-static comes from .cargo/config.toml.
# Runs on any Alpine x86_64, zero deps.
build-musl-x86_64:
    cargo build --release --target {{TARGET_MUSL_AMD64}} --workspace

# Shared dist step: copy the target's release binaries into ./dist, strip
# them, print what we shipped. Parameterized by target triple + strip tool
# so both dist recipes stay identical.
dist-copy target strip:
    mkdir -p dist
    for bin in {{BINS}}; do cp target/{{target}}/release/$bin dist/; {{strip}} dist/$bin; file dist/$bin; done

# aarch64 build + strip + copy into ./dist.
dist-gnu-aarch64: build-gnu-aarch64
    just dist-copy {{TARGET_GNU_AARCH64}} {{STRIP_GNU_AARCH64}}

# musl build + strip + copy into ./dist.
dist-musl-x86_64: build-musl-x86_64
    just dist-copy {{TARGET_MUSL_AMD64}} {{STRIP_MUSL_AMD64}}

# Remove local build output.
clean:
    rm -rf target dist
