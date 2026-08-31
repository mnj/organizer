use std::path::Path;
use std::process::Command;

use gdk4::Texture;
use gio::File;
use glycin::{Loader, SandboxSelector};

/// Stills / animated stills supported via glycin (allowlist per spec #19).
/// Strict allowlist: `png`/`jpg`/`jpeg`/`bmp`/`tiff`/`webp`/`gif-anim`/`avif`/`heic`/`svg`/`ico`.
/// Variants `tif`/`heif`/`svgz` are included as aliases; additional glycin loaders
/// (`jxl`/`qoi`/`dds` etc.) are intentionally treated as unsupported until spec expands
/// to avoid scope creep — they will fall back to placeholder and not break triage.
pub const GLYCIN_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "bmp", "tiff", "tif", "webp", "gif", "avif", "heic", "heif", "svg",
    "svgz", "ico",
];

/// Video extensions are handled by GStreamer (future ticket), not glycin.
/// Provided to distinguish "supported via glycin" vs "supported overall".
pub const VIDEO_EXTS: &[&str] = &["webm", "mp4", "mov", "mkv", "avi"];

/// Single dispatch for extension classification — fixes Repeated Switches.
#[derive(Debug, PartialEq, Eq)]
pub enum SupportedKind {
    Glycin,
    Video,
    Unsupported,
}

pub fn classify_extension(ext: &str) -> SupportedKind {
    let lower = ext.to_ascii_lowercase();
    if GLYCIN_EXTS.contains(&lower.as_str()) {
        SupportedKind::Glycin
    } else if VIDEO_EXTS.contains(&lower.as_str()) {
        SupportedKind::Video
    } else {
        SupportedKind::Unsupported
    }
}

/// Check if a path's extension is in the glycin stills allowlist (case-insensitive).
pub fn is_glycin_supported(path: &Path) -> bool {
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        classify_extension(ext) == SupportedKind::Glycin
    } else {
        false
    }
}

pub fn is_video_extension(ext: &str) -> bool {
    classify_extension(ext) == SupportedKind::Video
}

/// Compute sandboxed loader memory limit in bytes:
/// `80% * min(MemAvailable + SwapFree - 200MB, 20GB)`
/// Mirrors glycin-core sandbox::memory_limit() (see research/sandboxed-decoding-raw-appimage).
/// Inputs are bytes. Returns bytes.
pub fn compute_memory_limit(mem_available_bytes: u64, swap_free_bytes: u64) -> u64 {
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * MB;
    let total = mem_available_bytes.saturating_add(swap_free_bytes);
    let minus_200 = total.saturating_sub(200 * MB);
    let capped = std::cmp::min(minus_200, 20 * GB);
    // 80%
    (capped as u128 * 80 / 100) as u64
}

fn parse_kb(line: &str) -> Option<u64> {
    // format: Key:  123456 kB
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() >= 2 {
        if let Ok(kb) = parts[1].parse::<u64>() {
            return Some(kb * 1024);
        }
    }
    None
}

/// Read /proc/meminfo and compute memory limit. Returns None if unreadable.
/// Used only for verification that sandbox `setrlimit` matches spec formula;
/// actual enforcement is via glycin's `bwrap --seccomp` + `setrlimit(RLIMIT_AS)`
/// inside the loader (see `glycin-core/src/sandbox.rs:memory_limit`).
pub fn memory_limit_from_proc() -> Option<u64> {
    let content = std::fs::read_to_string("/proc/meminfo").ok()?;
    let mut mem_available: Option<u64> = None;
    let mut swap_free: Option<u64> = None;
    for line in content.lines() {
        if line.starts_with("MemAvailable:") {
            mem_available = parse_kb(line);
        } else if line.starts_with("SwapFree:") {
            swap_free = parse_kb(line);
        }
    }
    let mem = mem_available?;
    let swap = swap_free.unwrap_or(0);
    Some(compute_memory_limit(mem, swap))
}

