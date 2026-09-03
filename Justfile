# Build & deploy recipes. Linkage convention: musl builds are fully static,
# gnu builds are dynamically linked (the board runs glibc).
#
#   just                     build (dynamic release for the current host)
#   just build-musl-x86_64   fully static x86_64 musl build
#   just build-musl-aarch64  fully static aarch64 musl build
#   just build-gnu-aarch64   dynamic aarch64 (gnu) build
#   just dist-musl-x86_64    musl build + strip + copy into ./dist
#   just dist-musl-aarch64   aarch64 musl build + strip + copy into ./dist
#   just dist-gnu-aarch64    aarch64 build + strip + copy into ./dist
#   just clean               remove ./target and ./dist

# All shipped binaries; the dist recipes copy/strip/file exactly these.
BINS := "simple-graphics-controller sgc-fbdev-client sgc-drm-client"

TARGET_GNU_AARCH64 := "aarch64-unknown-linux-gnu"
TARGET_MUSL_AMD64 := "x86_64-unknown-linux-musl"
STRIP_GNU_AARCH64 := "aarch64-linux-gnu-strip"
STRIP_MUSL_AMD64 := "x86_64-linux-gnu-strip"

# Dynamic host build (static glibc would drag in png/z/brotli deps and breaks
# proc-macros; the static builds live in build-musl-x86_64 / build-musl-aarch64).
default: build

build:
    cargo build --release --workspace

# Fully static x86_64 musl build. crt-static comes from .cargo/config.toml.
# Runs on any Alpine x86_64, zero deps.
build-musl-x86_64:
    cargo build --release --target {{TARGET_MUSL_AMD64}} --workspace

# arm64 system libs (freetype/fontconfig/expat for linfb) live in the arm64
# pkg-config dir; allow cross-linking against them (static .a for the font
# -sys crates in the demo clients).
pkg_env := 'PKG_CONFIG_ALLOW_CROSS=1 PKG_CONFIG_PATH=/usr/lib/aarch64-linux-gnu/pkgconfig'

# Dynamically linked aarch64 (gnu) build — the board runs glibc.
build-gnu-aarch64:
    {{pkg_env}} PKG_CONFIG_ALL_STATIC=1 cargo build --release --target {{TARGET_GNU_AARCH64}} --workspace

# Fully static aarch64 musl build (musl.cc toolchain via ~/.cargo/bin
# symlinks). The font -sys crates build vendored sources for musl targets,
# so no system font libs are needed. Runs on any Alpine aarch64.
build-musl-aarch64:
    cargo build --release --target aarch64-unknown-linux-musl --workspace

# Shared dist step: copy the target's release binaries into ./dist, strip
# them, print what we shipped. Parameterized by target triple + strip tool
# so both dist recipes stay identical.
dist-copy target strip:
    mkdir -p dist
    for bin in {{BINS}}; do cp target/{{target}}/release/$bin dist/; {{strip}} dist/$bin; file dist/$bin; done

# musl build + strip + copy into ./dist.
dist-musl-x86_64: build-musl-x86_64
    just dist-copy {{TARGET_MUSL_AMD64}} {{STRIP_MUSL_AMD64}}

# aarch64 musl build + strip + copy into ./dist
dist-musl-aarch64: build-musl-aarch64
    just dist-copy aarch64-unknown-linux-musl aarch64-linux-gnu-strip

# aarch64 build + strip + copy into ./dist.
dist-gnu-aarch64: build-gnu-aarch64
    just dist-copy {{TARGET_GNU_AARCH64}} {{STRIP_GNU_AARCH64}}

# Remove local build output.
clean:
    rm -rf target dist

# --- Debian packages (cargo-deb; install with `cargo install cargo-deb`) ---
# Produces two packages in target/debian/ per architecture:
#   simple-graphics-controller  daemon + runtime libsgc.so
#   libsgc-dev                  headers + static libsgc.a (depends on the above)
# The demo/sample clients are deliberately NOT packaged — build them from
# source (rust-samples/, c-samples/).
# deb              host packages (amd64)
# deb-gnu-aarch64  board packages (arm64)

deb: build
    cargo deb --manifest-path simple-graphics-controller/Cargo.toml --no-build --multiarch=same
    cargo deb --manifest-path libsgc-c/Cargo.toml --no-build --multiarch=same

deb-gnu-aarch64: build-gnu-aarch64
    # Dynamic gnu build produces the runtime libsgc.so and libsgc.a natively.
    cargo deb --target {{TARGET_GNU_AARCH64}} --no-build --multiarch=same --manifest-path simple-graphics-controller/Cargo.toml
    cargo deb --target {{TARGET_GNU_AARCH64}} --no-build --multiarch=same --manifest-path libsgc-c/Cargo.toml
