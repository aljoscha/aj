//! The account-label rule: allow UTF-8, and be done with it.
//!
//! A label is presentation, not protocol. Text rendering handles arbitrary
//! Unicode everywhere else in aj, so the only judgments made here are the ones
//! every label site shares: trim the padding, require at least one character,
//! refuse control characters because they are terminal escape hazards rather
//! than characters of a name, and bound the bytes.

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
/// `Ok(None)` covers everything that names nothing. An account cannot exist
/// without a label, so callers refuse that case with their own words rather
/// than receiving a synthesized one. Surrounding whitespace is trimmed,
/// because a label that differs from another only by padding reads as the
/// same label.
///
/// Control characters are refused rather than stripped: a label reaches a
/// terminal, and the newline and the escape in one are a rendering hazard
/// rather than a label. Refusing also keeps a rewritten label from claiming
/// to be something the user did not type.
///
/// Deliberately the same rule as a session tag's (`aj_session::normalize_tag`)
/// and a host name's (`aj_wire::normalize_host_name`) over a field with a
/// different owner. The three crates are siblings with no edges between them,
/// so the rule is stated three times on purpose. They are free to diverge,
/// and a change to what a label may contain is worth making in all three.
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
/// Tolerant where [`normalize_account_label`] is strict: empty, all-space,
/// and edge-space buffers are legitimate intermediate states while typing,
/// so only the two properties no later keystroke can repair are checked, the
/// byte bound and the control-character exclusion. Submission applies the
/// complete rule.
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
        assert_eq!(normalize_account_label("   "), Ok(None));
        assert_eq!(normalize_account_label(""), Ok(None));
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
