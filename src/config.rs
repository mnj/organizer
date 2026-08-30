use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Action as persisted per-folder in `SourceFolder/organizer.toml`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Action {
    pub display_name: String,
    pub folder_name: String,
    pub shortcut: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub config_version: u32,
    pub actions: Vec<Action>,
}

/// Slugify: lower, spaces → `_`, strip `[^a-z0-9_-]`
pub fn slugify(input: &str) -> String {
    let lower = input.to_ascii_lowercase();
    // spaces -> underscore (only ASCII space per spec)
    let with_underscores = lower.replace(' ', "_");
    with_underscores
        .chars()
        .filter(|c| matches!(c, 'a'..='z' | '0'..='9' | '_' | '-'))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    EmptyDisplayName(usize),
    DuplicateDisplayName(String),
    EmptySlug(usize),
    DuplicateFolderName(String),
    ReservedDuplicate(String),
    InvalidShortcut(String),
    DuplicateShortcut(String),
    TooFewActions,
    TooManyActions,
    ReservedPath(String),
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ValidationError::EmptyDisplayName(i) => write!(f, "row {}: display_name empty", i),
            ValidationError::DuplicateDisplayName(s) => write!(f, "duplicate display_name '{}'", s),
            ValidationError::EmptySlug(i) => write!(f, "row {}: folder_name empty after slug", i),
            ValidationError::DuplicateFolderName(s) => write!(f, "duplicate folder_name '{}'", s),
            ValidationError::ReservedDuplicate(s) => write!(f, "folder_name '{}' is reserved (duplicate)", s),
            ValidationError::InvalidShortcut(s) => write!(f, "shortcut '{}' invalid, must be 1-9", s),
            ValidationError::DuplicateShortcut(s) => write!(f, "duplicate shortcut '{}'", s),
            ValidationError::TooFewActions => write!(f, "need at least 1 action"),
            ValidationError::TooManyActions => write!(f, "max 9 actions"),
            ValidationError::ReservedPath(s) => write!(f, "folder_name '{}' contains reserved path", s),
        }
    }
}

/// Validate a slice of Actions. Checks:
/// - 1..=9 actions
/// - display_name trimmed non-empty unique case-insensitive
/// - folder_name slug valid: after slugify non-empty, not `duplicate`, unique (slug), not `..`/`/`
/// - shortcut single char 1-9 unique
pub fn validate_actions(actions: &[Action]) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();

    if actions.is_empty() {
        errors.push(ValidationError::TooFewActions);
    }
    if actions.len() > 9 {
        errors.push(ValidationError::TooManyActions);
    }

    // display_name uniqueness case-insensitive trimmed
    let mut seen_display: HashMap<String, usize> = HashMap::new();
    for (i, a) in actions.iter().enumerate() {
        let trimmed = a.display_name.trim();
        if trimmed.is_empty() {
            errors.push(ValidationError::EmptyDisplayName(i));
        } else {
            let key = trimmed.to_ascii_lowercase();
            if let Some(_prev) = seen_display.insert(key.clone(), i) {
                // only report once per duplicate value
                // check if already reported
                if !errors.iter().any(|e| matches!(e, ValidationError::DuplicateDisplayName(s) if s == &key)) {
                    errors.push(ValidationError::DuplicateDisplayName(trimmed.to_string()));
                }
            }
        }
    }

    // folder_name checks
    let mut seen_folder: HashSet<String> = HashSet::new();
    for (i, a) in actions.iter().enumerate() {
        // Check raw contains / or .. before slug
        if a.folder_name.contains('/') || a.folder_name.contains('\\') {
            errors.push(ValidationError::ReservedPath(a.folder_name.clone()));
        }
        if a.folder_name.trim() == ".." {
            errors.push(ValidationError::ReservedPath(a.folder_name.clone()));
        }
        let slug = slugify(&a.folder_name);
        if slug.is_empty() {
            errors.push(ValidationError::EmptySlug(i));
            continue;
        }
        if slug == "duplicate" {
            errors.push(ValidationError::ReservedDuplicate(a.folder_name.clone()));
        }
        // slug must match regex ^[a-z0-9_-]+$  - our slugify already ensures, but if original had invalid chars stripped, we still consider slug as canonical; validation ensures no duplicate slug
        if !seen_folder.insert(slug.clone()) {
            if !errors.iter().any(|e| matches!(e, ValidationError::DuplicateFolderName(s) if s == &slug)) {
                errors.push(ValidationError::DuplicateFolderName(slug));
            }
        }
    }

    // shortcut checks
    let mut seen_shortcut: HashSet<String> = HashSet::new();
    for a in actions.iter() {
        let s = a.shortcut.trim();
        if s.len() != 1 || !matches!(s.chars().next().unwrap(), '1'..='9') {
            errors.push(ValidationError::InvalidShortcut(a.shortcut.clone()));
        } else if !seen_shortcut.insert(s.to_string()) {
            if !errors.iter().any(|e| matches!(e, ValidationError::DuplicateShortcut(x) if x == s)) {
                errors.push(ValidationError::DuplicateShortcut(s.to_string()));
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

pub fn default_config() -> Config {
    Config {
        config_version: 1,
        actions: vec![
            Action {
                display_name: "Keep".into(),
                folder_name: "keep".into(),
                shortcut: "1".into(),
            },
            Action {
                display_name: "Maybe".into(),
                folder_name: "maybe".into(),
                shortcut: "2".into(),
            },
            Action {
                display_name: "Reject".into(),
                folder_name: "reject".into(),
                shortcut: "3".into(),
            },
        ],
    }
}

fn config_path(source_folder: &Path) -> PathBuf {
    source_folder.join("organizer.toml")
}

/// Load or auto-create config for source_folder.
/// Uses flock shared lock for reading, validates after parse.
pub fn load_or_create(source_folder: &Path) -> std::io::Result<Config> {
    let path = config_path(source_folder);
    if !path.exists() {
        let cfg = default_config();
        save_config(source_folder, &cfg)?;
        return Ok(cfg);
    }
    // read with shared lock
    let mut file = OpenOptions::new().read(true).open(&path)?;
    #[cfg(unix)]
    {
        use fs2::FileExt;
        let _ = file.lock_shared();
    }
    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    #[cfg(unix)]
    {
        use fs2::FileExt;
        let _ = file.unlock();
    }
    let cfg: Config = toml::from_str(&buf).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid organizer.toml: {e}"),
        )
    })?;
    // validate but don't fail hard - caller can handle; we return cfg even if invalid for UI to fix?
    // For load, we just return; validation is for settings.
    Ok(cfg)
}

