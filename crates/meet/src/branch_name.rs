//! Branch names from action-item titles. Git refuses most punctuation and every space, and a
//! title like "Add a `--json` flag to `meet record`" should land on `add-a-json-flag-to-meet-record`.

/// Lowercase ASCII words joined by single hyphens, at most `MAX_LEN` characters, cut on a
/// word boundary. Empty input becomes `action-item`.
pub fn slugify(title: &str) -> String {
    const MAX_LEN: usize = 48;
    let mut out = String::new();
    let mut pending_hyphen = false;
    for ch in title.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_hyphen && !out.is_empty() {
                out.push('-');
            }
            pending_hyphen = false;
            out.push(ch.to_ascii_lowercase());
        } else {
            pending_hyphen = true;
        }
    }
    if out.len() > MAX_LEN {
        let cut = out[..MAX_LEN].rfind('-').unwrap_or(MAX_LEN);
        out.truncate(cut);
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "action-item".to_string()
    } else {
        out
    }
}

/// `base`, or `base-2`, `base-3`, … — the first name `taken` says no to.
pub fn unique(base: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(base) {
        return base.to_string();
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|c| !taken(c))
        .expect("an infinite range yields an untaken name")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_become_git_safe_slugs() {
        assert_eq!(
            slugify("Add a `--json` flag to `meet record`"),
            "add-a-json-flag-to-meet-record"
        );
        assert_eq!(slugify("  Fix: crash on stop!!  "), "fix-crash-on-stop");
        assert_eq!(slugify("Ünïcode → ascii"), "n-code-ascii");
        assert_eq!(slugify("///"), "action-item");
        assert_eq!(slugify(""), "action-item");
    }

    #[test]
    fn long_titles_are_cut_on_a_word_boundary() {
        let s = slugify("Rewrite the entire transcription pipeline so that it never drops a single buffer again ever");
        assert!(s.len() <= 48, "{s}");
        assert!(!s.ends_with('-'));
        assert!(s.starts_with("rewrite-the-entire-transcription-pipeline"));
    }

    #[test]
    fn unique_suffixes_until_free() {
        let taken = ["x".to_string(), "x-2".to_string()];
        assert_eq!(unique("x", |c| taken.iter().any(|t| t == c)), "x-3");
        assert_eq!(unique("y", |c| taken.iter().any(|t| t == c)), "y");
    }
}
