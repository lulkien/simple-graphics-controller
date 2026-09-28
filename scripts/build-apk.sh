#!/bin/sh
# Build the Alpine package from the APKBUILD at the repo root.
#
#   scripts/build-apk.sh aarch64        # package the aarch64 dist binary
#   scripts/build-apk.sh x86_64
#
# Normally run through the recipes, which build the binary first - the .apk
# packages a cargo artifact and this script compiles nothing:
#
#   just package-apk-aarch64
#
# abuild does the packaging and signing. It runs natively when abuild is
# installed for the architecture being packaged (an Alpine board), otherwise in
# an alpine container of that architecture - on an x86_64 host that is qemu
# binfmt doing the work, which is fine: abuild only reads, tars and signs.
#
# Output, both under the gitignored dist/:
#
#   dist/apk/<pkgname>/<pkgname>-<version>-r0.apk   the package
#   dist/apk-keys/<key>.rsa.pub                     the public key, reused
#                                                   between builds
#
# A board installs the package with that public key in /etc/apk/keys, so
# `apk add <file>` needs no --allow-untrusted.
set -eu

ARCH=${1:-}
case "$ARCH" in
  aarch64 | x86_64) ;;
  *) echo "usage: $0 aarch64|x86_64" >&2; exit 1 ;;
esac

REPO=$(cd "$(dirname "$0")/.." && pwd)
BIN="$REPO/dist/simple-graphics-controller"
[ -f "$BIN" ] || {
  echo "no $BIN: run 'just dist-musl-$ARCH' first" >&2
  exit 1
}
mkdir -p "$REPO/dist/apk-keys"

# abuild-keygen is idempotent: it only generates when no key is present, so the
# same key signs every build and a board only ever has to learn one pubkey.
keygen() {
  ls "$REPO/dist/apk-keys"/*.rsa >/dev/null 2>&1 || abuild-keygen -a -n -q
}

if command -v abuild >/dev/null 2>&1; then
  host_arch=$(apk --print-arch 2>/dev/null || echo unknown)
  if [ "$host_arch" != "$ARCH" ]; then
    echo "abuild here targets $host_arch, not $ARCH: run this on an $ARCH host" >&2
    exit 1
  fi
  echo "--- abuild on this host ($host_arch)"
  keygen
  # the index step verifies the packages it indexes, so the key has to be trusted
  # here too (not only where the package is installed)
  if [ -w /etc/apk/keys ] || [ "$(id -u)" = 0 ]; then
    install -d /etc/apk/keys
    install -m 644 "$REPO/dist/apk-keys"/*.rsa.pub /etc/apk/keys/
  fi
  cd "$REPO"
  # -F: abuild refuses to run as root otherwise, and on a board root is the only
  # user there is. -d: this package compiles nothing, so the dependency check
  # (which insists on an implicit build-base) does not apply.
  if [ "$(id -u)" = 0 ]; then
    abuild -F -d -P "$REPO/dist/apk"
  else
    abuild -d -P "$REPO/dist/apk"
  fi
else
  case "$ARCH" in
    aarch64) platform=linux/arm64 ;;
    x86_64) platform=linux/amd64 ;;
  esac
  # one source of truth for the maintainer: the APKBUILD's Maintainer line, which
  # is also what abuild-keygen names the signing key after
  packager=${PACKAGER:-$(sed -n 's/^# Maintainer: //p' "$REPO/APKBUILD" | head -1)}
  echo "--- abuild in an alpine:3.22 container ($platform, no local abuild)"
  docker run --rm --privileged --platform "$platform" \
    -v "$REPO":/work -v "$REPO/dist/apk-keys":/root/.abuild \
    -e PACKAGER="$packager" \
    -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
    alpine:3.22 sh -c '
      set -e
      apk add --no-cache abuild tar gzip file >/dev/null
      ls /root/.abuild/*.rsa >/dev/null 2>&1 || abuild-keygen -a -n -q
      # the key may predate this container, so install it explicitly: the index
      # step below verifies the packages it indexes
      mkdir -p /etc/apk/keys && cp /root/.abuild/*.rsa.pub /etc/apk/keys/
      cd /work
      # -F: abuild refuses to run as root otherwise. -d: nothing is compiled
      # here, so the dependency check (which insists on an implicit build-base)
      # does not apply. Options come before the command word, and with none the
      # command is the build, which also updates (and signs) the repo index.
      abuild -F -d -P /work/dist/apk
      # hand the result back to the user who owns the checkout, so dist/ stays
      # editable (and removable by just clean) on the host
      chown -R "$HOST_UID:$HOST_GID" /work/dist /root/.abuild
    '
fi

echo
echo "packages:"
ls -l "$REPO/dist/apk"/*/*.apk
apk_dir=$(dirname "$(ls "$REPO/dist/apk"/*/*.apk | head -1)")
echo
echo "install on a board:"
echo "  scp $apk_dir/*.apk board:/tmp/"
echo "  scp $REPO/dist/apk-keys/*.rsa.pub board:/etc/apk/keys/"
echo "  ssh board 'apk add /tmp/simple-graphics-controller-*.apk'"
echo "  ssh board 'rc-update add simple-graphics-controller default'"
