use sha2::{Digest, Sha256};
use std::io::Read;
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

#[cfg(test)]
mod tests {
    use super::*;
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
}
