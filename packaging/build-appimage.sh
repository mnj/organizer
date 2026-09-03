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

# Fail fast on missing toolchain: cargo-c provides `cargo cinstall`, which
# stages gst-plugin-gtk4 into the AppDir (step 3). Install with
# `cargo install cargo-c --locked` (or `apt install cargo-c` on Debian/Ubuntu).
if ! cargo cinstall --version >/dev/null 2>&1; then
  echo "ERROR: 'cargo cinstall' not found — install cargo-c first:" >&2
  echo "ERROR:   cargo install cargo-c --locked" >&2
  exit 1
fi
command -v linuxdeploy >/dev/null 2>&1 \
  || { echo "ERROR: linuxdeploy not found (see docs/packaging.md)." >&2; exit 1; }
command -v appimagetool >/dev/null 2>&1 \
  || { echo "ERROR: appimagetool not found (see docs/packaging.md)." >&2; exit 1; }

APPDIR="${1:-Organizer.AppDir}"
OUTPUT="${2:-Organizer-x86_64.AppImage}"
REPO_ROOT="$(dirname "$(readlink -f "$0")")/.."

# Cargo target dir (the container script points this at container-local
# storage so bind-mounted host target/ never gets foreign-owned files).
TARGET_DIR="${CARGO_TARGET_DIR:-target}"

# Toolchain guards: linuxdeploy/appimagetool must be on PATH
# (container script and CI install them; see docs/packaging.md).
for tool in linuxdeploy appimagetool; do
  command -v "$tool" >/dev/null 2>&1 \
    || { echo "ERROR: $tool not found on PATH." >&2; exit 1; }
done

# linuxdeploy --plugin <name> resolves to a linuxdeploy-plugin-<name>.sh
# script next to the binary (or on PATH) — not bundled with linuxdeploy
# itself. Fetch the gtk/gstreamer plugins if absent.
fetch_plugin() {
  name="$1"
  url="$2"
  if command -v "linuxdeploy-plugin-$name.sh" >/dev/null 2>&1; then
    return 0
  fi
  dest_dir="$(dirname "$(command -v linuxdeploy)")"
  if [ -w "$dest_dir" ]; then
    curl -fSL --retry 3 -A "organizer-appimage-build/1.0" \
      -o "$dest_dir/linuxdeploy-plugin-$name.sh" "$url"
    chmod +x "$dest_dir/linuxdeploy-plugin-$name.sh"
  else
    echo "ERROR: linuxdeploy-plugin-$name.sh not found and $dest_dir not writable." >&2
    exit 1
  fi
}
fetch_plugin gtk "https://raw.githubusercontent.com/linuxdeploy/linuxdeploy-plugin-gtk/master/linuxdeploy-plugin-gtk.sh"
fetch_plugin gstreamer "https://raw.githubusercontent.com/linuxdeploy/linuxdeploy-plugin-gstreamer/master/linuxdeploy-plugin-gstreamer.sh"

# 1) Raw release binary — the same artifact shipped as tarball.
cargo build --release --locked
strip "$TARGET_DIR/release/organizer" || true

# 2) Stage AppDir skeleton.
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications" \
  "$APPDIR/usr/share/icons/hicolor/256x256/apps"

cp "$TARGET_DIR/release/organizer" "$APPDIR/usr/bin/organizer"

# 3) gtk4paintablesink plugin (video Preview) as a GStreamer cdylib.
#    wayland/x11egl keep GL paths available on both compositors.
#    NOTE: `cargo cinstall <name>` does not fetch from crates.io — it builds
#    the current package (cwd). Download a release and install via
#    --manifest-path. Version 0.12.x is deliberate: 0.13+ needs system
#    gstreamer>=1.22/1.24 at runtime (0.15 uses 1.24-only
#    gst_video_info_dma_drm_to_video_info unconditionally), but jammy ships
#    1.20. The plugin is a registry-loaded cdylib, so its older gstreamer-rs
#    (0.22) is ABI-compatible at runtime; element name (gtk4paintablesink)
#    and the paintable property are unchanged.
#    (crates.io API needs a User-Agent, else 403 with an empty body.)
GST_GTK4_VER="$(curl -fsSL -A "organizer-appimage-build/1.0" \
  https://crates.io/api/v1/crates/gst-plugin-gtk4 \
  | python3 -c 'import json,sys; print(sorted((v["num"] for v in json.load(sys.stdin)["versions"] if v["num"].startswith("0.12.") and not v["yanked"]))[-1])')"
