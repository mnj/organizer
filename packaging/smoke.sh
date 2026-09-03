#!/bin/sh
# Smoke test for Organizer distribution (spec #21).
#
# Two layers (mirrors CI .github/workflows/ci.yml):
#   1. headless tempdir acceptance — cargo test (Queue + Config + Dedup +
#      Mover + Undo seam, no display required);
#   2. bundled-layout checks — glycin-loaders 2+, bwrap, libseccomp,
#      gstreamer-1.0 plugins + gtk4paintablesink inside the AppDir/AppImage;
#   3. xvfb Preview smoke — organizer --help/--self-test-sandbox under
#      xvfb-run so the GTK Preview path initializes without a real display.
#
# Usage:
#   ./packaging/smoke.sh [APPDIR_OR_APPIMAGE]
#   No arg: checks the raw target/release/organizer + host layout.
#   APPDIR dir: checks $ARG/usr/libexec/glycin-loaders, $ARG/usr/bin/bwrap,
#     $ARG/usr/lib/gstreamer-1.0, gtk4paintablesink via GST_PLUGIN_SYSTEM_PATH.
#   *.AppImage: mounts (--appimage-mount) or extracts and checks the same.
set -eu

TARGET="${1:-}"

pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1" >&2; exit 1; }

# 1) Headless tempdir acceptance (no display).
echo "== cargo test (headless tempdir seam) =="
cargo test --locked || fail "cargo test failed"

# Resolve the layout root to check.
APPDIR=""
if [ -z "$TARGET" ]; then
  echo "== raw binary checks =="
  [ -x target/release/organizer ] || [ -x target/debug/organizer ] \
    || fail "no organizer binary (run cargo build first)"
  command -v bwrap >/dev/null 2>&1 || fail "bwrap missing on host"
  bwrap --version || fail "bwrap --version failed"
  pass "raw binary + host bwrap present"
  if [ -d /usr/libexec/glycin-loaders ]; then
    ls /usr/libexec/glycin-loaders/2+ >/dev/null 2>&1 \
      && pass "host glycin-loaders 2+ present" \
      || echo "WARN: host glycin-loaders layout unexpected"
  else
    echo "WARN: no host glycin-loaders (AppImage bundles them instead)"
  fi
  if command -v gst-inspect-1.0 >/dev/null 2>&1; then
    gst-inspect-1.0 gtk4paintablesink >/dev/null 2>&1 \
      && pass "host gtk4paintablesink discoverable" \
      || echo "WARN: host gtk4paintablesink missing (AppImage bundles libgstgtk4.so)"
  fi
  BIN="${BIN:-target/release/organizer}"
  [ -x "$BIN" ] || BIN="target/debug/organizer"
  if command -v xvfb-run >/dev/null 2>&1; then
    echo "== xvfb Preview smoke ($BIN --help) =="
    xvfb-run -a "$BIN" --help >/dev/null 2>&1 \
      && pass "xvfb Preview smoke (--help under Xvfb)" \
      || fail "xvfb smoke failed"
    xvfb-run -a "$BIN" --self-test-sandbox 2>&1 | tee /tmp/organizer-smoke.log
    grep -q "bwrap" /tmp/organizer-smoke.log \
      && pass "xvfb --self-test-sandbox reports bwrap" \
      || fail "--self-test-sandbox did not report bwrap"
  else
    echo "WARN: xvfb-run missing — skipping xvfb Preview smoke"
  fi
  exit 0
fi

case "$TARGET" in
  *.AppImage)
    echo "== AppImage checks ($TARGET) =="
    [ -f "$TARGET" ] || fail "AppImage not found: $TARGET"
    [ -f "$TARGET.zsync" ] \
      && pass "zsync sidecar present (AppImageUpdate deltas)" \
      || echo "WARN: no .zsync sidecar (run appimagetool -u gh-releases-zsync)"
    # FUSE-mount is unavailable on most CI runners, so extract instead.
    "$TARGET" --appimage-extract >/dev/null 2>&1 || fail "AppImage extract failed"
    APPDIR="$PWD/squashfs-root"
    ;;
  *)
    APPDIR="$TARGET"
    ;;
esac

echo "== bundled layout checks ($APPDIR) =="
[ -d "$APPDIR/usr/libexec/glycin-loaders/2+" ] \
  && pass "bundled glycin-loaders 2+ present" \
  || fail "missing $APPDIR/usr/libexec/glycin-loaders/2+"
[ -x "$APPDIR/usr/bin/bwrap" ] \
  && pass "bundled bwrap present" \
  || fail "missing $APPDIR/usr/bin/bwrap"
"$APPDIR/usr/bin/bwrap" --version \
  && pass "bundled bwrap --version runs" \
  || fail "bundled bwrap --version failed"
if find "$APPDIR/usr/lib" "$APPDIR/usr/lib/x86_64-linux-gnu" -name 'libseccomp.so.2' 2>/dev/null | grep -q .; then
  pass "bundled libseccomp.so.2 present"
else
  fail "missing bundled libseccomp.so.2"
fi
if ls "$APPDIR"/usr/lib*/libgtk-4.so* >/dev/null 2>&1; then
  pass "bundled libgtk-4 present"
else
  fail "missing bundled libgtk-4 (usr/lib/gtk-4.0 payload)"
fi
[ -d "$APPDIR/usr/lib/gstreamer-1.0" ] \
  && pass "bundled gstreamer-1.0 plugins present" \
  || fail "missing $APPDIR/usr/lib/gstreamer-1.0"
if [ -f "$APPDIR/usr/lib/gstreamer-1.0/libgstgtk4.so" ]; then
  pass "bundled libgstgtk4.so (gtk4paintablesink) present"
else
  fail "missing libgstgtk4.so (gtk4paintablesink)"
fi
if command -v gst-inspect-1.0 >/dev/null 2>&1; then
  GST_PLUGIN_SYSTEM_PATH="$APPDIR/usr/lib/gstreamer-1.0" \
  GST_PLUGIN_SCANNER="$APPDIR/usr/libexec/gstreamer-1.0/gst-plugin-scanner" \
    gst-inspect-1.0 gtk4paintablesink >/dev/null 2>&1 \
    && pass "gtk4paintablesink discoverable via GST_PLUGIN_SYSTEM_PATH" \
    || fail "gtk4paintablesink not discoverable from AppDir"
fi
grep -q "GLYCIN_DATA_DIR" "$APPDIR/AppRun" \
  && pass "AppRun sets GLYCIN_DATA_DIR" \
  || fail "AppRun missing GLYCIN_DATA_DIR"
grep -q "GST_PLUGIN_SYSTEM_PATH" "$APPDIR/AppRun" \
  && pass "AppRun sets GST_PLUGIN_SYSTEM_PATH" \
  || fail "AppRun missing GST_PLUGIN_SYSTEM_PATH"

echo "ALL SMOKE CHECKS PASSED"
