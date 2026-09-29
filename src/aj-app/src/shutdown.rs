//! Frontend-agnostic resume hint. Frontends own styling and rendering.

/// Build the resume-hint line for the given session id.
///
/// Returns plain text for the caller to style and print.
pub fn format_resume_hint(session_id: &str) -> String {
    format!("Session: {session_id} (resume with: aj continue {session_id})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_resume_hint_round_trips_session_id() {
        let hint = format_resume_hint("abc123");
        assert_eq!(hint, "Session: abc123 (resume with: aj continue abc123)");
    }
}
