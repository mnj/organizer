#!/bin/sh
# Containerized AppImage build (spec #21) — for hosts missing the toolchain
# (cargo-c, linuxdeploy, ...) or not on ubuntu:22.04. Only a container
# runtime is required on the host. Builds on ubuntu:22.04 (glibc 2.35
# baseline) inside the container and drops Organizer-x86_64.AppImage +
# .zsync in the repo root.
#
# Usage: ./packaging/build-appimage-container.sh [APPDIR] [OUTPUT]
#   Defaults: APPDIR=Organizer.AppDir OUTPUT=Organizer-x86_64.AppImage
set -eu

APPDIR="${1:-Organizer.AppDir}"
OUTPUT="${2:-Organizer-x86_64.AppImage}"
REPO_ROOT="$(dirname "$(readlink -f "$0")")/.."

if command -v podman >/dev/null 2>&1; then
  RUNTIME="podman"
elif command -v docker >/dev/null 2>&1; then
  RUNTIME="docker"
else
  echo "ERROR: neither podman nor docker found — install one first." >&2
  exit 1
fi

# AppImages (linuxdeploy/appimagetool) need no FUSE this way: the runtime
# extracts-and-runs them instead of mounting.
"$RUNTIME" run --rm \
  -v "$REPO_ROOT:/work:z" -w /work \
  -v organizer-cargo-home:/root/.cargo \
  -e APPDIR="$APPDIR" -e OUTPUT="$OUTPUT" \
  -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
  -e APPIMAGE_EXTRACT_AND_RUN=1 \
  -e DEBIAN_FRONTEND=noninteractive \
  ubuntu:22.04 bash -ec '
    set -eu
    apt update
    apt install -y libgtk-4-dev libadwaita-1-dev \
      libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
      gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-libav \
      liblcms2-dev libfontconfig1-dev libseccomp-dev bubblewrap \
      libssl-dev pkg-config build-essential curl ca-certificates git \
      cmake gettext gperf meson ninja-build python3-pip sassc \
      libffi-dev zlib1g-dev libmount-dev libselinux1-dev libpcre2-dev \
      libelf-dev libdbus-1-dev libexpat1-dev libxml2-dev libfreetype-dev \
      libfribidi-dev libthai-dev \
      libepoxy-dev libxkbcommon-dev libjpeg-dev libtiff-dev libpng-dev \
      libgraphene-1.0-dev libdrm-dev libgbm-dev libegl-dev libgles-dev \
      libvulkan-dev libx11-dev libxrandr-dev libxi-dev libxinerama-dev \
      libxcursor-dev libxdamage-dev libxfixes-dev libxcomposite-dev libxext-dev \
      libappstream-dev desktop-file-utils appstream librsvg2-dev
    # Jammy ships meson 0.61 but glycin requires >=1.2 — upgrade via pip.
    pip3 install -U meson tomli
    export PATH="$HOME/.local/bin:$PATH"
    # Jammy system libs predate the app stack (gtk4-rs 0.11 wants GTK>=4.16
    # vs jammy 4.6; glycin-svg wants cairo>=1.17 vs 1.16; gtk wants pango
    # >=1.52/glbasic deps newer than jammy). Build the chain from source
    # into /usr — each step proven in podman, in dependency order.
    SRC=/tmp/dist-src
    mkdir -p $SRC
    cd $SRC
    curl -sL -o glib.tar.xz https://download.gnome.org/sources/glib/2.82/glib-2.82.5.tar.xz
    tar xf glib.tar.xz
    meson setup glib-2.82.5/builddir glib-2.82.5 -Dprefix=/usr -Dselinux=disabled -Dlibmount=enabled -Dtests=false -Ddocumentation=false
    meson compile -C glib-2.82.5/builddir
    meson install -C glib-2.82.5/builddir
    curl -sL -o fontconfig.tar.xz https://www.freedesktop.org/software/fontconfig/release/fontconfig-2.16.0.tar.xz
    tar xf fontconfig.tar.xz
    meson setup fontconfig-2.16.0/builddir fontconfig-2.16.0 -Dprefix=/usr -Dtests=disabled -Ddoc=disabled
    meson compile -C fontconfig-2.16.0/builddir
    meson install -C fontconfig-2.16.0/builddir
    curl -sL -o hb.tar.xz https://github.com/harfbuzz/harfbuzz/releases/download/10.4.0/harfbuzz-10.4.0.tar.xz
    tar xf hb.tar.xz
    meson setup harfbuzz-10.4.0/builddir harfbuzz-10.4.0 -Dprefix=/usr -Dtests=disabled -Ddocs=disabled -Dbenchmark=disabled
    meson compile -C harfbuzz-10.4.0/builddir
    meson install -C harfbuzz-10.4.0/builddir
    curl -sL -o cairo.tar.xz https://cairographics.org/releases/cairo-1.18.4.tar.xz
    tar xf cairo.tar.xz
    meson setup cairo-1.18.4/builddir cairo-1.18.4 -Dprefix=/usr -Dtests=disabled
    meson compile -C cairo-1.18.4/builddir
    meson install -C cairo-1.18.4/builddir
    curl -sL -o pango.tar.xz https://download.gnome.org/sources/pango/1.55/pango-1.55.0.tar.xz
    tar xf pango.tar.xz
    meson setup pango-1.55.0/builddir pango-1.55.0 -Dprefix=/usr -Dintrospection=disabled -Dgtk_doc=false
    meson compile -C pango-1.55.0/builddir
    meson install -C pango-1.55.0/builddir
    curl -sL -o wayland.tar.xz https://gitlab.freedesktop.org/wayland/wayland/-/releases/1.24.0/downloads/wayland-1.24.0.tar.xz
    tar xf wayland.tar.xz
    meson setup wayland-1.24.0/builddir wayland-1.24.0 -Dprefix=/usr -Ddocumentation=false -Dtests=false
    meson compile -C wayland-1.24.0/builddir
    meson install -C wayland-1.24.0/builddir
    curl -sL -o wproto.tar.xz https://gitlab.freedesktop.org/wayland/wayland-protocols/-/releases/1.45/downloads/wayland-protocols-1.45.tar.xz
    tar xf wproto.tar.xz
    meson setup wayland-protocols-1.45/builddir wayland-protocols-1.45 -Dprefix=/usr -Dtests=false
    meson install -C wayland-protocols-1.45/builddir
    curl -sL -o gtk.tar.xz https://download.gnome.org/sources/gtk/4.16/gtk-4.16.13.tar.xz
    tar xf gtk.tar.xz
    meson setup gtk-4.16.13/builddir gtk-4.16.13 -Dprefix=/usr -Dintrospection=disabled -Ddocumentation=false -Dtracker=disabled -Dcloudproviders=disabled -Dsysprof=disabled -Dmedia-gstreamer=disabled -Dprint-cups=disabled -Dvulkan=disabled -Dbuild-tests=false -Dbuild-testsuite=false -Dbuild-demos=false -Dbuild-examples=false -Dman-pages=false
    meson compile -C gtk-4.16.13/builddir
    meson install -C gtk-4.16.13/builddir
    curl -sL -o adw.tar.xz https://download.gnome.org/sources/libadwaita/1.6/libadwaita-1.6.4.tar.xz
    tar xf adw.tar.xz
    meson setup libadwaita-1.6.4/builddir libadwaita-1.6.4 -Dprefix=/usr -Dintrospection=disabled -Dvapi=false -Dgtk_doc=false -Dtests=false -Dexamples=false
    meson compile -C libadwaita-1.6.4/builddir
    meson install -C libadwaita-1.6.4/builddir
    cd /work
    curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --profile minimal
    export PATH="$HOME/.cargo/bin:$PATH"
    # cargo-c is not packaged on jammy: try apt (newer distros), then build.
    if ! cargo cinstall --version >/dev/null 2>&1; then
      apt install -y cargo-c || cargo install cargo-c --locked
    fi
    git clone --depth 1 https://gitlab.gnome.org/GNOME/glycin.git /tmp/glycin
    # Loaders trimmed to what jammy can build: image-rs covers
    # png/jpg/webp/gif/bmp/tiff/ico (+qoi/dds/exr/pnm), svg via librsvg.
    # libglycin/gtk4 bindings, thumbnailer and tests are not needed to
    # decode (gtk4>=4.16 is newer than the 4.6 shipped by jammy).
    meson setup /tmp/glycin/builddir /tmp/glycin -Dglycin-loaders=true \
      -Dloaders=glycin-image-rs,glycin-svg \
      -Dlibglycin=false -Dlibglycin-gtk4=false -Dglycin-thumbnailer=false \
      -Dintrospection=false -Dprefix=/usr -Dtests=false
    meson compile -C /tmp/glycin/builddir
    meson install -C /tmp/glycin/builddir
    curl -L -o /usr/local/bin/linuxdeploy https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage
    curl -L -o /usr/local/bin/appimagetool https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage
    chmod +x /usr/local/bin/linuxdeploy /usr/local/bin/appimagetool
    ./packaging/build-appimage.sh "$APPDIR" "$OUTPUT"
    chown -R "$HOST_UID:$HOST_GID" target "$OUTPUT" "$OUTPUT.zsync" || true
  '

ls -lh "$REPO_ROOT/$OUTPUT" "$REPO_ROOT/$OUTPUT.zsync"
