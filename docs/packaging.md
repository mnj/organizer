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
# from source on ubuntu:22.04. Jammy predates the app stack, so the AppImage
# recipe builds a chain into /usr first (proven in podman, in dependency
# order): pip meson>=1.2, glib 2.82, fontconfig 2.16, harfbuzz 10.4,
# cairo 1.18, pango 1.55, wayland 1.24 + protocols 1.45, gtk 4.16
# (no tracker/cloudproviders/sysprof/gstreamer-media/cups/vulkan/docs),
# libadwaita 1.6, then glycin with
# -Dloaders=glycin-image-rs,glycin-svg -Dlibglycin=false -Dlibglycin-gtk4=false
# -Dglycin-thumbnailer=false -Dintrospection=false -Dtests=false
# (heif/jxl loaders need libheif/libjxl and stay out of the jammy AppImage).

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

Release tarball (maintainers, manual only — CI does not build it):

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
--plugin gstreamer` + `cargo cinstall gst-plugin-gtk4 0.12.x --features
wayland,x11egl --library-type=cdylib --prefix=/usr` (0.12.x is deliberate:
0.13+ needs system gstreamer>=1.22 at runtime, jammy ships 1.20; element
name and paintable property are unchanged) +
`appimagetool -u gh-releases-zsync` (measured 106 MB compressed with the
source-built GNOME stack; `packaging/smoke.sh` warns outside 55–95 MB).

```sh
./packaging/build-appimage.sh            # -> Organizer-x86_64.AppImage + .zsync
./Organizer-x86_64.AppImage /tmp/src     # runs with no host gtk/gstreamer/glycin
```

Only a container runtime is needed on the host — this is the easiest path
when the toolchain (`cargo-c`, `linuxdeploy`, ...) is missing or you are not
on ubuntu:22.04 (e.g. Fedora):

```sh
./packaging/build-appimage-container.sh  # podman/docker, ubuntu:22.04 inside
```

Bundled inside the AppDir: `gtk4`/`libadwaita`, GStreamer plugins +
`libgstgtk4.so` (`gtk4paintablesink`), `glycin-loaders` 2+ binaries + conf.d,
`bwrap` + `libseccomp.so.2`, schemas/icon caches. Host-assumed (not bundled):
`glibc`, `libfuse2`, Wayland/X11 + GL stack, GPU drivers.

### GPU-less hosts (software rendering)

On hosts without a GPU (llvmpipe/lavapipe software GL — typical VMs and
containers), the default GSK Vulkan renderer can run out of host memory and
kill the app. Symptoms in the terminal:

```text
Gsk-WARNING: func_vkImportSemaphoreFdKHR(): ... VK_ERROR_OUT_OF_HOST_MEMORY
Gdk-Message: Error 22 (Invalid argument) dispatching to Wayland display.
```

This is a renderer/environment failure, not an Organizer bug. Workaround —
force the CPU renderer (or the X11 backend) for the session:

```sh
GSK_RENDERER=cairo ./Organizer-x86_64.AppImage /tmp/src
# or: GDK_BACKEND=x11 ./Organizer-x86_64.AppImage /tmp/src
```

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

CI (`.github/workflows/appimage.yml`) does one thing: build
`Organizer-x86_64.AppImage` (+ `.zsync`) on push to `main` (workflow
artifact) and attach both to a GitHub Release on `v*` tags. No tests, no
matrix, no raw tarball — local `cargo test` and `./packaging/smoke.sh` cover
that before push.
