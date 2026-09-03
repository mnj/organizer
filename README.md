# Organizer

File-triaging desktop app — shows one image at a time and moves it to a
chosen action folder with shortcuts, while tracking duplicates by content
hash. Native GTK4 Rust, maximized split-view: **Preview** on top,
**Action Bar** below.

## Quick start

```sh
organizer /tmp/src        # triage a folder
organizer                 # folder picker when no arg is given
```

## Building from source

Per-distro system dependencies (see `docs/packaging.md` for the full matrix):

```sh
# Ubuntu 22.04 / 24.04 / Debian trixie
sudo apt update && sudo apt install -y libgtk-4-dev libadwaita-1-dev \
  libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
  gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-libav \
  liblcms2-dev libfontconfig1-dev libseccomp-dev bubblewrap \
  pkg-config build-essential curl

# Fedora 41
sudo dnf install -y gtk4-devel libadwaita-devel \
  gstreamer1-devel gstreamer1-plugins-base-devel gstreamer1-plugins-good \
  gstreamer1-plugins-bad-free gstreamer1-plugin-libav \
  lcms2-devel fontconfig-devel libseccomp-devel bubblewrap \
  pkgconf-pkg-config gcc

# Arch
sudo pacman -S --needed gtk4 libadwaita gstreamer gst-plugins-base \
  gst-plugins-good gst-plugins-bad gst-libav lcms2 fontconfig libseccomp \
  bubblewrap pkgconf base-devel
```

Then:

```sh
cargo build --release --locked
./target/release/organizer /tmp/src
```

## AppImage

Single-file download, runs on any ext4/btrfs host with no system
gtk/gstreamer/glycin installed:

```sh
./packaging/build-appimage.sh
./Organizer-x86_64.AppImage /tmp/src
```

`AppImageUpdate` applies zsync deltas from GitHub Releases.

## Docs

- `CONTEXT.md` — domain vocabulary (Source Folder, Queue Snapshot, …)
- `docs/packaging.md` — raw vs AppImage, sys-dep matrix, AppDir layout, updates
- `docs/adr/` — architecture decisions
- `docs/research/` — rendering, sandboxing, packaging research
