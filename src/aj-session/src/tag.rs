//! Session tags: user-set display metadata kept beside the log.
//!
//! A tag is session-scoped, not branch-scoped, so it lives in a sidecar file
//! rather than in the log: a head switch moves the session's history and must
//! not move its label (spec 6.8). Untagged sessions have no file, which is
//! what keeps an untagged store free of tag reads.

use std::fmt;

/// Longest tag we store, in bytes.
///
/// A tag is a sidebar label in a 24-column strip, so anything near this is
/// already truncated on screen. The cap exists to bound the sidecar and the
/// row payload, not to fit the display.
pub const MAX_TAG_BYTES: usize = 80;

/// Why a tag was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TagError {
    /// Longer than [`MAX_TAG_BYTES`] after trimming.
    TooLong { bytes: usize },
    /// Carries a control character, which includes the newline that would
    /// make the sidecar ambiguous to read back.
    Control,
}

impl fmt::Display for TagError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TagError::TooLong { bytes } => write!(
                f,
                "a tag is at most {MAX_TAG_BYTES} bytes, this one is {bytes}"
            ),
            TagError::Control => write!(f, "a tag is a single line without control characters"),
        }
    }
}

impl std::error::Error for TagError {}

/// Validate and normalize a tag as it arrives from a user.
///
/// Returns `Ok(None)` for anything that clears the tag, which is what an empty
/// string means on the wire (spec 6.6), so a caller can treat set and clear as
/// one path. Surrounding whitespace is trimmed, because a tag that differs from
/// another only by padding reads as the same label.
///
/// Rejects control characters rather than stripping them: a tag is a label the
/// user typed, and silently rewriting it would leave them with something they
/// did not ask for. The newline matters most, since the sidecar is one line.
pub fn normalize_tag(tag: &str) -> Result<Option<String>, TagError> {
    let trimmed = tag.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.chars().any(char::is_control) {
        return Err(TagError::Control);
    }
    if trimmed.len() > MAX_TAG_BYTES {
        return Err(TagError::TooLong {
            bytes: trimmed.len(),
        });
    }
    Ok(Some(trimmed.to_string()))
}

/// Read a tag back from a sidecar's contents.
///
/// Tolerant where [`normalize_tag`] is strict: the file is ours, but it may
/// have been hand-edited or half-written, and a directory listing must not fail
/// over one unreadable label. A body that does not normalize reads as no tag.
pub fn tag_from_sidecar(contents: &str) -> Option<String> {
    // Only the first line, so a hand-appended second line cannot smuggle
    // anything into the row.
    let first = contents.lines().next().unwrap_or_default();
    normalize_tag(first).ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tag_is_trimmed_and_kept() {
        assert_eq!(normalize_tag("  fix-auth  "), Ok(Some("fix-auth".into())));
    }

    #[test]
    fn emptiness_in_any_form_clears() {
        for input in ["", "   ", "\t", " \n "] {
            assert_eq!(
                normalize_tag(input),
                Ok(None),
                "{input:?} asks for the tag to be cleared",
            );
        }
    }

    /// A newline inside the tag is the case the sidecar's one-line shape cannot
    /// represent, so it is refused rather than truncated.
    #[test]
    fn control_characters_are_refused() {
        assert_eq!(normalize_tag("two\nlines"), Err(TagError::Control));
        assert_eq!(normalize_tag("bell\u{7}"), Err(TagError::Control));
        assert_eq!(normalize_tag("tab\tinside"), Err(TagError::Control));
    }

    #[test]
    fn an_overlong_tag_is_refused_by_bytes_not_characters() {
        let long = "a".repeat(MAX_TAG_BYTES + 1);
        assert_eq!(
            normalize_tag(&long),
            Err(TagError::TooLong {
                bytes: MAX_TAG_BYTES + 1
            }),
        );
        assert!(normalize_tag(&"a".repeat(MAX_TAG_BYTES)).is_ok());

        // Multi-byte characters count for what they cost on disk.
        let wide = "é".repeat(MAX_TAG_BYTES / 2 + 1);
        assert!(wide.chars().count() <= MAX_TAG_BYTES, "fits by characters");
        assert!(
            matches!(normalize_tag(&wide), Err(TagError::TooLong { .. })),
            "but not by bytes",
        );
    }

    /// The trim happens before the length check, so padding cannot push an
    /// otherwise legal tag over the cap.
    #[test]
    fn padding_does_not_count_towards_the_cap() {
        let padded = format!("  {}  ", "a".repeat(MAX_TAG_BYTES));
        assert!(padded.len() > MAX_TAG_BYTES);
        assert!(normalize_tag(&padded).is_ok());
    }

    #[test]
    fn a_sidecar_yields_its_first_line() {
        assert_eq!(tag_from_sidecar("fix-auth\n"), Some("fix-auth".into()));
        assert_eq!(
            tag_from_sidecar("fix-auth\nleftover\n"),
            Some("fix-auth".into()),
            "a second line is not part of the tag",
        );
    }

    /// A sidecar we cannot make sense of reads as untagged. It must not fail a
    /// directory listing.
    #[test]
    fn an_unusable_sidecar_reads_as_untagged() {
        assert_eq!(tag_from_sidecar(""), None);
        assert_eq!(tag_from_sidecar("   \n"), None);
        assert_eq!(tag_from_sidecar(&"a".repeat(MAX_TAG_BYTES + 1)), None);
    }
}
