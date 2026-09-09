//! The account-label rule: allow UTF-8, and be done with it.
//!
//! Labels use the same lexical rule as session tags and host names: trim
//! padding, refuse control characters, and bound the bytes. Blank input has
//! no name. Credential storage owns the unnamed account's empty identity.

/// Maximum UTF-8 size of an account label.
pub const MAX_ACCOUNT_LABEL_BYTES: usize = 256;

/// Why an account label was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccountLabelError {
    /// The label contains a control character.
    Control,
    /// The trimmed label exceeds [`MAX_ACCOUNT_LABEL_BYTES`].
    TooLong {
        /// UTF-8 size of the refused label.
        bytes: usize,
    },
}

impl std::fmt::Display for AccountLabelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AccountLabelError::Control => {
                write!(f, "a single line, with no control characters")
            }
            AccountLabelError::TooLong { bytes } => write!(
                f,
                "at most {MAX_ACCOUNT_LABEL_BYTES} bytes of UTF-8, got {bytes}"
            ),
        }
    }
}

impl std::error::Error for AccountLabelError {}

/// Validate and normalize an account label as it arrives from a user.
///
/// `Ok(None)` means no name. Credential storage accepts the exact empty string
/// for an unnamed account, but refuses whitespace-only insertion. Selecting
/// Provider default is a separate operation, not label normalization.
/// Surrounding whitespace on a nonblank name is trimmed.
///
/// Control characters are refused rather than stripped: a label reaches a
/// terminal, and the newline and the escape in one are a rendering hazard
/// rather than a label. Refusing also keeps a rewritten label from claiming
/// to be something the user did not type.
pub fn normalize_account_label(label: &str) -> Result<Option<String>, AccountLabelError> {
    let trimmed = label.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.chars().any(char::is_control) {
        return Err(AccountLabelError::Control);
    }
    if trimmed.len() > MAX_ACCOUNT_LABEL_BYTES {
        return Err(AccountLabelError::TooLong {
            bytes: trimmed.len(),
        });
    }
    Ok(Some(trimmed.to_string()))
}

/// Whether an in-progress edit could still become a valid label.
///
/// Empty is a valid identity. All-space and edge-space buffers are legitimate
/// intermediate states while typing, so only the byte bound and the
/// control-character exclusion are checked. Submission applies the complete
/// [`normalize_account_label`] rule.
pub fn validate_account_label_edit(candidate: &str) -> Result<(), AccountLabelError> {
    if candidate.chars().any(char::is_control) {
        return Err(AccountLabelError::Control);
    }
    if candidate.len() > MAX_ACCOUNT_LABEL_BYTES {
        return Err(AccountLabelError::TooLong {
            bytes: candidate.len(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_label_is_trimmed_and_kept() {
        assert_eq!(normalize_account_label("  work  "), Ok(Some("work".into())));
    }

    #[test]
    fn blank_names_nothing() {
        for whitespace in ["", " ", "   ", "\t\n", "\u{2003}"] {
            assert_eq!(normalize_account_label(whitespace), Ok(None));
        }
    }

    #[test]
    fn an_edit_admits_intermediate_states_a_submission_refuses() {
        assert_eq!(validate_account_label_edit(""), Ok(()));
        assert_eq!(validate_account_label_edit("  draft "), Ok(()));
        assert_eq!(
            validate_account_label_edit("dr\u{7}aft"),
            Err(AccountLabelError::Control)
        );
        let long = "a".repeat(MAX_ACCOUNT_LABEL_BYTES + 1);
        assert_eq!(
            validate_account_label_edit(&long),
            Err(AccountLabelError::TooLong {
                bytes: MAX_ACCOUNT_LABEL_BYTES + 1
            })
        );
    }
}
