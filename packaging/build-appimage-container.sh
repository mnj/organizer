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
      cmake gettext meson ninja-build python3-pip \
      desktop-file-utils appstream librsvg2-dev
    # Jammy ships meson 0.61 but glycin requires >=1.2 — upgrade via pip.
    pip3 install -U meson tomli
    export PATH="$HOME/.local/bin:$PATH"
    # Jammy ships cairo 1.16 but glycin-svg requires >=1.17 — build it.
    # (Proven in podman; heif/jxl loaders stay out — needs libheif/libjxl.)
    curl -sL -o /tmp/cairo.tar.xz https://cairographics.org/releases/cairo-1.18.4.tar.xz
    tar xf /tmp/cairo.tar.xz -C /tmp
    meson setup /tmp/cairo-1.18.4/builddir /tmp/cairo-1.18.4 -Dprefix=/usr -Dtests=disabled
    meson compile -C /tmp/cairo-1.18.4/builddir
    meson install -C /tmp/cairo-1.18.4/builddir
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
