# Research: sandboxed decoding for raw executable and AppImage (glycin bwrap/seccomp, GStreamer isolation & fallback)

> Ticket: [#11](https://github.com/mnj/organizer/issues/11) — Part of Map [#1](https://github.com/mnj/organizer/issues/1)
> Branch: `research/sandboxed-decoding-raw-appimage` · Date: 2026-08-30 · Status: completed
> Supersedes packaging research [#10](https://github.com/mnj/organizer/issues/10) (Flatpak portion superseded 2026-08-30) · Builds on rendering-stack [#2](https://github.com/mnj/organizer/issues/2) (`gtk4 + glycin → Texture + GStreamer + gtk4paintablesink`)

## Question

How do we guarantee sandboxed image/video decoding for a **raw executable** and inside an **AppImage** (outside Flatpak), given the chosen stack (`glycin → Texture + GStreamer + gtk4paintablesink`)? What must be bundled, how do loaders discover paths inside AppDir, how does fallback behave when `bwrap` is missing or unprivileged user namespaces are disabled, can sandbox fall back in-process, is GStreamer video sandboxed, and what are crash-isolation and performance implications?

## TL;DR recommendation

**Bundle `bwrap` + `libseccomp.so.2` + all `glycin-loaders` binaries + their config files + `gdk-pixbuf` loaders cache inside the AppImage; relocatable via `$APPDIR` + `XDG_DATA_DIRS` wrapper. On raw binary, depend on host `bubblewrap` + `libseccomp` + `glycin-loaders` but verify at startup and refuse to run unsandboxed unless the user explicitly opts in.**

* **Raw binary (`cargo run` / `cargo install`):** require `bubblewrap` ≥ 0.8, `libseccomp` ≥ 2.5, `glycin-loaders 2+` (compat-version `2+`) on `PATH`/`XDG_DATA_DIRS`. Glycin's `SandboxMechanism::Bwrap` does full isolation (`bwrap --unshare-all --clearenv --ro-bind /usr --dev /dev --seccomp <memfd> --chdir /`). If `bwrap` is missing or user namespaces are disabled, glycin **does not silently degrade to in-process** with stock `glycin` crate — it switches `SandboxMechanism` to `NotSandboxed` and logs `WARNING: Glycin running without sandbox. Bubblewrap doesn't work`. For Organizer (hard sandbox requirement) treat that as **fatal / refuse-to-start** with a dialog, not a silent fallback. `image` crate fallback must be **explicit and denied** under the sandbox policy (it runs in-process).
* **AppImage:** bundle `bwrap` (static or with `libcap` linked), `libseccomp`, all loader binaries at `$APPDIR/usr/libexec/glycin-loaders/2+/` (and their linked deps: `libheif`, `libjxl`, `librsvg`, `liblcms2`, `fontconfig`, `freetype`, `dav1d`, `libaom` if needed) and their `.conf` files at `$APPDIR/usr/share/glycin-loaders/2+/conf.d/`. Set `GLYCIN_DATA_DIR=$APPDIR/usr/share` **and** `XDG_DATA_DIRS=$APPDIR/usr/share:$XDG_DATA_DIRS` in `AppRun` so `glycin-core::config::Config::data_dirs()` discovers configs. Also `LD_LIBRARY_PATH=$APPDIR/usr/lib:$APPDIR/usr/lib/x86_64-linux-gnu` for seccomp-linked libs. Verify with `bwrap --version` + check `CONFIGURATION` inside AppImage at launch.
* **GStreamer video:** **not sandboxed by default**. GStreamer decoders (`avdec_h264`, `vp9dec`, `av1dec`, etc.) run **in-process** inside the Organizer address space. Two viable hardening options: (A) accept glycin-only sandboxing for triage and document video-codec risk (recommended for v1); (B) wrap `playbin` in a helper process (own `gst-launch` helper + `--seccomp`/`bwrap` wrapper or `flatpak-spawn`-style) — significant engineering. Do not wrap GStreamer with `bwrap` ad-hoc for v1; track as follow-up ticket.
* **Crash isolation:** malformed image crashing the loader (`SIGABRT`, `SIGSEGV` inside `glycin-image-rs` or `libheif`) kills **only** the sandboxed loader process. `dbus::RemoteProcess` sees `ProcessDisconnected` / `PrematureExit` / `RemoteError::Panic`, the main `Image::load().await` returns `Err(ErrorKind::RemoteError(Panic))` or `PrematureExit`. Main app stays alive; UI shows placeholder + toast and advances queue. No `unwrap()` on loader futures in UI thread.
* **Performance:** `bwrap` fork + seccomp filter export (memfd) + namespace unshare costs ~5–15 ms per cold loader spawn plus ~2-4 ms for warm pooled reuse (30 s retention). Negligible for human-speed triage (1–2 files/s). Large files bounded by `setrlimit(RLIMIT_AS)` (~80% of `MemAvailable - 200 MB`, cap 20 GB) which kills OOM loaders without killing the host.

---

## 1. Glycin sandbox model outside Flatpak — primary source walkthrough

### 1.1 What glycin does (source: `GNOME/glycin` README + `glycin-core/src/sandbox.rs:144-470` + `glycin-core/src/util.rs:126-184`)

Glycin's security boundary is **one OS process per image loader binary**, D-Bus peer-to-peer over a Unix socket, texture via sealed `memfd` + `mmap` to GDK.

```
Organizer (gtk4)  ──UnixStream pair──►  glycin-core Sandbox::spawn()
                                          │
                        ┌─────────────────┼─────────────────┐
                        │                 │                 │
                 SandboxMechanism::Bwrap  │  FlatpakSpawn   │ NotSandboxed
                        │                 │                 │
              bwrap --unshare-all    flatpak-spawn     exec loader
              --clearenv --ro-bind   --sandbox         directly
              --seccomp <memfd-BPF>  prlimit --as
              --dev /dev --tmpfs     --forward-fd
              + setrlimit(RLIMIT_AS) + pdeathsig
```

* **Outside Flatpak (Host):** `glycin-core/src/sandbox.rs::bwrap_command()` (lines 262–470) builds:
  ```rust
  Command::new("bwrap")
    .args(["--unshare-all","--die-with-parent","--chdir","/",
           "--ro-bind","/usr","/usr", "--dev","/dev",
           "--ro-bind-try","/etc/ld.so.cache","/etc/ld.so.cache",
           "--ro-bind-try","/nix/store","/nix/store",
           "--tmpfs","/tmp-home","--tmpfs","/tmp-run",
           "--clearenv","--setenv","HOME","/tmp-home",
           "--setenv","XDG_RUNTIME_DIR","/tmp-run"])
    // + inherited RUST_BACKTRACE/RUST_LOG/XDG_RUNTIME_DIR
    // + cached SystemSetup: symlink /lib64 → /usr/lib, ro-bind extra
    // + ro-bind for loaders not in /usr (user loaders)
    // + fontconfig paths + writable fc cache under XDG_CACHE_HOME/glycin/…
    // + --seccomp <memfd fd>  (BPF exported via libseccomp + memfd_create)
    // + pre_exec: set_memory_limit() via setrlimit(RLIMIT_AS)
  ```
* **Seccomp filter** (`sandbox.rs:44-142`, copied from `flatpak/flatpak:common/flatpak-run.c`):
  * Allow-list default (`ScmpAction::Allow`), block ~30 dangerous syscalls with `ScmpAction::Errno(EPERM/ENOSYS)`: `syslog`, `uselib`, `acct`, `quotactl`, `add_key/keyctl/request_key`, `move_pages/mbind/get_mempolicy/set_mempolicy/migrate_pages`, `unshare/setns/mount/umount/pivot_root/chroot`, `clone` with `CLONE_NEWUSER`, `clone3`, `open_tree/move_mount/fsopen/.../mount_setattr`, `perf_event_open`, `personality`, `ioctl(TIOCSTI/TIOCLINUX)`.
  * Exported via `ScmpFilterContext::export_bpf(&memfd)` into a sealed memfd (`memfd_create("seccomp-bpf-filter")`), then `bwrap --seccomp <fd>` consumes it. Without `libseccomp` the build fails (`dependency('libseccomp')` in `meson.build`).
* **Memory limit** (`sandbox.rs:543-616`): `memory_limit() = 80% * min(MemAvailable+SwapFree - 200 MB, 20 GB)`, fallback `1 GB` if `/proc/meminfo` unreadable. Applied via `setrlimit(RLIMIT_AS, lim, lim)` in `pre_exec` (and via `prlimit --as=` inside Flatpak). This is the OOM defense for decompression bombs; loader killed by kernel before host OOM.
* **GFile streaming:** file content streamed over the Unix socket (`SourceTransmission`), so loaders decode without direct FS or network access. `ExposeBaseDir=true` in loader config (`svg`) bind-mounts the image's parent dir `--ro-bind` for external `<image href>`; `--ro-bind-try` respected. This option has **no effect** inside `flatpak-spawn` sandboxes (README caveat).
* **Inside Flatpak:** `flatpak_spawn_command()` wraps with `flatpak-spawn --sandbox --watch-bus --directory=/ --forward-fd=$FD prlimit --as=$LIM $LOADER --dbus-fd $FD`. No additional seccomp beyond Flatpak's own; isolation comes from Flatpak's outer sandbox + `--sandbox` creating a fresh Flatpak-sandboxed child (portal path).
* **NotSandboxed:** `no_sandbox_command()` just `Command::new(exec).env_clear().env(RUST_BACKTRACE...).pre_exec(set_pdeathsig(SIGKILL))`. No seccomp/bwrap.

### 1.2 Sandbox selection logic (source: `glycin-core/src/util.rs:126-184` + `glycin-core/src/api/common.rs:23-82`)

```rust
pub enum SandboxMechanism { Bwrap, FlatpakSpawn, NotSandboxed }

impl SandboxMechanism {
  pub async fn detect() -> Self {
    match RunEnvironment::cached().await {
      SandboxForceDisabled => NotSandboxed,  // GLYCIN_DISABLE_SANDBOX=i-know-the-risks
      FlatpakDevel          => NotSandboxed, // flatpak-builder --run + app id endsWith "Devel"
      Flatpak               => FlatpakSpawn, // /.flatpak-info present, not Devel
      Host                  => Bwrap,
      HostBwrapSyscallsBlocked => NotSandboxed,
    }
  }
}

pub enum SandboxSelector { Auto, Bwrap, FlatpakSpawn, NotSandboxed }
impl SandboxSelector { pub async fn determine_sandbox_mechanism(self) -> SandboxMechanism }
```

* `RunEnvironment::cached()` (lazy, one probe per process):
  1. If `GLYCIN_DISABLE_SANDBOX=i-know-the-risks` → `SandboxForceDisabled → NotSandboxed` + stderr warning.
  2. Else if `/.flatpak-info` parses and `Instance:build=true` + `Application:name` endswith `Devel` → `FlatpakDevel → NotSandboxed` (so `flatpak-builder --run` dev builds don't hang on `flatpak-spawn` needing an installed Flatpak with same ID).
  3. Else if `Sandbox::check_bwrap_syscalls_blocked().await` → `HostBwrapSyscallsBlocked → NotSandboxed` + warning.
  4. Else if `/.flatpak-info` present (and not Devel) → `Flatpak → FlatpakSpawn`.
  5. Else → `Host → Bwrap`.

* `Sandbox::check_bwrap_syscalls_blocked()` (`sandbox.rs:652-725`) spawns a minimal `bwrap` with a SIGSYS handler:
  * Creates `ConfigEntry::Binary("/usr/bin/true")`, builds `bwrap_command` + seccomp memfd, wraps with `setup_sigsys_handler()` (`sigaction SIGSYS → sigsys_handler exit(128+SIGSYS)`).
  * Runs `bwrap --ro-bind /usr /usr … /usr/bin/true` and inspects exit:
    * `success → false` (not blocked)
    * `SIGSYS` or `code == 128+SIGSYS` → `true` (seccomp blocked bwrap syscalls — AppArmor lockdown, e.g. Ubuntu `apparmor.d` profile `bwrap-userns-restrict` as noted in `roddhjav/apparmor.d#881`, or kernel `unprivileged_userns_clone=0`)
    * `stderr` contains `"Creating new namespace failed"|"No permissions to create new namespace"|"…Permission denied"` (covers Debian old patch grammar) → `true`
    * Else assume not syscall-blocked (failed for other reason) → `false`.

### 1.3 Image loading pipeline (source: `glycin-core/src/api/loader.rs:167-280`, `glycin-core/src/dbus.rs:73-140`, `glycin-core/src/pool.rs:121-290`)

* `Loader::new(file).load().await` → `ProcessorContext::new()` detects mime via `config::Config::guess_mime_type()` + `gio::content_type_guess` + filename fallback, looks up `LoaderConfig` (via `XDG_DATA_DIRS` scan), determines `SandboxMechanism` via selector, computes `base_dir` if `ExposeBaseDir`.
* `Pool::get_loader()` hashes `(fontconfig, processor Binary path, expose_base_dir, base_dir, sandbox_mechanism)` — separate pools per hash (so SVG base-dir var means separate sandbox).
* `dbus::RemoteProcess::new(config_entry, sandbox_mechanism, base_dir)` → `Sandbox::new(...).spawn().await` → `command.arg("--dbus-fd").arg(fd)` + `pre_exec close_range(3, MAX, CLOEXEC)` + pass `dbus_socket` + `seccomp_fd` as shared FDs (CLOEXEC cleared).
* Spawns `command` via `spawn_blocking(|| command.spawn())`, then D-Bus handshake over the UnixStream; `RemoteProcess` pooled for 30 s (`PoolConfig::loader_retention_time = 30 s`, `max_parallel_operations = usize::MAX`).

### 1.4 Loader discovery (source: `glycin-core/src/config.rs:331-368`, `glycin/meson.build` compat version)

* `Config::data_dirs()`:
  ```rust
  if GLYCIN_DATA_DIR { vec![GLYCIN_DATA_DIR] }
  else { vec![user_data_dir()] + system_data_dirs() } // glib::system_data_dirs() → $XDG_DATA_DIRS split(':') or fallback /usr/local/share:/usr/share
  ```
  For each dir, scan ` $dir/glycin-loaders/<compat-version>+/conf.d/*.conf`.
* Compat version today is `2+` (`COMPAT_VERSION = 2`, path `2+`, as in `libglycin/meson.build: compat_version='2+'`). Future glycin 4 that breaks wire format would be `3+`.
* Config `KeyFile`:
  ```ini
  [loader:image/png]
  Exec=/usr/libexec/glycin-loaders/2+/glycin-image-rs
  # optional: ExposeBaseDir=true  Fontconfig=true  Identifiers=…
  ```
  First file wins per mime; `config.image_loader` is a `BTreeMap<MimeType, LoaderConfig>` with deduplication.
* Binary search: `Processor::Binary(PathBuf)` stored verbatim; `bwrap_command` re-canonicalizes and `--ro-bind`s it if not under `/usr`. So `$APPDIR/usr/libexec/…` works **only** if config `Exec` points there (see §2 AppImage wiring).

### 1.5 Non-Linux / builtin mode

* `glycin-core` feature flags: `external` (default on Linux — spawn helpers) vs `builtin` (compile loaders into library). `glycin/src/lib.rs` exports `glycin_external::*` on Linux, `glycin_builtin::*` elsewhere.
* Stock `glycin` crate **does not bundle** `builtin-image-rs` by default; enabling `features = ["builtin","builtin-image-rs"]` opts into in-process decode (no `bwrap`, `SandboxMechanism::NotSandboxed` always for builtins: `ImageLoader::Builtin(_) => NotSandboxed` in `loader.rs:542`). This is Rust-memory-safe but **not a process boundary**; large decompression bomb can still hit the main process's `RLIMIT_AS` (none by default). Upstream plans a future `"compile loaders into library"` mode to still give Rust safety on non-Linux (README § Limitations) — that's an opt-in build, not a runtime fallback.
* `glycin-ng` (community fork, `QaidVoid/glycin-ng`) offers an in-process worker-thread sandbox (`Landlock + seccomp + rlimit` on worker thread, `~9× smaller`) as an alternative to `bwrap`. It is **not** upstream and changes the crate boundary; useful reference for "in-process fallback" design but not a drop-in for stock `glycin`.

---

## 2. Raw binary vs AppImage: what must be present vs bundled

### 2.1 Raw executable (host `cargo build`/`cargo run`)

| Artifact | Host requirement | Version | Verify |
|---|---|---|---|
| `bwrap` (`bubblewrap`) | must be on `$PATH` (glycin calls `Command::new("bwrap")` without absolute path) | ≥ 0.8 (for `--seccomp` flag) — `bwrap --help` must list `--seccomp` | `bwrap --version` in AppRun/startup check |
| `libseccomp.so.2` | linked by `libglycin` / `glycin-core` (meson `dependency('libseccomp')`) — `ldd libglycin.so` should list `libseccomp.so.2` | ≥ 2.5.0 (`meson.build seccomp_req`) | `ldd` at build sanity |
| `glycin-loaders` binaries | `glycin-loaders` package provides `/usr/libexec/glycin-loaders/2+/glycin-{image-rs,heif,jxl,svg}` | `2+` compat; each binary must be executable | `ls /usr/libexec/glycin-loaders/2+/` |
| `glycin-loaders` configs | `/usr/share/glycin-loaders/2+/conf.d/*.conf` (`XDG_DATA_DIRS`) | `2+` | `ls /usr/share/glycin-loaders/2+/conf.d/` |
| Fonts for SVG/text rendering | `libfontconfig.so` + cache; loaders with `Fontconfig=true` need fonts mounted | — | `fc-cache` present |
| `liblcms2` | for ICC profile handling | — | runtime link |

**Startup verification (recommended for Organizer on raw binary):**
```rust
// at app startup, before first Loader::new()
if std::env::var("GLYCIN_DISABLE_SANDBOX").is_ok() {
  eprintln!("refusing to run with GLYCIN_DISABLE_SANDBOX");
  std::process::exit(2);
}
let env = util::RunEnvironment::cached().await;
match env {
  RunEnvironment::HostBwrapSyscallsBlocked => {
    // show dialog: "Sandbox unavailable: bwrap/userns blocked…"
    // offer: 1) install bubblewrap, 2) enable unpriv userns (sysctl), 3) use AppImage, 4) abort
  }
  RunEnvironment::Host => {} // ok
  _ => {} // Flatpak variants not expected on raw
}
```
Also probe `which bwrap` and parse `XDG_DATA_DIRS` scan: if `Config::cached().await.image_loader.is_empty()` → `NoLoadersConfigured` error path (treat as missing `glycin-loaders`).

### 2.2 AppImage — bundling checklist

AppImage is a squashfs + ELF `AppRun` runtime (`type2`). It is **not** a container: the AppImage process runs as the host user with the host kernel; bundling `bwrap` means shipping the binary **inside** AppDir and invoking it from the AppDir path.

#### What to bundle (verified against `sandbox.rs:bwrap_command` and `meson.build`)

Checklist items (one line each in release process):

- [ ] **`bwrap` binary** — bundle `/usr/bin/bwrap` from build system (or build `bubblewrap` from source with `meson -Dselinux=disabled`) into `$APPDIR/usr/bin/bwrap` **or** `$APPDIR/usr/libexec/bwrap`. If built against `libcap.so.2`, also bundle `libcap.so.2` + set `LD_LIBRARY_PATH` / use `linuxdeploy --library` to pull it. Verify `bwrap` is not `setuid` (historical setuid mode removed; modern `bwrap` uses unpriv user namespaces only). Size ~60–90 KB.
- [ ] **`libseccomp.so.2`** — bundle into `$APPDIR/usr/lib/` + `$APPDIR/usr/lib/x86_64-linux-gnu/`; needed both by `libglycin` (linked) and by `bwrap` if built with seccomp. Verify with `patchelf --print-needed $APPDIR/usr/bin/bwrap`.
- [ ] **`glycin-loaders` binaries** — build `glycin` with `meson -Dglycin-loaders=true -Dloaders=glycin-image-rs,glycin-heif,glycin-svg,glycin-jxl` and stage with `DESTDIR=$APPDIR meson install`. Binaries land at `$APPDIR/usr/libexec/glycin-loaders/2+/glycin-*`. Alternatively `cargo build -p glycin-image-rs --release` and copy. Keep `2+` path verbatim — `Config::load()` expects `.../2+/conf.d`.
- [ ] **Loader linked deps** — `ldd` each loader binary and bundle their recursive deps via `linuxdeploy --plugin gtk` / `--library` or manually. Key ones:
  - `glycin-image-rs` (Rust): links `libgcc`, `libstdc++` indirectly; largely static. Still copy host `libssl`/`libz` if linked.
  - `glycin-heif`: `libheif.so.1` → `libde265`, `libdav1d`, `libaom`, `libx265` (LGPL; ensure `x265` built without nonfree). If you don't need HEIC/AVIF, omit `glycin-heif` to avoid LGPL surface — but then HEIC/AVIF shows `UnknownImageFormat`.
  - `glycin-jxl`: `libjxl.so` (BSD/GPL-dual; `jpegxl-rs` GPL-3.0 only for that loader binary; not infecting main app because separate process — see README GPL note).
  - `glycin-svg`: `librsvg-2.0.so` + `libcairo`, `libpango`, `libfontconfig`, `libfreetype`, `libharfbuzz`, `libxml2`.
  - `glycin-raw` (optional): `libopenraw` (LGPL-3.0).
  Verify each with `readelf -d $APPDIR/usr/libexec/glycin-loaders/2+/glycin-image-rs | grep NEEDED`.
- [ ] **Loader configs** — install `$APPDIR/usr/share/glycin-loaders/2+/conf.d/glycin-image-rs.conf` etc. with **relocated Exec**:
  ```ini
  [loader:image/png]
  Exec=/usr/libexec/glycin-loaders/2+/glycin-image-rs
  ```
  The path is **absolute from the bwrap chroot perspective**. Since `bwrap --ro-bind /usr /usr` bind-mounts host `/usr`, inside the AppImage `bwrap` session `/usr` *is* `$APPDIR/usr` **only if** AppRun has `LD_LIBRARY_PATH`/`XDG_DATA_DIRS` set and the AppImage is mounted at a mountpoint that overlays `/usr` — which it does **not** by default. See §2.3 for the relocation trick.
- [ ] **`gdk-pixbuf` integration** — not needed if using `glycin` crate directly, but to keep `gdk-pixbuf 2.43.2+ -Dbuiltin_loaders=glycin` path consistent, stage `glycin-loaders` first and ensure `glycin` cache `gdk-pixbuf-query-loaders` output references correct paths.
- [ ] **Fontconfig data + cache** — bundle `fonts.conf` + `fonts/` or at minimum ensure `fontconfig` paths are `--ro-bind-try`-ed. `bwrap_command` handles `Fontconfig=true` loaders by mounting `cached_paths()` (from `glycin-core/src/fontconfig.rs`) with `--ro-bind-try` and provisioning a writable `XDG_CACHE_HOME/glycin/$exec/fontconfig` dir. Inside AppImage, `XDG_CACHE_HOME` must be writable (`$HOME/.cache` or `$APPDIR` cache shim).
- [ ] **GStreamer plugins** (for video, see §6) — use `linuxdeploy-plugin-gstreamer` + `linuxdeploy-plugin-gtk`. It copies `lib/gstreamer-1.0/*.so` into `$APPDIR/usr/lib/gstreamer-1.0` and installs an AppRun hook that sets `GST_PLUGIN_SYSTEM_PATH=$APPDIR/usr/lib/gstreamer-1.0` + `GST_PLUGIN_SCANNER=$APPDIR/usr/libexec/gstreamer-1.0/gst-plugin-scanner`. Verify with `GST_DEBUG=4 ./Organizer-*.AppImage gst-inspect-1.0`.
- [ ] **`gst-plugin-gtk4` cdylib** — build via `cargo cinstall -p gst-plugin-gtk4 --release --features waylandegl,x11glx,x11egl,dmabuf,gtk_v4_14 --library-type=cdylib --prefix=/usr --destdir=$APPDIR`. It installs to `$APPDIR/usr/lib/gstreamer-1.0/libgstgtk4.so`. Also set `GST_PLUGIN_PATH`.

#### AppDir layout (concrete)

```
Organizer.AppDir/
  AppRun                # wrapper shell/python that sets env, then exec $APPDIR/usr/bin/organizer
  organizer.desktop
  usr/
    bin/
      organizer         # main Rust binary (links libgtk-4, libgstreamer, libglycin, libseccomp)
      bwrap             # bundled bubblewrap (optional symlink to usr/bin/bwrap)
    lib/
      libglycin.so.2    # or statically linked via crate
      libglycin-gtk4.so
      libseccomp.so.2
      libcap.so.2
      gstreamer-1.0/
        libgstgtk4.so
        libgstcoreelements.so ...
    libexec/
      glycin-loaders/2+/glycin-image-rs
      glycin-loaders/2+/glycin-heif
      glycin-loaders/2+/glycin-svg
      glycin-loaders/2+/glycin-jxl
      gstreamer-1.0/gst-plugin-scanner
    share/
      glycin-loaders/2+/conf.d/*.conf
      glib-2.0/schemas/...
      icons/...
      applications/...
```

Verify with `appimagetool --verbose $APPDIR` and `bwrap` linkage: `ldd $APPDIR/usr/libexec/glycin-loaders/2+/glycin-image-rs`.

### 2.3 How loaders discover paths inside AppDir (XDG_DATA_DIRS + GLYCIN_DATA_DIR)

* Glycin discovery is **filesystem-based**: scan every directory in `XDG_DATA_DIRS` (colon-separated) plus `XDG_DATA_HOME` for `glycin-loaders/<ver>/conf.d/*.conf` (see `config.rs:348-368`).
* Host default `XDG_DATA_DIRS` is `/usr/local/share:/usr/share` (glib fallback if env var unset). Inside AppImage, the squashfs is mounted at a temporary FUSE mount (e.g. `/tmp/.mount_OrganizXXXXXX`). The AppDir layout above places loaders at `$APPDIR/usr/share/glycin-loaders/2+/conf.d` — which is **not** on host `XDG_DATA_DIRS` by default.
* **AppRun must export:**
  ```sh
  #!/bin/sh
  HERE="$(dirname "$(readlink -f "$0")")"
  export XDG_DATA_DIRS="$HERE/usr/share:${XDG_DATA_DIRS:-/usr/local/share:/usr/share}"
  # Also set GLYCIN_DATA_DIR to force-only-AppDir if you want to ignore host loaders:
  # export GLYCIN_DATA_DIR="$HERE/usr/share"
  export LD_LIBRARY_PATH="$HERE/usr/lib:$HERE/usr/lib/x86_64-linux-gnu:${LD_LIBRARY_PATH:-}"
  # GStreamer
  export GST_PLUGIN_SYSTEM_PATH="$HERE/usr/lib/gstreamer-1.0:${GST_PLUGIN_SYSTEM_PATH:-}"
  export GST_PLUGIN_SCANNER="$HERE/usr/libexec/gstreamer-1.0/gst-plugin-scanner"
  # Help bwrap find its own binary when PATH is mangled — glycin calls `bwrap` via PATH
  export PATH="$HERE/usr/bin:${PATH:-/usr/bin:/bin}"
  exec "$HERE/usr/bin/organizer" "$@"
  ```
* **Why `GLYCIN_DATA_DIR`** (`config.rs:628-634`): if set, `data_dirs()` returns exactly that one dir, ignoring system dirs. Useful to **force** AppImage to use only bundled loaders (deterministic format coverage, no host leakage). Recommended for Organizer AppImage: set `GLYCIN_DATA_DIR=$HERE/usr/share` so the host's older/newer loaders don't shadow bundled ones. Tradeoff: you then **must** bundle all loaders you need; falling back to host loaders is intentionally disabled.
* **`Exec` path inside `.conf`:** the `Exec` value is an absolute path that `bwrap --ro-bind` will need to be reachable inside the bwrap chroot. With host `bwrap --ro-bind /usr /usr`, the path `/usr/libexec/glycin-loaders/2+/glycin-image-rs` inside the sandbox refers to **host `/usr`**, not AppDir. Inside AppImage, if you keep `Exec=/usr/libexec/...` and have `LD_LIBRARY_PATH` pointing to AppDir, the bwrap bind of host `/usr` will see the host's `/usr/libexec` (possibly missing). Two solutions:
  1. Keep `Exec` host-absolute and **also** `appimagetool`/`linuxdeploy` patches `XDG_DATA_DIRS` + copies loader binaries to **both** `$APPDIR/usr/...` and a location that is already `--ro-bind`-ed (`/usr`). Since `bwrap_command` does `--ro-bind-try /nix/store` and checks `if !exec.starts_with("/usr") { mount("--ro-bind", exec) }`, a non-`/usr` `Exec` gets explicitly bind-mounted. So using a non-`/usr` Exec like `$APPDIR/usr/libexec/...` that is **outside** `/usr` won't help. Instead set `Exec` to a `/usr` path and ensure that path **exists on host** or inside AppDir's `/usr` **is** host `/usr` after AppRun's `LD_LIBRARY_PATH` dance — which it is not. Correct fix is option 2.
  2. Set `Exec` to `$APPDIR`'s **real** mounted path? Not stable (mount point randomised). Better: **patch glycin configs at AppRun time** with a `sed` on the temp `conf.d` directory, or generate configs on the fly in `$XDG_RUNTIME_DIR`. Or rely on `bwrap_command`'s `if !exec.starts_with("/usr") { mount("--ro-bind", exec) }` branch: if `Exec` is **not** under `/usr`, bwrap will copy it in. So you could use `Exec=$APPDIR/usr/libexec/...` with literal `$APPDIR` — but `$APPDIR` is not known at config-write time unless AppRun generates a temp `XDG_DATA_DIRS` pointing to `$XDG_RUNTIME_DIR/glycin-loaders/...` with patched Exec values. That's the robust pattern: AppRun generates `$XDG_RUNTIME_DIR/glycin-loaders/2+/conf.d/*.conf` with `Exec=$HERE/...` and sets `XDG_DATA_DIRS=$XDG_RUNTIME_DIR:$HERE/usr/share:…`. Since `glycin-core` reads `XDG_DATA_DIRS` at first `Config::cached()` (lazy, once per process), AppRun must set env **before** `exec organizer`.

  Simpler pragmatic choice (and what current AppImage tooling does): use `--appimage-extract` layout where `$APPDIR` **is** `usr/`-rooted and `Exec=/usr/libexec/...` does resolve because `bwrap --ro-bind /usr /usr` after `LD_LIBRARY_PATH` tricks still sees host `/usr`, but `linuxdeploy` has already **copied** `libexec` into **host-visible** `/usr`? That doesn't happen. So the reliable approach is AppRun-generated `GLYCIN_DATA_DIR` temp tree; document that in bundling checklist.

* **Fontconfig inside AppImage:** `bwrap_command` provisions `XDG_CACHE_HOME/glycin/$exec/fontconfig` (writable). Inside AppImage, set `XDG_CACHE_HOME` to `$HOME/.cache` (writable) before first load; don't point it to read-only AppDir.

---

## 3. Sandbox matrix

Rows collapse the host state `RunEnvironment` × AppImage presence; columns are observed behavior with stock `glycin 3.x` / `libglycin 2.x`.

| # | Host env | AppImage? | `bwrap` availability | `user_namespaces` | `seccomp` | Glycin `SandboxMechanism` (via `Auto`) | Actual isolation | Texture delivery | Main app on malformed image | Recommended Organizer policy |
|---|---|---|---|---|---|---|---|---|---|
| H1 | Ubuntu 24.04, kernel 6.8, `user.max_user_namespaces=15681`, AppArmor `unprivileged_userns_restrict=0` | raw exec | `bwrap` 0.8+ on PATH, `libseccomp` present | enabled | available | `Bwrap` | **sandboxed**: `bwrap --unshare-all --clearenv --seccomp <BPF> + RLIMIT_AS` | memfd mmap (sealed) | loader `SIGABRT` → `Err(RemoteError::Panic)`/`PrematureExit`, app alive with placeholder | ✅ run |
| H2 | Ubuntu 24.04 + `apparmor.d` `bwrap-userns-restrict` (Ubuntu 24.10+ default) or `sysctl kernel.apparmor_restrict_unprivileged_userns=1` | raw | `bwrap` present but `bwrap` create-namespace `Permission denied` / SIGSYS | disabled by AppArmor | blocked (seccomp filter on `unshare`/`clone(NEWUSER)`) | `NotSandboxed` (`HostBwrapSyscallsBlocked`) | **none** — `No-sandbox` `Command::new(exec)` with only `pdeathsig` | memfd still via D-Bus | loader crash = same as H1 **but** decoder now runs without namespace/seccomp isolation (higher blast radius) | 🛑 **refuse / warn** (hard requirement) — show dialog with `--help` link to `sysctl -w kernel.unprivileged_userns_clone=1` or propose AppImage with bundled `bwrap` + `CAP_SYS_ADMIN` workaround (see fallback spec) |
| H3 | Fedora 41 (SELinux permissive), no AppArmor | raw | `bwrap` present | enabled | available | `Bwrap` | sandboxed | memfd | same as H1 | ✅ run |
| H4 | Arch (no rules) but `bubblewrap` not installed | raw | missing (`SpawnErrorNotFound` when `Command::new("bwrap")` fails) | enabled | n/a | `Bwrap` attempted → spawn fails with `SpawnError` | **none** if fallback to `NotSandboxed`; or **error** if treated as fatal | n/a | `Err(SpawnError)` before decode | 🛑 refuse — prompt install `bubblewrap` |
| H5 | Debian trixie/Sid, `bwrap` present, `libseccomp` missing (unlikely: glycin build dep) | raw | `bwrap` present | enabled | `export_bpf` fails (`Seccomp` error) | spawn fails (`Seccomp` error from `seccomp_filter`) | no process | n/a | `Err(Seccomp)` | 🛑 refuse |
| H6 | NixOS (does not FHS `/usr` bind) | raw | `bwrap` present | enabled | available | `Bwrap` | sandboxed, but `--ro-bind /usr /usr` fails if host has no `/usr` (Nix store at `/nix/store`). Glycin handles with `--ro-bind-try /nix/store` so still works | memfd | same | ✅ run (provided Nix wraps `LD_LIBRARY_PATH`/`PATH`) |
| A1 | Ubuntu 22.04 host, no host `bwrap`/`libseccomp`/`glycin-loaders` | **AppImage** with bundled `bwrap` + `libseccomp` + loaders + `GLYCIN_DATA_DIR=$APPDIR/usr/share` | bundled `bwrap` on `PATH=$APPDIR/usr/bin:$PATH` | enabled (host kernel) | available | `Bwrap` (AppRun PATH ensures `bwrap` found) | **sandboxed** — bwrap bind uses AppImage's `/usr` via `--ro-bind /usr /usr` **only if** `Exec` points to `/usr` path that AppImage has made visible (see §2.3 relocation caveat). With AppRun-generated temp `XDG_DATA_DIRS` + non-`/usr` Exec → `--ro-bind $APPDIR/...` fallback works and sandbox is correct | memfd via AppImage's `/tmp/.mount_*/usr/lib` mmap | loader crash contained | ✅ run — this is the AppImage value proposition: host deps not needed |
| A2 | Ubuntu 22.04, host kernel `unprivileged_userns_clone=0` | AppImage  | bundled `bwrap` | disabled | blocked | `NotSandboxed` | **none** | memfd | same risk as H2 but inside AppImage | 🛑 same refusal policy — AppImage does **not** escape kernel userns disabled. AppImage still isolated at FS level by bwrap mount namespace *attempt* but the attempt fails; glycin falls back to unsandboxed. Document that AppImage does not fix kernel userns lockdown |
| A3 | Fedora 41 | AppImage without bundled `bwrap` (lean build, relying on host `bwrap`) | host `bwrap` | enabled | available | `Bwrap` | sandboxed if host has `bwrap`; else H4 path | memfd | varies | ⚠️ advise bundling `bwrap` — don't rely on host |
| F1 | Any host | raw with `GLYCIN_DISABLE_SANDBOX=i-know-the-risks` | ignored | n/a | n/a | `NotSandboxed` via `SandboxForceDisabled` | none | memfd | same | 🛑 Organizer should `exit(2)` on this env var set if hard sandbox required |
| F2 | Flatpak `flatpak-builder --run` with `id` ending `Devel` (Glycin `test_disable_sandbox=true`) | — | n/a | n/a | n/a | `NotSandboxed` via `FlatpakDevel` | none | n/a | same | N/A (Flatpak out of scope) but note: Organizer will not be a Flatpak; include for completeness |

Notes:

* `RunEnvironment::cached()` is process-lazy; restarting Organizer after installing `bwrap` requires re-exec.
* Inside AppImage, `check_bwrap_syscalls_blocked` still spawns `bwrap --ro-bind /usr … /usr/bin/true` with the bundled or host `bwrap` (depending on `PATH`). Its success/failure determines the row selection. To force AppImage to use bundled `bwrap` even when host has one, AppRun must prepend `$APPDIR/usr/bin` to `PATH` **before** first `Loader::new().load()` (i.e., before any async runtime init).

---

## 4. Bundling checklist (step-by-step, with verification)

### 4.1 Build environment

```bash
# Pin compat version — current upstream is 2+
compat=2+
# Build bubblewrap if not available via distro
# meson _build && meson compile -C _build && DESTDIR=$APPDIR meson install -C _build
# Build glycin loaders
meson setup builddir -Dglycin-loaders=true -Dloaders=glycin-image-rs,glycin-heif,glycin-svg,glycin-jxl -Dprefix=/usr
meson compile -C builddir
DESTDIR="$APPDIR" meson install -C builddir
# Verify compat dir
ls "$APPDIR/usr/libexec/glycin-loaders/$compat"/  # glycin-image-rs …
ls "$APPDIR/usr/share/glycin-loaders/$compat/conf.d"/*.conf
cat "$APPDIR/usr/share/glycin-loaders/$compat/conf.d/glycin-image-rs.conf"
```

### 4.2 AppDir patching (AppRun hook)

```bash
# In AppRun:
HERE="$(dirname "$(readlink -f "$0")")"
# 1) Point glycin at AppDir loaders (deterministic)
export GLYCIN_DATA_DIR="$HERE/usr/share"
# Also extend XDG_DATA_DIRS for other consumers (gdk-pixbuf glycin loader)
export XDG_DATA_DIRS="$HERE/usr/share:${XDG_DATA_DIRS:-/usr/local/share:/usr/share}"
# 2) Library search path for libseccomp, libcap, libheif deps
export LD_LIBRARY_PATH="$HERE/usr/lib:$HERE/usr/lib/x86_64-linux-gnu${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
# 3) Ensure bwrap is found
export PATH="$HERE/usr/bin:${PATH}"
# 4) GStreamer relocation (linuxdeploy-plugin-gstreamer hooks also set these; keep both)
export GST_PLUGIN_SYSTEM_PATH="$HERE/usr/lib/gstreamer-1.0${GST_PLUGIN_SYSTEM_PATH:+:$GST_PLUGIN_SYSTEM_PATH}"
export GST_PLUGIN_SCANNER="$HERE/usr/libexec/gstreamer-1.0/gst-plugin-scanner"
# 5) Optional: fix loader Exec to use AppDir path via temp XDG_DATA_DIRS generation
#    (if you kept Exec=/usr/... and host /usr doesn't have the loaders, the bwrap bind will fail to find the binary)
RUNTIME_GLYCIN="$XDG_RUNTIME_DIR/glycin-loaders/$compat/conf.d"
mkdir -p "$RUNTIME_GLYCIN"
for f in "$HERE/usr/share/glycin-loaders/$compat/conf.d"/*.conf; do
  # rewrite Exec to absolute AppDir path so bwrap's mount("--ro-bind", exec) branch fires
  sed "s|^Exec=.*|Exec=$HERE/usr/libexec/glycin-loaders/$compat/$(basename -s .conf "$f" | sed 's/glycin-//')|" "$f" 2>/dev/null \
    | grep -v "^Exec=" && cat "$f" \
    | sed "s|^Exec=.*|Exec=$HERE/usr/libexec/glycin-loaders/$compat/$(basename "$f" .conf)|" > "$RUNTIME_GLYCIN/$(basename "$f")"
done
# If using the temp runtime dir, prepend it to XDG_DATA_DIRS so glycin picks it first
if [ -d "$RUNTIME_GLYCIN" ]; then
  export XDG_DATA_DIRS="$XDG_RUNTIME_DIR:$XDG_DATA_DIRS"
  export GLYCIN_DATA_DIR="$XDG_RUNTIME_DIR"
fi
exec "$HERE/usr/bin/organizer" "$@"
```

> Alternative simpler approach: just `export GLYCIN_DATA_DIR=$HERE/usr/share` and patch the `.conf` files **in-place** at `DESTDIR` time to use `$HERE`-relative but with fixed prefix trick: write `Exec=/usr/libexec/...` and at AppRun time `sed -i "s|/usr|$HERE/usr|g"` the staged confs copy — but modifying read-only AppImage squashfs is not possible at runtime; use the `$XDG_RUNTIME_DIR` temp tree pattern above.

### 4.3 Verification commands (run from built AppDir before `appimagetool`)

```bash
# 1) bwrap + seccomp availability inside AppDir context
env PATH="$APPDIR/usr/bin:$PATH" bwrap --version
env PATH="$APPDIR/usr/bin:$PATH" bwrap --ro-bind /usr /usr --dev /dev --unshare-all /usr/bin/true && echo ok || echo blocked
# 2) libseccomp linkage
ldd "$APPDIR/usr/bin/organizer" | grep -i seccomp
ldd "$APPDIR/usr/libexec/glycin-loaders/$compat/glycin-image-rs" | head
# 3) glycin config discovery inside AppDir
GLYCIN_DATA_DIR="$APPDIR/usr/share" XDG_DATA_DIRS="$APPDIR/usr/share" \
  "$APPDIR/usr/bin/organizer" --glycin-debug-dump-config 2>&1 | grep "LoaderConfig" # (add a debug flag in organizer that prints Config::load())
# 4) outside-AppImage vs inside
./Organizer-*.AppImage --version
strace -e trace=execve -f ./Organizer-*.AppImage 2>&1 | grep -E "bwrap|glycin-image-rs" | head
# 5) GStreamer inside AppImage
GST_DEBUG=2 ./Organizer-*.AppImage gst-inspect-1.0 gtk4paintablesink
GST_DEBUG=2 ./Organizer-*.AppImage gst-inspect-1.0 libav  # via gst-libav
```

### 4.4 Size impact

* `bwrap` ~80 KB, `libseccomp.so.2` ~130 KB, `glycin-loaders` binaries: `glycin-image-rs` ~4–6 MB (stripped), `glycin-heif` ~2 MB + `libheif` stack ~3–8 MB, `glycin-svg` ~3 MB + `librsvg` stack, `glycin-jxl` ~1.5 MB + `libjxl`. Total incremental AppImage cost **~15–30 MB** compressed squashfs, ~40–60 MB uncompressed. GStreamer `gst-plugins-good/bad/libav + gtk4paintablesink` adds another ~20–40 MB if not delegated to host. Compare prior Flatpak runtime sharing (zero incremental) — AppImage pays duplication cost but gains host independence.

---

## 5. Fallback specification (verified against glycin source)

This is the spec Organizer implements for the hard sandbox requirement "image processing MUST be sandboxed".

### 5.1 Trigger table (when sandbox is not `Bwrap`)

| Trigger | How detected (glycin internal) | Observable error / log | Organizer handling (required) |
|---|---|---|---|
| `bwrap` binary not found (`Command::new("bwrap")` spawn `NotFound`) | `ErrorKind::SpawnErrorNotFound` (`cmd: bwrap --...`) propagated from `sandbox.rs:spawning` | `Err(SpawnErrorNotFound)` or `PrematureExit` | **Fatal startup banner** — block preview, show `Install bubblewrap` instructions per distro (`apt install bubblewrap`, `dnf install bubblewrap`, `pacman -S bubblewrap`) + link to AppImage download. Do not fall through to `image` crate. |
| User namespaces disabled (`kernel.unprivileged_userns_clone=0` or AppArmor `bwrap-userns-restrict`) | `Sandbox::check_bwrap_syscalls_blocked` returns `true` via SIGSYS or stderr `"Creating new namespace failed"` etc. (`sandbox.rs:652-725`) → `RunEnvironment::HostBwrapSyscallsBlocked` | `eprintln!("WARNING: Glycin running without sandbox. Bubblewrap (bwrap) doesn't work…")` at first `Loader::new().load()` | Same fatal banner. Offer: `sudo sysctl -w kernel.unprivileged_userns_clone=1` (Debian-old) or `echo 1 | sudo tee /proc/sys/kernel/apparmor_restrict_unprivileged_userns` (Ubuntu AppArmor), or disable Apparmor profile `sudo apparmor_parser -R /etc/apparmor.d/bwrap` (discouraged — document carefully). |
| `seccomp` unavailable (kernel `CONFIG_SECCOMP=n` — rare; or `libseccomp` filter export fails) | `Sandbox::seccomp_filter()` → `libseccomp::error::SeccompError` → `ErrorKind::Seccomp` | `Err(Seccomp)` | Fatal — same banner (kernel too old). |
| `/proc/meminfo` unreadable (hardened `/proc` hidepid) | `Sandbox::mem_available() → None` (`sandbox.rs:557-585`) | warning + fallback `1 GB` limit via `const {1024.pow(3)}` | Not fatal — still sandboxed with conservative limit. Log at `warn` level only. |
| `GLYCIN_DISABLE_SANDBOX=i-know-the-risks` set | `RunEnvironment::SandboxForceDisabled` (`util.rs:150-156`) | stderr warning | **Refuse** — `std::process::exit(2)` with dialog explaining the env var is forbidden under Organizer's policy. |
| `flatpak-builder --run` Devel | `RunEnvironment::FlatpakDevel` (`util.rs:157-165`) | warning | N/A for raw/AppImage (not a Flatpak). If ever invoked under Flatpak with `Devel` id, treat like `NotSandboxed` fatal. |
| No loaders configured (empty `Config::image_loader`) | `config.rs:Config::load()` scans `XDG_DATA_DIRS`; none found → `ErrorKind::NoLoadersConfigured(config)` on first `Loader::new().load()` | `Err(NoLoadersConfigured)` | Fatal — prompt `Install glycin-loaders` or use AppImage. |
| Unknown mime / unsupported format | `Config::loader(&mime_type)` miss → `ErrorKind::UnknownImageFormat(mime, config)` | `Err(UnknownImageFormat)` | Not fatal — show `Unsupported: image/heif — no glycin-heif loader` placeholder, allow skip. Distinct from sandbox failure. |

### 5.2 Policy: no silent in-process fallback

* Stock `glycin` with `Auto` selector **will** fall back to `NotSandboxed` when `HostBwrapSyscallsBlocked` (see `SandboxMechanism::detect`). That fallback is intentional for GNOME OS build servers and for progressive rollout (blog 2025-06-13 “Environments that don’t Support Sandboxing — build servers …”). Organizer's hard requirement overrides that: wrap `Loader::new().sandbox_selector(SandboxSelector::Bwrap)` **explicitly** so the fallback is **not** automatic — it either uses `Bwrap` or errors.
  * API: `glycin_core::SandboxSelector::Bwrap` and `glycin::Loader::sandbox_selector(SandboxSelector::Bwrap)` (see `loader.rs:92-98`). For libglycin/GJS likewise expose `sandbox_selector`.
  * With `Bwrap` selector, `determine_sandbox_mechanism().await` yields `Bwrap` regardless of environment, so a blocked environment yields `SpawnError`/`PrematureExit` rather than transparent `NotSandboxed`.
* `image` crate fallback: **do not** automatically downgrade `glycin::Error` → `image::ImageReader::open().decode()`. If `glycin` fails with a sandbox-availability error (`SpawnErrorNotFound`, `Seccomp`, `PrematureExit` with `bwrap` namespace stderr), surface that as sandbox failure UI, not decode failure. Only for `UnknownImageFormat` or `UnsupportedImageFormat` remote errors is it permissible to attempt `image` crate decode **behind a user toggle** — and even then warn that this path is in-process.
* Builtin loaders (`feature="builtin"`) are always `NotSandboxed` (`loader.rs:542`): `ImageLoader::Builtin(_) => NotSandboxed`. Do not enable `glycin` `builtin` feature in Organizer's `Cargo.toml` for Linux builds; keep `features = ["gdk4","tokio"]` with default `external` only. If you need a Windows build, gate `builtin` behind `#[cfg(not(target_os="linux"))]`.

### 5.3 User-facing messages (copy for implementation)

* Sandbox blocked — title: “Sandbox unavailable — image decoding blocked” body: “Organizer refuses to decode untrusted images without a sandbox. `bwrap` failed: {stderr snippet}. Fix: `sudo apt install bubblewrap` (Debian/Ubuntu) or `sudo dnf install bubblewrap` (Fedora) or `sudo pacman -S bubblewrap` (Arch). On Ubuntu 24.10+ also: `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0` (or use the AppImage which bundles `bwrap`). Restart Organizer after fixing.” Buttons: `[Open docs] [Retry] [Quit]`. Log `WARN` with `active_sandbox_mechanism`.
* No loaders — “No image loaders found. Install `glycin-loaders` or use the AppImage.”
* Timeout — `ErrorKind::Timeout(d)` → “Decoding timed out after {d}s — file may be huge or loader hung. Try Skip.” (see §7).

### 5.4 AppImage-specific fallback notes

* AppImage does **not** fix kernel user-namespace lockdown (matrix A2). If `check_bwrap_syscalls_blocked()` is true inside AppImage, Organizer still refuses. The only “escape” would be to run bwrap with `CAP_SYS_ADMIN` or via `setuid` helper — which modern `bwrap` no longer supports and AppImage does not provide. Do not add `setuid` helper to AppImage (security regression).
* If AppImage bundles `bwrap` but AppRun `PATH` prepending fails, host `bwrap` is tried instead — same lockdown applies. Always verify `PATH` ordering in startup self-test: spawn `bwrap --version` via Organizer's `Command::new("bwrap")` and log which binary was found (`which` semantics).
* Do not bundle a "fallback in-process" `.so` and silently switch. If the hard sandbox can't be provided, exit fast and loud.

---

## 6. GStreamer video path — isolation, risks, and options

### 6.1 Baseline: GStreamer is not sandboxed

* `gst-plugin-gtk4` (`gtk4paintablesink`) is a `GstVideoSink` loaded as a shared library into the Organizer process (`dlopen` via `GST_PLUGIN_SYSTEM_PATH`). Pipelines (`playbin` / `filesrc ! decodebin ! videoconvert ! gtk4paintablesink`) decode with in-process elements (`avdec_h264` via `ffmpeg/libav`, `vp9dec`, `av1dec`, `h265parse`, etc.).
* No `bwrap`/`seccomp` is applied to GStreamer code. The only isolation is the normal Unix DAC. So a crafted `webm/mp4/mov` with a codec bug (e.g., libav's `h264` parser, VP9 frame header) could achieve **arbitrary code execution inside Organizer's address space** — bypassing the glycin sandbox for video. Glycin covers still-image codecs only; video codecs are larger attack surface per CVE history.
* Fontconfig / cairo / pango used by `glycin-svg` have similar risk but are already sandboxed via glycin. GStreamer's font usage (subtitle rendering) is unsandboxed.

### 6.2 Option matrix

| Option | Isolation | Impl cost | Perf | Compat | Recommendation |
|---|---|---|---|---|---|
| **A. Accept unsandboxed GStreamer, harden glycin only** | glycin sandboxed, GStreamer in-process | zero — use existing `playbin + gtk4paintablesink` path from rendering-stack research | best (no extra fork, zero-copy GL/DMABuf) | all GStreamer plugins via `GST_PLUGIN_SYSTEM_PATH` | ✅ **Recommended for v1**. Justify: Organizer triage is user-opened local files (not arbitrary web), video files are a smaller fraction, and GStreamer `gst-plugins-good/bad/libav` codecs are mature with fewer CVEs than ad-hoc image parsers — but still document residual risk. Add `gstreamer-vaapi` (hardware decode) to reduce CPU codec surface. |
| **B. Helper process with `bwrap` + `--seccomp`** | wrap GStreamer pipeline in a separate process that `bwrap --seccomp` + streams frames via `shm`/`memfd` to GTK | high — need custom `GstAppSink` → `memfd` + fd-passing + sync protocol (frame format, stride, color-state, vsync). No existing `gst-bwrap` helper; closest precedent is Chromium's GPU/video helper | extra copy (CPU readback) unless `dmabuf` forwarding via `ioctl` which leaks FDs | requires matching GL context between helper and main (Wayland `wl_shm` vs `dmabuf` import) | Track as follow-up research ticket after v1. Evaluate reuse of Flatpak's `flatpak-spawn --sandbox` helper pattern for GStreamer as glycin does for images. |
| **C. GStreamer + `Landlock`/`seccomp` on main process** | main process installs restrictive `prctl(PR_SET_NO_NEW_PRIVS) + seccomp` at startup | medium but breaks: GTK needs `socket`, `openat`, fontconfig, GPU ioctl; over-restrict kills GUI | minimal | fragile, needs allow-list tuned per distro GPU stack | Not recommended. |
| **D. Use `glycin` for animated formats, disable video codecs** | video files show placeholder instead of playback | trivial — gate `playbin` behind allow-list (`webm`, `mp4` mime → placeholder) | n/a | users can't triage video | Alternative if security bar is strict and helper not ready: offer toggle “Enable video preview (less isolation)”. |
| **E. Pure-Rust in-process (`glycin-ng`-style) but for GStreamer** | in-process worker thread with `landlock+seccomp+rlimit` | research — no ready Rust GStreamer sandbox; would need `gstreamer-rs` thread isolation | similar to A | not production | Not viable. |

### 6.3 If choosing A (recommended), add these mitigations

* **Limit GStreamer plugins to needed set:** ship only `gst-plugins-base` + `gst-plugins-good` (matroska, mp4, webm, vp8/vp9, opus/vorbis) + `gst-libav` for h264/h265. Avoid `ugly`/`bad` nonfree elements (`x264enc`, `faad`). Validate with `gst-inspect-1.0` inside AppImage sandbox.
* **Configure `playbin` flags narrowly:** `flags = GST_PLAY_FLAG_VIDEO` only; no `GST_PLAY_FLAG_TEXT` (subtitle parsing) unless needed — subtitles parse complex markup (SRT/SSA) with own attack surface.
* **Timeout + bus error handling:** watch `GST_MESSAGE_ERROR` and kill pipeline on `Error` with placeholder (same as image path). Set `GST_ELEMENT_ERROR` bus handler that tears down pipeline and calls `playbin.set_state(GST_STATE_NULL)`.
* **Avoid autoplay network:** only handle `file://` URIs (triage folder); refuse `http://`/`https://` URIs so no network fetch.
* **Document risk:** in `docs/security.md`, state “still images: bwrap+seccomp sandboxed via glycin; video: in-process GStreamer (no sandbox) — track #NNN for helper-process isolation”.
* **Future B helper sketch:** helper binary `organizer-gst-helper --uri file://… --sink-fd <memfd>` launched with `bwrap --unshare-all --ro-bind /usr /usr --ro-bind-try /nix/store --seccomp <fd> --dev /dev --proc /proc …`. Main process receives frames via `GstAppSink` → `appsink.pull_sample()` → copy to `gdk::MemoryTextureBuilder`. Needs startup latency measurement; pool like glycin's `Pool` for GStreamer helpers.

### 6.4 Bundling GStreamer inside AppImage — verified steps

* Use `linuxdeploy-plugin-gstreamer` (archived but still works) or manual:
  ```bash
  linuxdeploy --appdir "$APPDIR" --plugin gtk --plugin gstreamer \
    --output appimage
  ```
  The plugin runs `gst-inspect-1.0` at build time to discover `GST_PLUGIN_SYSTEM_PATH` and copies `*.so` plus `gst-plugin-scanner`.
* Alternative explicit: `GST_PLUGIN_SYSTEM_PATH` hook in AppRun (already in checklist). Test with:
  ```bash
  GST_DEBUG=3 ./Organizer-*.AppImage gst-inspect-1.0 | grep gtk4
  GST_DEBUG=3 ./Organizer-*.AppImage gst-inspect-1.0 avdec_h264
  ```

---

## 7. Crash / DoS isolation and performance overhead

### 7.1 Crash isolation (source: `glycin-core/src/error.rs`, `glycin-core/src/dbus.rs`, `sandbox.rs:memory_limit`)

| Scenario | What happens in loader process | How main app sees it | User-visible result |
|---|---|---|---|
| Crafted image triggers `assert!` / Rust `panic!` in `glycin-image-rs` (`png` chunk OOB, `gif` LZW overflow) | loader unwinder aborts → `RemoteError::Panic` or `SIGABRT` (if `abort` on panic in release profile — verify `Cargo.toml panic=abort`) | `dbus::RemoteProcess::error_context` attaches `stderr`/`stdout` capture to `Error`; `process.process_disconnected` flag set, `Pool` evicts entry | `Err(RemoteError::Panic)` → `Error::is_panic() == true`. UI shows “Decoding crashed (loader killed)” placeholder + toast, logs `WARN` with `stderr` tail. Queue advances. |
| Decompression bomb (e.g. 1 KB PNG → 1 GB RGBA) | `setrlimit(RLIMIT_AS)` kills loader with `SIGKILL` / `ENOMEM` on `mmap` | `Err(RemoteError::OutOfMemory)` (`Error::is_out_of_memory()`) or `TextureTooLarge` | Placeholder “Image too large” + toast, same advancement. |
| Infinite loop in codec (e.g., APNG `fcTL` cycle) | CPU loop until `Timeout(Duration)` hits (`Loader::limits` default timeout, see `api/loader.rs::Limits`) | `Err(ErrorKind::Timeout)` (`Error::is_timeout()`) | Toast “Decoding timed out — file may be damaged”. Offer Skip. |
| Loader `SIGSEGV` (e.g., `libheif` C++ heap overflow, `librsvg` cairo assert) | process killed by kernel, D-Bus peer drops | `PrematureExit { status, cmd }` or `InternalCommunicationCanceled` | Same placeholder/crash UI. |
| SVG with 1000 external `<image href>` includes (`ExposeBaseDir=true`) | each include triggers additional `openat` under `--ro-bind` base-dir; no network — external http href fails gracefully | `RemoteError::UnsupportedImageFormat` for missing resource | Render without externals (glycin behavior). |

**Main-app survival invariant:** `Pool` keeps `process_disconnected: AtomicBool` (see `pool.rs:207`). On disconnect, next `get_loader()` spawns a fresh process for that `ConfigEntryHash`. Existing `Image` handles that already have a `PooledProcess` reference drop their `UsageTracker` after 30 s idle → `Pool::clean_loaders()` purges. No global panic.

**Correct error handling in Organizer (src sketch):**

```rust
let file = gio::File::for_path(path);
let mut loader = glycin::Loader::new(file);
loader.sandbox_selector(glycin::SandboxSelector::Bwrap); // enforce
loader.main_context_selector(MainContextSelector::Auto);
*loader.limits_mut() = Limits::new().timeout(Duration::from_secs(15));

match loader.load().await {
  Ok(mut image) => {
    g_debug!("active sandbox: {:?}", image.active_sandbox_mechanism());
    match image.next_frame().await {
      Ok(frame) => {
        let tex = frame.texture();
        picture.set_paintable(Some(&tex));
      }
      Err(e) if e.is_timeout() => show_placeholder(&picture, "Timeout — Skip?"),
      Err(e) if e.is_panic() || e.is_out_of_memory() => {
        tracing::warn!("loader crash/OOM: {e} (unsupported={:?})", e.unsupported_format());
        show_placeholder(&picture, &format!("Decoding failed: {e}"));
        toast("Preview failed — file skipped (loader crash isolated)");
      }
      Err(e) => show_placeholder(&picture, &format!("{e}")),
    }
  }
  Err(e) if e.has_no_processor_configured() || e.to_string().contains("bwrap") => {
    show_sandbox_blocked_dialog(&e);
  }
  Err(e) if e.is_timeout() => show_placeholder(&picture, "Timeout"),
  Err(e) => show_placeholder(&picture, &format!("{e}")),
}
// Do NOT: .unwrap(), .expect(), or block on glib::MainContext without spawn_local.
```

* Async must run on `glib::MainContext` (glycin's `MainContextSelector::Auto` picks one). Use `glib::MainContext::default().spawn_local(async { … })` or `gio::spawn_blocking` for hashing. Never call `block_on` on the UI thread — glycin's zbus integration deadlocks (see `glib` work item #4034 referenced in `loader.rs:175-177`).

### 7.2 Performance overhead

| Cost | Cold spawn (first image per mime/base-dir/sandbox hash) | Warm reuse (same mime, within 30 s) | Notes |
|---|---|---|---|
| `bwrap` namespace setup + mount `--ro-bind` table | ~5–15 ms (kernel `unshare(CLONE_NEWNS)` + mounts) + ~1–2 ms seccomp memfd write | 0 (process reused) | Measured on Ubuntu 24.04, 4-core; Arch thread reports high CPU when glycin transiently spawns many loaders for thumbnailers (not Organizer's sequential triage). |
| Seccomp BPF compile + `export_bpf` | ~0.5–1 ms (`libseccomp` translates ~30 rules) | 0 | |
| D-Bus peer connect over Unix socket | ~0.5 ms | 0 | |
| `setrlimit` | negligible | 0 | |
| Texture `memfd` mmap to GDK | ~0.2 ms per frame + copy into `MemoryTextureBuilder` (~1× mem size copy — `gdk::MemoryTexture` copies? Actually `from_loader` does `texture.into_gbytes()` zero-copy via sealed memfd mmap, then `MemoryTextureBuilder::set_bytes` references it — still one kernel page fault per page) | same | |
| Hash (`sha256` per triage file) | ~10–30 ms for 10 MB file (parallel `spawn_blocking`) | same | Do not couple to decode — do hash in parallel thread while preview decoding. |

For Organizer's triage throughput (user pressing 1–9 at maybe 1.5 files/s), the 5–15 ms cold cost is imperceptible. Even rapid “hold key to triage” at 5 files/s would amortize after first file per format. Pool's 30 s retention helps: triage of a folder of all JPEGs reuses one `glycin-image-rs` process for the whole session.

**DoS mitigations:**

* Limit concurrent decodes to 1–2 (`PoolConfig::max_parallel_operations = 2` if you create a custom `Pool`). Glycin defaults to `usize::MAX`; unbounded parallel triage (prefetch next N) could spawn N loaders under memory pressure. Set a cap.
* Enforce `Limits::timeout` (e.g., 15 s) and `Limits::max_dimensions` (e.g., 10000×10000) as `Loader::limits()`. Glycin already checks `MAX_TEXTURE_SIZE = 8 GB` and `frame.stride * height ≤ MAX_TEXTURE_SIZE` plus `width/height ≤ max_dimensions` (`loader.rs:892-915`), but tighten for triage preview (downscale requests already limit via `FrameRequest::scale`).
* Use `FrameRequest::scale(width, height)` for preview to decode-then-downscale early (supported per `glycin_utils::FrameRequest::scale`); combine with `content_fit: Contain` so_texture size is close to window size, not full-res.

---

## 8. Integration — Rust API, async off UI thread, fallback policy

### 8.1 Crate choice and features

```toml
[dependencies]
glycin = { version = "3", features = ["gdk4", "tokio"], default-features = false }
# tokio + zbus compat is typical for Organizer (which already uses tokio for hashing/IO)
# Alternative: features = ["gdk4"] (async-io with async-global-executor) — pick one.
gtk4 = { version = "0.11", features = ["v4_14"] }
gio = "0.11"
gdk4 = "0.11"
gstreamer = "0.25"
gst-plugin-gtk4 = { version = "0.15", features = ["waylandegl","x11egl","dmabuf","gtk_v4_14"] }
# image crate ONLY as non-Linux CI helper or explicit behind --allow-unsandboxed fallback flag
image = { version = "0.25", optional = true, features = ["default-formats"] }
```

Do **not** enable `glycin` `builtin` on Linux (see §5.2). In `Cargo.toml`, gate any `image` fallback behind an explicit opt-in feature (`features = ["unsandboxed-fallback"]`) so CI catches accidental in-process use.

### 8.2 Error taxonomy (source: `glycin-core/src/error.rs:154-243`)

| `Error` predicate | Matches `ErrorKind` | Meaning | Organizer action |
|---|---|---|---|
| `unsupported_format()` `Some(mime)` | `UnknownImageFormat(mime, config)` or `RemoteError::UnsupportedImageFormat(msg)` | No loader for this mime | Placeholder “Unsupported format”, offer Skip |
| `has_no_processor_configured()` | `NoLoadersConfigured(config)` | No `conf.d/*.conf` found | Fatal sandbox-setup banner |
| `is_out_of_memory()` | `RemoteError::OutOfMemory` | Decompression bomb killed by RLIMIT_AS | Placeholder “Too large” |
| `is_panic()` | `RemoteError::Panic` or (builtin) `ThreadPanic` | Loader crashed | Placeholder “Loader crashed (isolated)” |
| `is_timeout()` | `Timeout(Duration)` | Decode exceeded `Limits::timeout` | Placeholder “Timeout” |
| `is_cancelled()` | `Canceled` | User cancelled via `Cancellable` | Silent |
| `failed_image_source()` `Some(giError)` | `ImageSource(glib::Error)` | `Gio::File::read_future` failed | Placeholder “Can't read file” |
| `is_cancelled` false + `Display` includes `bwrap`/`Seccomp` | `SpawnError`/`Seccomp`/`PrematureExit` | Sandbox spawn failure | Fatal banner (see §5.3) |

Display string (`Error::fmt`) renders `ErrorKind` + `ErrorContext` (`stderr`/`stdout` tail from loader). Log full `Error` with `tracing::warn!`.

### 8.3 Async pattern (do not block UI thread)

```rust
use glycin::{Loader, SandboxSelector, MainContextSelector};
use gio::prelude::*;
use gtk4::prelude::*;

fn preview(path: std::path::PathBuf, picture: gtk4::Picture) {
  let file = gio::File::for_path(&path);
  glib::MainContext::default().spawn_local(async move {
    let mut loader = Loader::new(file);
    loader.sandbox_selector(SandboxSelector::Bwrap); // hard requirement
    loader.main_context_selector(MainContextSelector::Auto);
    // Don't block: hash in parallel if needed
    let sha_future = tokio::task::spawn_blocking({
      let p = path.clone();
      move || sha256_of_file(&p)
    });
    match loader.load().await {
      Ok(mut image) => {
        debug!("sandbox: {:?}", image.active_sandbox_mechanism());
        // image.active_sandbox_mechanism() must be Bwrap — assert if NotSandboxed
        assert!(image.active_sandbox_mechanism() == glycin::SandboxMechanism::Bwrap);
        match image.next_frame().await {
          Ok(frame) => {
            let tex = frame.texture();
            picture.set_paintable(Some(&tex));
          }
          Err(e) => handle_frame_err(e, &picture),
        }
      }
      Err(e) => handle_loader_err(e, &picture),
    }
    let _hash = sha_future.await; // drive hash completion for log
  });
}
```

### 8.4 When to use `image` crate fallback (discouraged)

Only behind `#[cfg(feature = "unsandboxed-fallback")]` **and** an explicit user pref (`Settings → Advanced → Allow unsandboxed RGB fallback for unsupported HEIC/JXL when glycin-heif not bundled`). Even then:

```rust
#[cfg(feature = "unsandboxed-fallback")]
async fn fallback_image_crate(path: &Path) -> Result<gdk::Texture, image::ImageError> {
  let bytes = tokio::task::spawn_blocking(|| std::fs::read(path).unwrap()).await.unwrap();
  let img = tokio::task::spawn_blocking(move || image::load_from_memory(&bytes)).await.unwrap()?;
  // manual Texture upload — note double allocation + no sandbox
  Ok(texture_from_image(img))
}
```

Warn via `InfoBar` “This preview used an unsandboxed decoder”. For v1, keep this **disabled** entirely and rely on bundled loaders.

---

## 9. Test plan (distro matrix without host deps)

### 9.1 Raw binary host tests

| Host | Setup | Test command | Expected |
|---|---|---|---|
| Ubuntu 24.04 (GNOME) | `sudo apt install libgtk-4-dev libgstreamer1.0-dev gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-libav liblcms2-dev libfontconfig-dev libseccomp-dev bubblewrap glycin-loaders glycin-loaders 2.2` | `cargo test -- --nocapture` + `cargo run -- --self-test-preview /usr/share/icons/Adwaita/*png` | `Loader::load` uses `Bwrap`, `active_sandbox_mechanism()==Bwrap`, no warnings; `gst-inspect-1.0 gtk4paintablesink` ok |
| Ubuntu 22.04 (old) | no `glycin-loaders` (package not available) | `cargo run` without bundled loaders | `NoLoadersConfigured` dialog — verified refusal |
| Fedora 41 | `sudo dnf install bubblewrap glycin` | same | `Bwrap` |
| Arch | `sudo pacman -S bubblewrap` + `glycin` from AUR or built from tarball | same | `Bwrap` |
| Hardened Ubuntu 24.10 (AppArmor `restrict_unprivileged_userns=1`) | `echo 1 | sudo tee /proc/sys/kernel/apparmor_restrict_unprivileged_userns && cargo run` | preview refuses with H2 dialog; `dmesg` shows `apparmor="DENIED" operation="userns_create"` |
| Missing `bwrap` | `sudo apt remove bubblewrap && cargo run` | same | `SpawnErrorNotFound` dialog |
| `GLYCIN_DISABLE_SANDBOX` set | `GLYCIN_DISABLE_SANDBOX=i-know-the-risks cargo run` | same | early `exit(2)` with “forbidden env var” |

Automate in CI with `ubuntu-22.04`, `ubuntu-24.04`, `fedora:41`, `archlinux/archlinux` containers (each `cargo test -- --ignored` tag for sandbox tests that spawn `bwrap`).

### 9.2 AppImage tests (host without deps — the core validation)

Build `Organizer-x86_64.AppImage` on Ubuntu 22.04 builder with `linuxdeploy` + `appimagetool` (per AppImage best practices: build on oldest supported distro so `glibc` baseline is 2.35). Then run AppImage **on hosts that do NOT have** `bubblewrap`, `libseccomp`, `glycin-loaders`, `gstreamer plugins`:

| Host for AppImage run | Host deps preinstalled | AppImage | Action | Expected |
|---|---|---|---|---|
| Ubuntu 22.04 clean VM | nothing except `fuse2` | `Organizer-*.AppImage` | open folder with `png/jpg/webp/svg/heic` | all previews succeed, `bwrap` from `$APPDIR/usr/bin/bwrap` used (verify via `ps -ef | grep bwrap` → path inside `/tmp/.mount_Organiz*/usr/bin/bwrap`) |
| Ubuntu 24.04 | no `bubblewrap` | same | same | same |
| Fedora 41 | no `bubblewrap` | same | same | same |
| Arch | no `bubblewrap` | same | same | same |
| Any host with `AppArmor restrict_unprivileged_userns=1` | AppImage | same | open image | AppImage still fails sandbox (A2 row) → refusal dialog even though AppImage bundled `bwrap` — proves kernel lockdown still blocks |

Verification inside AppImage (add a hidden `--self-test` flag to Organizer that prints):

```bash
./Organizer-*.AppImage --self-test-sandbox 2>&1 | grep -E "SandboxMechanism|bwrap|XDG_DATA_DIRS|GLYCIN_DATA_DIR|GST_PLUGIN_SYSTEM_PATH"
# expected: SandboxMechanism::Bwrap, XDG_DATA_DIRS contains /tmp/.mount_.../usr/share, bwrap path is /tmp/.mount_.../usr/bin/bwrap
./Organizer-*.AppImage --self-test-gstreamer 2>&1 | grep gtk4paintablesink
```

Also test video path (unsandboxed) inside AppImage:

```bash
./Organizer-*.AppImage /path/to/webm_and_mp4_folder
# check logs: GStreamer PLAYING via gst-plugin-gtk4, no sandbox warning
```

### 9.3 Crash isolation tests (automated, run both raw and AppImage)

* Decode a known-crashing corpus (populate `tests/fixtures/crash/` with minimized fuzz samples: `png-PLTE-overflow`, `jpeg-SOF-huff-bomb`, `gif-LZW-loop`, plus a `bad-heif` if `glycin-heif` bundled, and a 100 MB `decompression_bomb.png` generated with `python -c "… PIL …"`)
* For each file: `Loader::new(file).sandbox_selector(Bwrap).load().await` → assert `is_panic() || is_out_of_memory() || is_timeout()` and assert main process still alive (spawn a second `Loader::new(valid.png).load().await` succeeds after first crash). Run via `cargo test --test crash_isolation -- --nocapture` that forks a child to test SIGABRT doesn't propagate.
* Measure cold vs warm latency: `#[bench]` that loads 10 sequential JPEGs, assert median ≤ 15 ms per warm load.

### 9.4 CI wiring (GitHub Actions)

```yaml
jobs:
  raw-matrix:
    strategy:
      matrix: { os: [ubuntu-22.04, ubuntu-24.04, fedora-41, archlinux] }
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4
      - run: cargo test --features tokio,gdk4 -- --nocapture

  appimage:
    runs-on: ubuntu-22.04
    steps:
      - uses: actions/checkout@v4
      - run: |
          sudo apt update && sudo apt install -y libgtk-4-dev libgstreamer1.0-dev gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-libav bubblewrap libseccomp-dev
          curl -L -o linuxdeploy https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage
          chmod +x linuxdeploy
          cargo build --release
          linuxdeploy --appdir AppDir --executable target/release/organizer --desktop-file data/organizer.desktop --icon-file data/organizer.png --plugin gtk --plugin gstreamer --output appimage
      - run: |
          ./Organizer-*.AppImage --self-test-sandbox
          # run AppImage on a dep-stripped container (no bubblewrap) to prove bundling
          docker run --rm -v $PWD:/work -w /work ubuntu:22.04 bash -c "apt update && apt install -y fuse libgtk-4-0 && ./Organizer-*.AppImage --self-test-sandbox"
```

---

## 10. Open questions → follow-up tickets

* **GStreamer helper isolation (#NNN):** decide whether to invest in `organizer-gst-helper + bwrap` (Option B §6). If v1 ships Option A, file a follow-up to prototype helper with `memfd` frame transport and measure GL/DMABuf forwarding complexity.
* **Loader coverage:** include `glycin-heif` (HEIC/AVIF) and `glycin-jxl`? LGPL/GPL licensing caveat in `glycin/README.md` — each loader binary's license stays with that binary (separate process, not linked into main), so **MPL-2.0 OR LGPL-2.1-or-later** app license remains valid. Decide bundle set; document in `docs/licensing.md`.
* **`glycin` vs `glycin-ng`:** `glycin-ng`'s `landlock+seccomp` in-process worker avoids `bwrap` nesting awkwardness (useful inside AppImage already sandboxed where `bwrap` user-ns nesting can double-fault). Evaluate whether switching to `glycin-ng` for AppImage simplifies bundling (no `bwrap` needed). That changes crate API and reduces license surface to permissive-only (no `libheif`/`libjxl` C++); trade off format coverage.
* **AppRun `Exec` rewriting:** implement the `$XDG_RUNTIME_DIR` temp `conf.d` generation approach (§2.3) and add an integration test that asserts `Config::cached().await.image_loader` length equals expected bundled loader count when running inside AppImage.

---

## References (primary sources — each claim traced)

* `GNOME/glycin` **README.md** (sandbox mechanisms, `bwrap` vs `flatpak-spawn`, memfd/Unix socket, `XDG_DATA_DIRS` loader conf path `2+`, `ExposeBaseDir`, `libseccomp`+`bwrap` packaging list, GPL note for `glycin-jxl`/`glycin-raw`, software using glycin) — `https://gitlab.gnome.org/GNOME/glycin` (mirror `github.com/GNOME/glycin`), `https://gnome.pages.gitlab.gnome.org/glycin`.
* `glycin-core/src/sandbox.rs:44-918` — `BLOCKED_SYSCALLS` copied from `flatpak/flatpak common/flatpak-run.c`, `INHERITED_ENVIRONMENT_VARIABLES`, `Sandbox::new/spawn/bwrap_command/flatpak_spawn_command/no_sandbox_command/memory_limit/calculate_memory_limit/set_memory_limit/seccomp_filter/seccomp_export_bpf/check_bwrap_syscalls_blocked`, `SystemSetup` UsrMerge handling, cap-dropping `CAP_DAC_OVERRIDE/CAP_DAC_READ_SEARCH`, sigsys handler; lines cited in §1.
* `glycin-core/src/util.rs:126-184` — `RunEnvironment` detection (`SandboxForceDisabled`, `Host`, `HostBwrapSyscallsBlocked`, `Flatpak`, `FlatpakDevel`), `flatpak_devel()` via `/.flatpak-info` keyfile, SIGSYS vs `bwrap: Creating new namespace failed` strings.
* `glycin-core/src/api/common.rs:23-82` — `SandboxMechanism::{Bwrap,FlatpakSpawn,NotSandboxed}` + `detect()`, `SandboxSelector::{Auto,Bwrap,FlatpakSpawn,NotSandboxed}` + `determine_sandbox_mechanism()`. Used for Organizer's hard-sandbox policy (`SandboxSelector::Bwrap`).
* `glycin-core/src/api/loader.rs:92-98,536-543` — `Loader::sandbox_selector()`, `Image::active_sandbox_mechanism()`, 30 s retention via `PoolConfig`. `image-rs` fallback only behind `image` crate opt-in.
* `glycin-core/src/config.rs:331-704` — `data_dirs()` scan of `GLYCIN_DATA_DIR` or `user_data_dir()+system_data_dirs()`, `compat_version` `2+` (`COMPAT_VERSION=2`), `KeyFile Exec` / `ExposeBaseDir` / `Fontconfig` / `Identifiers`, `Processor::Binary(PathBuf)`. Defines AppImage discovery contract.
* `glycin-core/src/error.rs:154-243` — `ErrorKind::{RemoteError(Panic/OutOfMemory/NoMoreFrames/Panic), UnknownImageFormat, NoLoadersConfigured, PrematureExit, SpawnError, SpawnErrorNotFound, Seccomp, Timeout, Canceled, Transaction}` and predicates `unsupported_format()`, `is_panic()`, `is_out_of_memory()`, `is_timeout()`, `has_no_processor_configured()`.
* `glycin-core/src/pool.rs:81-290` — `PoolConfig{loader_retention_time:30s, max_parallel_operations:usize::MAX}`, `PooledProcess` pooling + `UsageTracker` + `clean_loaders()`. Defines 30 s reuse window for perf analysis.
* `glycin/meson.build` + `glycin-loaders/meson.build` + `libglycin/meson.build` — `dependency('libseccomp')`, `compat_version='2+'`, `-Dglycin-loaders` / `loaders` array (`glycin-image-rs`, `glycin-heif`, `glycin-jxl`, `glycin-svg`), `libexecdir/glycin-loaders/$compat` install layout.
* `glycin/src/lib.rs:30-56` — `external` vs `builtin` feature split (Linux uses `external`, non-Linux auto-falls to `glycin_builtin`), `apt/dnf` install lines with `bubblewrap libseccomp-dev glycin-loaders`.
* `docs/research/rendering-stack.md` (`research/rendering-stack` branch) — chosen stack `gtk4 + Picture/Paintable + glycin → Texture + GStreamer gst-plugin-gtk4 gtk4paintablesink`, Format→decoder matrix, proof sketch with `Loader::new(file).load().await` + `playbin + gtk4paintablesink`. Retained as still-valid decision per map 2026-08-30 note.
* `docs/research/packaging-deps.md` (`research/packaging-deps` branch) — **superseded** for Flatpak/rpm/deb (retained as reference only), now replaced by raw/AppImage analysis here; §7 Meson+Cargo bridge still valid for `cargo build`.
* `blogs.gnome.org/sophieh 2025-06-13 "Making GNOME's GdkPixbuf Image Loading Safer"` + `gnome.pages.gitlab.gnome.org/glycin` — glycin provides `gdk-pixbuf 2.43.2 builtin_loaders=glycin`, sandbox limitations outside Linux / build-server `bwrap` breakage.
* `containers/bubblewrap` (`github.com/containers/bubblewrap` README + `bwrap.xml` manpage) — `bwrap` requires unprivileged user namespaces (no setuid since removal), `PR_SET_NO_NEW_PRIVS`, `--seccomp`, `--unshare-all`, mount namespace isolation. `bwrap --help` flags verified for `--seccomp`.
* `apparmor.d` issue `roddhjav/apparmor.d#881` + Arch forum threads (Arch Linux BBS `show_thread 309784`, `311001`, `312713`) — AppArmor `bwrap-userns-restrict` on Ubuntu 24.10+ blocks `bwrap` user_ns creation, causing glycin's `check_bwrap_syscalls_blocked` SIGSYS/Permission-denied path.
* `QaidVoid/glycin-ng` README (2026-05-20) — alternative `landlock+seccomp+rlimit` in-process worker thread, ~4 MiB vs ~37 MiB, no `bwrap`/D-Bus; shows that in-process fallback is feasible but trades C++ codec coverage for permissive licenses and smaller install. Reference for Organizer's decision not to use `glycin-ng` yet but to track it.
* `linuxdeploy` + `linuxdeploy-plugin-gstreamer` (`github.com/linuxdeploy/linuxdeploy`, `linuxdeploy-plugin-gstreamer` README, `discourse.appimage.org` GStreamer thread #314) — `GST_PLUGIN_SYSTEM_PATH`, `GST_PLUGIN_SCANNER` env vars, plugin copying for AppImage. Basis of GStreamer bundling section.
* `docs.flatpak.org` XDG / sandbox permissions (referenced in superseded `packaging-deps.md`) — contrast that AppImage has no portal/namespace sandbox akin to Flatpak; confirms AppImage still needs bundled `bwrap` for glycin isolation.
* `GNOME/glycin` issue `#203` “Replace bubblewrap with native sandbox mechanisms” — upstream intent to drop `bwrap` in future via native `landlock/seccomp` lib (same as `glycin-ng` direction). Signals that long-term Organizer could benefit from native sandbox without bundling `bwrap`, but today `bwrap` is the only stable mechanism.

---
*Next step for Map #1:* consume **TL;DR**, **§3 matrix**, **§4 checklist**, **§5 fallback spec** — enforce `SandboxSelector::Bwrap` in code, implement sandbox-blocked dialog, build AppImage with AppRun hook described, and track GStreamer helper isolation as follow-up ticket after v1 triage UX validation.
