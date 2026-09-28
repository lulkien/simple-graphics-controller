# Packaging

Install sources for the daemon: `packaging/` holds the systemd unit and the
drop-in for boards that run the binary from `/root`, plus the OpenRC init script
for Alpine; this file holds the notes. Build output lives in `dist/`
(gitignored), `.deb`s in `target/debian/` and `.apk`s in `dist/apk/`.

## What builds what

One recipe per action, and the target is an argument rather than a recipe name:

| recipe | produces |
|---|---|
| `just build` | release build for this machine |
| `just build-target <triple>` | release build for one triple |
| `just dist-target <triple>` | that build, stripped, into `dist/` |
| `just package-deb <triple> <variant>` | one `.deb` flavor (both empty: the host) |
| `just package-apk <arch>` | one `.apk` from the current `dist/` binary |

Named shortcuts (`build-musl-aarch64`, `dist-gnu-aarch64`,
`package-apk-aarch64`, `packages` for all four `.deb`s) are one line each and
carry no logic of their own.

Two things are derived instead of listed per target, because enumerating them
was how the recipes drifted apart:

* **the strip tool**: `<arch>-linux-gnu-strip` from the triple's first field.
  Binutils strips an aarch64 ELF the same way whether musl or glibc linked it,
  so one tool covers all three targets.
* **the linker for a cross build**: musl targets use the musl.cc toolchain
  (`.cargo/config.toml` names it); the gnu cross target is built by
  `cargo-zigbuild`, which brings its own libc and linker. There is no cross gcc
  on a Debian workstation - only binutils - which is why
  `.cargo/config.toml` no longer names an `aarch64-unknown-linux-gnu-gcc` that
  could not be found.

The `.deb` installs the binary to `/usr/bin` and the unit to
`/lib/systemd/system`; the drop-in is for a board that keeps the binary in
`/root` and wants no package (see the last section).

## systemd unit

`simple-graphics-controller.service` starts the daemon at boot and restarts it
if it dies. Clients are sgc-or-die, so a client unit wants:

    [Unit]
    After=simple-graphics-controller.service
    Requires=simple-graphics-controller.service

The unit runs the daemon as root because it opens `/dev/dri` and `/dev/input`
directly and takes DRM master. It deliberately does **not** set
`PrivateDevices=`/`DevicePolicy=`, which would hide the devices it brokers, and
leaves `CapabilityBoundingSet` alone for the same reason. It is ordered after
`dev-dri-card0.device`, so a boot-time start does not race udev and come up
without DRM master.

## Installing from the package

`just packages` builds four flavors into `target/debian/`:

    package-gnu-x86_64     amd64, dynamic glibc - workstations
    package-musl-x86_64    amd64, fully static musl
    package-gnu-aarch64    arm64, dynamic glibc - the boards
    package-musl-aarch64   arm64, fully static musl

Each installs `/usr/bin/simple-graphics-controller` and
`/lib/systemd/system/simple-graphics-controller.service`, and its `postinst`
reloads the unit, enables it at boot and starts it - so on a device:

    dpkg -i simple-graphics-controller_*_arm64.deb
    systemctl status simple-graphics-controller       # active, enabled

Two things to know:

* glibc and musl are **alternatives**, not companions: both own those two
  paths, so installing one over the other replaces it (and restarts the daemon).
  Each declares the conflict, so the swap works in both directions. The musl
  package is named `simple-graphics-controller-musl` because cargo-deb derives
  its output filename from name_version_arch whatever `--variant` says.
* A unit in `/etc/systemd/system/` **overrides** the packaged one in
  `/lib/systemd/system/`. If you previously hand-installed a unit or a drop-in
  (the /root recipe below), remove it before testing the package, or the packaged
  file is silently ignored.

## Alpine: .apk and OpenRC

The same cargo-built static musl binary, packaged for `apk` with the OpenRC
service instead of a systemd unit:

    just package-apk-aarch64      # board: dist/apk/aarch64/*.apk
    just package-apk-x86_64       # workstation on Alpine
    just info <file>              # what a .deb or .apk installs and declares

The `.apk` installs `/usr/bin/simple-graphics-controller` and
`/etc/init.d/simple-graphics-controller`. It compiles nothing: `abuild` packages
the artifact `just dist-musl-<arch>` already produced, which is how the `.deb`
flavors work too (`cargo deb --no-build`). `APKBUILD` in the repo root is the
definition, and it fails rather than packages the wrong thing if `dist/` holds a
binary of another architecture or one that is not statically linked.

