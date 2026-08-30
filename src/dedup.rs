use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;

/// Compute lower-case hex sha256 of file at `path` via streaming read.
/// Uses `spawn_blocking` caller-side for background; this function is blocking.
pub fn compute_sha256(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let result = hasher.finalize();
    Ok(hex::encode(result))
}

/// Returns true if s is valid lower-case sha256 hex (64 hex chars).
/// We accept case-insensitive input but validate length and hex chars.
pub fn is_valid_hash(s: &str) -> bool {
    if s.len() != 64 {
        return false;
    }
    s.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f' | 'A'..='F'))
}

/// Domain type for a validated lower-case sha256 hex (Primitive Obsession fix).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FileHash(String);

impl FileHash {
    /// Validate and normalize to lower-case. Accepts case-insensitive 64 hex.
    pub fn new(raw: &str) -> std::io::Result<Self> {
        let lower = raw.to_ascii_lowercase();
        if !is_valid_hash(&lower) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid hash: {}", raw),
            ));
        }
        Ok(Self(lower))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for FileHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<FileHash> for String {
    fn from(h: FileHash) -> Self {
        h.0
    }
}

/// Helper: run closure while holding a shared lock (unix) or direct on non-unix.
/// Centralizes flock boilerplate (Duplicated Code fix).
fn with_shared_lock<R>(file: &std::fs::File, f: impl FnOnce() -> R) -> R {
    #[cfg(unix)]
    {
        let _ = fs2::FileExt::lock_shared(file);
        let r = f();
        let _ = fs2::FileExt::unlock(file);
        r
    }
    #[cfg(not(unix))]
    {
        f()
    }
}

/// Helper: run closure while holding an exclusive lock + fsync on unix.
fn with_exclusive_lock_and_fsync(file: &mut std::fs::File, data: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use fs2::FileExt;
        file.lock_exclusive()?;
        file.write_all(data)?;
        file.write_all(b"\n")?;
        file.flush()?;
        file.sync_all()?;
        file.unlock()?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        file.write_all(data)?;
        file.write_all(b"\n")?;
        file.flush()?;
        file.sync_all()?;
        Ok(())
    }
}

fn is_txt_log(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    matches!(path.extension().and_then(|e| e.to_str()), Some(ext) if ext.eq_ignore_ascii_case("txt"))
}

fn txt_log_paths(source_folder: &Path) -> Vec<std::path::PathBuf> {
    let Ok(dir) = std::fs::read_dir(source_folder) else {
        return Vec::new();
    };
    dir.flatten()
        .map(|e| e.path())
        .filter(|p| is_txt_log(p))
        .collect()
}

/// Load union HashSet from all `SourceFolder/*.txt` logs (inside Source Folder).
/// Each line trimmed, lower-cased, validated as hex; corrupted lines are skipped
/// with `tracing::warn` and counted. Returns (union, warning_count).
pub fn load_union(source_folder: &Path) -> (HashSet<String>, usize) {
    let mut set = HashSet::new();
    let mut warnings = 0usize;
    for path in txt_log_paths(source_folder) {
        let file = match OpenOptions::new().read(true).open(&path) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let (inner_set, inner_warnings) = with_shared_lock(&file, || {
            let mut local_set = HashSet::new();
            let mut local_warnings = 0usize;
            let reader = BufReader::new(&file);
            for line in reader.lines().flatten() {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if !is_valid_hash(trimmed) {
                    local_warnings += 1;
                    tracing::warn!("corrupted log line skipped in {}: {:?}", path.display(), trimmed);
                    continue;
                }
                local_set.insert(trimmed.to_ascii_lowercase());
            }
            (local_set, local_warnings)
        });
        set.extend(inner_set);
        warnings += inner_warnings;
    }
    (set, warnings)
}

/// Find the first Action folder name whose `SourceFolder/<folder>.txt` contains `hash`.
/// Scans `*.txt` inside Source Folder, case-insensitive hash compare. Returns the stem
/// (e.g. "keep") without extension, or None if not found.
pub fn find_duplicate_origin(source_folder: &Path, hash: &str) -> Option<String> {
    let lower = hash.to_ascii_lowercase();
    if !is_valid_hash(&lower) {
        return None;
    }
    for path in txt_log_paths(source_folder) {
        let file = OpenOptions::new().read(true).open(&path).ok()?;
        let found = with_shared_lock(&file, || {
            let reader = BufReader::new(&file);
            for line in reader.lines().flatten() {
                if line.trim().to_ascii_lowercase() == lower {
                    return true;
                }
            }
            false
        });
        if found {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                return Some(stem.to_string());
            }
        }
    }
    None
}