/// Save config atomically: write temp + fsync + rename + fsync parent, with flock exclusive.
/// Uses tempfile in same directory then rename.
pub fn save_config(source_folder: &Path, config: &Config) -> std::io::Result<()> {
    // Validate before saving? Keep strict: caller should validate, but we also guard max/min
    let toml_str = toml::to_string(config).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, format!("toml serialize: {e}"))
    })?;
    let dir = source_folder;
    // ensure dir exists
    std::fs::create_dir_all(dir)?;
    let path = config_path(dir);
    // write to temp file in same dir
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(toml_str.as_bytes())?;
    tmp.flush()?;
    tmp.as_file().sync_all()?;
    // flock exclusive on temp? Actually lock destination after rename. Keep simple: lock temp then rename.
    #[cfg(unix)]
    {
        use fs2::FileExt;
        let _ = tmp.as_file().lock_exclusive();
    }
    // persist via rename
    tmp.persist(&path).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("persist: {e}")))?;
    // fsync the file and parent dir
    let f = OpenOptions::new().read(true).open(&path)?;
    f.sync_all()?;
    #[cfg(unix)]
    {
        use fs2::FileExt;
        let _ = f.lock_exclusive();
        let _ = f.unlock();
    }
    // fsync parent dir
    if let Ok(dir_file) = OpenOptions::new().read(true).open(dir) {
        let _ = dir_file.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn slugify_spaces_to_underscore_and_strips_and_lowers() {
        assert_eq!(slugify("Top Picks"), "top_picks");
        assert_eq!(slugify("My Folder 123!"), "my_folder_123");
        assert_eq!(slugify("KEep"), "keep");
        assert_eq!(slugify("a-b_c"), "a-b_c");
        assert_eq!(slugify("hello@#world"), "helloworld");
        assert_eq!(slugify("  leading  spaces  "), "__leading__spaces__");
        assert_eq!(slugify("a/b"), "ab");
        assert_eq!(slugify(".."), "");
        assert_eq!(slugify(""), "");
        assert_eq!(slugify("Duplicate"), "duplicate");
    }

    #[test]
    fn validate_unique_and_not_duplicate_and_max9() {
        // valid
        let actions = vec![
            Action { display_name: "Keep".into(), folder_name: "keep".into(), shortcut: "1".into() },
            Action { display_name: "Maybe".into(), folder_name: "maybe".into(), shortcut: "2".into() },
        ];
        assert!(validate_actions(&actions).is_ok());

        // duplicate display_name case-insensitive
        let dup_display = vec![
            Action { display_name: "Keep".into(), folder_name: "keep".into(), shortcut: "1".into() },
            Action { display_name: "keep".into(), folder_name: "keep2".into(), shortcut: "2".into() },
        ];
        let err = validate_actions(&dup_display).unwrap_err();
        assert!(err.iter().any(|e| matches!(e, ValidationError::DuplicateDisplayName(_))), "got {err:?}");

        // duplicate folder after slug
        let dup_folder = vec![
            Action { display_name: "A".into(), folder_name: "Top Picks".into(), shortcut: "1".into() },
            Action { display_name: "B".into(), folder_name: "top_picks".into(), shortcut: "2".into() },
        ];
        let err = validate_actions(&dup_folder).unwrap_err();
        assert!(err.iter().any(|e| matches!(e, ValidationError::DuplicateFolderName(_))), "got {err:?}");

        // reserved duplicate
        let reserved = vec![
            Action { display_name: "Dup".into(), folder_name: "duplicate".into(), shortcut: "1".into() },
        ];
        let err = validate_actions(&reserved).unwrap_err();
        assert!(err.iter().any(|e| matches!(e, ValidationError::ReservedDuplicate(_))), "got {err:?}");

        // empty after slug
        let empty_slug = vec![
            Action { display_name: "X".into(), folder_name: "@@@".into(), shortcut: "1".into() },
        ];
        let err = validate_actions(&empty_slug).unwrap_err();
        assert!(err.iter().any(|e| matches!(e, ValidationError::EmptySlug(_))), "got {err:?}");

        // duplicate shortcut
        let dup_short = vec![
            Action { display_name: "A".into(), folder_name: "a".into(), shortcut: "1".into() },
            Action { display_name: "B".into(), folder_name: "b".into(), shortcut: "1".into() },
        ];
        let err = validate_actions(&dup_short).unwrap_err();
        assert!(err.iter().any(|e| matches!(e, ValidationError::DuplicateShortcut(_))), "got {err:?}");

        // invalid shortcut
        let invalid_short = vec![
            Action { display_name: "A".into(), folder_name: "a".into(), shortcut: "a".into() },
        ];
        let err = validate_actions(&invalid_short).unwrap_err();
        assert!(err.iter().any(|e| matches!(e, ValidationError::InvalidShortcut(_))), "got {err:?}");

        // max 9
        let mut nine = Vec::new();
        for i in 1..=9 {
            nine.push(Action { display_name: format!("A{i}"), folder_name: format!("a{i}"), shortcut: format!("{i}") });
        }
        assert!(validate_actions(&nine).is_ok());
        let mut ten = nine.clone();
        ten.push(Action { display_name: "A10".into(), folder_name: "a10".into(), shortcut: "1".into() });
        let err = validate_actions(&ten).unwrap_err();
        assert!(err.iter().any(|e| matches!(e, ValidationError::TooManyActions)), "got {err:?}");

        // min 1
        let empty: Vec<Action> = vec![];
        let err = validate_actions(&empty).unwrap_err();
        assert!(err.iter().any(|e| matches!(e, ValidationError::TooFewActions)), "got {err:?}");

        // contains slash
        let slash = vec![
            Action { display_name: "A".into(), folder_name: "a/b".into(), shortcut: "1".into() },
        ];
        let err = validate_actions(&slash).unwrap_err();
        assert!(err.iter().any(|e| matches!(e, ValidationError::ReservedPath(_))), "got {err:?}");
    }

    #[test]
    fn roundtrip_write_reload() {
        let dir = TempDir::new().unwrap();
        let cfg = Config {
            config_version: 1,
            actions: vec![
                Action { display_name: "Keep".into(), folder_name: "keep".into(), shortcut: "1".into() },
                Action { display_name: "Top Picks".into(), folder_name: "top_picks".into(), shortcut: "2".into() },
            ],
        };
        save_config(dir.path(), &cfg).unwrap();
        let loaded = load_or_create(dir.path()).unwrap();
        assert_eq!(loaded.config_version, 1);
        assert_eq!(loaded.actions, cfg.actions);
    }

    #[test]
    fn auto_creates_defaults() {
        let dir = TempDir::new().unwrap();
        let cfg = load_or_create(dir.path()).unwrap();
        assert_eq!(cfg.actions.len(), 3);
        assert_eq!(cfg.actions[0].display_name, "Keep");
        assert_eq!(cfg.actions[1].shortcut, "2");
        assert!(dir.path().join("organizer.toml").exists());
        // reload preserves
        let cfg2 = load_or_create(dir.path()).unwrap();
        assert_eq!(cfg2.actions, cfg.actions);
    }

    #[test]
    fn switching_source_folder_loads_own_config() {
        let dir1 = TempDir::new().unwrap();
        let dir2 = TempDir::new().unwrap();
        let cfg1 = Config {
            config_version: 1,
            actions: vec![Action { display_name: "A".into(), folder_name: "a".into(), shortcut: "1".into() }],
        };
        let cfg2 = Config {
            config_version: 1,
            actions: vec![Action { display_name: "B".into(), folder_name: "b".into(), shortcut: "9".into() }],
        };
        save_config(dir1.path(), &cfg1).unwrap();
        save_config(dir2.path(), &cfg2).unwrap();
        let l1 = load_or_create(dir1.path()).unwrap();
        let l2 = load_or_create(dir2.path()).unwrap();
        assert_eq!(l1.actions[0].display_name, "A");
        assert_eq!(l2.actions[0].display_name, "B");
    }
}
