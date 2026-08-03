_pkgname=tsukimi
pkgname="${_pkgname}-git"
pkgver=26.8.2.r0.g0000000
pkgrel=1
epoch=1
pkgdesc="GTK4 Jellyfin client for Linux built from the latest Git commit"
arch=(x86_64)
url="https://github.com/tsukinaha/tsukimi"
license=(GPL-3.0-only)
provides=("${_pkgname}")
conflicts=("${_pkgname}")
depends=(
  dbus
  ffmpeg
  'glib2>=2.76'
  'glycin>=2.0'
  gstreamer
  gst-libav
  gst-plugins-bad-libs
  gst-plugins-base-libs
  gst-plugins-good
  'gtk4>=4.22'
  'libadwaita>=1.9'
  libepoxy
  'mpv>=0.38'
  openssl
)
makedepends=(
  appstream
  blueprint-compiler
  cargo
  desktop-file-utils
  gettext
  git
  libxml2
  meson
  ninja
  pkgconf
  python
  'rust>=1.91'
)
source=(tsukimi-src.tar.gz)
sha256sums=(SKIP)
options=(!lto)

build() {
  arch-meson "${srcdir}/${_pkgname}" build -Drust-target=release
  meson compile -C build
}

package() {
  meson install -C build --no-rebuild --destdir "${pkgdir}"
}
