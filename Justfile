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
#   just packages            all four installable .debs (binary + systemd unit)
#   just ci                  the gate: fmt + clippy + tests + packages
#   just clean               remove ./target and ./dist

# All shipped binaries; the dist recipes copy/strip/file exactly these.
# (The demo clients moved to the sgc-demos repo; only the daemon ships here.)
BINS := "simple-graphics-controller"

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

# --- packages: installable .debs (daemon binary + systemd unit) ------------
#
# Four flavors, one per target the fleet runs:
#   package-gnu-x86_64    amd64, dynamically linked glibc - workstations
#   package-musl-x86_64   amd64, fully static musl - workstations without glibc
#   package-gnu-aarch64   arm64, dynamically linked glibc - the boards
#   package-musl-aarch64  arm64, fully static musl - boards whose glibc differs
#
# Every package installs /usr/bin/simple-graphics-controller and
# /lib/systemd/system/simple-graphics-controller.service, enables the service at
# boot and starts it (see debian/postinst). The glibc and musl flavors are
# alternatives, not companions: they own the same paths. The daemon package is
# daemon-only since the repo split; the client library packaging (libsgc-dev
# headers + libsgc.a, runtime libsgc.so) lives in the libsgc-c repo.
#
# Cross packaging cannot run dpkg-shlibdeps against the target's libraries, so
# the variants in Cargo.toml state each flavor's runtime dependency explicitly.
#
#   just packages                all four .debs into target/debian/
#   just package-musl-aarch64    just one
#   just deb-info <file.deb>     what a package installs and depends on

# Each flavor's package name (and therefore its output filename) differs, so the
# packages cannot collide: cargo-deb derives the filename from name_version_arch
# whatever --variant says, and it rewrites target/debian when it packages -
# renaming afterwards is a race the next flavor wins. The glibc and musl builds
# for one architecture share nothing but the version; the musl package carries its
# own name and declares itself an alternative to the glibc package (see the
# variants in Cargo.toml).
package-gnu-x86_64: build
    cargo deb --no-build

package-musl-x86_64: build-musl-x86_64
    cargo deb --no-build --target {{TARGET_MUSL_AMD64}} --variant musl

package-gnu-aarch64: build-gnu-aarch64
    cargo deb --no-build --target {{TARGET_GNU_AARCH64}} --variant gnu-aarch64

package-musl-aarch64: build-musl-aarch64
    cargo deb --no-build --target aarch64-unknown-linux-musl --variant musl

packages: package-gnu-x86_64 package-musl-x86_64 package-gnu-aarch64 package-musl-aarch64
    @echo "built:"
    @ls -1 target/debian/*.deb

# Show what a package installs and what it depends on.
deb-info file:
    dpkg-deb -c {{file}} | grep -E "usr/bin|systemd" || true
    dpkg-deb -I {{file}} | grep -E "^ (Package|Version|Architecture|Depends)" || true

# --- the gate ---------------------------------------------------------------
# Same on a workstation and on a runner: format, lints, tests, then build every
# package flavor, leaving the .debs in target/debian/.
ci: check test packages

check:
    cargo fmt --check
    cargo clippy --workspace --all-targets -- -D warnings

test:
    cargo test --workspace
