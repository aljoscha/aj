//! Shared normalization and resolution for user-supplied filesystem paths.

use std::path::{Path, PathBuf};

use unicode_normalization::UnicodeNormalization;
use url::Url;

const NARROW_NO_BREAK_SPACE: char = '\u{202f}';

/// Resolve a user-supplied path against `cwd` without requiring it to exist.
///
/// A leading `@` is accepted because models sometimes preserve mention syntax
/// in tool arguments. Unicode space variants are normalized before expanding
/// `~` and local `file://` URLs.
fn resolve_path(input: &str, cwd: &Path) -> PathBuf {
    let normalized = normalize_input(input);
    let path = file_url_path(&normalized).unwrap_or_else(|| expand_tilde(&normalized));
    let joined = if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    };
    std::path::absolute(&joined).unwrap_or(joined)
}

/// Resolve a readable path, including common macOS filename variants.
pub fn resolve_read_path(input: &str, cwd: &Path) -> PathBuf {
    let resolved = resolve_path(input, cwd);
    if resolved.exists() {
        return resolved;
    }

    let mut variants = Vec::with_capacity(4);
    variants.push(mac_os_ampm_variant(&resolved));
    variants.push(unicode_variant(&resolved, |value| value.nfd().collect()));
    variants.push(unicode_variant(&resolved, |value| value.replace('\'', "’")));
    variants.push(unicode_variant(&variants[1], |value| {
        value.replace('\'', "’")
    }));

    variants
        .into_iter()
        .find(|variant| variant != &resolved && variant.exists())
        .unwrap_or(resolved)
}

fn normalize_input(input: &str) -> String {
    input
        .strip_prefix('@')
        .unwrap_or(input)
        .chars()
        .map(|ch| if is_unicode_space(ch) { ' ' } else { ch })
        .collect()
}

fn is_unicode_space(ch: char) -> bool {
    matches!(
        ch,
        '\u{00a0}' | '\u{2000}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
    )
}

fn expand_tilde(path: &str) -> PathBuf {
    let Some(home) = std::env::home_dir() else {
        return PathBuf::from(path);
    };
    if path == "~" {
        return home;
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home.join(rest);
    }
    PathBuf::from(path)
}

fn file_url_path(path: &str) -> Option<PathBuf> {
    if !path.starts_with("file://") {
        return None;
    }
    Url::parse(path).ok()?.to_file_path().ok()
}

fn mac_os_ampm_variant(path: &Path) -> PathBuf {
    unicode_variant(path, |value| {
        ["AM.", "Am.", "aM.", "am.", "PM.", "Pm.", "pM.", "pm."]
            .into_iter()
            .fold(value.to_string(), |value, suffix| {
                value.replace(
                    &format!(" {suffix}"),
                    &format!("{NARROW_NO_BREAK_SPACE}{suffix}"),
                )
            })
    })
}

fn unicode_variant(path: &Path, transform: impl FnOnce(&str) -> String) -> PathBuf {
    let Some(value) = path.to_str() else {
        return path.to_path_buf();
    };
    PathBuf::from(transform(value))
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn resolves_relative_tilde_and_file_url_paths() {
        let cwd = tempdir().expect("tempdir");
        assert_eq!(
            resolve_path("@note.txt", cwd.path()),
            cwd.path().join("note.txt")
        );
        assert_eq!(
            resolve_path("@@note.txt", cwd.path()),
            cwd.path().join("@note.txt")
        );

        let file = cwd.path().join("with space.txt");
        let url = Url::from_file_path(&file).expect("file URL");
        assert_eq!(resolve_path(url.as_str(), cwd.path()), file);

        if let Some(home) = std::env::home_dir() {
            assert_eq!(resolve_path("~", cwd.path()), home);
            assert_eq!(
                resolve_path("~/note.txt", cwd.path()),
                home.join("note.txt")
            );
        }
    }

    #[test]
    fn normalizes_unicode_spaces() {
        let cwd = tempdir().expect("tempdir");
        assert_eq!(
            resolve_path("hello\u{00a0}world.txt", cwd.path()),
            cwd.path().join("hello world.txt")
        );
    }

    #[test]
    fn finds_mac_os_filename_variants() {
        let cwd = tempdir().expect("tempdir");
        let screenshot = cwd
            .path()
            .join(format!("Screenshot 1.00{NARROW_NO_BREAK_SPACE}PM.png"));
        std::fs::write(&screenshot, "image").expect("write");
        assert_eq!(
            resolve_read_path("Screenshot 1.00 PM.png", cwd.path()),
            screenshot
        );

        let lowercase = cwd
            .path()
            .join(format!("Screenshot 2.00{NARROW_NO_BREAK_SPACE}pm.png"));
        std::fs::write(&lowercase, "image").expect("write");
        assert_eq!(
            resolve_read_path("Screenshot 2.00 pm.png", cwd.path()),
            lowercase
        );

        let curly = cwd.path().join("Capture d’écran.txt");
        std::fs::write(&curly, "text").expect("write");
        assert_eq!(resolve_read_path("Capture d'écran.txt", cwd.path()), curly);

        let decomposed = cwd.path().join("cafe\u{301}.txt");
        std::fs::write(&decomposed, "text").expect("write");
        assert_eq!(resolve_read_path("café.txt", cwd.path()), decomposed);
    }
}
