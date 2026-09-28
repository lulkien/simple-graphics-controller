# Build and package recipes. The reasoning behind them - linkage per target, what
# each package installs, the abuild details - is in docs/packaging.md.

BINS := "simple-graphics-controller"

# The release targets this repo ships: static musl for boards and Alpine,
# dynamic glibc for the boards that run Debian.
MUSL_X86_64 := "x86_64-unknown-linux-musl"
MUSL_AARCH64 := "aarch64-unknown-linux-musl"
GNU_AARCH64 := "aarch64-unknown-linux-gnu"

# Dynamic release build for the machine you are on.
default: build
build:
    cargo build --release --workspace

# --- build and dist, per target ---------------------------------------------
# Two things are derived from the triple rather than enumerated per target: the
# strip tool (binutils strips an aarch64 ELF the same way whether musl or glibc
# linked it) and the linker for a cross build (see below).

# Build one target triple.
build-target triple:
    #!/usr/bin/env bash
    set -euo pipefail
    triple="{{triple}}"
    if [[ $triple == *-unknown-linux-gnu && ${triple%%-*} != "$(uname -m)" ]]; then
        # Cross-linking to glibc needs a linker for the target's libc. zigbuild
        # brings its own libc and linker, so no cross gcc is installed and
        # .cargo/config.toml has no linker line for this triple.
        exec cargo zigbuild --release --target "$triple" --workspace
    fi
    exec cargo build --release --target "$triple" --workspace

# Build, strip and copy the binaries into ./dist.
dist-target triple: (build-target triple)
    #!/usr/bin/env bash
    set -euo pipefail
    arch=$(cut -d- -f1 <<<"{{triple}}")
    mkdir -p dist
    for bin in {{BINS}}; do
        cp "target/{{triple}}/release/$bin" dist/
        "${arch}-linux-gnu-strip" "dist/$bin"
        file "dist/$bin"
    done

build-musl-x86_64: (build-target MUSL_X86_64)
build-musl-aarch64: (build-target MUSL_AARCH64)
build-gnu-aarch64: (build-target GNU_AARCH64)

dist-musl-x86_64: (dist-target MUSL_X86_64)
dist-musl-aarch64: (dist-target MUSL_AARCH64)
dist-gnu-aarch64: (dist-target GNU_AARCH64)

# --- .deb: daemon binary + systemd unit -------------------------------------
# The flavor names are cargo-deb's, declared in Cargo.toml. An empty argument
# means "no flag": the host build has neither a target nor a variant, and the
# host and the cross flavors must not share one output name.

# Package one cargo-deb flavor (target and variant may be empty).
package-deb target="" variant="":
    #!/usr/bin/env bash
    set -euo pipefail
    args=()
    [ -n "{{target}}" ] && args+=(--target "{{target}}")
    [ -n "{{variant}}" ] && args+=(--variant "{{variant}}")
    cargo deb --no-build "${args[@]}"

package-gnu-x86_64: (build) (package-deb "" "")
package-musl-x86_64: (build-musl-x86_64) (package-deb MUSL_X86_64 "musl")
package-gnu-aarch64: (build-gnu-aarch64) (package-deb GNU_AARCH64 "gnu-aarch64")
package-musl-aarch64: (build-musl-aarch64) (package-deb MUSL_AARCH64 "musl")

# All four flavors into target/debian/.
packages: package-gnu-x86_64 package-musl-x86_64 package-gnu-aarch64 package-musl-aarch64
    @ls -1 target/debian/*.deb

# --- .apk: daemon binary + OpenRC service -----------------------------------
# Packaging only: abuild takes the binary dist-target produced. It runs natively
# where it exists (an Alpine board), otherwise in an alpine container. Not part
# of `just packages`, so docker never becomes a prerequisite of the gate.

# Build the .apk for one arch (aarch64 or x86_64).
package-apk arch:
    scripts/build-apk.sh {{arch}}

package-apk-aarch64: (dist-musl-aarch64) (package-apk "aarch64")
package-apk-x86_64: (dist-musl-x86_64) (package-apk "x86_64")

# --- inspect, clean, gate ---------------------------------------------------
# What a package installs and declares, .deb or .apk.
info file:
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{file}}" in
        *.deb)
            dpkg-deb -c "{{file}}" | grep -E "usr/bin|systemd" || true
            dpkg-deb -I "{{file}}" | grep -E "^ (Package|Version|Architecture|Depends)" || true
            ;;
        *.apk)
            tar -xzOf "{{file}}" .PKGINFO
            tar -tzvf "{{file}}" | grep -vE "\.(SIGN|PKGINFO)$" | sed 's/^/  /'
            ;;
        *)
            echo "not a .deb or .apk: {{file}}" >&2
            exit 1
            ;;
    esac

clean:
    rm -rf target dist

# The gate: format, lints, tests, then every .deb flavor.
ci: check test packages
check:
    cargo fmt --check
    cargo clippy --workspace --all-targets -- -D warnings
test:
    cargo test --workspace
