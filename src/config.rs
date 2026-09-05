use std::collections::{HashMap, HashSet};

/// Category as persisted in the Organizer Database (`Source Folder/organizer.db`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Category {
    pub display_name: String,
    pub folder_name: String,
    pub shortcut: String,
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
    TooFewCategories,
    TooManyCategories,
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
            ValidationError::TooFewCategories => write!(f, "need at least 1 category"),
            ValidationError::TooManyCategories => write!(f, "max 9 categories"),
            ValidationError::ReservedPath(s) => write!(f, "folder_name '{}' contains reserved path", s),
        }
    }
}

/// Validate a slice of Categories. Checks:
/// - 1..=9 categories
/// - display_name trimmed non-empty unique case-insensitive
/// - folder_name slug valid: after slugify non-empty, not `duplicate`, unique (slug), not `..`/`/`
/// - shortcut single char 1-9 unique
pub fn validate_categories(categories: &[Category]) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();

    if categories.is_empty() {
        errors.push(ValidationError::TooFewCategories);
    }
    if categories.len() > 9 {
        errors.push(ValidationError::TooManyCategories);
    }

    // display_name uniqueness case-insensitive trimmed
    let mut seen_display: HashMap<String, usize> = HashMap::new();
    for (i, c) in categories.iter().enumerate() {
        let trimmed = c.display_name.trim();
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
    for (i, c) in categories.iter().enumerate() {
        // Check raw contains / or .. before slug
        if c.folder_name.contains('/') || c.folder_name.contains('\\') {
            errors.push(ValidationError::ReservedPath(c.folder_name.clone()));
        }
        if c.folder_name.trim() == ".." {
            errors.push(ValidationError::ReservedPath(c.folder_name.clone()));
        }
        let slug = slugify(&c.folder_name);
        if slug.is_empty() {
            errors.push(ValidationError::EmptySlug(i));
            continue;
        }
        if slug == "duplicate" {
            errors.push(ValidationError::ReservedDuplicate(c.folder_name.clone()));
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
    for c in categories.iter() {
        let s = c.shortcut.trim();
        if s.len() != 1 || !matches!(s.chars().next().unwrap(), '1'..='9') {
            errors.push(ValidationError::InvalidShortcut(c.shortcut.clone()));
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

/// Default Categories seeded into a fresh Organizer Database.
/// Sole source of defaults now that `organizer.toml` is gone (#25).
pub fn default_categories() -> Vec<Category> {
    vec![
        Category {
            display_name: "Keep".into(),
            folder_name: "keep".into(),
            shortcut: "1".into(),
        },
        Category {
            display_name: "Maybe".into(),
            folder_name: "maybe".into(),
            shortcut: "2".into(),
        },
        Category {
            display_name: "Reject".into(),
            folder_name: "reject".into(),
            shortcut: "3".into(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let categories = vec![
            Category { display_name: "Keep".into(), folder_name: "keep".into(), shortcut: "1".into() },
            Category { display_name: "Maybe".into(), folder_name: "maybe".into(), shortcut: "2".into() },
        ];
        assert!(validate_categories(&categories).is_ok());

        // duplicate display_name case-insensitive
        let dup_display = vec![
            Category { display_name: "Keep".into(), folder_name: "keep".into(), shortcut: "1".into() },
            Category { display_name: "keep".into(), folder_name: "keep2".into(), shortcut: "2".into() },
        ];
        let err = validate_categories(&dup_display).unwrap_err();
        assert!(err.iter().any(|e| matches!(e, ValidationError::DuplicateDisplayName(_))), "got {err:?}");

        // duplicate folder after slug
        let dup_folder = vec![
            Category { display_name: "A".into(), folder_name: "Top Picks".into(), shortcut: "1".into() },
            Category { display_name: "B".into(), folder_name: "top_picks".into(), shortcut: "2".into() },
        ];
        let err = validate_categories(&dup_folder).unwrap_err();
        assert!(err.iter().any(|e| matches!(e, ValidationError::DuplicateFolderName(_))), "got {err:?}");

        // reserved duplicate
        let reserved = vec![
            Category { display_name: "Dup".into(), folder_name: "duplicate".into(), shortcut: "1".into() },
        ];
        let err = validate_categories(&reserved).unwrap_err();
        assert!(err.iter().any(|e| matches!(e, ValidationError::ReservedDuplicate(_))), "got {err:?}");

        // empty after slug
        let empty_slug = vec![
            Category { display_name: "X".into(), folder_name: "@@@".into(), shortcut: "1".into() },
        ];
        let err = validate_categories(&empty_slug).unwrap_err();
        assert!(err.iter().any(|e| matches!(e, ValidationError::EmptySlug(_))), "got {err:?}");

        // duplicate shortcut
        let dup_short = vec![
            Category { display_name: "A".into(), folder_name: "a".into(), shortcut: "1".into() },
            Category { display_name: "B".into(), folder_name: "b".into(), shortcut: "1".into() },
        ];
        let err = validate_categories(&dup_short).unwrap_err();
        assert!(err.iter().any(|e| matches!(e, ValidationError::DuplicateShortcut(_))), "got {err:?}");

        // invalid shortcut
        let invalid_short = vec![
            Category { display_name: "A".into(), folder_name: "a".into(), shortcut: "a".into() },
        ];
        let err = validate_categories(&invalid_short).unwrap_err();
        assert!(err.iter().any(|e| matches!(e, ValidationError::InvalidShortcut(_))), "got {err:?}");

        // max 9
        let mut nine = Vec::new();
        for i in 1..=9 {
            nine.push(Category { display_name: format!("A{i}"), folder_name: format!("a{i}"), shortcut: format!("{i}") });
        }
        assert!(validate_categories(&nine).is_ok());
        let mut ten = nine.clone();
        ten.push(Category { display_name: "A10".into(), folder_name: "a10".into(), shortcut: "1".into() });
        let err = validate_categories(&ten).unwrap_err();
        assert!(err.iter().any(|e| matches!(e, ValidationError::TooManyCategories)), "got {err:?}");

        // min 1
        let empty: Vec<Category> = vec![];
        let err = validate_categories(&empty).unwrap_err();
        assert!(err.iter().any(|e| matches!(e, ValidationError::TooFewCategories)), "got {err:?}");

        // contains slash
        let slash = vec![
            Category { display_name: "A".into(), folder_name: "a/b".into(), shortcut: "1".into() },
        ];
        let err = validate_categories(&slash).unwrap_err();
        assert!(err.iter().any(|e| matches!(e, ValidationError::ReservedPath(_))), "got {err:?}");
    }

    #[test]
    fn default_categories_seed_keep_maybe_reject() {
        let defaults = default_categories();
        assert_eq!(defaults.len(), 3);
        assert_eq!(defaults[0].display_name, "Keep");
        assert_eq!(defaults[0].folder_name, "keep");
        assert_eq!(defaults[0].shortcut, "1");
        assert_eq!(defaults[1].shortcut, "2");
        assert_eq!(defaults[2].shortcut, "3");
        assert!(validate_categories(&defaults).is_ok());
    }
}
