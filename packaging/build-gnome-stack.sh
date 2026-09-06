#!/bin/sh
# GNOME source stack for ubuntu:22.04 jammy (spec #21) — jammy predates the
# app stack (gtk4-rs 0.11 wants GTK>=4.16 vs jammy 4.6; glycin-svg wants
# cairo>=1.17 vs 1.16; gtk wants pango>=1.52), so build the chain into /usr
# in dependency order. Also builds glycin-loaders (image-rs + svg; heif/jxl
# need libheif/libjxl) since jammy has no package.
#
# Run the WHOLE script as root (sudo sh packaging/build-gnome-stack.sh):
# mixing a runner-owned checkout with `sudo meson install` breaks ninja's
# build-log writes during install-time regeneration. Used by .github/workflows/ci.yml
# (`raw` and `appimage` jobs); mirrors packaging/build-appimage-container.sh.
set -eu

pip3 install -U meson tomli
mkdir -p /tmp/dist-src && cd /tmp/dist-src
curl --fail --retry 5 --retry-all-errors -sL -o glib.tar.xz https://download.gnome.org/sources/glib/2.82/glib-2.82.5.tar.xz
tar xf glib.tar.xz
meson setup glib-2.82.5/builddir glib-2.82.5 -Dprefix=/usr -Dselinux=disabled -Dlibmount=enabled -Dtests=false -Ddocumentation=false
meson compile -C glib-2.82.5/builddir
meson install -C glib-2.82.5/builddir
curl --fail --retry 5 --retry-all-errors -sL -o fontconfig.tar.xz https://www.freedesktop.org/software/fontconfig/release/fontconfig-2.16.0.tar.xz
tar xf fontconfig.tar.xz
meson setup fontconfig-2.16.0/builddir fontconfig-2.16.0 -Dprefix=/usr -Dtests=disabled -Ddoc=disabled
meson compile -C fontconfig-2.16.0/builddir
meson install -C fontconfig-2.16.0/builddir
curl --fail --retry 5 --retry-all-errors -sL -o hb.tar.xz https://github.com/harfbuzz/harfbuzz/releases/download/10.4.0/harfbuzz-10.4.0.tar.xz
tar xf hb.tar.xz
meson setup harfbuzz-10.4.0/builddir harfbuzz-10.4.0 -Dprefix=/usr -Dtests=disabled -Ddocs=disabled -Dbenchmark=disabled
meson compile -C harfbuzz-10.4.0/builddir
meson install -C harfbuzz-10.4.0/builddir
curl --fail --retry 5 --retry-all-errors -sL -o cairo.tar.xz https://cairographics.org/releases/cairo-1.18.4.tar.xz
tar xf cairo.tar.xz
meson setup cairo-1.18.4/builddir cairo-1.18.4 -Dprefix=/usr -Dtests=disabled
meson compile -C cairo-1.18.4/builddir
meson install -C cairo-1.18.4/builddir
curl --fail --retry 5 --retry-all-errors -sL -o pango.tar.xz https://download.gnome.org/sources/pango/1.55/pango-1.55.0.tar.xz
tar xf pango.tar.xz
meson setup pango-1.55.0/builddir pango-1.55.0 -Dprefix=/usr -Dintrospection=disabled -Dgtk_doc=false
meson compile -C pango-1.55.0/builddir
meson install -C pango-1.55.0/builddir
curl --fail --retry 5 --retry-all-errors -sL -o wayland.tar.xz https://gitlab.freedesktop.org/wayland/wayland/-/releases/1.24.0/downloads/wayland-1.24.0.tar.xz
tar xf wayland.tar.xz
meson setup wayland-1.24.0/builddir wayland-1.24.0 -Dprefix=/usr -Ddocumentation=false -Dtests=false
meson compile -C wayland-1.24.0/builddir
meson install -C wayland-1.24.0/builddir
curl --fail --retry 5 --retry-all-errors -sL -o wproto.tar.xz https://gitlab.freedesktop.org/wayland/wayland-protocols/-/releases/1.45/downloads/wayland-protocols-1.45.tar.xz
tar xf wproto.tar.xz
meson setup wayland-protocols-1.45/builddir wayland-protocols-1.45 -Dprefix=/usr -Dtests=false
meson install -C wayland-protocols-1.45/builddir
curl --fail --retry 5 --retry-all-errors -sL -o gtk.tar.xz https://download.gnome.org/sources/gtk/4.16/gtk-4.16.13.tar.xz
tar xf gtk.tar.xz
meson setup gtk-4.16.13/builddir gtk-4.16.13 -Dprefix=/usr -Dintrospection=disabled -Ddocumentation=false -Dtracker=disabled -Dcloudproviders=disabled -Dsysprof=disabled -Dmedia-gstreamer=disabled -Dprint-cups=disabled -Dvulkan=disabled -Dbuild-tests=false -Dbuild-testsuite=false -Dbuild-demos=false -Dbuild-examples=false -Dman-pages=false
meson compile -C gtk-4.16.13/builddir
meson install -C gtk-4.16.13/builddir
curl --fail --retry 5 --retry-all-errors -sL -o adw.tar.xz https://download.gnome.org/sources/libadwaita/1.6/libadwaita-1.6.4.tar.xz
tar xf adw.tar.xz
meson setup libadwaita-1.6.4/builddir libadwaita-1.6.4 -Dprefix=/usr -Dintrospection=disabled -Dvapi=false -Dgtk_doc=false -Dtests=false -Dexamples=false
meson compile -C libadwaita-1.6.4/builddir
meson install -C libadwaita-1.6.4/builddir
git clone --depth 1 https://gitlab.gnome.org/GNOME/glycin.git /tmp/glycin
meson setup /tmp/glycin/builddir /tmp/glycin -Dglycin-loaders=true \
  -Dloaders=glycin-image-rs,glycin-svg \
  -Dlibglycin=false -Dlibglycin-gtk4=false -Dglycin-thumbnailer=false \
  -Dintrospection=false -Dprefix=/usr -Dtests=false
meson compile -C /tmp/glycin/builddir
meson install -C /tmp/glycin/builddir
# Hand the trees back to the invoking user: later steps run unprivileged
# (e.g. build-appimage.sh reinstalls glycin into the AppDir) and must be able
# to write ninja/cargo locks. Without sudo the fallback is the current user.
chown -R "${SUDO_UID:-0}:${SUDO_GID:-0}" /tmp/dist-src /tmp/glycin
