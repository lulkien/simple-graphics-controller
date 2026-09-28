# Maintainer: lulkien <kien.luuhoang.arch@proton.me>
#
# Alpine package for the @sgc daemon: the release binary plus the OpenRC
# service.
#
# It compiles nothing. The binary is a cargo artifact already built for the
# target (just dist-musl-aarch64 / dist-musl-x86_64), exactly as the .deb
# flavors package a cargo artifact with `cargo deb --no-build`. abuild's job
# here is to package and sign it, which is what makes the result installable
# with `apk add` instead of a file copied into an image by hand.
#
#   just package-apk-aarch64     # or package-apk-x86_64
#
# The service is installed but not enabled: enabling at boot is the image's
# decision (rc-update add simple-graphics-controller default), which is the
# Alpine convention and one line in the image profile.

pkgname=simple-graphics-controller
pkgver=0.1.0
pkgrel=0
pkgdesc="DRM/fbdev/input resource broker: leases display and input devices to @sgc clients"
url="https://github.com/sgc-project/simple-graphics-controller"
arch="aarch64 x86_64"
license="Unlicense"
depends=""
makedepends=""
source=""
# abuild defaults srcdir/pkgdir to $startdir/src and $startdir/pkg. This repo's
# Rust sources live in src/, and abuild's "Cleaning up srcdir" step therefore
# deleted them the first time this package was built. Both are ours instead,
# under the gitignored dist/, which no part of the project reads.
srcdir="$startdir/dist/abuild/srcdir"
pkgdir="$startdir/dist/abuild/pkgdir"
builddir="$srcdir"
# !check: nothing to compile and no test suite to run here
# !strip: the dist recipes already strip the binary, so abuild needs no binutils
options="!check !strip"
#
# abuild warns that /etc/init.d is shipped by the main package rather than by a
# <pkgname>-openrc subpackage. That is deliberate: this APKBUILD only exists for
# Alpine (the Debian flavors are cargo-deb's), so a separate service package
# would split one installable artifact into two for no gain. The warning is
# non-fatal; drop it by splitting the package if it ever ships a non-Alpine
# artifact too.

# The dist recipes strip the binary; abuild would only repeat that. Check the
# artifact is present and is the architecture being packaged - dist/ holds one
# binary at a time, so packaging the wrong target is otherwise silent.
build() {
	local bin="$startdir/dist/simple-graphics-controller"
	if [ ! -f "$bin" ]; then
		die "missing $bin: build it first with 'just dist-musl-$CARCH'"
	fi
	local want have
	case "$CARCH" in
		aarch64) want="ARM aarch64" ;;
		x86_64)  want="x86-64" ;;
		*)       die "unsupported architecture $CARCH" ;;
	esac
	have=$(file -b "$bin")
	case "$have" in
		*"$want"*) ;;
		*) die "$bin is not $CARCH: $have" ;;
	esac
	# a fully static musl binary is the point: it runs on any Alpine, with no
	# runtime dependencies for apk to resolve
	case "$have" in
		*"statically linked"*) ;;
		*) die "$bin is not statically linked: $have" ;;
	esac
}

package() {
	install -Dm755 "$startdir/dist/simple-graphics-controller" \
		"$pkgdir/usr/bin/simple-graphics-controller"
	install -Dm755 "$startdir/packaging/openrc/simple-graphics-controller" \
		"$pkgdir/etc/init.d/simple-graphics-controller"
	install -Dm644 "$startdir/LICENSE" \
		"$pkgdir/usr/share/licenses/$pkgname/LICENSE"
}
