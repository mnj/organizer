//! Distribution seam for spec #21 (AppImage + raw executable + CI smoke).
//!
//! External behavior only: what ships on disk (AppDir layout recipe,
//! AppRun env, desktop file, docs one-liners, CI workflow). No GTK/GStreamer
//! internals — a refactor of `preview`/`video` must not break these tests.

use std::path::PathBuf;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read_repo(path: &str) -> String {
    std::fs::read_to_string(manifest_dir().join(path))
        .unwrap_or_else(|_| panic!("{path} must exist (spec #21 distribution)"))
}

fn assert_contains_all(content: &str, origin: &str, markers: &[&str]) {
    for marker in markers {
        assert!(
            content.contains(marker),
            "{origin} must cover {marker} (spec #21 distribution)"
        );
    }
}

#[test]
fn apprun_hook_covers_full_distribution_contract() {
    let hook = read_repo("packaging/AppRun");
    // Carried over from #20 (must not regress).
    assert_contains_all(
        &hook,
        "AppRun hook",
        &[
            "GST_PLUGIN_SYSTEM_PATH",
            "$HERE/usr/lib/gstreamer-1.0",
            "GST_PLUGIN_SCANNER",
            "gst-plugin-scanner",
            "GLYCIN_DATA_DIR",
            "LD_LIBRARY_PATH",
            "gtk4paintablesink",
        ],
    );
    // New for #21: full distribution contract from research §3.
    assert_contains_all(
        &hook,
        "AppRun hook (#21)",
        &[
            // GTK schema/icon caches or plugin-generated equivalent.
            "GSETTINGS_SCHEMA_DIR",
            "GTK_PATH",
            // Bundled bwrap wins via PATH before first Loader::new().load().
            "PATH=\"$HERE/usr/bin",
            // Temp conf.d Exec relocation so bwrap --ro-bind finds loaders.
            "conf.d",
            "Exec=",
            "--self-test-sandbox",
            "XDG_DATA_DIRS",
        ],
    );
}

#[test]
fn build_appimage_script_covers_toolchain_and_zsync() {
    let script = read_repo("packaging/build-appimage.sh");
    assert_contains_all(
        &script,
        "build-appimage.sh",
        &[
            "cargo build --release --locked",
            "cinstall",
            "gst-plugin-gtk4",
            "linuxdeploy",
            "appimagetool",
            "gh-releases-zsync",
            ".zsync",
            "glycin-loaders",
            "bwrap",
            "gst-inspect-1.0",
            "gtk4paintablesink",
            // Review lock: fail fast on a missing toolchain with a
            // copy-pasteable install hint instead of a bare cargo error.
            "cargo cinstall --version",
            "cargo install cargo-c --locked",
        ],
    );
}

#[test]
fn build_appimage_script_fails_fast_on_missing_bundle_inputs() {
    // Review lock: an AppImage without loaders/bwrap fails its own
    // acceptance, so the script must fail fast instead of warning through.
    let script = read_repo("packaging/build-appimage.sh");
    assert_contains_all(
        &script,
        "build-appimage.sh (fail-fast)",
        &["ERROR: no glycin-loaders found", "exit 1"],
    );
    assert!(
        !script.contains("--plugin gtk \\\n  --output appimage"),
        "build-appimage.sh must not fall back to --plugin gtk without gstreamer"
    );
}

#[test]
fn container_build_needs_only_a_container_runtime() {
    // Hosts without cargo-c/linuxdeploy (or not on ubuntu:22.04) build the
    // AppImage in a container instead of installing the toolchain.
    let script = read_repo("packaging/build-appimage-container.sh");
    assert_contains_all(
        &script,
        "build-appimage-container.sh",
        &[
            "podman",
            "ubuntu:22.04",
            "APPIMAGE_EXTRACT_AND_RUN=1",
            "build-appimage.sh",
            "Organizer-x86_64.AppImage",
            // cargo-c is not packaged on jammy: try apt, then build; pip
            // meson because jammy ships 0.61 but glycin needs >=1.2.
            "cargo cinstall --version",
            "apt install -y cargo-c || cargo install cargo-c --locked",
            "pip3 install -U meson",
            // meson needs an explicit source dir (builddir alone would
            // take the repo root as source and fail); loaders trimmed to
            // what jammy can build (cairo>=1.17 from source for svg).
            "builddir /tmp/glycin",
            "glycin-image-rs,glycin-svg",
            "cairo-1.18",
        ],
    );
}

#[test]
fn desktop_file_is_valid_entry() {
    let desktop = read_repo("packaging/organizer.desktop");
    assert_contains_all(
        &desktop,
        "organizer.desktop",
        &[
            "[Desktop Entry]",
            "Name=Organizer",
            "Exec=organizer",
            "Icon=organizer",
            "Type=Application",
            "Categories=",
        ],
    );
}

#[test]
fn sys_deps_docs_cover_all_distros_and_raw_flow() {
    let docs = read_repo("docs/packaging.md");
    assert_contains_all(
        &docs,
        "docs/packaging.md",
        &[
            "libgtk-4-dev",
            "libadwaita",
            "gstreamer1.0-plugins",
            "bubblewrap",
            "libseccomp",
            "dnf install",
            "pacman -S",
            "cargo build --release --locked",
            "organizer /tmp/src",
            "ubuntu:22.04",
        ],
    );
}

#[test]
fn ci_workflow_builds_both_artifacts_and_smokes() {
    let ci = read_repo(".github/workflows/ci.yml");
    assert_contains_all(
        &ci,
        ".github/workflows/ci.yml",
        &[
            "cargo test",
            "cargo build --release --locked",
            "xvfb",
            "linuxdeploy",
            "appimagetool",
            "upload-artifact",
            "Organizer",
            ".AppImage",
            "organizer",
            // Review locks: FUSE-free smoke, checksummed canonical tarball,
            // zsync delta-update verification, fedora/arch matrix.
            "appimage-extract",
            "SHA256SUMS",
            "gh-releases-zsync",
            "distro-matrix",
            "fedora",
            "arch",
        ],
    );
    assert!(
        !ci.contains("uses: flatpak/") && !ci.contains("uses: flathub"),
        "CI must not use flatpak actions (spec #21: raw + AppImage only)"
    );
}

#[test]
fn smoke_script_covers_bundled_layout_checks() {
    let smoke = read_repo("packaging/smoke.sh");
    assert_contains_all(
        &smoke,
        "packaging/smoke.sh",
        &[
            "glycin-loaders",
            "bwrap",
            "libseccomp",
            "libgtk-4",
            "gstreamer-1.0",
            "gtk4paintablesink",
            "cargo test",
        ],
    );
}
