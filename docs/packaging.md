# Packaging: raw executable + AppImage (spec #21)

> Scope: **raw `cargo` executable and AppImage only** — Flatpak/rpm/deb
> explicitly out of scope. Research: `docs/research/appimage-raw-packaging.md`
> (builds on `rendering-stack.md` + `sandboxed-decoding-raw-appimage.md`).

## Raw executable

Prerequisites per distro (one-liners for `cargo build --release --locked`):

```sh
# Ubuntu 22.04 jammy (build baseline, glibc 2.35) / Ubuntu 24.04 noble / Debian trixie
sudo apt update && sudo apt install -y libgtk-4-dev libadwaita-1-dev \
  libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
  gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-libav \
  liblcms2-dev libfontconfig1-dev libseccomp-dev bubblewrap \
  pkg-config build-essential curl
# plus glycin-loaders 2+: Ubuntu 24.04 universe (`glycin-loaders`), or build
# from source on ubuntu:22.04 (see CI): meson setup /tmp/glycin/builddir
# -Dglycin-loaders=true -Dprefix=/usr && meson compile -C /tmp/glycin/builddir

# Fedora 41
sudo dnf install -y gtk4-devel libadwaita-devel \
  gstreamer1-devel gstreamer1-plugins-base-devel gstreamer1-plugins-good \
  gstreamer1-plugins-bad-free gstreamer1-plugin-libav \
  lcms2-devel fontconfig-devel libseccomp-devel bubblewrap \
  pkgconf-pkg-config gcc

# Arch (rolling)
sudo pacman -S --needed gtk4 libadwaita gstreamer gst-plugins-base \
  gst-plugins-good gst-plugins-bad gst-libav lcms2 fontconfig libseccomp \
  bubblewrap pkgconf base-devel
# glycin on Arch: AUR `glycin` or source; gst-plugin-gtk4: cargo cinstall
```

Build and run:

```sh
git clone https://github.com/mnj/organizer && cd organizer
cargo build --release --locked
./target/release/organizer /tmp/src
# or: cargo install --locked --path .   # installs ~/.cargo/bin/organizer
organizer /tmp/src
```

Version pinning: `Cargo.lock` is committed — always build with `--locked`.
`ubuntu:22.04` (glibc 2.35) is the oldest-supported baseline so the binary
runs on newer hosts. If `bubblewrap` is missing or unprivileged user
namespaces are locked down, Organizer refuses sandboxed decode with a dialog
(use the AppImage instead, which bundles `bwrap`).

Release tarball (maintainers, manual — zero extra tooling; canonical recipe,
kept identical in `.github/workflows/ci.yml`):

```sh
cargo build --release --locked
strip target/release/organizer
cp README.md target/release/
tar czf organizer-x86_64-linux-gnu.tar.gz -C target/release organizer README.md
sha256sum organizer-*.tar.gz > SHA256SUMS
```

Size: stripped raw binary ~10–14 MB; startup ~80 ms.

## AppImage

Built on **`ubuntu:22.04`** (glibc 2.35) via `linuxdeploy --plugin gtk
--plugin gstreamer` + `cargo cinstall gst-plugin-gtk4 --features
waylandegl,x11egl,dmabuf --library-type=cdylib --prefix=/usr` +
`appimagetool -u gh-releases-zsync` (~55–95 MB compressed, ~120–200 MB
uncompressed).

```sh
./packaging/build-appimage.sh            # -> Organizer-x86_64.AppImage + .zsync
./Organizer-x86_64.AppImage /tmp/src     # runs with no host gtk/gstreamer/glycin
```

Bundled inside the AppDir: `gtk4`/`libadwaita`, GStreamer plugins +
`libgstgtk4.so` (`gtk4paintablesink`), `glycin-loaders` 2+ binaries + conf.d,
`bwrap` + `libseccomp.so.2`, schemas/icon caches. Host-assumed (not bundled):
`glibc`, `libfuse2`, Wayland/X11 + GL stack, GPU drivers.

The `packaging/AppRun` hook derives every path from `$HERE` at launch:
`LD_LIBRARY_PATH`, `PATH` (bundled `bwrap` first), `GLYCIN_DATA_DIR` /
`XDG_DATA_DIRS` (bundled loader discovery), `GST_PLUGIN_SYSTEM_PATH` /
`GST_PLUGIN_SCANNER` (bundled plugins incl. `gtk4paintablesink`),
`GSETTINGS_SCHEMA_DIR` / `GTK_PATH`, plus a temp
`$XDG_RUNTIME_DIR/glycin-loaders/2+/conf.d` tree with `Exec` rewritten to
`$HERE` so `bwrap --ro-bind` finds the bundled loaders.

Updates: `appimagetool -u` embeds
`gh-releases-zsync|mnj|organizer|latest|Organizer-*x86_64.AppImage.zsync` and
emits `Organizer-x86_64.AppImage.zsync` — `AppImageUpdate` fetches only
changed blocks (~5–15 MB delta). Raw users re-extract the Releases tarball.

## Smoke tests

```sh
cargo test --locked                      # headless tempdir acceptance (no display)
./packaging/smoke.sh                     # raw binary + host layout checks
./packaging/smoke.sh Organizer.AppDir    # AppDir layout (glycin-loaders, bwrap,
                                         # gstreamer-1.0, gtk4paintablesink)
xvfb-run -a ./target/release/organizer --self-test-sandbox
```

`--self-test-sandbox` is a hidden headless probe (per research
`appimage-raw-packaging.md` §7: CI greps `SandboxMechanism`, `XDG_DATA_DIRS`,
`GST_PLUGIN_SYSTEM_PATH` without launching the GUI). Expected AppImage sizes
(spec estimate): ~55–95 MB compressed squashfs, ~120–200 MB uncompressed —
`packaging/smoke.sh` warns when an artifact falls outside that range.

CI (`.github/workflows/ci.yml`) runs all of the above on push/tag and
uploads `Organizer-x86_64.AppImage` (+ `.zsync`) and the raw
`organizer` binary.
