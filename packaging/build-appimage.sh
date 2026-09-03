#!/bin/sh
# Build the Organizer AppImage on ubuntu:22.04 (glibc 2.35 baseline).
# Spec #21: raw binary + AppImage distribution, no Flatpak/rpm/deb.
#
# Toolchain:
#   cargo build --release --locked          -> target/release/organizer
#   cargo cinstall gst-plugin-gtk4 --features waylandegl,x11egl,dmabuf
#     --library-type=cdylib                 -> $APPDIR/usr/lib/gstreamer-1.0/libgstgtk4.so
#                                             (gtk4paintablesink for video Preview)
#   linuxdeploy --plugin gtk --plugin gstreamer -> bundles gtk4/libadwaita/
#     GStreamer base+good+libav, schemas, icons into $APPDIR
#   appimagetool -u gh-releases-zsync       -> Organizer-x86_64.AppImage + .zsync
#                                             (AppImageUpdate delta updates)
#
# Bundled manually before linuxdeploy so ldd-walk picks them up:
#   glycin-loaders 2+ binaries + conf.d (sandboxed stills), bwrap + libseccomp.
#
# Usage:
#   ./packaging/build-appimage.sh [APPDIR] [OUTPUT]
#   Defaults: APPDIR=Organizer.AppDir OUTPUT=Organizer-x86_64.AppImage
# Requires (see docs/packaging.md for apt line): linuxdeploy, appimagetool,
#   cargo-c (cargo cinstall), meson/ninja (glycin-loaders source build).
set -eu

APPDIR="${1:-Organizer.AppDir}"
OUTPUT="${2:-Organizer-x86_64.AppImage}"
REPO_ROOT="$(dirname "$(readlink -f "$0")")/.."

# 1) Raw release binary — the same artifact shipped as tarball.
cargo build --release --locked
strip target/release/organizer || true

# 2) Stage AppDir skeleton.
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications" \
  "$APPDIR/usr/share/icons/hicolor/256x256/apps"

cp target/release/organizer "$APPDIR/usr/bin/organizer"

# 3) gtk4paintablesink plugin (video Preview) as a GStreamer cdylib.
#    waylandegl,x11egl keep GL zero-copy on both compositors; dmabuf enables
#    DMABuf import on GTK 4.14+.
cargo cinstall gst-plugin-gtk4 \
  --features waylandegl,x11egl,dmabuf \
  --library-type=cdylib \
  --prefix=/usr \
  --destdir="$PWD/$APPDIR"

# 4) glycin-loaders 2+ into the AppDir (sandboxed stills).
#    Prefers an installed meson build dir (/tmp/glycin-builddir, see CI);
#    otherwise copies host loaders if present.
if [ -d /tmp/glycin/builddir ]; then
  DESTDIR="$PWD/$APPDIR" meson install -C /tmp/glycin/builddir
elif [ -d /usr/libexec/glycin-loaders ]; then
  mkdir -p "$APPDIR/usr/libexec" "$APPDIR/usr/share"
  cp -r /usr/libexec/glycin-loaders "$APPDIR/usr/libexec/"
  if [ -d /usr/share/glycin-loaders ]; then
    cp -r /usr/share/glycin-loaders "$APPDIR/usr/share/"
  fi
else
  echo "WARN: no glycin-loaders found (no /tmp/glycin/builddir, no /usr/libexec/glycin-loaders);" >&2
  echo "WARN: AppImage will show the sandbox-missing dialog on hosts without loaders." >&2
fi

# 5) bwrap + libseccomp so sandboxed preview works with no host bubblewrap.
if command -v bwrap >/dev/null 2>&1; then
  cp -f "$(command -v bwrap)" "$APPDIR/usr/bin/bwrap"
fi

# 6) Desktop file + icon (linuxdeploy requirement).
cp "$REPO_ROOT/packaging/organizer.desktop" "$APPDIR/usr/share/applications/organizer.desktop"
if [ -f "$REPO_ROOT/packaging/organizer.png" ]; then
  cp "$REPO_ROOT/packaging/organizer.png" "$APPDIR/usr/share/icons/hicolor/256x256/apps/organizer.png"
elif command -v rsvg-convert >/dev/null 2>&1; then
  rsvg-convert -w 256 -h 256 "$REPO_ROOT/packaging/organizer.svg" \
    -o "$APPDIR/usr/share/icons/hicolor/256x256/apps/organizer.png"
else
  echo "WARN: no organizer.png and no rsvg-convert; linuxdeploy needs an icon — staging SVG." >&2
  cp "$REPO_ROOT/packaging/organizer.svg" "$APPDIR/usr/share/icons/hicolor/256x256/apps/organizer.svg"
fi

# 7) linuxdeploy: ldd-walk + gtk/gstreamer plugins + schemas/icon caches.
#    Build on ubuntu:22.04 so glibc baseline stays 2.35 (runs on newer hosts).
#    No fallback: if the gstreamer plugin fails the AppImage must fail, not
#    silently ship without video decoding.
if [ -f "$APPDIR/usr/share/icons/hicolor/256x256/apps/organizer.png" ]; then
  ICON="$APPDIR/usr/share/icons/hicolor/256x256/apps/organizer.png"
else
  ICON="$APPDIR/usr/share/icons/hicolor/256x256/apps/organizer.svg"
fi
linuxdeploy --appdir "$APPDIR" \
  --executable "$APPDIR/usr/bin/organizer" \
  --desktop-file "$APPDIR/usr/share/applications/organizer.desktop" \
  --icon-file "$ICON" \
  --plugin gtk --plugin gstreamer \
  --output appimage

# 8) Overlay the glycin-aware AppRun hook (conf.d Exec relocation +
#    GLYCIN_DATA_DIR / XDG_DATA_DIRS / GST_PLUGIN_SYSTEM_PATH).
cp "$REPO_ROOT/packaging/AppRun" "$APPDIR/AppRun"
chmod +x "$APPDIR/AppRun"

# 9) Pack with zsync update string for AppImageUpdate delta updates.
ARCH=x86_64 appimagetool "$APPDIR" \
  -u 'gh-releases-zsync|mnj|organizer|latest|Organizer-*x86_64.AppImage.zsync' \
  "$OUTPUT"

ls -lh "$OUTPUT" "$OUTPUT.zsync" 2>&1 | head -n 10

# 10) Verify the bundled sink is discoverable from the AppDir path alone.
GST_PLUGIN_SYSTEM_PATH="$APPDIR/usr/lib/gstreamer-1.0" \
GST_PLUGIN_SCANNER="$APPDIR/usr/libexec/gstreamer-1.0/gst-plugin-scanner" \
  gst-inspect-1.0 gtk4paintablesink 2>&1 | head -n 20
