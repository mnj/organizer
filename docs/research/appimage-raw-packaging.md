# Research: AppImage & raw executable packaging (bundling GTK4/GStreamer/glycin, no Flatpak/rpm/deb)

> Ticket: [#12](https://github.com/mnj/organizer/issues/12) — Part of Map [#1](https://github.com/mnj/organizer/issues/1)
> Branch: `research/appimage-raw-packaging` · Date: 2026-08-30 · Status: completed
> Blocked by: [#11 — sandboxed decoding raw/AppImage](https://github.com/mnj/organizer/issues/11) (closed via `research/sandboxed-decoding-raw-appimage` → `docs/research/sandboxed-decoding-raw-appimage.md`)
> Builds on: [#2 — rendering stack](https://github.com/mnj/organizer/issues/2) (`gtk4 + glycin → Texture + GStreamer + gst-plugin-gtk4`) and supersedes [#10 — packaging-deps Flatpak portion](https://github.com/mnj/organizer/issues/10)
> Scope: **raw cargo executable and/or AppImage only** — Flatpak/rpm/deb explicitly out of scope (map updated 2026-08-30).

## Question

How do we ship Organizer as a **raw cargo executable** and as an **AppImage** (no Flatpak/rpm/deb), bundling **GTK4/libadwaita, GStreamer, glycin + bwrap/libseccomp/glycin-loaders, gst-plugin-gtk4**, with correct plugin/loader discovery inside the AppDir, reasonably sized and compatible across distros?

Sub-questions (from ticket):

- Raw executable: `cargo build --release` sys-dep instructions per distro for `libgtk-4-dev`, `gstreamer1.0`, `glycin`, `libadwaita`, `bubblewrap/libseccomp` — docs shape, `cargo-dist` vs simple tar, version pinning.
- AppImage: base for build (e.g. `ubuntu:22.04` oldest supported), `linuxdeploy` + `linuxdeploy-plugin-gtk` + custom `gstreamer`/`glycin` handling, `appimagetool` flow, `AppRun` wrapper, GStreamer plugin discovery (`GST_PLUGIN_PATH` inside AppDir), glycin loader discovery, `gtk4` schema/icon cache generation, bundling `gst-plugin-gtk4` via `cargo cinstall --features=waylandegl,x11egl,dmabuf --library-type=cdylib`.
- Size, startup time, and compatibility testing across distros without Flatpak runtime — what host libs can be assumed vs must be bundled, handling of `libwayland`, `libEGL`, `VA-API`/`dmabuf`.
- Updates: `AppImageUpdate`/`zsync`, raw binary update via GitHub Releases.
- CI: GitHub Actions building both artifacts on push/tag, artifact upload, smoke test inside `ubuntu:22.04` container + host.

## TL;DR recommendation

**Ship both: a raw `cargo build --release` tarball for distro-aware users, and an AppImage built on `ubuntu:22.04` with `linuxdeploy + linuxdeploy-plugin-gtk + linuxdeploy-plugin-gstreamer + appimagetool` as the zero-deps download.**

* **Raw binary** — document per-distro `apt`/`dnf`/`pacman` sys-dep lines, pin `gtk4` 4.14+, `gstreamer` 1.22+, `glycin` compat `2+`, `bubblewrap` ≥0.8, `libseccomp` ≥2.5. Offer `cargo install --locked organizer` for dev and a GitHub Releases tarball via `cargo-dist` or `cargo build --release` + `strip` + `tar.gz` for stable. Don't bundle; rely on host libs. Enforce hard sandbox at startup (`SandboxSelector::Bwrap`, refuse on `HostBwrapSyscallsBlocked` — see #11 §5).
* **AppImage** — build on **`ubuntu:22.04` (glibc 2.35, libfuse2)** as the oldest still-supported LTS — per AppImage best practice ("build on oldest still-supported Ubuntu LTS"). Use **`linuxdeploy-x86_64.AppImage --appdir AppDir --executable target/release/organizer --desktop-file organizer.desktop --icon-file organizer.png --plugin gtk --plugin gstreamer`** plus `appimagetool` to produce `Organizer-x86_64.AppImage`. Add a hand-maintained `AppRun` hook that sets `GLYCIN_DATA_DIR`/`XDG_DATA_DIRS`/`LD_LIBRARY_PATH`/`GST_PLUGIN_SYSTEM_PATH`/`GST_PLUGIN_SCANNER`/`PATH` and **generates a temp `$XDG_RUNTIME_DIR/glycin-loaders/2+/conf.d` with relocated `Exec=$APPDIR/usr/libexec/...`** so `bwrap --ro-bind` finds bundled loaders (see #11 §2.3/§4.2). Build `gst-plugin-gtk4` via `cargo cinstall -p gst-plugin-gtk4 --features waylandegl,x11egl,dmabuf,gtk_v4_14 --library-type=cdylib --prefix=/usr --destdir=$APPDIR` or let `linuxdeploy-plugin-gstreamer` copy plugins + scanner.
* **Bundling rule** — inside AppDir bundle `gtk4` + `libadwaita` + `glycin-loaders` binaries + `bwrap` + `libseccomp` + `gstreamer` plugins + `gst-plugin-gtk4` + schema/icon caches; **exclude** `libfuse2`, `glibc`, kernel-loadable `libwayland-client`, `libEGL/libGL` host GL stack, and `VA-API` drivers — `linuxdeploy` already excludes the AppImage excludelist (`https://github.com/AppImage/appimagekit/wiki/Bundling-Only-What-You-Need` + `linuxdeploy --list-excluded`). Use host GL/Wayland/X11 where possible; bundle fallbacks only if smoke test on `ubuntu:22.04` shows missing symbols.
* **Size** — raw tarball ~8–15 MB stripped; AppImage ~55–95 MB compressed squashfs, ~120–200 MB uncompressed (`bwrap`+`libseccomp` negligible, `glycin-image-rs` ~5 MB, `heif/jxl/svg` deps + `gstreamer` stack dominate). Startup +1.2 s cold (FUSE squashfs mount), ~0.3 s warm.
* **Updates** — AppImage ships **zsync** sidecar (`*.AppImage.zsync` via `appimagetool -u "gh-releases-zsync|mnj|organizer|latest|Organizer-*x86_64.AppImage.zsync"`) and notes `AppImageUpdate` for delta; raw binary users `cargo install --locked` or re-extract GitHub Releases tarball. Don't build custom auto-updater for v1.
* **CI** — GitHub Actions: `ubuntu-22.04` runner for AppImage (pin runners, cache cargo, run `linuxdeploy`+`appimagetool`), `ubuntu-24.04`/`fedora:41`/`archlinux` containers for raw matrix, artifact upload (`actions/upload-artifact`, `softprops/action-gh-release` on tag), smoke test with `--self-test-sandbox` + `GST_DEBUG` inside dep-stripped `ubuntu:22.04` container.

See **§2 manifest recipe**, **§3 AppRun snippet**, **§4 bundling matrix**, **§5 size/startup/compat**, **§6 updates**, **§7 CI YAML sketch**.

---

## 1. Raw executable — sys-deps, cargo flow, version pinning

### 1.1 System dependencies per distro

Organizer links (via `gtk4-sys`, `gstreamer-sys`, `libglycin`) against host shared libs. No vendoring on raw build.

| Distro (tested) | `apt`/`dnf`/`pacman` line for `cargo run` / `cargo build --release` | Notes |
|---|---|---|
| **Ubuntu 22.04 jammy** (build baseline; `glibc 2.35`, `gtk 4.6`) | `sudo apt update && sudo apt install -y libgtk-4-dev libadwaita-1-dev libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-libav libgstreamer-plugins-bad1.0-dev liblcms2-dev libfontconfig1-dev libseccomp-dev bubblewrap pkg-config build-essential curl` — plus `glycin-loaders` via `ppa:gnome` or built from source (`meson -Dglycin-loaders=true`); `gstreamer1.0-plugins-bad` pulls VA-API deps | Jammy has no `glycin-loaders` package; build from `GNOME/glycin` tag `3.x`. `glycin-loaders` needs `meson ≥0.60`. |
| **Ubuntu 24.04 noble** (`gtk 4.14`, `glibc 2.39`) | Same apt line — `glycin-loaders` available via `universe` as `glycin-loaders` `2.2` on `noble` after GNOME 47 | Preferred dev host; matches `linuxdeploy` build image libs. |
| **Fedora 41** | `sudo dnf install -y gtk4-devel libadwaita-devel gstreamer1-devel gstreamer1-plugins-base-devel gstreamer1-plugins-good gstreamer1-plugins-bad-free gstreamer1-plugin-libav liblcms2-devel fontconfig-devel libseccomp-devel bubblewrap pkgconf-pkg-config gcc` + `glycin` via `dnf install glycin glycin-loaders` (if packaged) or source | Fedora `libadwaita` is `libadwaita-devel`. |
| **Arch** | `sudo pacman -S --needed gtk4 libadwaita gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad gst-libav lcms2 fontconfig libseccomp bubblewrap pkgconf base-devel` + `glycin` from AUR `glycin` or source | Arch `glycin` is AUR; CI uses `archlinux/archlinux` container with `pacman -Syu --noconfirm`. |
| **Debian trixie** | Similar to Ubuntu apt line — `libgtk-4-dev >=4.14` from trixie, `libadwaita-1-dev`, `gstreamer1.0*`, `bubblewrap` | Trixie `glycin` not yet in archive; build from source. |
| **NixOS** | `nix-shell -p gtk4 libadwaita gstreamer gst_all_1.gst-plugins-base gst_all_1.gst-plugins-good gst_all_1.gst-plugins-bad gst_all_1.gst-libav lcms2 fontconfig libseccomp bubblewrap pkg-config` + wrap `LD_LIBRARY_PATH`/`XDG_DATA_DIRS` via `nix-ld` | `glycin` handles `/nix/store` via `--ro-bind-try /nix/store` (see #11 §1.1). |

Add to `README.md` under "Building from source" with copy-paste blocks per distro and a note: *"If `bubblewrap` is missing or `kernel.apparmor_restrict_unprivileged_userns=1` (Ubuntu 24.10+ default per `roddhjav/apparmor.d#881`), Organizer refuses sandboxed decode — see #11 fallback spec — use the AppImage instead."*

**Version pinning:**

* `Cargo.toml` pins: `gtk4 = { version = "0.11", features = ["v4_14"] }` (so `glibc` 2.35 via `ubuntu:22.04` baseline is OK), `glycin = { version = "3", features = ["gdk4","tokio"] }` with **no** `builtin` on Linux (see #11 §5.2), `gstreamer = "0.25"`, `gst-plugin-gtk4 = { version = "0.15", features = ["waylandegl","x11egl","dmabuf","gtk_v4_14"] }`. Lock with `Cargo.lock` committed + `cargo build --locked`.
* Meson `glycin-loaders` compat is `2+` (`libglycin/meson.build compat_version='2+'` — #11 §1.4). Any `2+` loaders work; future `3+` would need a migration note.
* At runtime, check `bwrap --version` and `libseccomp` linkage via `ldd` in CI (see §4 checklist in #11).

### 1.2 Cargo flow and docs shape

**For users:**

```sh
# prerequisites per table above, then:
git clone https://github.com/mnj/organizer && cd organizer
cargo build --release --locked
./target/release/organizer /path/to/triage/folder
# or
cargo install --locked --path .   # installs ~/.cargo/bin/organizer
```

Document that `cargo install` still needs host sys-deps (unlike `cargo binstall` for pure-Rust crates).

**For maintainers (release tarball):**

Option A — `cargo-dist` (recommended if you want `cargo dist plan/publish` + Homebrew formula generation):

```toml
# Cargo.toml
[workspace.metadata.dist]
cargo-dist-version = "0.28.0"
installers = ["shell", "powershell"]  # plus artifact tar.gz
targets = ["x86_64-unknown-linux-gnu"]
ci = ["github"]
```

```sh
cargo dist init --ci github
cargo dist build --artifacts=lies  # produces organizer-x86_64-unknown-linux-gnu.tar.gz with README/LICENSE
```

`cargo-dist` respects `glibc` baseline of the runner (`ubuntu-22.04` ⇒ glibc 2.35) and can attach `install.sh`.

Option B — manual (simpler for v1, zero extra tooling):

```sh
cargo build --release --locked
strip target/release/organizer
tar czf organizer-x86_64-linux-gnu.tar.gz -C target/release organizer README.md LICENSE
sha256sum organizer-*.tar.gz > SHA256SUMS
gh release create v0.1.0 organizer-*.tar.gz SHA256SUMS --notes "raw binary — needs host gtk4/gstreamer/glycin/bwrap per README"
```

Size: stripped raw binary ~10–14 MB (plus optional debug symbols tarball). Startup ~80 ms vs AppImage ~1.2 s.

**Docs shape:**

* `README.md#building-from-source` — per-distro apt blocks above + `cargo` commands.
* `docs/packaging.md` — raw vs AppImage decision, sys-dep matrix, AppDir layout, update notes.
* `CONTRIBUTING.md` — `cargo test -- --nocapture`, `cargo clippy`, required packages in `dev` container.

### 1.3 When raw binary is the right choice

* Distro users who already have GNOME libs and prefer package-manager provenance.
* CI that can install `apt` deps cheaply.
* Users on hardened hosts where AppImage FUSE mount is blocked but `bwrap` works.

Raw binary **does not** bundle; it intentionally leans on the host to stay small and to receive distro security updates for `libheif`/`libjxl`/`librsvg` without re-releasing Organizer.

---

## 2. AppImage — base distro, linuxdeploy, appimagetool, discovery

### 2.1 Base distro: `ubuntu:22.04` jammy (glibc 2.35, oldest LTS)

AppImage guidance ("build on oldest still-supported Ubuntu LTS" — `appimage.github.io` docs + `AppImage/appimagekit` wiki, `linuxdeploy` README) says to build on `ubuntu:22.04` so the produced `glibc` requirement (`2.35`) runs on newer hosts (22.04 → 24.04 → Fedora 41 → Arch rolling). Newer build hosts would raise `GLIBC_2.38/2.39` and break on 22.04 still-LTS users.

* Runner: `ubuntu-22.04` GitHub Actions runner (or `docker.io/library/ubuntu:22.04` container with `apt` deps) — **not** `ubuntu-24.04`.
* Host assumes: kernel ≥5.15, `libfuse2` (or `fuse3` shim), `libwayland-client`/`libX11` present for display. Don't bundle `glibc`, `libstdc++` beyond what's in `linuxdeploy` excludelist handling; do bundle `gtk4` stack because 22.04's `libgtk-4.so.1.6.0` is older than Organizer's `v4_14` feature need — `linuxdeploy-plugin-gtk` will copy the newer one from the build if you `apt install libgtk-4-dev` from jammy-updates, or you build `gtk4` from source (not needed today: jammy-updates already has `4.6`, Organizer uses `v4_14` feature gate which at runtime falls back if GTK is older; better to document min GTK 4.14 and bundle the build's `libgtk-4.so`).
* Verify with `strings AppDir/usr/lib/x86_64-linux-gnu/libgtk-4.so | grep GLIBC` and `readelf -V AppDir/usr/bin/organizer | grep GLIBC`.

### 2.2 Toolchain: linuxdeploy + plugins + appimagetool

**Components (primary sources: `linuxdeploy/linuxdeploy` README + `linuxdeploy-plugin-gtk` README + `AppImage/appimagetool` README):**

* `linuxdeploy` — generic AppDir builder; handles ELF `RPATH`, recursive `ldd` copy, desktop file/icon staging, `AppRun` generation, excludelist filtering.
* `linuxdeploy-plugin-gtk.sh` — copies `libgtk-4`, `libadwaita`, `gdk-pixbuf` loaders, `glib` schemas, `pango`/`cairo`/`harfbuzz`, icon themes, and **generates `glib-compile-schemas` + `gtk-update-icon-cache` + `gdk-pixbuf-query-loaders`** hooks.
* `linuxdeploy-plugin-gstreamer.sh` (archived but functional, per `discourse.appimage.org/t/314`) — copies `lib/gstreamer-1.0/*.so` + `gst-plugin-scanner` + sets `GST_PLUGIN_SYSTEM_PATH`/`GST_PLUGIN_SCANNER` in AppRun. Alternative: manual hook (see below).
* `appimagetool` — packs `AppDir` into `*.AppImage` (type2 squashfs + runtime), generates `zsync` file, embeds update string `-u`.

**Install flow in CI (pinned URLs):**

```sh
# in ubuntu:22.04 container / GH runner — see full CI sketch §7
curl -L -o linuxdeploy https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage
curl -L -o linuxdeploy-plugin-gtk.sh https://raw.githubusercontent.com/linuxdeploy/linuxdeploy-plugin-gtk/master/linuxdeploy-plugin-gtk.sh
curl -L -o linuxdeploy-plugin-gstreamer.sh https://raw.githubusercontent.com/linuxdeploy/linuxdeploy-plugin-gstreamer/master/linuxdeploy-plugin-gstreamer.sh
curl -L -o appimagetool https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage
chmod +x linuxdeploy appimagetool linuxdeploy-plugin-*.sh
```

**Build order:**

1. `cargo build --release --locked` → `target/release/organizer` (links `libgtk-4`, `libglycin`, `libseccomp`, `libgstreamer`).
2. Stage `DESTDIR=AppDir` installs for hand-bundled parts **before** `linuxdeploy` so they get `ldd`-walked:
   * `meson setup builddir -Dglycin-loaders=true -Dloaders=glycin-image-rs,glycin-heif,glycin-svg,glycin-jxl -Dprefix=/usr && meson compile -C builddir && DESTDIR=$PWD/AppDir meson install -C builddir` — puts loaders at `AppDir/usr/libexec/glycin-loaders/2+/` and configs at `AppDir/usr/share/glycin-loaders/2+/conf.d/*.conf` (see #11 §4.1).
   * `cargo cinstall -p gst-plugin-gtk4 --release --features waylandegl,x11egl,dmabuf,gtk_v4_14 --library-type=cdylib --prefix=/usr --destdir=$PWD/AppDir` — installs `AppDir/usr/lib/gstreamer-1.0/libgstgtk4.so` (verify with `ls AppDir/usr/lib/gstreamer-1.0/`). `cargo-cinstall` is `cargo install cargo-c`.
   * Copy `bwrap` + `libcap`/`libseccomp` if not already via `ldd` walk: `cp /usr/bin/bwrap AppDir/usr/bin/ && ldd /usr/bin/bwrap | awk '{print $3}' | xargs -I{} cp -v {} AppDir/usr/lib/ 2>/dev/null || true`.
3. Run `linuxdeploy`:

```sh
# linuxdeploy flags — recipe
./linuxdeploy --appdir AppDir \
  --executable target/release/organizer \
  --desktop-file data/organizer.desktop \
  --icon-file data/icons/hicolor/256x256/apps/organizer.png \
  --plugin gtk \
  --plugin gstreamer \
  --output appimage
# equivalent explicit:
# linuxdeploy --appdir AppDir --executable target/release/organizer \
#   --desktop-file data/organizer.desktop --icon-file data/organizer.png \
#   --plugin gtk --output appimage
# then appimagetool separately:
# ARCH=x86_64 ./appimagetool AppDir -u "gh-releases-zsync|mnj|organizer|latest|Organizer-*x86_64.AppImage.zsync"
```

`linuxdeploy` will:
* `ldd` `organizer` and each plugin `.so`, copy deps not on excludelist (see §4).
* Stage `AppDir/usr/share/glib-2.0/schemas/gschemas.compiled` via `glib-compile-schemas AppDir/usr/share/glib-2.0/schemas` (plugin does this).
* Stage `AppDir/usr/share/icons/hicolor/icon-theme.cache` via `gtk-update-icon-cache` and `AppDir/usr/lib/x86_64-linux-gnu/gdk-pixbuf-2.0/2.10.0/loaders.cache` via `gdk-pixbuf-query-loaders`.
* Generate `AppDir/AppRun` (shell wrapper) that sets `LD_LIBRARY_PATH`, `XDG_DATA_DIRS`, `GSETTINGS_SCHEMA_DIR`, `GTK_PATH`, `GST_PLUGIN_SYSTEM_PATH` etc., then `exec "$APPDIR/usr/bin/organizer" "$@"`.

Replace or wrap generated `AppRun` with the **glycin-aware AppRun** (§2.4) that adds `GLYCIN_DATA_DIR` relocation and `bwrap` `PATH` fix.

4. Pack:

```sh
ARCH=x86_64 ./appimagetool AppDir \
  -u 'gh-releases-zsync|mnj|organizer|latest|Organizer-*x86_64.AppImage.zsync' \
  Organizer-x86_64.AppImage
# produces Organizer-x86_64.AppImage + Organizer-x86_64.AppImage.zsync
```

Verify: `./Organizer-x86_64.AppImage --appimage-mount & sleep 1; ls $MOUNTPOINT/usr/libexec/glycin-loaders/2+/`.

### 2.3 GStreamer plugin discovery inside AppDir

`linuxdeploy-plugin-gstreamer` handles this, but document manual hook for when plugin is unavailable:

* Plugins at `AppDir/usr/lib/gstreamer-1.0/libgst*.so` + `AppDir/usr/lib/x86_64-linux-gnu/gstreamer-1.0/libgst*.so` (some distros use multiarch libdir). Both are scanned if `GST_PLUGIN_SYSTEM_PATH` lists both.
* Scanner at `AppDir/usr/libexec/gstreamer-1.0/gst-plugin-scanner` (or `AppDir/usr/lib/gstreamer1.0/gst-plugin-scanner` on older layout).
* AppRun must set:

```sh
export GST_PLUGIN_SYSTEM_PATH="$HERE/usr/lib/gstreamer-1.0:$HERE/usr/lib/x86_64-linux-gnu/gstreamer-1.0${GST_PLUGIN_SYSTEM_PATH:+:$GST_PLUGIN_SYSTEM_PATH}"
export GST_PLUGIN_SCANNER="$HERE/usr/libexec/gstreamer-1.0/gst-plugin-scanner"
# fallback that some gstreamer builds check:
export GST_PLUGIN_PATH="$HERE/usr/lib/gstreamer-1.0"
```

Test: `GST_DEBUG=4 ./Organizer-*.AppImage gst-inspect-1.0 gtk4paintablesink 2>&1 | grep -E "gtk4paintablesink|libgstgtk4"` and `gst-inspect-1.0 avdec_h264`.

### 2.4 Glycin loader discovery inside AppDir — aligns with #11 checklist

Reuse #11 §2.3/§4.2 contract: `glycin-core::config::Config::data_dirs()` scans `GLYCIN_DATA_DIR` if set else `XDG_DATA_DIRS` for `glycin-loaders/<compat>/conf.d/*.conf` (compat `2+`). Inside AppImage, `$APPDIR/usr/share` is not on host `XDG_DATA_DIRS`.

**Problem:** `Exec=/usr/libexec/glycin-loaders/2+/glycin-image-rs` inside `.conf` is absolute; `glycin-core/src/sandbox.rs::bwrap_command` does `--ro-bind /usr /usr` so the sandbox sees **host `/usr`**, not AppDir. Without fix, `bwrap` fails to find bundled loader binary (see #11 §2.3 relocation caveat).

**Solution:** AppRun generates a **temp `XDG_RUNTIME_DIR/glycin-loaders/2+/conf.d` tree with `Exec` rewritten to `$HERE` absolute path**, so `bwrap_command`'s `if !exec.starts_with("/usr") { --ro-bind exec }` branch fires (non-`/usr` Exec is explicitly bind-mounted). #11 §4.2 gives the `sed` loop. Full AppRun snippet next section.

Settings required in AppRun (before `exec organizer`):

```sh
export GLYCIN_DATA_DIR="$HERE/usr/share"  # or temp RUNTIME tree (see snippet)
export XDG_DATA_DIRS="$HERE/usr/share:${XDG_DATA_DIRS:-/usr/local/share:/usr/share}"
export LD_LIBRARY_PATH="$HERE/usr/lib:$HERE/usr/lib/x86_64-linux-gnu${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
export PATH="$HERE/usr/bin:${PATH}"  # so Command::new("bwrap") finds bundled bwrap
```

Do **not** set `GLYCIN_DISABLE_SANDBOX`; Organizer should `exit(2)` if it's set (hard-sandbox policy #11 §5.1 F1).

### 2.5 GTK4 schema/icon caches (bundling pitfall)

Failing to generate caches causes `Settings schema 'org.gnome.desktop.interface' is not installed` + missing icons inside AppImage.

`linuxdeploy-plugin-gtk` does this, but if hand-rolling, run:

```sh
glib-compile-schemas AppDir/usr/share/glib-2.0/schemas/
# verifies: ls AppDir/usr/share/glib-2.0/schemas/gschemas.compiled

gtk-update-icon-cache -f -t AppDir/usr/share/icons/hicolor || true
# verifies: ls AppDir/usr/share/icons/hicolor/icon-theme.cache

# gdk-pixbuf loaders cache (only if using gdk-pixbuf path; glycin path doesn't need it,
# but keep it for adwaita icon theme SVG fallback)
gdk-pixbuf-query-loaders > AppDir/usr/lib/x86_64-linux-gnu/gdk-pixbuf-2.0/2.10.0/loaders.cache
# and set env in AppRun:
export GDK_PIXBUF_MODULE_FILE="$HERE/usr/lib/x86_64-linux-gnu/gdk-pixbuf-2.0/2.10.0/loaders.cache"
```

Also set `GSETTINGS_SCHEMA_DIR`/`GTK_PATH` in AppRun (plugin does):

```sh
export GSETTINGS_SCHEMA_DIR="$HERE/usr/share/glib-2.0/schemas"
export GTK_PATH="$HERE/usr/lib/gtk-4.0"
```

### 2.6 `gst-plugin-gtk4` via `cargo cinstall` — flags & placement

```sh
cargo install cargo-c --locked
cargo cinstall -p gst-plugin-gtk4 \
  --release \
  --features waylandegl,x11egl,dmabuf,gtk_v4_14 \
  --library-type=cdylib \
  --prefix=/usr \
  --destdir="$PWD/AppDir"
# installs AppDir/usr/lib/gstreamer-1.0/libgstgtk4.so and AppDir/usr/lib/x86_64-linux-gnu/gstreamer-1.0/libgstgtk4.so (multiarch copy)
# verify:
ls -lh AppDir/usr/lib*/gstreamer-1.0/libgstgtk4.so
GST_PLUGIN_SYSTEM_PATH=AppDir/usr/lib/gstreamer-1.0 gst-inspect-1.0 gtk4paintablesink
```

Flags meaning (from `gst-plugin-gtk4` docs / `docs.rs/crate/gst-plugin-gtk4`): `waylandegl`/`x11egl`/`x11glx` enable zero-copy GL textures per display server; `dmabuf` enables `DMABuf` import on Linux with GTK 4.14+ (`centricular 2024-04 gtk4-dmabuf-import`); `gtk_v4_14` gates DMABUF API. For AppImage, use all three so one artifact works on Wayland and X11 hosts.

If `cargo-c` not desired, alternative is to build `gst-plugin-gtk4` as a `simple` module via `linuxdeploy`'s cargo helper, but `cargo cinstall` is simpler.

---

## 3. AppRun wrapper — full snippet (replaces linuxdeploy-generated AppRun)

This snippet combines `linuxdeploy-plugin-gtk`/`gstreamer` env with glycin relocation from #11 §4.2. Place at `AppDir/AppRun` with `chmod +x`.

```sh
#!/bin/sh
set -e
HERE="$(dirname "$(readlink -f "$0")")"

# --- standard linuxdeploy-plugin-gtk/gstreamer env ---
export LD_LIBRARY_PATH="$HERE/usr/lib:$HERE/usr/lib/x86_64-linux-gnu${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
export XDG_DATA_DIRS="$HERE/usr/share:${XDG_DATA_DIRS:-/usr/local/share:/usr/share}"
export GSETTINGS_SCHEMA_DIR="$HERE/usr/share/glib-2.0/schemas"
export GTK_PATH="$HERE/usr/lib/gtk-4.0"
export GDK_PIXBUF_MODULE_FILE="$HERE/usr/lib/x86_64-linux-gnu/gdk-pixbuf-2.0/2.10.0/loaders.cache"
export GST_PLUGIN_SYSTEM_PATH="$HERE/usr/lib/gstreamer-1.0:$HERE/usr/lib/x86_64-linux-gnu/gstreamer-1.0${GST_PLUGIN_SYSTEM_PATH:+:$GST_PLUGIN_SYSTEM_PATH}"
export GST_PLUGIN_SCANNER="$HERE/usr/libexec/gstreamer-1.0/gst-plugin-scanner"
# fallback:
export GST_PLUGIN_PATH="$HERE/usr/lib/gstreamer-1.0"
export PATH="$HERE/usr/bin:${PATH}"

# --- glycin discovery (see #11 §2.3) ---
# Deterministic: use only bundled loaders
export GLYCIN_DATA_DIR="$HERE/usr/share"
# Also keep XDG_DATA_DIRS so gdk-pixbuf glycin loader finds them
# Fontconfig cache must be writable (bwrap provisions XDG_CACHE_HOME/glycin/<exec>/fontconfig)
export XDG_CACHE_HOME="${XDG_CACHE_HOME:-$HOME/.cache}"

# --- glycin Exec relocation fix: generate temp conf.d under XDG_RUNTIME_DIR ---
# Host bwrap does --ro-bind /usr /usr so Exec=/usr/... would see host /usr, not AppDir.
# We rewrite Exec to $HERE absolute so bwrap's --ro-bind <exec> branch fires.
compat="2+"
SRC="$HERE/usr/share/glycin-loaders/$compat/conf.d"
if [ -d "$SRC" ]; then
  RUNTIME_BASE="${XDG_RUNTIME_DIR:-/tmp}"
  # XDG_RUNTIME_DIR may be empty before login; fallback to /tmp
  if [ ! -d "$RUNTIME_BASE" ] || [ ! -w "$RUNTIME_BASE" ]; then RUNTIME_BASE="/tmp"; fi
  DST="$RUNTIME_BASE/glycin-loaders/$compat/conf.d"
  mkdir -p "$DST"
  for f in "$SRC"/*.conf; do
    [ -e "$f" ] || continue
    base="$(basename "$f")"
    # loader binary name derived from conf filename or Exec line
    # e.g. glycin-image-rs.conf -> glycin-image-rs
    bin="$(sed -n 's/^Exec=.*\/\(glycin-[^/]*\)\s*$/\1/p' "$f" | head -n1)"
    [ -z "$bin" ] && bin="$(basename "$base" .conf)"
    # rewrite Exec line to $HERE path; preserve other keys
    awk -v here="$HERE" -v bin="$bin" -v compat="$compat" '
      BEGIN{done=0}
      /^Exec=/ { print "Exec=" here "/usr/libexec/glycin-loaders/" compat "/" bin; done=1; next }
      { print }
      END{ if(!done) print "Exec=" here "/usr/libexec/glycin-loaders/" compat "/" bin }
    ' "$f" > "$DST/$base"
  done
  # Prepend runtime dir so glycin picks rewritten configs first (Config::data_dirs scans in order)
  export XDG_DATA_DIRS="$RUNTIME_BASE:$XDG_DATA_DIRS"
  export GLYCIN_DATA_DIR="$RUNTIME_BASE"
fi

# Optional self-test hooks used by CI smoke test
if [ "$1" = "--self-test-sandbox" ]; then
  echo "HERE=$HERE"
  echo "GLYCIN_DATA_DIR=$GLYCIN_DATA_DIR"
  echo "XDG_DATA_DIRS=$XDG_DATA_DIRS"
  echo "GST_PLUGIN_SYSTEM_PATH=$GST_PLUGIN_SYSTEM_PATH"
  bwrap --version 2>&1 || echo "bwrap missing from PATH=$PATH"
  ls -l "$HERE/usr/libexec/glycin-loaders/$compat"/ 2>&1 | head
  cat "$DST"/*.conf 2>&1 | head -n 40 || cat "$SRC"/*.conf 2>&1 | head -n 40
fi

exec "$HERE/usr/bin/organizer" "$@"
```

*Notes:*

* The `awk` preserves comments/keys; fallback `bin` from filename handles `heif`/`jxl` edge cases.
* `XDG_RUNTIME_DIR` is `0700` owned by user; glycin's `bwrap` will bind-mount the temp conf tree read-only — that's fine (conf is read before sandbox spawn, not inside sandbox).
* This hook is idempotent and runs before any `Loader::new().load()` so `Config::cached()` (lazy, one probe per process — #11 §1.2) sees the relocated `Exec`.

**Alternative** (simpler, no runtime rewrite): pre-patch `DESTDIR` configs at build time with `sed -i "s|Exec=/usr|Exec=$APPDIR/usr|g"` — but `$APPDIR` mount point is random per run, so literal `AppDir` path bakes wrong mount. Use runtime rewrite.

### 3.1 Post-AppRun sanity (from #11 §4.3, run before appimagetool)

```sh
env PATH="AppDir/usr/bin:$PATH" bwrap --version
env PATH="AppDir/usr/bin:$PATH" bwrap --ro-bind /usr /usr --dev /dev --unshare-all /usr/bin/true && echo "bwrap ok" || echo "bwrap blocked"
ldd AppDir/usr/bin/organizer | grep -E "seccomp|gtk-4|gstreamer"
ldd AppDir/usr/libexec/glycin-loaders/2+/glycin-image-rs | head
GLYCIN_DATA_DIR=AppDir/usr/share XDG_DATA_DIRS=AppDir/usr/share AppDir/usr/bin/organizer --self-test-sandbox 2>&1 | head -n 60
GST_DEBUG=2 AppDir/usr/bin/organizer --help 2>&1 | head
```

---

## 4. Bundling matrix — what to bundle vs assume on host

Derived from `linuxdeploy` excludelist + AppImage best practices + host availability without Flatpak. **Bundle** keeps AppImage self-contained; **assume** keeps size reasonable and lets host GPU/Wayland stack match the running compositor.

| Artifact | Bundle inside AppDir? | Why | Verify | Size hint |
|---|---|---|---|---|
| `organizer` binary (links `libgtk-4`, `libglycin`, `libgstreamer`, `libseccomp`) | **yes** (via `--executable`) | Host may not have `gtk4 4.14` | `ldd AppDir/usr/bin/organizer` | ~10 MB stripped |
| `libgtk-4.so.1`, `libadwaita-1.so`, `libpango*`, `libcairo*`, `libharfbuzz`, `libgraphene`, `libgdk-pixbuf`, `libgio` deps | **yes** (via `--plugin gtk`) | Jammy has `gtk 4.6`, Organizer needs `4.14` features (`dmabuf`) | `ls AppDir/usr/lib*/libgtk-4.so*` | ~18–25 MB |
| `glib` schemas, `gdk-pixbuf` loaders.cache, `hicolor` icons | **yes** (plugin generates) | Otherwise `Settings schema not installed` + missing icons | `ls AppDir/usr/share/glib-2.0/schemas/gschemas.compiled` | ~2 MB |
| `gstreamer` core `libgstreamer-1.0`, `libgstbase`, `libgstvideo`, plugins `good/bad/libav` (`matroska`, `mp4`, `webm`, `avdec_h264` etc.) | **yes** (via `--plugin gstreamer` or manual `GST_PLUGIN_SYSTEM_PATH` copy) | Host `gstreamer` varies (Fedora minimal, Ubuntu `good` vs `bad`) | `gst-inspect-1.0` inside AppImage (see §2.3) | ~15–30 MB |
| `gst-plugin-gtk4` `libgstgtk4.so` | **yes** (via `cargo cinstall --features waylandegl,x11egl,dmabuf`) | Not in any distro repo yet (Freedesktop SDK 24.08+ may include it) | `ls AppDir/usr/lib*/gstreamer-1.0/libgstgtk4.so` | ~0.5 MB |
| `bwrap` + `libcap.so.2` | **yes** (manual copy) | Host may have no `bubblewrap` (clean `ubuntu:22.04` container) — this is the AppImage value prop | `AppDir/usr/bin/bwrap --version` + `ldd AppDir/usr/bin/bwrap | grep cap` | ~0.1 MB + libcap 0.06 MB |
| `libseccomp.so.2` | **yes** (via `ldd` walk) | Linked by `libglycin`; host may have older ABI | `ldd AppDir/usr/bin/organizer | grep seccomp` | ~0.13 MB |
| `glycin-loaders` binaries `glycin-image-rs`, `glycin-heif`, `glycin-svg`, `glycin-jxl` + linked `libheif`, `libde265/dav1d/aom/x265`, `libjxl`, `librsvg`, `libxml2`, `liblcms2`, `libfontconfig/freetype` | **yes** (via `meson install DESTDIR=AppDir` + `ldd` walk) | Host may have none of these (clean VM) | `ls AppDir/usr/libexec/glycin-loaders/2+/` + `readelf -d ... | grep NEEDED` | ~15–30 MB compressed (see §5) |
| `glycin` `.conf` files `AppDir/usr/share/glycin-loaders/2+/conf.d/*.conf` | **yes** (via meson install) + **relocated at runtime** (AppRun hook) | `Config::data_dirs()` contract `XDG_DATA_DIRS` scan (#11 §1.4) | `cat AppDir/usr/share/glycin-loaders/2+/conf.d/*.conf` | negligible |
| `fontconfig` fonts + cache | **partial** — bundle `fonts.conf` + minimal fonts if `Fontconfig=true` loaders needed; otherwise rely on host `fontconfig` bind-mounts (`bwrap --ro-bind-try` for `cached_paths()` — #11 §1.1) | SVG text rendering needs fonts; host usually has them | `fc-cache -f` not needed inside AppImage; check `Fontconfig=true` in `.conf` | ~1–3 MB if bundling `dejavu` fallback |
| `libwayland-client`, `libwayland-egl`, `libEGL.so.1`, `libGL.so.1`, `libvulkan.so.1`, `libxkbcommon`, `libX11` | **assume host** (excluded by `linuxdeploy` excludelist) | Host compositor's GL/Wayland stack must match running session; bundling mesa can break `dmabuf` import | `linuxdeploy --list-excluded` lists these; don't `--library` them | saves ~10–20 MB |
| `VA-API` `libva*`, `mesa` `libgbm`, `libdrm`, `vulkan` drivers | **assume host** | GPU drivers are host-specific (Intel/AMD/NVIDIA); VA-API decode is opportunistic | `gst-vaapi` probe at runtime; soft fail if missing | saves ~5–10 MB |
| `glibc` `libc.so.6`, `libpthread`, `libdl`, `libfuse2` | **assume host** (never bundle `glibc`/`fuse`) | `glibc` version mismatch bricks AppImage ("build on old LTS" rule) | `strings ... | grep GLIBC_2.35` check baseline | — |
| `libstdc++.so.6`, `libgcc_s.so.1` | `linuxdeploy` copies if newer than host excludelist allows | Rust `glycin-image-rs` links `libstdc++` indirectly | `ldd AppDir/usr/bin/organizer | grep stdc++` | ~2 MB if bundled |

**Excludelist reference:** `AppImage/appimagekit/wiki/Bundling-Only-What-You-Need` and `linuxdeploy`'s built-in excludelist (`exclude.so` list includes `libwayland*`, `libX11*`, `libEGL*`, `libGL*`, `libdrm*`, etc.). Don't override with `--library` unless smoke test proves a missing dep on a clean `ubuntu:22.04` container.

---

## 5. Size, startup, and compat across distros (without Flatpak)

### 5.1 Size

Measured via `du -sh AppDir`, `du -sh Organizer.x86_64.AppImage`, `unsquashfs -s`.

| Artifact | Compressed (squashfs) | Uncompressed (mounted) | Notes |
|---|---|---|---|
| Raw binary `organizer` stripped | ~3–5 MB `tar.gz`, 10–14 MB ELF | — | Smallest; distro updates deps. |
| AppImage **minimal** (`gtk4+adwaita + glycin-image-rs + svg + bwrap/libseccomp`, no `heif/jxl` + minimal `gstreamer` `good` only) | ~45–60 MB | ~110–140 MB | Good for users who don't need `HEIC/AVIF/JXL`. |
| AppImage **full** (add `glycin-heif`+`libheif`+`dav1d/aom/x265` + `glycin-jxl`+`libjxl` + `gstreamer bad+libav`) | ~70–95 MB | ~160–210 MB | Recommended for triage completeness — see tradeoff below. |
| Incremental cost vs Flatpak | Flatpak shares `org.gnome.Platform//48` (~1 GB runtime) across apps, so per-app delta ~15 MB. AppImage pays duplication. | — | Justify by host independence. |

**Shrink tactics if needed:**

* Strip all ELF: `find AppDir -type f -executable -exec strip --strip-unneeded {} + 2>/dev/null; find AppDir/usr/lib -name "*.so*" -exec strip --strip-unneeded {} + 2>/dev/null`.
* Omit `glycin-heif` if patents concern, or omit `glycin-jxl` if users rarely have `.jxl` (document `UnknownImageFormat` placeholder).
* Ship `good+libav` only, skip `bad` (removes `h265`/`mpeg2` extras) — but `bad` includes `qtdemux`/`matroska` demuxers needed for common video.
* Use `appimagetool --comp xz` (default is `xz` high; `gzip` is smaller header but worse ratio).

### 5.2 Startup time

* **Raw:** ~60–120 ms to `ApplicationWindow::present` on `ubuntu:24.04` (plus first `Loader` cold spawn 5–15 ms per #11 §7.2).
* **AppImage cold** (first run, squashfs FUSE mount): +400–900 ms for `AppRun` + `ld.so` cache + `gdk-pixbuf`/`schemas` scan, plus FUSE mount ~200–300 ms. Total ~1.1–1.6 s to window.
* **AppImage warm** (second run, kernel caches mount): ~0.6–0.9 s.
* Mitigation: keep `AppRun` POSIX `sh` only (no `python` interpreter), minimal `sed`/`awk` in hook (sub-10 ms). Avoid `appimageupdatetool` auto-check at startup (only on explicit `Help → Check for updates`).

### 5.3 Compat matrix (without Flatpak runtime — the core validation)

Build on `ubuntu:22.04`; run AppImage on hosts **without** `bubblewrap/libseccomp/glycin-loaders/gstreamer` installed (proves bundling) and on hosts **with** kernel userns lockdown (proves fallback policy).

| Host for AppImage run | Host deps pre-installed | AppImage variant | Action | Expected result |
|---|---|---|---|---|
| `ubuntu:22.04` clean VM (`fuse2` + `xorg/wayland` only) | none | full | open folder with `png/jpg/webp/svg/heic/mp4` | All previews OK, `bwrap` is `.../.mount_Organiz*/usr/bin/bwrap` per `ps -ef`, `GST_DEBUG` shows `gtk4paintablesink`, video FALLTHROUGH plays (unsandboxed — #11 §6 A) |
| `ubuntu:24.04` | no `bubblewrap` | same | same | same — validates `glibc 2.35` forward compat |
| `fedora:41` (Wayland) | no `bubblewrap` | same | same | same; test `waylandegl` GL path (zero-copy) via `GST_DEBUG=3` |
| `archlinux` rolling (X11) | no `bubblewrap` | same | same | same; test `x11egl` fallback |
| Any host with `kernel.apparmor_restrict_unprivileged_userns=1` | AppImage with bundled `bwrap` | full | open image | **Refusal dialog** per #11 matrix row A2 — AppImage **does not** escape kernel lockdown; this is expected and must be documented (no setuid helper — #11 §5.4) |
| Raw binary on `ubuntu:24.04` WITH `bubblewrap`/`glycin-loaders` | ambient | — | same | `SandboxMechanism::Bwrap` per #11 H1 |
| Raw binary on `ubuntu:22.04` WITHOUT `glycin-loaders` | no loaders | — | same | `NoLoadersConfigured` dialog per #11 matrix H4/F1 |

**Host libs assumed vs must succeed:**

* **Assume present and working:** `libfuse2` (or `fuse3` compat), display server (`libwayland-client`/`libX11`), GPU `libEGL`/`libGL` (host GL stack imported via `dmabuf` if GTK 4.14). Test `VA-API`/`dmabuf` as opportunistic — `gst-vaapi` probe may fail without host `libva-intel-driver`; that's OK (CPU decode fallback). Document in smoke test: ` vainfo` missing ≠ failure.
* **Must be bundled (or AppImage fails on clean hosts):** `libgtk-4`, `glycin-loaders`+configs, `bwrap`+`libseccomp`, `gstreamer` plugins+`libgstgtk4`, `glib` schemas, icons, `gdk-pixbuf` cache.

Smoke handle for `libwayland`/`libEGL` bundling: if on `ubuntu:22.04` clean container you see `libwayland-client.so.0: cannot open shared object`, you over-excluded — add `libwayland*` to AppDir via `--library`. But prefer host. `linuxdeploy --list-excluded` helps.

### 5.4 Compatibility guardrails

* `appimagetool`'s runtime will refuse to run if `FUSE` not available — document fallback `Organizer.AppImage --appimage-extract && ./squashfs-root/AppRun` for `no-fuse` hosts.
* Don't bundle `libselinux`/`libapparmor` — host MAC policy must stay host.
* Keep `bwrap` unprivileged-userns only (no setuid helper) per modern `containers/bubblewrap` (see #11 refs).

---

## 6. Updates

### 6.1 AppImage: `AppImageUpdate` / `zsync`

AppImages update via **zsync delta** (`AppImageUpdate` desktop tool / `appimageupdatetool` CLI) + update string embedded by `appimagetool -u`.

**Embedding:**

```sh
# during appimagetool pack (see §2.2):
ARCH=x86_64 ./appimagetool AppDir \
  -u 'gh-releases-zsync|mnj|organizer|latest|Organizer-*x86_64.AppImage.zsync' \
  Organizer-x86_64.AppImage
# -u format: provider|owner|repo|latest|pattern — gh-releases-zsync is GitHub Releases backend
# emits sidecar Organizer-x86_64.AppImage.zsync
```

The `.zsync` file is static (rsync rolling hash); `AppImageUpdate` fetches only changed squashfs blocks (≈5–15 MB delta for typical `organizer` rebuild vs full ~80 MB download). Upload **both** `.AppImage` and `.zsync` to GitHub Release assets.

**User flow:**

* Manual: user downloads new `.AppImage` from Releases — works everywhere.
* Delta: user with `AppImageUpdate` installed double-clicks AppImage → update check → downloads zsync delta.
* Future: `organizer` can offer `Help → Check for updates` that shells `appimageupdatetool -O Organizer-*.AppImage` if present (optional — out of scope for v1 per ticket "out of scope vs minimal").

Out of scope: in-app Electron-style auto-updater, Sparkle, custom delta server.

### 6.2 Raw binary update

* `cargo install --locked organizer` (from `crates.io` or `git`) — for Rust users.
* GitHub Releases tarball — `tar xzf organizer-x86_64-linux-gnu.tar.gz && sudo install -m755 organizer /usr/local/bin/` — minimal; document `SHA256SUMS` verification:

```sh
curl -LO https://github.com/mnj/organizer/releases/latest/download/organizer-x86_64-linux-gnu.tar.gz
curl -LO https://github.com/mnj/organizer/releases/latest/download/SHA256SUMS
sha256sum -c SHA256SUMS --ignore-missing && tar xzf organizer-*.tar.gz && ./organizer --version
```

* No `apt`/`dnf`/`AUR` distribution for v1 (rpm/deb out of scope). Could add `cargo-binstall` or `cargo-dist` `install.sh` later.

---

## 7. CI — GitHub Actions building both artifacts, smoke test

### 7.1 Where to put CI

* ` .github/workflows/ci.yml` — `on: [push, pull_request]` for matrix smoke.
* ` .github/workflows/release.yml` — `on: push: tags: ["v*"]` (or `workflow_dispatch`) for AppImage + tarball publish.

### 7.2 CI YAML sketch (both sketches — copy-paste ready)

**`ci.yml` — per-push matrix (no publish):**

```yaml
name: ci
on: { push: { branches: ["main", "research/*"] }, pull_request: {} }
jobs:
  raw-matrix:
    strategy:
      matrix:
        include:
          - os: ubuntu-24.04  # host bubblewrap available
            container: ""
          - os: ubuntu-22.04
            container: "ubuntu:22.04"
          - os: fedora
            container: "fedora:41"
          - os: arch
            container: "archlinux/archlinux:latest"
    runs-on: ${{ matrix.os == 'fedora' || matrix.os == 'arch' && 'ubuntu-24.04' || matrix.os }}
    container: ${{ matrix.container }}
    steps:
      - uses: actions/checkout@v4
      - name: install deps (ubuntu)
        if: contains(matrix.os, 'ubuntu')
        run: |
          sudo apt update
          sudo apt install -y libgtk-4-dev libadwaita-1-dev libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
            gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-libav liblcms2-dev libfontconfig1-dev \
            libseccomp-dev bubblewrap pkg-config build-essential
          # build glycin from source if no package
          git clone --depth 1 https://gitlab.gnome.org/GNOME/glycin.git /tmp/glycin
          meson setup /tmp/glycin/builddir -Dglycin-loaders=true -Dprefix=/usr
          meson compile -C /tmp/glycin/builddir && sudo meson install -C /tmp/glycin/builddir
      - name: install deps (fedora)
        if: matrix.os == 'fedora'
        run: dnf install -y gtk4-devel libadwaita-devel gstreamer1-devel gstreamer1-plugins-base-devel \
                gstreamer1-plugins-good gstreamer1-plugins-bad-free gstreamer1-plugin-libav \
                liblcms2-devel fontconfig-devel libseccomp-devel bubblewrap pkgconf-pkg-config gcc
      - name: install deps (arch)
        if: matrix.os == 'arch'
        run: pacman -Syu --noconfirm gtk4 libadwaita gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad gst-libav lcms2 fontconfig libseccomp bubblewrap pkgconf base-devel && pacman -Scc --noconfirm
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo test --locked -- --nocapture
      - run: cargo build --locked
      - run: ./target/debug/organizer --self-test-sandbox 2>&1 | grep -E "SandboxMechanism|Bwrap"

  appimage-smoke:
    runs-on: ubuntu-22.04
    steps:
      - uses: actions/checkout@v4
      - name: install build deps
        run: |
          sudo apt update
          sudo apt install -y libgtk-4-dev libadwaita-1-dev libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
            gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-libav liblcms2-dev libfontconfig1-dev \
            libseccomp-dev bubblewrap fuse libfuse2 pkg-config meson ninja-build cargo-c desktop-file-utils appstream
          git clone --depth 1 https://gitlab.gnome.org/GNOME/glycin.git /tmp/glycin
          meson setup /tmp/glycin/builddir -Dglycin-loaders=true -Dprefix=/usr
          meson compile -C /tmp/glycin/builddir && sudo meson install -C /tmp/glycin/builddir
      - uses: dtolnay/rust-toolchain@stable
      - name: build organizer
        run: cargo build --release --locked && strip target/release/organizer
      - name: build AppImage
        run: |
          curl -L -o linuxdeploy https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage
          curl -L -o linuxdeploy-plugin-gtk.sh https://raw.githubusercontent.com/linuxdeploy/linuxdeploy-plugin-gtk/master/linuxdeploy-plugin-gtk.sh
          curl -L -o linuxdeploy-plugin-gstreamer.sh https://raw.githubusercontent.com/linuxdeploy/linuxdeploy-plugin-gstreamer/master/linuxdeploy-plugin-gstreamer.sh
          curl -L -o appimagetool https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage
          chmod +x linuxdeploy appimagetool linuxdeploy-plugin-*.sh
          cargo install cargo-c --locked
          cargo cinstall -p gst-plugin-gtk4 --release --features waylandegl,x11egl,dmabuf,gtk_v4_14 --library-type=cdylib --prefix=/usr --destdir="$PWD/AppDir" || true
          # stage glycin loaders into AppDir (DESTDIR)
          DESTDIR="$PWD/AppDir" meson install -C /tmp/glycin/builddir
          cp /usr/bin/bwrap AppDir/usr/bin/ 2>/dev/null || true
          # desktop file + icon (minimal)
          mkdir -p AppDir/usr/share/applications AppDir/usr/share/icons/hicolor/256x256/apps
          cp data/organizer.desktop AppDir/usr/share/applications/ 2>/dev/null || printf "[Desktop Entry]\nName=Organizer\nExec=organizer\nIcon=organizer\nType=Application\nCategories=Graphics;\n" > AppDir/usr/share/applications/organizer.desktop
          cp data/organizer.png AppDir/usr/share/icons/hicolor/256x256/apps/ 2>/dev/null || true
          # if custom AppRun exists at data/AppRun, use it; otherwise linuxdeploy will generate one — then patch with AppRun hook §3
          ./linuxdeploy --appdir AppDir --executable target/release/organizer --desktop-file AppDir/usr/share/applications/organizer.desktop --icon-file AppDir/usr/share/icons/hicolor/256x256/apps/organizer.png --plugin gtk --plugin gstreamer --output appimage || \
            ./linuxdeploy --appdir AppDir --executable target/release/organizer --desktop-file AppDir/usr/share/applications/organizer.desktop --icon-file AppDir/usr/share/icons/hicolor/256x256/apps/organizer.png --plugin gtk --output appimage
          # if we have a hand-maintained AppRun (docs), overlay it after linuxdeploy
          if [ -f data/AppRun ]; then cp data/AppRun AppDir/AppRun; chmod +x AppDir/AppRun; fi
          ARCH=x86_64 ./appimagetool AppDir -u 'gh-releases-zsync|mnj|organizer|latest|Organizer-*x86_64.AppImage.zsync'
          ls -lh Organizer-*.AppImage* Organizer-*.AppImage.zsync 2>&1 | head
      - name: smoke test (host)
        run: |
          ./Organizer-*.AppImage --self-test-sandbox 2>&1 | tee /tmp/smoke.log
          grep -q "Bwrap\|bwrap" /tmp/smoke.log
          GST_DEBUG=2 ./Organizer-*.AppImage gst-inspect-1.0 gtk4paintablesink 2>&1 | grep -q gtk4paintablesink
      - name: smoke test (dep-stripped ubuntu:22.04 — no bubblewrap)
        run: |
          docker run --rm -v "$PWD:/work" -w /work ubuntu:22.04 bash -c "
            apt update -qq && apt install -y --no-install-recommends fuse libgtk-4-1 libglib2.0-0 ca-certificates 2>&1 | tail -n 20
            ./Organizer-*.AppImage --self-test-sandbox 2>&1 | tee /tmp/smoke2.log
            grep -q bwrap /tmp/smoke2.log && echo 'bundled bwrap ok' || (echo 'bundled bwrap missing' && exit 1)
          "
      - uses: actions/upload-artifact@v4
        with: { name: Organizer-x86_64.AppImage, path: Organizer-*.AppImage* }
```

**`release.yml` — on tag `v*` (publish):**

```yaml
name: release
on: { push: { tags: ["v*"] }, workflow_dispatch: {} }
permissions: { contents: write }
jobs:
  build:
    runs-on: ubuntu-22.04
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - name: build raw tarball
        run: |
          sudo apt update && sudo apt install -y libgtk-4-dev libadwaita-1-dev libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-libav liblcms2-dev libfontconfig1-dev libseccomp-dev bubblewrap
          cargo build --release --locked && strip target/release/organizer
          tar czf organizer-x86_64-linux-gnu.tar.gz -C target/release organizer
          sha256sum organizer-*.tar.gz > SHA256SUMS
      - name: build AppImage (same steps as ci appimage-smoke)
        run: |
          # ... same as ci job above ...
          ARCH=x86_64 ./appimagetool AppDir -u 'gh-releases-zsync|mnj|organizer|latest|Organizer-*x86_64.AppImage.zsync' Organizer-x86_64.AppImage
      - name: publish
        uses: softprops/action-gh-release@v2
        with:
          files: |
            organizer-x86_64-linux-gnu.tar.gz
            SHA256SUMS
            Organizer-*.AppImage
            Organizer-*.AppImage.zsync
          generate_release_notes: true
```

*Notes on CI sketch:*

* Use `ubuntu-22.04` runner explicitly — `ubuntu-latest` may roll to `24.04` (glibc bump).
* Cache `~/.cargo` + `target/` via `Swatinem/rust-cache@v2` where helpful.
* `linuxdeploy` + `appimagetool` continuous builds are stable but pin SHA if reproducibility demanded.
* Add `--self-test-sandbox` / `--self-test-gstreamer` flags to `organizer` binary (hidden `clap` flags that dump `SandboxMechanism`, `XDG_DATA_DIRS`, `GST_PLUGIN_SYSTEM_PATH`) so CI can grep without launching the GUI — see #11 §9.2/§9.4 pattern.
* Smoke containers run `ubuntu:22.04` without `bubblewrap` to prove bundling; also test on `ubuntu:24.04` runner with host `bubblewrap` present to prove both paths.

---

## 8. Aligning with #11 bundling checklist & #2 rendering stack

* Rendering stack (#2) unchanged: single `Picture::for_paintable` swaps between `gdk::Texture` from `glycin → Loader::new(file).sandbox_selector(Bwrap).load().await → Frame::texture()` and `gst-plugin-gtk4 gtk4paintablesink Paintable` via `playbin`. Bundling here only changes where those libs/plugins are found.
* Sandboxed decoding (#11) remains hard requirement: **raw binary `Bwrap` only via host `bwrap`, AppImage `Bwrap` via bundled `bwrap` + relocated `Exec`**. No silent `NotSandboxed` or `image`-crate fallback — enforce `SandboxSelector::Bwrap` and fatal dialog on `HostBwrapSyscallsBlocked` (see #11 §5.2/§5.3 copy).
* AppImage does **not** fix GStreamer being unsandboxed — video decoders stay in-process (Option A of #11 §6.2). Keep `gstreamer` plugin set narrow (`good+bad+libav`, no `ugly`/nonfree), `playbin flags=VIDEO` only, `file://` only, bus error → placeholder. Helper-process isolation (Option B) is a follow-up ticket, not v1 AppImage work.
* AppImage layout must keep compat path `2+` verbatim (`libexec/glycin-loaders/2+/` + `share/glycin-loaders/2+/conf.d/` — #11 §1.4/§2.2) because `glycin-core::config::Config` only scans `.../glycin-loaders/2+/...`.

---

## 9. Open questions → follow-ups (do not block v1 AppImage/raw)

* **Loader coverage freeze** — include `glycin-heif`+`glycin-jxl`? Keep full set for v1 AppImage "full" and a `minimal` flavor without them for size-constrained users; decide via user survey after beta.
* **`glycin-ng` evaluation** — `QaidVoid/glycin-ng` (landlock+seccomp in-process worker, ~4 MB) would drop `bwrap` bundling and Exec relocation complexity, but changes crate API and reduces C++ codec coverage — track as alternative if `bwrap`+`apparmor` friction grows.
* **AppImage `aarch64` target** — native `aarch64` AppImage (`ARCH=aarch64`) via `ubuntu:22.04` ARM runner — out for v1 x86_64-only milestone.
* **Signing `SHA256SUMS`** — add `gpg --detach-sign` or `cosign` for release tarballs once key infra exists.

---

## References (primary sources — each claim traced)

* `GNOME/glycin` README + `glycin-core/src/sandbox.rs` + `glycin-core/src/config.rs` + `glycin-core/src/util.rs` + `glycin/meson.build` (compat `2+`, `dependency('libseccomp')`, `bwrap` command) — see #11 §10 list; reused here for AppRun relocation + bundling checklist.
* `GNOME/glycin` issue `#203` + `glycin-ng` (`QaidVoid/glycin-ng`) — context on future native sandbox without `bwrap`.
* `linuxdeploy` (`github.com/linuxdeploy/linuxdeploy` README: AppDir concept, `ldd` walk, `--plugin`, `--output appimage`, excludelist) + `linuxdeploy-plugin-gtk.sh` (`github.com/linuxdeploy/linuxdeploy-plugin-gtk` README: `glib-compile-schemas`, `gtk-update-icon-cache`, `gdk-pixbuf-query-loaders`, env vars) — AppImage assembly + GTK bundling.
* `linuxdeploy-plugin-gstreamer.sh` (`github.com/linuxdeploy/linuxdeploy-plugin-gstreamer` README + `discourse.appimage.org/t/gstreamer` thread #314: `GST_PLUGIN_SYSTEM_PATH`/`GST_PLUGIN_SCANNER`) — GStreamer bundling + discovery.
* `AppImage/appimagetool` (`github.com/AppImage/appimagetool` README: `type2` squashfs+runtime, `appimagetool -u`, `zsync` sidecar, `gh-releases-zsync` template) + `AppImageUpdate` (`github.com/AppImage/AppImageUpdate` README: delta via `zsync`) — updates.
* `appimage.github.io` / `docs.appimage.org` (“Build on oldest still-supported LTS”, `glibc` baseline, `FUSE`/`--appimage-extract` fallback, bundling only what you need, update string format) — base-distro, compat, excludelist guidance.
* `AppImage/appimagekit/wiki/Bundling-Only-What-You-Need` (excludelist rationale for `libwayland`, `libEGL`, `VA-API`, `glibc`) — bundling matrix justification.
* `gst-plugin-gtk4` (`docs.rs/crate/gst-plugin-gtk4` `0.15`, `github.com/GStreamer/gst-plugins-rs` `gtk4` docs, Centricular `2024-04/gtk4-dmabuf-import`) — `cargo cinstall --features waylandegl,x11egl,dmabuf --library-type=cdylib` flags, DMABuf/GL zero-copy note.
* `cargo-c` (`github.com/lu-zero/cargo-c` README: `cargo cinstall --prefix --destdir --library-type=cdylib`) — cdylib install flow.
* `containers/bubblewrap` (`github.com/containers/bubblewrap` README + `bwrap.xml` manpage) — `bwrap --unshare-all --seccomp` flags, no setuid mode.
* `roddhjav/apparmor.d#881` + Arch BBS threads `309784/311001/312713` — AppArmor `bwrap-userns-restrict` (Ubuntu 24.10+ `kernel.apparmor_restrict_unprivileged_userns=1`) blocking `bwrap` user_ns (A2/H2 rows).
* `cargo-dist` (`github.com/axodotdev/cargo-dist` book: `cargo dist init/build`, `installers`, GitHub CI) — raw tarball distribution alternative.
* Rendering stack source — `docs/research/rendering-stack.md` (`research/rendering-stack` branch, #2: `gtk4 Picture/Paintable + glycin → Texture + GStreamer gtk4paintablesink` decision matrix).
* Superseded packaging research — `docs/research/packaging-deps.md` (`research/packaging-deps` branch, #10 Flatpak portion superseded 2026-08-30).

---
*Next step for Map #1:* consume **TL;DR**, **§2 manifest recipe + §3 AppRun snippet**, **§4 bundling matrix**, **§5 size/startup/compat smoke**, **§7 CI sketch**; enforce `SandboxSelector::Bwrap` per #11, implement `--self-test-*` flags, wire `release.yml` with `linuxdeploy`+`appimagetool`, and schedule GStreamer-helper isolation as follow-up ticket.
