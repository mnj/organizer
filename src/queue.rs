use std::path::{Path, PathBuf};

/// Supported extensions for the Queue Snapshot.
/// Stills via glycin and video via GStreamer, case-insensitive.
/// Legacy `organizer.toml` / `*.txt` are unsupported and ignored like any
/// other non-media file (#25); they never enter the Queue.
const SUPPORTED_EXTS: &[&str] = &[
    // stills / animated stills via glycin
    "png", "jpg", "jpeg", "bmp", "tiff", "tif", "webp", "gif", "avif", "heic", "heif", "svg",
    "ico",
    // video via GStreamer
    "webm", "mp4", "mov", "mkv", "avi",
];

fn is_supported(path: &Path) -> bool {
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        SUPPORTED_EXTS.contains(&ext.to_ascii_lowercase().as_str())
    } else {
        false
    }
}

/// Build a flat, natural case-insensitive sorted Snapshot of Supported Formats
/// from `source_folder`. Unsupported files (including legacy `organizer.toml`/
/// `*.txt` and the Organizer Database `organizer.db` + WAL artifacts) are
/// silently excluded and never enter the Queue. No recursion, no live watch.
pub fn build_snapshot(source_folder: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut entries: Vec<PathBuf> = Vec::new();
    let dir = std::fs::read_dir(source_folder)?;
    for entry in dir {
        let entry = entry?;
        let path = entry.path();
        // Only regular files, flat only — no recursion, skip directories
        // (category/duplicate subfolders live inside the Source Folder)
        if !path.is_file() {
            continue;
        }
        // Portable Organizer Database never enters the Queue, even though `.db`
        // is already unsupported — explicit so future format changes cannot regress.
        if crate::store::is_internal_db_file(&path) {
            continue;
        }
        if is_supported(&path) {
            entries.push(path);
        } else {
            // Silently skip: legacy .toml/.txt and other unsupported (e.g. .pdf, .zip)
            continue;
        }
    }
    // Natural case-insensitive sort by file name
    entries.sort_by(|a, b| {
        let an = a
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let bn = b
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        natord::compare(&an, &bn)
    });
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use tempfile::TempDir;

    fn touch(dir: &Path, name: &str) {
        File::create(dir.join(name)).unwrap();
    }

    fn snapshot_names(dir: &Path) -> Vec<String> {
        build_snapshot(dir)
            .unwrap()
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn empty_folder_yields_empty_snapshot() {
        let dir = TempDir::new().unwrap();
        let snap = build_snapshot(dir.path()).unwrap();
        assert!(snap.is_empty(), "empty dir should give empty snapshot");
    }

    #[test]
    fn filters_and_sorts_naturally_case_insensitive_and_skips_internal() {
        let dir = TempDir::new().unwrap();
        let p = dir.path();
        // Supported
        touch(p, "b.JPG"); // case-insensitive
        touch(p, "a.png");
        touch(p, "10.jpg");
        touch(p, "2.jpg");
        touch(p, "clip.webm");
        touch(p, "photo.TIFF");
        // Legacy + other unsupported silently skipped (#25)
        touch(p, "organizer.toml");
        touch(p, "keep.txt");
        touch(p, "maybe.TXT");
        touch(p, "notes.pdf");
        touch(p, "archive.zip");
        touch(p, "README");
        // Subdirectory should be ignored (flat only)
        std::fs::create_dir(p.join("subdir")).unwrap();
        touch(&p.join("subdir"), "inside.jpg");

        let names = snapshot_names(p);

        // Expected: only supported, natural sorted case-insensitive
        // Natural order: 2.jpg, 10.jpg, a.png, b.JPG, clip.webm, photo.TIFF
        assert_eq!(
            names,
            vec!["2.jpg", "10.jpg", "a.png", "b.JPG", "clip.webm", "photo.TIFF"],
            "snapshot must be filtered and natural sorted: got {names:?}"
        );
        // Ensure legacy and other unsupported not present
        assert!(!names.iter().any(|n| n.ends_with(".toml") || n.ends_with(".txt") || n.ends_with(".pdf")));
    }

    #[test]
    fn flat_only_ignores_subdirectories() {
        let dir = TempDir::new().unwrap();
        let p = dir.path();
        touch(p, "top.jpg");
        std::fs::create_dir(p.join("keep")).unwrap();
        touch(&p.join("keep"), "inside.png");
        let snap = build_snapshot(p).unwrap();
        assert_eq!(snap.len(), 1);
        assert!(snap[0].ends_with("top.jpg"));
    }

    #[test]
    fn unsupported_never_enters_queue() {
        let dir = TempDir::new().unwrap();
        let p = dir.path();
        touch(p, "doc.pdf");
        touch(p, "archive.zip");
        touch(p, "keep.txt");
        let snap = build_snapshot(p).unwrap();
        assert!(snap.is_empty(), "only unsupported should give empty queue, not placeholder");
    }

    #[test]
    fn legacy_files_ignored_by_snapshot() {
        let dir = TempDir::new().unwrap();
        let p = dir.path();
        touch(p, "photo.jpg");
        touch(p, "organizer.toml");
        touch(p, "keep.txt");
        touch(p, "maybe.txt");
        assert_eq!(snapshot_names(p), vec!["photo.jpg"], "legacy files must never enter the Queue (#25)");
    }

    #[test]
    fn organizer_database_never_enters_queue() {
        let dir = TempDir::new().unwrap();
        let p = dir.path();
        touch(p, "a.jpg");
        touch(p, "organizer.db");
        touch(p, "organizer.db-wal");
        touch(p, "organizer.db-shm");
        touch(p, "organizer.db-journal");
        assert_eq!(snapshot_names(p), vec!["a.jpg"]);
    }
}