[ -n "$GST_GTK4_VER" ] || { echo "ERROR: could not resolve gst-plugin-gtk4 0.12.x from crates.io." >&2; exit 1; }
echo "gst-plugin-gtk4 version: $GST_GTK4_VER"
rm -rf /tmp/gst-plugin-gtk4 && mkdir -p /tmp/gst-plugin-gtk4
curl -fSL --retry 5 --retry-all-errors -A "organizer-appimage-build/1.0" \
  -o /tmp/gst-plugin-gtk4/plugin.tar.gz \
  "https://crates.io/api/v1/crates/gst-plugin-gtk4/$GST_GTK4_VER/download"
tar xzf /tmp/gst-plugin-gtk4/plugin.tar.gz -C /tmp/gst-plugin-gtk4
cargo cinstall \
  --manifest-path "/tmp/gst-plugin-gtk4/gst-plugin-gtk4-$GST_GTK4_VER/Cargo.toml" \
  --features wayland,x11egl \
  --library-type=cdylib \
  --prefix=/usr \
  --destdir="$PWD/$APPDIR"

# 4) glycin-loaders 2+ into the AppDir (sandboxed stills).
#    Prefers an installed meson build dir (/tmp/glycin/builddir, see CI);
#    otherwise copies host loaders if present. Fails fast: an AppImage
#    without loaders would fail its own acceptance (sandbox-missing dialog
#    on loader-less hosts), so never ship one silently.
if [ -d /tmp/glycin/builddir ]; then
  DESTDIR="$PWD/$APPDIR" meson install -C /tmp/glycin/builddir
elif [ -d /usr/libexec/glycin-loaders ]; then
  mkdir -p "$APPDIR/usr/libexec" "$APPDIR/usr/share"
  cp -r /usr/libexec/glycin-loaders "$APPDIR/usr/libexec/"
  if [ -d /usr/share/glycin-loaders ]; then
    cp -r /usr/share/glycin-loaders "$APPDIR/usr/share/"
  fi
else
  echo "ERROR: no glycin-loaders found (no /tmp/glycin/builddir, no /usr/libexec/glycin-loaders)." >&2
  echo "ERROR: refusing to build an AppImage that fails its own acceptance — build loaders first (see docs/packaging.md)." >&2
  exit 1
fi
[ -d "$APPDIR/usr/libexec/glycin-loaders/2+" ] \
  || { echo "ERROR: glycin-loaders staged but 2+ compat path missing in $APPDIR." >&2; exit 1; }

# 5) bwrap + libseccomp so sandboxed preview works with no host bubblewrap.
#    Fails fast for the same reason as loaders above (acceptance requires
#    `bwrap --version` inside the AppDir).
if command -v bwrap >/dev/null 2>&1; then
  cp -f "$(command -v bwrap)" "$APPDIR/usr/bin/bwrap"
else
  echo "ERROR: bwrap not found on build host — install bubblewrap (see docs/packaging.md)." >&2
  exit 1
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

# 10) Verify the bundled sink is discoverable from the AppDir paths alone
# (same multiarch + scanner resolution as packaging/AppRun; host GL/X11
# libs are assumed present per linuxdeploy excludelist, so this runs on
# the build host, not in a bare container).
GST_PLUGIN_SYSTEM_PATH="$APPDIR/usr/lib/gstreamer-1.0:$APPDIR/usr/lib/x86_64-linux-gnu/gstreamer-1.0"
if [ -x "$APPDIR/usr/libexec/gstreamer-1.0/gst-plugin-scanner" ]; then
  GST_PLUGIN_SCANNER="$APPDIR/usr/libexec/gstreamer-1.0/gst-plugin-scanner"
elif [ -x "$APPDIR/usr/lib/gstreamer1.0/gstreamer-1.0/gst-plugin-scanner" ]; then
  GST_PLUGIN_SCANNER="$APPDIR/usr/lib/gstreamer1.0/gstreamer-1.0/gst-plugin-scanner"
fi
export GST_PLUGIN_SYSTEM_PATH GST_PLUGIN_SCANNER
gst-inspect-1.0 gtk4paintablesink 2>&1 | head -n 20