`abuild` runs natively where it exists (an Alpine board), otherwise in an
`alpine:3.22` container of the target architecture - qemu binfmt does the work,
which is fine for reading, tarring and signing. The recipes are deliberately not
part of `just packages`/`just ci`, so docker never becomes a prerequisite of the
gate.

Installing on a board:

    scp dist/apk/aarch64/*.apk board:/tmp/
    scp dist/apk-keys/*.rsa.pub board:/etc/apk/keys/
    ssh board 'apk add /tmp/simple-graphics-controller-*.apk'
    ssh board 'rc-update add simple-graphics-controller default'

The package does **not** enable the service: that is the image's decision, and
one line in a profile (`services = [ "default:simple-graphics-controller" ]`).
The signing key lives in `dist/apk-keys/` and is reused between builds, so a
board learns one public key. Copying it to `/etc/apk/keys/` is what lets
`apk add` install the file without `--allow-untrusted`.

Two things about the APKBUILD are worth knowing before editing it:

* **`srcdir`/`pkgdir` are set explicitly.** abuild defaults them to
  `$startdir/src` and `$startdir/pkg`, and this repo's Rust sources live in
  `src/` - abuild's "Cleaning up srcdir" step deleted them. Both point into the
  gitignored `dist/abuild/` now.
* **`options="!check !strip"`, and abuild is run with `-d`.** Nothing is
  compiled, so the dependency check (which insists on an implicit `build-base`)
  and abuild's own stripping (the dist recipes already strip) do not apply.
  `-F` is there because abuild refuses to run as root, which is the only user a
  container or a board has. abuild also warns that `/etc/init.d` is shipped by
  the main package rather than a `<pkgname>-openrc` subpackage; that split is
  not worth it for a package that only exists for Alpine.

### OpenRC service

`packaging/openrc/simple-graphics-controller` mirrors the systemd unit, decision
for decision:

| systemd | OpenRC |
|---|---|
| `User=root` | `command_user="root:root"` |
| `Restart=always`, `RestartSec=5`, `StartLimitBurst=12`/60s | `supervisor="supervise-daemon"`, `respawn_delay=5`, `respawn_max=12`, `respawn_period=60` |
| `LimitNOFILE=4096` | `rc_ulimit="-n 4096"` |
| `After=dev-dri-card0.device` | `after modules devfs mdev` |
| `Environment=RUST_LOG=info` | defaults in the script, overridable in `/etc/conf.d/simple-graphics-controller` |
| `WantedBy=multi-user.target` | `rc-update add simple-graphics-controller default` |

`SGC_POLICY` (the window policy) also comes from
`/etc/conf.d/simple-graphics-controller`; it defaults to `fair-queue`, the
daemon's own default. Clients depend on the daemon with
`depend() { need simple-graphics-controller; }`, or on the stable name
`sgc-controller` the script provides.

The systemd drop-in for a binary in `/root` has no OpenRC equivalent, and needs
none: the package puts the binary at `/usr/bin/simple-graphics-controller`, and
an unpackaged binary is run from wherever it is without a unit.

## Installing on a board with the binary in /root

Superseded by the package above; kept for a board where the binary is already in
/root and no package is wanted. Keep the packaged unit and override only the
path:

    install -m 644 packaging/simple-graphics-controller.service \
        /etc/systemd/system/simple-graphics-controller.service
    install -d /etc/systemd/system/simple-graphics-controller.service.d
    install -m 644 packaging/board-override.conf \
        /etc/systemd/system/simple-graphics-controller.service.d/board.conf
    systemctl daemon-reload
    systemctl enable --now simple-graphics-controller

`board-override.conf`:

    [Service]
    ExecStart=
    ExecStart=/root/simple-graphics-controller-gnu
    ProtectHome=no

Both lines are needed. The empty `ExecStart=` clears the packaged value (systemd
drop-ins append otherwise), and `ProtectHome=no` undoes the packaged
`ProtectHome=yes`: `/root` is the root user's home, so the hardening hides the
binary and the service dies at `EXEC` with `status=203` and a misleading
`No such file or directory`. Drop `ProtectHome=no` when the daemon comes from the
`.deb`.

Check it:

    systemctl is-enabled simple-graphics-controller
    systemctl status simple-graphics-controller
    journalctl -u simple-graphics-controller -n 20

Clients started by hand (`start-agh-slint.sh`, `start-aghdash.sh`) keep working;
they just need the daemon up first, which the unit now guarantees.
