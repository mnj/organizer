#!/bin/sh
# Bundle the GStreamer video path into the Organizer AppDir (spec #20).
#
# What this wires (see packaging/AppRun for the runtime side):
#   cargo cinstall gst-plugin-gtk4 --features waylandegl,x11egl
#     --library-type=cdylib  →  $APPDIR/usr/lib/gstreamer-1.0/libgstgtk4.so
#     (the gtk4paintablesink behind the Preview Picture)
#   NOTE: upstream documents waylandegl,x11egl,dmabuf, but the dmabuf feature
#     needs system gstreamer>=1.24 (jammy ships 1.20), so the jammy AppImage
#     build in packaging/build-appimage.sh drops dmabuf (GL path remains).
#   linuxdeploy --plugin gtk --plugin gstreamer  →  base/good/libav plugin set
#     (h264/h265/vp8/vp9/av1) + gst-plugin-scanner under $APPDIR,
#     discovered at launch via GST_PLUGIN_SYSTEM_PATH (no host GStreamer).
#
# Usage: ./packaging/bundle-video.sh [APPDIR]   (default: Organizer.AppDir)
# Verify after bundling (acceptance: gtk4paintablesink listed from AppDir):
#   GST_PLUGIN_SYSTEM_PATH="$APPDIR/usr/lib/gstreamer-1.0" \
#   GST_PLUGIN_SCANNER="$APPDIR/usr/libexec/gstreamer-1.0/gst-plugin-scanner" \
#     gst-inspect-1.0 gtk4paintablesink
# Manual acceptance: run the AppImage on ubuntu:22.04 with no host GStreamer
# and confirm a mixed jpg + webm queue previews both (stills via glycin,
# video via the bundled plugins).
set -eu

APPDIR="${1:-Organizer.AppDir}"
mkdir -p "$APPDIR"

# 1) Build + install the GTK4 paintable sink as a GStreamer cdylib plugin.
#    waylandegl,x11egl keep GL paths available on both compositors
#    (dmabuf dropped on jammy: needs gstreamer>=1.24, see build-appimage.sh).
cargo cinstall gst-plugin-gtk4 \
    --features waylandegl,x11egl \
    --library-type=cdylib \
    --prefix=/usr \
    --destdir="$APPDIR"

# 2) Pull the GStreamer plugin set + scanner + GTK deps into the AppDir.
#    (linuxdeploy-plugin-gstreamer discovers GST_PLUGIN_SYSTEM_PATH.)
linuxdeploy --appdir "$APPDIR" --plugin gtk --plugin gstreamer

# 3) Verify the bundled sink is discoverable from the AppDir path alone.
GST_PLUGIN_SYSTEM_PATH="$APPDIR/usr/lib/gstreamer-1.0" \
GST_PLUGIN_SCANNER="$APPDIR/usr/libexec/gstreamer-1.0/gst-plugin-scanner" \
    gst-inspect-1.0 gtk4paintablesink
