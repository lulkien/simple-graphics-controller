# Packaging

Install sources for the daemon: `packaging/` holds the systemd unit and the
drop-in for boards that run the binary from `/root`; this file holds the notes.
Build output lives in `dist/` (gitignored) and packages in `target/debian/`.

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