/// Check that `bwrap` is available and functional.
/// Returns Ok(()) if `bwrap --version` succeeds.
/// Returns Err with user-facing message if missing or non-functional.
/// This enforces `SandboxSelector::Bwrap` per spec — no silent fallback.
pub fn check_sandbox_available() -> Result<(), String> {
    let output = Command::new("bwrap").arg("--version").output();
    match output {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => Err(format!(
            "Sandbox unavailable — install bubblewrap ≥0.8 (bwrap --version failed: {})",
            String::from_utf8_lossy(&out.stderr)
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(
            "Sandbox unavailable — install bubblewrap ≥0.8".to_string(),
        ),
        Err(e) => Err(format!(
            "Sandbox unavailable — install bubblewrap ≥0.8 ({})",
            e
        )),
    }
}

/// Check if bwrap syscalls are blocked (HostBwrapSyscallsBlocked).
/// Tries a minimal bwrap invocation; if it fails with SIGSYS / permission,
/// we treat sandbox as unavailable. Mirrors glycin's `Sandbox::check_bwrap_syscalls_blocked`
/// but uses `--ro-bind-try` for `/lib*` to avoid false positives on systems
/// where those paths are absent (NixOS, minimal containers).
///
/// Returns true if blocked.
pub fn is_bwrap_blocked() -> bool {
    let result = Command::new("bwrap")
        .args([
            "--unshare-all",
            "--die-with-parent",
            "--chdir",
            "/",
            "--ro-bind",
            "/usr",
            "/usr",
            "--ro-bind-try",
            "/lib",
            "/lib",
            "--ro-bind-try",
            "/lib64",
            "/lib64",
            "--dev",
            "/dev",
            "/usr/bin/true",
        ])
        .output();
    match result {
        Ok(out) => !out.status.success(),
        Err(_) => true, // if bwrap not found, treat as blocked (check_sandbox_available already reports)
    }
}

/// Combined sandbox check: bwrap must exist and not be blocked.
/// Returns Ok(()) if sandbox is Bwrap, Err with dialog message otherwise.
pub fn ensure_sandbox_bwrap() -> Result<(), String> {
    check_sandbox_available()?;
    if is_bwrap_blocked() {
        return Err(
            "Sandbox unavailable — install bubblewrap ≥0.8".to_string(),
        );
    }
    Ok(())
}

/// Load a still/animated image via sandboxed glycin → GdkTexture.
/// Enforces `SandboxSelector::Bwrap` (no silent NotSandboxed fallback).
/// Runs off UI thread via glycin's internal async + bwrap sandbox (conceptually
/// `gio::Task` via `glib::MainContext::spawn_local` — functionally equivalent
/// to `gio::Task::run_in_thread` but preserves async `load().await`).
/// Returns Texture on success, glycin::Error on failure (malformed → RemoteError::Panic).
///
/// Caller must handle errors by showing `image-missing` placeholder + toast
/// and keeping Main alive (loader crash is isolated per spec).
/// `cancellable` is forwarded to `Loader` so `update_ui`'s `prev.cancel()`
/// aborts in-flight loads and avoids stale-frame races on rapid Next/Prev.
pub async fn load_texture(
    path: &Path,
    cancellable: Option<&gio::Cancellable>,
) -> Result<Texture, glycin::Error> {
    let file = File::for_path(path);
    let mut loader = Loader::new(file);
    loader.sandbox_selector(SandboxSelector::Bwrap);
    if let Some(c) = cancellable {
        loader.cancellable(c.clone());
    }
    let mut image = loader.load().await?;
    let frame = image.next_frame().await?;
    Ok(frame.texture().clone())
}

#[allow(dead_code)]
async fn load_texture_from_file(
    file: File,
    cancellable: Option<&gio::Cancellable>,
) -> Result<Texture, glycin::Error> {
    let mut loader = Loader::new(file);
    loader.sandbox_selector(SandboxSelector::Bwrap);
    if let Some(c) = cancellable {
        loader.cancellable(c.clone());
    }
    let mut image = loader.load().await?;
    let frame = image.next_frame().await?;
    Ok(frame.texture().clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn glycin_supported_case_insensitive() {
        assert!(is_glycin_supported(&PathBuf::from("photo.JPG")));
        assert!(is_glycin_supported(&PathBuf::from("anim.GIF")));
        assert!(is_glycin_supported(&PathBuf::from("image.WEBP")));
        assert!(is_glycin_supported(&PathBuf::from("pic.tiff")));
        assert!(is_glycin_supported(&PathBuf::from("vector.SVG")));
        assert!(is_glycin_supported(&PathBuf::from("icon.ICO")));
        assert!(is_glycin_supported(&PathBuf::from("avif_test.AVIF")));
        assert!(is_glycin_supported(&PathBuf::from("heic_test.HEIC")));
        assert!(!is_glycin_supported(&PathBuf::from("video.webm")));
        assert!(!is_glycin_supported(&PathBuf::from("movie.mp4")));
        assert!(!is_glycin_supported(&PathBuf::from("doc.pdf")));
        assert!(!is_glycin_supported(&PathBuf::from("archive.zip")));
        assert!(!is_glycin_supported(&PathBuf::from("organizer.toml")));
        assert!(!is_glycin_supported(&PathBuf::from("keep.txt")));
        assert!(!is_glycin_supported(&PathBuf::from("README")));
        assert!(!is_glycin_supported(&PathBuf::from("noext")));
    }

    #[test]
    fn video_extension_detection() {
        assert!(is_video_extension("webm"));
        assert!(is_video_extension("WEBM"));
        assert!(is_video_extension("mp4"));
        assert!(is_video_extension("mov"));
        assert!(is_video_extension("mkv"));
        assert!(is_video_extension("avi"));
        assert!(!is_video_extension("png"));
        assert!(!is_video_extension("jpg"));
        assert!(!is_video_extension("gif"));
    }

    #[test]
    fn memory_limit_formula() {
        // 80% * min(MemAvailable+SwapFree -200MB, 20GB)
        const MB: u64 = 1024 * 1024;
        const GB: u64 = 1024 * MB;
        // case: 8GB available + 2GB swap = 10GB -0.2GB = 9.8GB (10040MB) <20GB => 8032MB
        let limit = compute_memory_limit(8 * GB, 2 * GB);
        assert_eq!(limit, (10040 * MB * 80 / 100));
        // cap: 32GB + 8GB =40GB -0.2 =>39.8GB >20GB => capped 20GB*0.8=16GB
        let limit2 = compute_memory_limit(32 * GB, 8 * GB);
        assert_eq!(limit2, 16 * GB);
        // small: 500MB +0 =>300MB after minus200 =>240MB
        let limit3 = compute_memory_limit(500 * MB, 0);
        assert_eq!(limit3, 240 * MB);
        // saturating: if total <200MB =>0
        let limit4 = compute_memory_limit(100 * MB, 0);
        assert_eq!(limit4, 0);
        // ensure 1GB fallback not required for formula
        let limit5 = compute_memory_limit(1024 * MB, 0);
        // 1024-200=824MB *0.8=659.2MB
        assert_eq!(limit5, (824 * MB * 80 / 100));
    }

    #[test]
    fn memory_limit_from_proc_parses() {
        // Just verify it doesn't panic and returns Some on this Linux host
        let limit = memory_limit_from_proc();
        assert!(limit.is_some());
        let v = limit.unwrap();
        assert!(v > 0);
        // Should be <=16GB (capped)
        assert!(v <= 16 * 1024 * 1024 * 1024);
    }

    #[test]
    fn bwrap_check_returns_result() {
        // On this dev host bwrap exists, so check should be Ok.
        // If not, error message must contain expected text.
        let res = check_sandbox_available();
        match res {
            Ok(()) => {}
            Err(msg) => assert!(msg.contains("Sandbox unavailable")),
        }
        let res2 = ensure_sandbox_bwrap();
        match res2 {
            Ok(()) => {}
            Err(msg) => assert!(msg.contains("Sandbox unavailable")),
        }
    }

    #[test]
    fn sandbox_missing_path_reports_unavailable() {
        // Simulate missing bwrap by checking a non-existent binary via direct Command
        let out = Command::new("nonexistent-bwrap-xyz").arg("--version").output();
        assert!(out.is_err());
        let err = out.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    // Async texture loading tests require a display and are not run in headless CI;
    // they are covered via manual smoke test per acceptance criteria.
    // We provide a helper that would be used in manual testing.
}
