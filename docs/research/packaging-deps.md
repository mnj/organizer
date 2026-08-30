# Research: packaging, system deps & distribution (Flatpak, ffmpeg/gstreamer vs native)

> Ticket: [#10](https://github.com/mnj/organizer/issues/10) — Part of Map [#1](https://github.com/mnj/organizer/issues/1)
> Branch: `research/packaging-deps` · Date: 2026-08-30 · Status: completed
> Blocked by: [#2](https://github.com/mnj/organizer/issues/2) — rendering stack already decided as **gtk4 + glycin loaders + GStreamer + gst-plugin-gtk4 (`gtk4paintablesink`)** (see [`docs/research/rendering-stack.md`](./rendering-stack.md)). This doc aligns packaging with that decision and does not re-decide the decoder matrix.

## Question

How is the app packaged and what system dependencies are acceptable for the chosen rendering stack (glycin + GStreamer `gtk4paintablesink`) — distro matrix, Flatpak manifest shape, system deps, build toolchain, XDG/portal model, auto-update, and Windows/macOS parity?

## TL;DR recommendation

* **v1 target = Linux Flatpak only.** Host-`cargo build` for dev, **Flathub-ready Flatpak** for distribution. Native distro packages (`.deb`/AUR/COPR) are *contributed* later; do not block v1.
* **Runtime:** `org.gnome.Platform//48` + `org.gnome.Sdk//48` + `org.freedesktop.Sdk.Extension.rust-stable//24.08` (or `//25.08` if you bump to GNOME 49/50). GNOME 48 maps to Freedesktop SDK 24.08 — the branch of `rust-stable` must match (see §3).
* **System deps in manifest:** GTK4 + libadwaita (from runtime) + **glycin** (from runtime on GNOME 47+ / bundled `glycin-loaders` module if needed) + **GStreamer plugins-good/bad/libav** (from runtime) + **`gst-plugin-gtk4` as a `simple` module built with `cargo cinstall`** + offline Rust deps via **`flatpak-cargo-generator` → `cargo-sources.json` + `.cargo/config.toml`**. No `ffmpeg` CLI, no `ffmpeg-next` crate, no vendored `ffmpeg` build.
* **`cargo build` stays the dev path;** `cargo bundle` / `cargo packager` / `cargo deb` deferred — they bundle `.deb`/`.AppImage` and require separate host-lib strategies that conflict with the sandboxed image-loader story.
* **Filesystem:** no blanket `--filesystem=host/home`. Use **FileChooser portal** for the triage folder (`--filesystem=xdg-pictures:rw` or `:ro` + portal is acceptable as broad-default; prefer portal-only). Persist app state via XDG state/config dirs which Flatpak maps to `~/.var/app/<ID>/` automatically.
* **CI:** `flatpak/flatpak-github-actions` (`flatpak-builder@v6`) + `flatpak-builder-lint` on every PR; optional native `cargo test --offline` job.
* **Windows/macOS:** out of scope for v1. Documented as **out-of-parity** (see §9): both work via `gtk4` + `gstreamer` on MSYS2/gvsbuild/Homebrew but **glycin sandboxes degrade to in-process** and packaging is `.msi`/`.app`/`.dmg`, not Flatpak.

---

## 1. Distribution matrix — what is v1, what is later

| Channel | Audience | Build artifact | v1? | Notes |
|---|---|---|---|---|
| **Flatpak (Flathub)** | All mainstream Linux | `*.flatpak` / Flathub repo | **Yes** | Solves GTK4/libadwaita/glycin/GStreamer version skew in one shot. |
| `cargo install` / `cargo build` | Dev + power users | binary on `$PATH` | **Yes (dev only)** | Document `apt/dnf/pacman` one-liners for host libs (see §2). |
| Native distro pkg (`.deb`, AUR, COPR/Fedora) | Distro-loyal users | `apt`/`pacman`/`dnf` | **No — community-contributed after v1** | Template provides `meson install` layout that makes this trivial later. |
| AppImage (via `cargo bundle` / `cargo-appimage`) | "download & run" | `.AppImage` | **Deferred** | Would need to vendor GTK4/GStreamer/glycin/bwrap — size + security burden; Flatpak already covers it. |
| Windows (MSYS2 / gvsbuild / `.msi`) | — | `.msi` / zip | **No** | Feasible but packaging story is `gvsbuild` + WiX, not Flatpak. |
| macOS (Homebrew / `.app` / `.dmg`) | — | `brew cask` / `.dmg` | **No** | Feasible via `brew install gtk4 libadwaita gstreamer ...` but not Flatpak. |

**Rationale for Flatpak-first:** every major distro now ships Flatpak (Fedora Workstation enables Flathub by default; Ubuntu/Arch document `flatpak install flathub ...`). GStreamer + GTK4 + glycin version matrices diverge sharply across Ubuntu 24.04 (GTK 4.14, GStreamer 1.22) vs Fedora 42 (GTK 4.18) vs Arch (rolling). Flatpak pins a single GNOME runtime and removes "you must upgrade your distro" support burden. Flathub is where GNOME users expect to find new GTK4 apps (see GNOME Circle criteria).

> **Out-of-scope v1 but not "never":** `cargo-packager`/`cargo-dist` can later emit `.deb`/`.rpm`/`.dmg`/`.msi` from the same binary. Keep project layout `meson`-clean so those tools can consume it without rewrite (see §8).

## 2. System dependencies aligned to the chosen rendering stack

Ticket #2 chose: `gtk4 (bare)` + `glycin → Texture → Picture` for stills/animated-stills + `GStreamer + gst-plugin-gtk4 (gtk4paintablesink) → Paintable → same Picture` for video/animated containers. This determines what must be present at build and at runtime.

| Component | Debian/Ubuntu `apt` (dev) | Fedora `dnf` | Arch `pacman` | Flatpak runtime? | License obligation |
|---|---|---|---|---|---|
| `gtk4` 4.14+ | `libgtk-4-dev` | `gtk4-devel` | `gtk4` | **Yes** — `org.gnome.Platform//48` ships GTK 4.16+ | App: your choice (MIT/Apache); GTK is LGPL-2.1+ |
| `libadwaita` 1.5+ | `libadwaita-1-dev` | `libadwaita-devel` | `libadwaita` | **Yes** — in GNOME runtime | LGPL-2.1+ |
| `gstreamer` 1.22+ + `gst-plugins-base` + `gst-plugins-good` + `gst-plugins-bad` + `gst-libav` | `libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-libav` | `gstreamer1-devel gstreamer1-plugins-base-devel gstreamer1-plugins-good gstreamer1-plugins-bad-free gstreamer1-plugin-libav` | `gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad gst-libav` | **Yes** — GNOME Sdk bundles GStreamer stack | GStreamer LGPL-2.1+; avoid `ugly`/`nonfree` elements (e.g. `x264enc` GPL); `gst-libav` is LGPL if FFmpeg is built `--disable-gpl` (Flathub validates this) |
| `glycin` 3.x + `glycin-loaders` + `libseccomp` + `bubblewrap` | `liblcms2-dev libfontconfig-dev libseccomp-dev bubblewrap glycin-loaders` (Debian trixie/Sid; older Ubuntu must build from `download.gnome.org` tarball — see BLFS) | `lcms2-devel fontconfig-devel libseccomp-devel bubblewrap glycin` | `lcms2 fontconfig libseccomp bubblewrap` + AUR `glycin` | **Conditionally** — GNOME 47+ SDK includes `glycin 2.x` + `glycin-loaders`; if targeting `47` you may still bundle a `glycin-loaders` module for codec freshness. On `48` the runtime is current. | Rust crates MIT/Apache; underlying image codec libs vary (LGPL/BSD) — glycin isolates them behind sandbox so linkage stays LGPL-safe. Sandbox needs `bwrap` at *runtime host* or `flatpak-spawn --sandbox` inside Flatpak. |
| `gst-plugin-gtk4` 0.15.x | _not packaged on Ubuntu 24.04_ — must `cargo cinstall` (see §5) | `rust-gst-plugin-gtk4` (Copr) or build | AUR `gst-plugin-gtk4` | **No** — must build as a manifest module (see §3). Freighted via `org.freedesktop.Sdk.Extension.rust-stable`. | MPL-2.0 (plugin crate) + GStreamer LGPL |
| `ffmpeg` CLI / `ffmpeg-next` crate | **Do not add.** | — | — | — | `ffmpeg` LGPL→GPL switch on `--enable-gpl` (`libx264`) or `nonfree` (`libfdk_aac`). Avoid; GStreamer already covers decode. If later you need frame-accurate CPU thumbnails without a pipeline, prefer `gst::Discoverer` / `thumbnailbin` before `ffmpeg-next`. |

**Host dev one-liners (for `cargo build` without Flatpak):**

```bash
# Ubuntu 25.04+ / Debian trixie (per lada docs pattern, adapted for this stack)
sudo apt install libgtk-4-dev libadwaita-1-dev \
  libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
  gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-libav \
  liblcms2-dev libfontconfig-dev libseccomp-dev bubblewrap \
  cargo rustc pkg-config meson

# Fedora 41+
sudo dnf install gtk4-devel libadwaita-devel \
  gstreamer1-devel gstreamer1-plugins-base-devel gstreamer1-plugins-good \
  gstreamer1-plugins-bad-free gstreamer1-plugin-libav \
  lcms2-devel fontconfig-devel libseccomp-devel bubblewrap \
  cargo meson

# Arch (rolling)
sudo pacman -S gtk4 libadwaita gstreamer gst-plugins-base gst-plugins-good \
  gst-plugins-bad gst-libav lcms2 libseccomp bubblewrap cargo meson
# glycin on Arch: build from GNOME tarball or AUR; gst-plugin-gtk4: cargo cinstall
```

> Arch/Ubuntu 24.04 do not ship `gstreamer1.0-gtk4` / `gst-plugin-gtk4` as a binary — `gst-inspect-1.0 gtk4paintablesink` will fail until you `cargo cbuild -p gst-plugin-gtk4 --libdir /usr/lib/... && cargo cinstall ...` (see ladaapp/lada Linux install notes) or use the Flatpak path.

## 3. Flatpak manifest shape (recommended)

Follow the **GNOME Rust + Meson + Flatpak template** (`gtk-rust-template` / `relm4-template`) rather than inventing a bespoke layout. That template already handles dual `Devel`/release app IDs, `metainfo`/`desktop`/`gschema`/`gresource` install, i18n, and NIE (no `cargo build` outside `meson`).

**Required base properties:**

```yaml
# build-aux/io.github.mnj.organizer.Devel.json  (also io.github.mnj.organizer.json for release)
id: io.github.mnj.organizer          # replace with your final reverse-DNS
runtime: org.gnome.Platform
runtime-version: '48'                # GNOME 48 = Freedesktop 24.08
sdk: org.gnome.Sdk
command: organizer
sdk-extensions:
  - org.freedesktop.Sdk.Extension.rust-stable
finish-args:
  - --share=ipc
  - --socket=fallback-x11            # X11 compat; keep even on Wayland
  - --socket=wayland
  - --device=dri                     # GL/DMABuf zero-copy for gtk4paintablesink
  - --talk-name=org.freedesktop.portal.FileChooser
  - --talk-name=org.freedesktop.portal.Documents  # optional: if Documents portal used
  # No --filesystem=host/home. See §6 for filesystem story.
```

> **Why `48`:** `docs.flatpak.org/en/latest/available-runtimes.html` + `org.gnome.Platform` refs show `48` as current stable. It bundles GTK 4.16, libadwaita 1.7, GStreamer 1.24, glycin 2.x. `org.freedesktop.Sdk.Extension.rust-stable//24.08` is the matching branch — **the `//branch` must equal the Freedesktop SDK version that GNOME 48 is built on** (`flatpak info -m org.gnome.Sdk | grep Extension` shows `version = 24.08`). GNOME 49 nightly uses `25.08`.

**Modules — the minimal shape:**

```json
{
  "sdk-extensions": ["org.freedesktop.Sdk.Extension.rust-stable"],
  "build-options": { "append-path": "/usr/lib/sdk/rust-stable/bin" },
  "modules": [
    {
      "name": "gst-plugin-gtk4",
      "buildsystem": "simple",
      "build-options": {
        "append-path": "/usr/lib/sdk/rust-stable/bin",
        "env": { "CARGO_HOME": "/run/build/gst-plugin-gtk4/cargo" }
      },
      "sources": [
        {
          "type": "archive",
          "url": "https://crates.io/api/v1/crates/gst-plugin-gtk4/0.15.2/download",
          "dest-filename": "gst-plugin-gtk4-0.15.2.tar.gz",
          "sha256": "<sha256 of the tarball — update via `sha256sum`>"
        },
        "gst-plugin-gtk4-sources.json"
      ],
      "build-commands": [
        "cargo --offline fetch --manifest-path Cargo.toml",
        "cargo cinstall --offline --release --features=waylandegl,x11egl,x11glx,dmabuf,gtk_v4_14 --library-type=cdylib --prefix=/app --destdir=/"
      ],
      "cleanup": ["*"]
    },
    {
      "name": "organizer",
      "buildsystem": "meson",
      "builddir": true,
      "config-opts": ["--buildtype=release"],
      "build-options": {
        "append-path": "/usr/lib/sdk/rust-stable/bin",
        "env": { "CARGO_NET_OFFLINE": "true" }
      },
      "sources": [
        "cargo-sources.json",
        { "type": "dir", "path": "." },
        { "type": "shell", "commands": ["mkdir -p .cargo && cp -f cargo/config .cargo/config.toml"] }
      ]
    }
  ]
}
```

**Key points cited to primary sources:**

* `sdk-extensions: org.freedesktop.Sdk.Extension.rust-stable` + `append-path: /usr/lib/sdk/rust-stable/bin` is the only supported way to get `cargo`/`rustc` inside the Flatpak sandbox (`flatpak-cargo-generator` README; `develop.kde.org Publishing your Rust app as flatpak`).
* `gst-plugin-gtk4` installs as a **cdylib** into `/app/lib/gstreamer-1.0/` via `cargo cinstall` with `--library-type=cdylib --prefix=/app` (crate docs `gst-plugin-gtk4 0.15.2` Flatpak Integration snippet). Features `waylandegl,x11egl,x11glx,dmabuf` enable zero-copy GL + DMABuf on GTK 4.14+ (`gst-plugins-rs/meson.build` derives features from `gstgl`/`gtk4` detection — we enable them explicitly for Flatpak where wl/x11/egl are always present). Add `gtk_v4_14` if you set `gtk4` feature `v4_14`.
* **Do not** use `buildsystem: simple` with inline `cargo build` for the main app if you adopt meson (which you should — see §7). The `gtk-rust-template` `src/meson.build` `custom_target('cargo-build', env: { CARGO_HOME: ... })` pattern is the GNOME-blessed bridge between Cargo and Meson. In Flatpak you set `CARGO_NET_OFFLINE=true` and copy `.cargo/config.toml` so the offline vendor is found.
* If you target GNOME 46 or older (no glycin in runtime), add a third module building `glycin`/`glycin-loaders` from `download.gnome.org` tarball with `meson -Dlibglycin-gtk4=true -Dtests=false`. On `48` skip it — the runtime already provides it and bundling duplicates loaders.

**`--share=network` anti-pattern:** never add `build-args: ["--share=network"]` except on a dedicated `cargo-c` bootstrap module that gets `cleanup: ["*"]`. The KDE tutorial's `cargo` module example uses `--share=network` only for the integer bootstrap; the app module stays offline. `flatpak-cargo-generator` exists precisely to avoid network in the build.

## 4. Offline Rust deps — `flatpak-cargo-generator` vs `cargo vendor` vs `cargo-c`

| Tool | What it does | When to use | Primary source |
|---|---|---|---|
| **`flatpak-cargo-generator.py` → `cargo-sources.json` + `cargo/config`** | Parses `Cargo.lock`, fetches each crate tarball (+ git deps) as individual `type: archive` / `type: git` sources, writes `.cargo/config.toml` rewrite rules so `cargo --offline` finds them. | **Recommended for this project.** Handles git deps, is what Flathub CI expects, and integrates with `meson` by copying `cargo/config → .cargo/config.toml` (KDE Flatpak guide). | `flatpak/flatpak-builder-tools/cargo/README.md` |
| `cargo vendor` → vendored tarball | Copies all deps into `vendor/`; you tar that and ship as one `type: archive`. | Alternative if you want "single vendored tarball per release" automation (the README calls this out as easier to automate on tag). Slightly larger manifest diff per release, but fewer moving parts. | `flatpak-builder-tools/cargo/README.md#Alternatives` + `cargo vendor` docs |
| `cargo-c` / `cargo cinstall` / `cargo cbuild` | Builds C ABI libraries (`cdylib` + headers + `.pc`) from Rust crates — *not* a source vendoring tool. Needed **only** for `gst-plugin-gtk4` (a GStreamer plugin is a `.so` loaded with `dlopen`, not a binary). | Use **only** for the `gst-plugin-gtk4` module (`cargo cinstall -p gst-plugin-gtk4 --library-type=cdylib`). Do not use for the main app binary. | `docs.rs/crate/gst-plugin-gtk4` Flatpak Integration; `gst-plugins-rs/meson.build` `cargo-c missing` check |

**Offline build checklist:**

```bash
# one-time: install deps for the generator
pip install aiohttp tomlkit pyyaml   # or: pipx install flatpak-cargo-generator
# or: python3 -m pip install --user flatpak-cargo-generator  (PyPI 0.1.4, unofficial)

# regenerate whenever Cargo.lock changes (commit both files)
python3 flatpak-cargo-generator.py Cargo.lock -o cargo-sources.json
# also: for gst-plugin-gtk4 if built as separate module
tar -xf gst-plugin-gtk4-0.15.2.tar.gz
python3 flatpak-cargo-generator.py gst-plugin-gtk4-0.15.2/Cargo.lock -o gst-plugin-gtk4-sources.json
```

> Commit `cargo-sources.json` and recompute on every `cargo update`. The Flatpak linter (`flatpak-builder-lint manifest`) will flag a stale `cargo-sources.json` where `Cargo.lock` newer than the generated file (seen in Flathub CI logs).

**Handling `gdk-pixbuf` loaders vs glycin inside the manifest:** `org.gnome.Platform` ships `gdk-pixbuf` loaders for png/jpeg etc. Glycin does not need extra manifest entries on `48`; its loaders are in `/usr/libexec/glycin-loaders` and `libglycin` is in `/usr/lib`. If you later decide to support AVIF/JXL/HEIC via glycin's optional loaders, those codec libs (`libheif`, `libjxl`, `dav1d`) are *already* in the GNOME runtime — no manifest change needed beyond keeping glycin current.

## 5. Build — dev, Flatpak, and the tools you don't need yet

**Host dev (`cargo`):**

```bash
cargo build                # debug, talks to host GTK4/GStreamer/glycin
cargo test
cargo run -- --help        # window maximized, picks glycin/GStreamer at runtime
meson setup builddir --prefix=/usr -Dprofile=development
meson install -C builddir  # installs gresource/desktop/gschema; template tests validate them
```

**Flatpak (dev iteration):**

```bash
flatpak install --user flathub org.gnome.Sdk//48 org.gnome.Platform//48 \
  org.freedesktop.Sdk.Extension.rust-stable//24.08
flatpak-builder --user --force-clean --install-deps-from=flathub \
  flatpak_app build-aux/io.github.mnj.organizer.Devel.json
flatpak run io.github.mnj.organizer.Devel
# or without install:
flatpak-builder --run flatpak_app build-aux/io.github.mnj.organizer.Devel.json organizer
```

**Flatpak (release bundle):**

```bash
flatpak-builder --repo=repo --force-clean --ccache flatpak_app build-aux/io.github.mnj.organizer.json
flatpak build-bundle repo organizer.flatpak io.github.mnj.organizer
flatpak install ./organizer.flatpak
```

**What not to adopt in v1:**

* `cargo bundle` — wraps `cargo build` output into `.app`/`.deb`/`.msi`/`.AppImage`. Useful for macOS/Windows later; on Linux it competes with Flatpak and requires you to solve GTK4/GStreamer distribution yourself. Keep `meson install` layout compatible but do not add `[package.metadata.bundle]` yet.
* `cargo packager` / `cargo-dist` — richer than `cargo bundle` (DMG/NSIS/AppImage, auto CI), but same caveat: they shine for CLI tools distributing static binaries, not for a GTK4 app with many dynamic libs and sandboxed decoders. They are the right tool for Windows/macOS *when* you tackle those platforms (see §9).
* `cargo deb` — Debian-specific; Flathub is strictly preferred for v1 discovery. Add later via `meson dist` + `cargo deb` is two-line if layout is clean.

## 6. Runtime — XDG dirs, portals, and filesystem permissions

### XDG Base Directories inside Flatpak

Flatpak remaps each XDG base dir to a per-app location (see `docs.flatpak.org` Sandbox Permissions):

| Logical dir | Host (outside sandbox) | Inside sandbox (`$XDG_*_HOME`) | Use for this app |
|---|---|---|---|
| `XDG_CONFIG_HOME` | `~/.config` | `~/.var/app/<ID>/config` | Action/shortcut config (`config.toml`/`settings.json`), per-action `action_name`/`folder` bindings. |
| `XDG_DATA_HOME` | `~/.local/share` | `~/.var/app/<ID>/data` | Persistent hash-log if you ever centralize logs; currently logs are per-triage-folder so may not use this. |
| `XDG_STATE_HOME` | `~/.local/state` | `~/.var/app/<ID>/.local/state` | Undo stack persistence (if you decide durable undo), session resume. |
| `XDG_CACHE_HOME` | `~/.cache` | `~/.var/app/<ID>/cache` | Thumbnail/LRU cache, pre-decoded next-file cache. |
| `XDG_RUNTIME_DIR` | `/run/user/$UID` | `/run/user/$UID` (or `/run/user/$UID/doc` for portal documents) | Portal document handles. |

**Implementation rule:** use the `dirs` / `directories` crate (Rust XDG) — it already respects `$XDG_*` overrides, and inside Flatpak those env vars are set to the per-app paths automatically. Do not hardcode `~/.config/...`.

> **Hash-log + `duplicate/` lifecycle interacts here** (open on tickets #5 and #6): if `action_name.txt` and `duplicate/` live *alongside the source folder being triaged* they are **not** in XDG — they are in the triage directory itself (which the user granted via portal or broad permission). If they live in XDG state/data they survive across folders but require design for per-source-folder multiplexing. The packaging doc takes no position on that decision — it only notes the two valid homes and that XDG needs zero extra `finish-args`.

### Filesystem permissions — portal-first

**Organizer's defining tension:** it needs to `read` arbitrary triage folders (png/jpeg/webp/gif/webm — anywhere on disk, including `/media`, `/run/media` USB sticks, SMB via `xdg-run/gvfs`) and `write` (move + append `sha256` into `action_name.txt` / `duplicate/`). Blanket `host`/`home` breaks the sandbox promise and gets flagged by Flathub's linter and GNOME Software as "Potentially unsafe" (see `sandbox-permissions` guidelines + portal discussion #1301).

**Recommended `finish-args` for v1:**

```yaml
finish-args:
  - --share=ipc
  - --socket=fallback-x11
  - --socket=wayland
  - --device=dri
  # No --filesystem=host or --filesystem=home
  # Optional broad default (UX discussion — see below):
  - --filesystem=xdg-pictures:rw
  - --filesystem=xdg-documents:rw
  - --filesystem=/media:ro
  - --filesystem=/run/media:ro
```

**And the primary access path is the portal:**

* **Folder picker:** `GtkFileDialog::select_folder` (GTK 4.10+) / `gtk::FileDialog` transparently uses `org.freedesktop.portal.FileChooser` → the document portal bind-mounts the chosen path into `/run/user/$UID/doc/<hash>/<basename>` and grants the app persistent access (stored in `~/.local/share/flatpak/db/documents`). The permission survives reboot but is keyed by *path + directory inode* (portal issue #1845 discussion) — atomic renames inside the triage folder are supported; replacing the whole folder will revoke access and the user must re-pick.
* For triage, **ask the user to pick the source folder on first launch**, then store the portal handle. This is the documented GNOME pattern ("On first launch, ask user to pick a directory") for music/photo apps that would otherwise need `host` (see portal Libraries/Folders discussion #1301, swick's recommendation).
* **Why not `host`/`home`:** `docs.flatpak.org` "As a general rule, static and permanent filesystem access should be limited" + "Use portals as an alternative to blanket filesystem access, wherever possible." `host` also defeats `bwrap`-style sandbox guarantees for image decoding — glycin's own design assumes the *host* portal + `flatpak-spawn --sandbox` path when inside Flatpak, not a full host mount.
* **Fallback for non-portal file access (external drives):** `--filesystem=/media:ro --filesystem=/run/media:ro` is explicitly documented as needed for `/media`/`/run/media` — these mounts are *not* covered by `home`/`host` unless requested. Add them read-only; writes (moves into `action subfolder/` and `duplicate/`) happen under the portal-granted triage folder, so read-only `/media` is fine — the portal grants write to the specific chosen directory regardless of the static permission.
* **`:create` suffix** — `--filesystem=xdg-pictures:rw:create` will auto-create `~/Pictures` if missing; useful for `--filesystem=xdg-pictures` but not critical for `xdg-pictures:rw` — the directory usually exists.

> **Grilling decision left open:** whether the "broad default" `xdg-pictures:rw` line is present at all. **Minimal sandbox** = no filesystem permission, portal-only (cleanest Software badge, but first launch requires an extra click and `xdg-pictures` files are invisible until user picks that folder). **Pragmatic default** = keep `xdg-pictures:rw` (matches what Loupe/Showtime do: grant `xdg-pictures:rw` so screenshots/Downloads show up without picker, while still using portal for arbitrary folders). Either passes Flathub lint; the pragmatic choice gets a weaker badge but better "open & triage" demo flow. Recommend grilling this with a UX pass.

### Auto-update story

| Channel | Update mechanism | User action |
|---|---|---|
| **Flathub Flatpak** | `flatpak update` (or GNOME Software/KDE Discover auto-update). `flatpak-builder-lint` enforces `metainfo` `releases` tag so Software shows "What's New." `flat-manager` deploys on push to Flathub. | None — or click Update in Software. |
| Host `cargo install` | `cargo install --force organizer` / `cargo binstall` | Manual. |
| Distro package (future) | `apt upgrade` / `pacman -Syu` / `dnf update` | Via system updater. |
| AppImage (future, if added) | Manual download or `appimageupdatetool` if zsync embedded | Manual. |
| Windows/macOS (future) | MSI/AppImage updater or Sparkle (macOS) | Installer re-run. |

Flatpak's delta-based OSTree updates are already atomic + rollback-capable (`flatpak update --commit=...`). No custom updater is needed — do not ship an in-app "check for updates" button (it would bypass the portal's trust model and duplicate Software).

## 7. CI — GitHub Actions

Use `flatpak/flatpak-github-actions` (`flatpak-builder@v6`), which is the single maintained action Flathub itself uses (217 stars, MIT, `ghcr.io/flathub-infra/flatpak-github-actions:gnome-48` image). The README covers single-arch, multi-arch (x86_64/aarch64 via `ubuntu-24.04` + `ubuntu-24.04-arm`), and lint.

**Minimal PR workflow:**

```yaml
# .github/workflows/flatpak.yml
name: CI
on:
  push:
    branches: [main, research/*]
  pull_request:

jobs:
  flatpak:
    name: Flatpak
    runs-on: ubuntu-latest
    container:
      image: ghcr.io/flathub-infra/flatpak-github-actions:gnome-48
      options: --privileged
    steps:
      - uses: actions/checkout@v4
      - uses: flatpak/flatpak-github-actions/flatpak-builder@v6
        with:
          bundle: organizer.flatpak
          manifest-path: build-aux/io.github.mnj.organizer.Devel.json
          cache-key: flatpak-builder-${{ github.sha }}
          # optional: arch matrix, verbose
      - name: Lint manifest
        run: |
          flatpak run --command=flatpak-builder-lint org.flatpak.Builder \
            manifest build-aux/io.github.mnj.organizer.Devel.json
      - name: Lint builddir
        run: |
          flatpak run --command=flatpak-builder-lint org.flatpak.Builder \
            builddir flatpak_app
```

**Also add (optional but recommended):**

* `cargo test --offline` / `cargo clippy` native job on `ubuntu-latest` (no container) — fast feedback when only Rust code changes.
* `appstreamcli validate` on `data/io.github.mnj.organizer.metainfo.xml` (lint covers this, but native job gives faster error).
* Use `flatpak-builder-lint`'s `--exceptions --user-exceptions exceptions.json` if you intentionally keep a lint that Flathub would flag (e.g. broad `xdg-pictures`).
* `flat-manager` deploy job on `push: branches: [main]` if you use `flat-manager` hosting (see KDE Flatpak tutorial's Deploy stage).

> The generic template `flathub-infra/flatpak-github-actions` images are `gnome-48`/`kde-6.9` etc. Pin `gnome-48` to match `runtime-version: '48'`. QEMU multi-arch builds are documented but use the ARM64 runner matrix instead of emulation when possible.

## 8. Meson + `cargo` bridge — why `gtk-rust-template` is the right scaffold

GTK Rust apps do not use `cargo install` layout directly — they need `desktop`, `metainfo`, `gschema`, `gresource` installed to `prefix/share/...` so distro packages and Flatpak can consume them. `gtk-rust-template` (GNOME, 269 commits) + `relm4-template` (48 stars, same layout) solve this with a thin Meson wrapper around Cargo:

```
meson.build          # root: project('organizer', version, license, meson_version)
meson.options        # -Dprofile=development|default
build-aux/           # *.Devel.json, *.json
data/
  meson.build        # desktop, metainfo, gschema, icons
  resources/meson.build  # gresource_bundle
po/meson.build       # gettext
src/meson.build      # custom_target('cargo-build', env: {CARGO_HOME, APP_ID, RESOURCES_FILE})
```

`src/meson.build`'s `custom_target` invokes `cargo build --manifest-path ... --target-dir ...` with `CARGO_HOME = meson.project_build_root() / 'cargo-home'` and `APP_ID`/`RESOURCES_FILE` env — then `cp target/.../organizer @OUTPUT@`. In Flatpak the `build-options` `CARGO_NET_OFFLINE=true` + the `.cargo/config.toml` copy (from `flatpak-cargo-generator`) makes this offline; outside Flatpak it goes online.

> The `gtk-rs` book chapter "Building with Meson" documents this exact setup (with `cargo_options` + `env: { CARGO_HOME, APP_ID, RESOURCES_FILE }`). Follow it verbatim — do not invent a bespoke `build.rs` that shells out to `glib-compile-resources`, which breaks cross-compilation inside Flatpak.

## 9. Windows / macOS parity — out of scope for v1, but not blocked

| Topic | Windows | macOS |
|---|---|---|
| **GTK4 install** | MSYS2 `pacman -S mingw-w64-ucrt-x86_64-gtk4` (Unix-like, `pacman`) **or** `gvsbuild build gtk4` + set `$env:Path/$env:LIB/$env:INCLUDE` for MSVC | `brew install gtk4 libadwaita` (Homebrew formula ships 4.16+, including `gdk-pixbuf` loaders) |
| **GStreamer** | MSYS2 `mingw-w64-ucrt-x86_64-gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad gst-libav` (same UCRT prefix) or `gvsbuild build gstreamer ...` | `brew install gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad gst-libav gst-plugins-rs` (Homebrew `gst-plugins-rs` includes `gtk4` feature if built with gtk4) |
| **gst-plugin-gtk4** | Inside MSYS2: `cargo cbuild -p gst-plugin-gtk4 && cargo cinstall -p gst-plugin-gtk4 --libdir /mingw64/lib` + `gst-inspect-1.0 gtk4paintablesink` should list it. With MSVC/gvsbuild: `cargo cinstall --prefix=C:/gtk ...`. `winegl` feature on Windows replaces `waylandegl`/`x11egl` (`gst-plugins-rs/meson.build` maps `host_system == 'windows'` to `winegl`). | `cargo cinstall -p gst-plugin-gtk4 --prefix=/opt/homebrew` (Homebrew prefix) + set `GST_PLUGIN_PATH=/opt/homebrew/lib/gstreamer-1.0`. |
| **glycin / libadwaita** | `glycin`: compiles on Windows but **sandbox degrades to in-process** (no `bwrap`/`flatpak-spawn`). Still memory-safe (Rust), but not isolated — document as "unsupported for untrusted images on Windows." `libadwaita`: `gvsbuild build libadwaita` or MSYS2 `mingw-w64-ucrt-x86_64-libadwaita`. | `brew install libadwaita`; glycin likewise in-process. Real macOS sandbox would need separate work (Seatbelt) — not planned. |
| **Rust toolchain** | `rustup` MSVC (`x86_64-pc-windows-msvc`) + MSYS2 UCRT64 shell for GTK, or GNU (`x86_64-pc-windows-gnu`) fully inside MSYS2. **Must match** GTK's ABI. gtk-rs book Windows page documents both. | `rustup` default `aarch64-apple-darwin` (Apple Silicon) / `x86_64`. |
| **Packaging** | `cargo packager` → `.msi` (WiX) / `.exe` (NSIS); `cargo bundle --format msi/wxsmsi`; or zip `C:/gtk` bundle + installer script (see `gtk.org/docs/installations/windows` "Building and distributing your application"). | `cargo bundle --format osx` → `.app`; or `cargo packager --format dmg`; plus `brew cask`. |
| **CI** | `windows-latest` runner + MSYS2 setup action or `gvsbuild` caching. | `macos-14` runner + `brew install`. |

**Why v1 skips these:** each platform reopens the decoder question (glycin sandbox loss) and forces you to ship a self-updating installer. The Linux Flatpak path already answers the core research question ("ffmpeg vs native vs GStreamer") with "GStreamer + glycin, Flatpak-sandboxed." Ship that, validate triage UX, then revisit Windows/macOS behind a feature flag (`--features glycin-sandbox` vs `glycin-inprocess`) and a separate ticket.

> **Meson already handles the Windows/macOS `install` prefix** — `meson setup builddir` + `meson install` writes `share/applications`, `share/icons`, `share/glib-2.0/schemas` on any OS — so later packaging does not require a layout rewrite.

## 10. License notes (packing vs code)

* **Your app code:** choose MIT OR Apache-2.0 OR GPL-3.0. GTK/GStreamer being LGPL does not infect your app if you dynamically link (Flatpak always dynamic).
* **GStreamer:** LGPL-2.1+. Flathub's `shared-modules` and `org.gnome.Sdk` FFmpeg are built **without** `--enable-gpl` / `--enable-nonfree` so they stay LGPL. **Do not add `gst-plugins-ugly` or custom FFmpeg with `libx264`/`libfdk_aac`** — those flip to GPL/nonfree and Flathub's linter will reject the app. If you need H.264 acceleration, use `vaapi` / `openh264` (BSD).
* **`glycin`:** MPL-2.0 for daemon + MIT/Apache for Rust crates. `libseccomp`/`bwrap` are LGPL/BSD. Safe.
* **`ffmpeg-next` (if you later add it):** Rust glue is MIT/WTFPL but linked `libav*` inherits FFmpeg's `LICENSE.md` — default `COPYING.LGPLv2.1`, `--enable-gpl` → GPL-2+, `--enable-nonfree` → unredistributable. Packaging doc recommends **not** adding this; if you do, you must audit the Flatpak FFmpeg module's `config-opts` and ship LGPL source notices.
* **Flatpak manifest licensing:** include `metainfo.xml` `<project_license>GPL-3.0-or-later</project_license>` (or your choice) so `flatpak-builder-lint` and Flathub can validate.

## 11. Decision checklist for the shell prototype

- [ ] Adopt `gtk-rust-template` layout (`meson.build` + `build-aux/*.Devel.json`) verbatim.
- [ ] Set `runtime: org.gnome.Platform` / `runtime-version: '48'` / `sdk: org.gnome.Sdk` + `org.freedesktop.Sdk.Extension.rust-stable//24.08`.
- [ ] Add `gst-plugin-gtk4` as a `simple` module with `cargo cinstall --features=waylandegl,x11egl,x11glx,dmabuf,gtk_v4_14 --library-type=cdylib --prefix=/app`.
- [ ] Vendor Rust deps: `flatpak-cargo-generator.py Cargo.lock -o cargo-sources.json` + commit `cargo/config`; copy in manifest `type: shell` step.
- [ ] Set `finish-args`: `--share=ipc --socket=fallback-x11 --socket=wayland --device=dri` + narrow filesystem (portal-first); grill whether `xdg-pictures:rw` stays.
- [ ] Implement `GtkFileDialog::select_folder` → store portal handle; do not `Gio::File` an arbitrary path without portal.
- [ ] Wire XDG dirs via `dirs` crate → maps to `~/.var/app/<ID>/` inside Flatpak with zero extra permissions.
- [ ] Add `.github/workflows/flatpak.yml` with `flatpak/flatpak-github-actions/flatpak-builder@v6` + `flatpak-builder-lint manifest/builddir/repo` checks.
- [ ] Validate: `flatpak-builder --run ... organizer` shows `gst-inspect-1.0 gtk4paintablesink` inside sandbox; `glycin` loads svg/webp/heic via loaders; no `ffmpeg` in `finish-args` or modules.
- [ ] Defer: `cargo bundle` / AppImage / Windows / macOS to post-v1 tickets.

## 12. Open questions / handoff to grilling

* **Broad filesystem default vs portal-only:** keep `xdg-pictures:rw` for "open & triage immediately" demos vs go portal-only for clean Software badge. Needs UX grilling (see §6).
* **Hash-log home:** per-triage-folder (`action_name.txt` beside source) vs `XDG_STATE_HOME` central store. Packaging supports either; the decision is `application-logic`, not manifest. Tracked on tickets #5 and #6 — filesystem permission choice (`:rw` vs portal) must match whichever home wins.
* **Glycin bundling on GNOME 48:** omit module (leanest) vs vendor `glycin 3.x` for fresher codecs (JXL/HEIC improvements). Current recommendation: omit on `48`; revisit if trixie vs runtime codec gap is reported.
* **GStreamer codec set:** `gst-plugins-good` + `bad` + `libav` vs adding `gst-plugins-ugly` for extra legacy codecs (patent risk). Keep minimal until a user reports a format that `good`+`bad`+`libav` cannot decode — then add narrowly and document license.

## References (primary sources)

* Flatpak manifests, permissions, runtimes — `docs.flatpak.org/en/latest/manifests.html` (basic props, `id`/`runtime`/`sdk`/`finish-args`/`modules`/`buildsystem`, `cleanup`), `docs.flatpak.org/en/latest/sandbox-permissions.html` (portals, `:ro`, `host`/`home`/`xdg-*`, `/media` & `/run/media` explicit, `~/.var/app` reserved, XDG remapping), `docs.flatpak.org/en/latest/available-runtimes.html`, `docs.flatpak.org/en/latest/flatpak-builder-command-reference.html`.
* `flatpak/flatpak-builder-tools/cargo/README.md` — `flatpak-cargo-generator.py` usage, `cargo-sources.json` + `cargo/config` offline pattern, `append-path: /usr/lib/sdk/rust-stable/bin`, `CARGO_HOME` / `.cargo/config.toml` handling for meson/cmake, `cargo-c` alternative note.
* `gst-plugin-gtk4` crate `0.15.2` — `docs.rs/crate/gst-plugin-gtk4` (Flatpak Integration snippet with `cargo cinstall --features=waylandegl,x11egl,x11glx,dmabuf`, `0.12.5→0.15.2` version drift), `gitlab.freedesktop.org/gstreamer/gst-plugins-rs` `video/gtk4` docs (GL/DMABuf features, `waylandegl/x11egl/dmabuf`), `gst-plugins-rs/meson.build` (`gtk4_features` derivation, `cargo-c missing`).
* Rust + GNOME templates — `gitlab.gnome.org/World/Rust/gtk-rust-template` (Meson+Cargo+Flatpak boilerplate, dual `Devel`/release), `github.com/Relm4/relm4-template` (same layout for Relm4), `gtk-rs.org/gtk4-rs/stable/latest/book/meson.html` (Meson `custom_target('cargo-build', env: {CARGO_HOME, APP_ID})` + `gresource_bundle`), `develop.kde.org/docs/getting-started/rust/rust-flatpak` (Corrosion/cmake variant, `cargo-sources.json` + `.cargo/config.toml` copy pattern).
* CI — `github.com/flatpak/flatpak-github-actions` (README: `flatpak-builder@v6`, `ghcr.io/flathub-infra/flatpak-github-actions:gnome-48`, ARM64 runner matrix, `flat-manager` deploy), `github.com/flathub-infra/flatpak-builder-lint` (manifest/builddir/repo lints, `appstreamcli`, `desktop-file-validate`), `docs.flathub.org/docs/for-app-authors/submission` + `linter` pages.
* Glycin — `gitlab.gnome.org/GNOME/glycin` (~1100 commits, `loader` binaries via `bwrap`/`flatpak-spawn`/seccomp/memfd), `linuxfromscratch.org/blfs` `glycin-2.1.5` (build deps `bubblewrap libseccomp lcms2 fontconfig`, `meson -Dlibglycin-gtk4`, optional `libheif/libjxl/librsvg`), `github.com/GNOME/glycin` (MPL-2.0).
* XDG Base Dir — `specifications.freedesktop.org/basedir/latest` (v0.8, `XDG_DATA_HOME`/`XDG_CONFIG_HOME`/`XDG_STATE_HOME`/`XDG_CACHE_HOME`/`XDG_DATA_DIRS`), `docs.flatpak.org` XDG remapping note (`~/.var/app/<ID>/{cache,config,data}`).
* Portals — `github.com/flatpak/xdg-desktop-portal` discussions #1301 (Libraries/Folders portal, "ask user to pick directory on first launch") + #1845 (portal permission persistence model: path + directory inode), `docs.flatpak.org` portals ("Interface toolkits like GTK3/Qt5 implement transparent support for portals").
* Windows/macOS — `gtk.org/docs/installations/windows` (MSYS2 `mingw-w64-ucrt-x86_64-gtk4` vs `gvsbuild build gtk4`, distributing `C:/gtk` bundle), `gtk-rs.org/gtk4-rs/*/book/installation_windows.html` + `installation_macos.html` (`brew install gtk4 libadwaita` on macOS, MSYS2 vs gvsbuild on Windows), `github.com/wingtk/gvsbuild` (Windows build scripts, `winget`/`choco` prereqs), `formulae.brew.sh/formula/gtk4` (Homebrew 4.22.4), `ladaapp/lada` `docs/linux_install.md` (per-distro GStreamer + `cargo cinstall gst-plugin-gtk4` workaround for Ubuntu 24.04).
* Packagers deferred — `github.com/burtonageo/cargo-bundle` (`.app`/`deb`/`msi`/`AppImage`, `[package.metadata.bundle]`), `github.com/crabnebula-dev/cargo-packager` + `AppFactorIo/cargo-packager` (DMG/AppImage/deb/pacman, `Packager.toml`), `github.com/axodotdev/cargo-dist` (tarballs + CI-generated releases), `rust-cli.github.io/book/tutorial/packaging.html` (ripgrep distribution survey: `cargo install` → GitHub releases → distro packages).
* License — `ffmpeg.org/legal.html` + `FFmpeg/LICENSE.md` (LGPL/GPL/nonfree matrix), `gstreamer.freedesktop.org/licensing.html` (LGPL, plugin good/bad/ugly split), `glycin` `LICENSE` (MPL-2.0), `gtk4`/`libadwaita` LGPL-2.1+.

---
*Next step for map:* adopt `gtk-rust-template` layout + the §3 manifest sketch (+ §4 vendoring + §6 portal-first FS + §7 CI) for the shell prototype; grill broad-filesystem vs portal-only and hash-log home (tickets #5/#6) before locking `finish-args`.