/// Append one lower-case hash line to `SourceFolder/<folder_name>.txt` with
/// `O_APPEND` + `flock` exclusive + `fsync`, fsync parent dir.
/// `folder_name` is already slug-sanitized; we use it as given.
/// On non-unix (spec is Linux-only per ADR 0004) we still fsync without flock.
pub fn append_hash_log(source_folder: &Path, folder_name: &str, hash: &str) -> std::io::Result<()> {
    let validated = FileHash::new(hash)?;
    let log_path = source_folder.join(format!("{}.txt", folder_name));
    std::fs::create_dir_all(source_folder)?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(&log_path)?;
    with_exclusive_lock_and_fsync(&mut file, validated.as_str().as_bytes())?;
    if let Ok(dir_file) = OpenOptions::new().read(true).open(source_folder) {
        let _ = dir_file.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn compute_sha256_deterministic_lowercase() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("hello.jpg");
        std::fs::write(&path, b"hello").unwrap();
        let h = compute_sha256(&path).unwrap();
        // sha256("hello") = 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824
        assert_eq!(h, "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
        assert!(h.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        // same bytes same hash
        let h2 = compute_sha256(&path).unwrap();
        assert_eq!(h, h2);
        // different bytes different hash
        let path2 = dir.path().join("world.jpg");
        std::fs::write(&path2, b"world").unwrap();
        let h3 = compute_sha256(&path2).unwrap();
        assert_ne!(h, h3);
    }

    #[test]
    fn is_valid_hash_checks_length_and_hex() {
        assert!(is_valid_hash("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"));
        assert!(is_valid_hash("2CF24DBA5FB0A30E26E83B2AC5B9E29E1B161E5C1FA7425E73043362938B9824"));
        assert!(!is_valid_hash("2cf24dba5fb0a30e26e83b2ac5b9e29e"));
        assert!(!is_valid_hash("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"));
        assert!(!is_valid_hash(""));
    }

    #[test]
    fn load_union_case_normalized_and_skips_corrupted_with_count() {
        let dir = TempDir::new().unwrap();
        let p = dir.path();
        let h_lower = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        let h_upper = "2CF24DBA5FB0A30E26E83B2AC5B9E29E1B161E5C1FA7425E73043362938B9824";
        // create two txt logs
        {
            let mut f = File::create(p.join("keep.txt")).unwrap();
            writeln!(f, "{}", h_lower).unwrap();
            writeln!(f, "not-a-hash-corrupted").unwrap();
            writeln!(f, "").unwrap();
            writeln!(f, "{}", h_upper).unwrap(); // duplicate after lower
            writeln!(f, "123").unwrap(); // corrupted
        }
        {
            let mut f = File::create(p.join("maybe.txt")).unwrap();
            writeln!(f, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
            writeln!(f, "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB").unwrap();
        }
        // non-txt should be ignored
        std::fs::write(p.join("notes.pdf"), b"not txt").unwrap();
        std::fs::write(p.join("organizer.toml"), b"ignore").unwrap();

        let (set, warnings) = load_union(p);
        // valid hashes (lower): 2cf24..., aaaa..., bbbb... (but upper normalized)
        // 2cf... duplicate counted once
        assert_eq!(warnings, 2, "two corrupted lines");
        assert!(set.contains(h_lower));
        assert!(set.contains("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
        assert!(set.contains("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"));
        assert_eq!(set.len(), 3);
    }

    #[test]
    fn append_hash_log_flock_fsync_one_per_line_lowercase() {
        let dir = TempDir::new().unwrap();
        let p = dir.path();
        let h = "2CF24DBA5FB0A30E26E83B2AC5B9E29E1B161E5C1FA7425E73043362938B9824";
        append_hash_log(p, "keep", h).unwrap();
        append_hash_log(p, "keep", h.to_ascii_lowercase().as_str()).unwrap();
        let content = std::fs::read_to_string(p.join("keep.txt")).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in lines {
            assert_eq!(line, line.to_ascii_lowercase());
            assert!(is_valid_hash(line));
        }
        // ensure flock exclusive doesn't corrupt on concurrent appends (sequential for test)
        let (set, warnings) = load_union(p);
        assert_eq!(warnings, 0);
        assert_eq!(set.len(), 1); // duplicate hash deduped in set
    }

    #[test]
    fn load_union_empty_folder_gives_empty() {
        let dir = TempDir::new().unwrap();
        let (set, warnings) = load_union(dir.path());
        assert!(set.is_empty());
        assert_eq!(warnings, 0);
    }

    #[test]
    fn find_duplicate_origin_returns_owning_folder() {
        let dir = TempDir::new().unwrap();
        let p = dir.path();
        let h_keep = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let h_maybe = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        {
            let mut f = File::create(p.join("keep.txt")).unwrap();
            writeln!(f, "{}", h_keep).unwrap();
        }
        {
            let mut f = File::create(p.join("maybe.txt")).unwrap();
            writeln!(f, "{}", h_maybe).unwrap();
        }
        assert_eq!(find_duplicate_origin(p, h_keep).unwrap(), "keep");
        assert_eq!(find_duplicate_origin(p, &h_keep.to_ascii_uppercase()).unwrap(), "keep");
        assert_eq!(find_duplicate_origin(p, h_maybe).unwrap(), "maybe");
        assert!(find_duplicate_origin(p, "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc").is_none());
        // invalid hash returns None
        assert!(find_duplicate_origin(p, "not-a-hash").is_none());
    }
}
