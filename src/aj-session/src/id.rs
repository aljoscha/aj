//! The session-id grammar the store guarantees and callers may rely on.
//!
//! A session id is a filename stem: it becomes
//! `<sessions_dir>/<id>.jsonl` for the log and `<sessions_dir>/locks/<id>.lock`
//! for the advisory lock. An id that arrives from outside this process must
//! therefore be checked before it reaches either, or a peer could name a file
//! outside the store, and taking a lock would create the directories to go
//! with it.
//!
//! Every store entry point that accepts an id from a caller enforces this:
//! [`ConversationPersistence::session_metadata`],
//! [`ConversationPersistence::is_current_format`],
//! [`ConversationLog::resume`], and the lock path itself, which is the one
//! join whose callers do not already hold an enumerated id.
//!
//! [`ConversationPersistence::session_metadata`]: crate::persistence::ConversationPersistence::session_metadata
//! [`ConversationPersistence::is_current_format`]: crate::persistence::ConversationPersistence::is_current_format
//! [`ConversationLog::resume`]: crate::log::ConversationLog::resume

/// Longest session id the store accepts.
///
/// Minted ids are 23 characters (`%Y-%m-%d-%H-%M-%S-%3f`) plus at most a
/// `_999` collision suffix. The bound is generous next to that and still
/// leaves room for the `.jsonl` suffix under every filesystem's name limit.
const MAX_LEN: usize = 128;

/// Whether `id` is a well-formed session id.
///
/// The grammar is non-empty, at most [`MAX_LEN`] bytes, ASCII alphanumerics
/// plus `-` and `_`. That admits every id [`ConversationLog::create`] mints
/// (a millisecond timestamp with an optional `_N` collision suffix) and rules
/// out path traversal categorically rather than by enumerating the sequences
/// that would be dangerous: with no `.`, no `/` and no `\` in the alphabet,
/// there is no `..`, no separator, and no second extension to smuggle.
///
/// [`ConversationLog::create`]: crate::log::ConversationLog::create
pub fn is_valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_LEN
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::ConversationPersistence;

    /// Whatever the minting path produces is, by construction, a valid id.
    #[test]
    fn a_minted_id_is_valid() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        for _ in 0..4 {
            let log = crate::log::ConversationLog::create(&persistence).expect("create");
            assert!(
                is_valid_session_id(log.session_id()),
                "minted {:?} is not a valid id",
                log.session_id(),
            );
        }
    }

    #[test]
    fn the_collision_suffix_is_valid() {
        assert!(is_valid_session_id("2026-08-06-10-15-30-123"));
        assert!(is_valid_session_id("2026-08-06-10-15-30-123_7"));
    }

    /// Nothing that could leave the sessions directory, or name a file in it
    /// that is not a log, gets through.
    #[test]
    fn traversal_and_separators_are_rejected() {
        for id in [
            "",
            ".",
            "..",
            "../secrets",
            "a/b",
            "a\\b",
            "/etc/passwd",
            "session.jsonl",
            ".hidden",
            "with space",
            "quote\"",
            "null\0byte",
            "newline\n",
            "unicode-é",
        ] {
            assert!(!is_valid_session_id(id), "{id:?} should be rejected");
        }
    }

    /// The bound is part of the grammar, so an id one byte over it is not a
    /// session id however well formed the rest of it is.
    #[test]
    fn an_overlong_id_is_rejected() {
        assert!(is_valid_session_id(&"a".repeat(MAX_LEN)));
        assert!(!is_valid_session_id(&"a".repeat(MAX_LEN + 1)));
    }
}
