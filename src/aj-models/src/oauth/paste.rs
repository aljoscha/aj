//! Parsing of user-pasted OAuth authorization input.
//!
//! Shared by both provider flows: when the browser can't reach the
//! local callback server (SSH, headless), the user pastes either the
//! full redirect URL or a bare code, and we extract `(code, state)`
//! from whatever shape they gave us.

/// Result of parsing user-pasted manual input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ParsedAuth {
    pub code: Option<String>,
    pub state: Option<String>,
}

/// Parse a string the user pasted in response to the manual-code
/// prompt. Tolerates four common shapes:
///
/// - A full redirect URL (`http://localhost:1455/auth/callback?code=X&state=Y`)
/// - The `code=X&state=Y` query fragment alone
/// - The `code#state` shorthand the upstream sometimes presents
/// - A bare authorization code (no state)
///
/// Empty input returns both fields as `None` so the caller can fall
/// through to the prompt fallback.
pub(super) fn parse_authorization_input(input: &str) -> ParsedAuth {
    let value = input.trim();
    if value.is_empty() {
        return ParsedAuth {
            code: None,
            state: None,
        };
    }

    // Full URL form.
    if let Ok(url) = reqwest::Url::parse(value) {
        let mut code = None;
        let mut state = None;
        for (k, v) in url.query_pairs() {
            match k.as_ref() {
                "code" => code = Some(v.into_owned()),
                "state" => state = Some(v.into_owned()),
                _ => {}
            }
        }
        if code.is_some() || state.is_some() {
            return ParsedAuth { code, state };
        }
    }

    // `code#state` shorthand.
    if let Some((code, state)) = value.split_once('#')
        && !code.is_empty()
        && !state.is_empty()
    {
        return ParsedAuth {
            code: Some(code.to_string()),
            state: Some(state.to_string()),
        };
    }

    // Bare query string. We re-parse via `reqwest::Url` so percent
    // decoding and `+` handling match the URL form above.
    if value.contains("code=")
        && let Ok(url) = reqwest::Url::parse(&format!("http://localhost?{value}"))
    {
        let mut code = None;
        let mut state = None;
        for (k, v) in url.query_pairs() {
            match k.as_ref() {
                "code" => code = Some(v.into_owned()),
                "state" => state = Some(v.into_owned()),
                _ => {}
            }
        }
        if code.is_some() {
            return ParsedAuth { code, state };
        }
    }

    // Bare code, no state.
    ParsedAuth {
        code: Some(value.to_string()),
        state: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `parse_authorization_input` must accept all four supported input
    /// shapes and return the right combination of fields. The cases
    /// live in one test so the contract is easy to read.
    #[test]
    fn parse_authorization_input_handles_all_shapes() {
        // Full URL.
        let parsed =
            parse_authorization_input("http://localhost:53692/callback?code=ABC&state=DEF");
        assert_eq!(parsed.code.as_deref(), Some("ABC"));
        assert_eq!(parsed.state.as_deref(), Some("DEF"));

        // Bare query string.
        let parsed = parse_authorization_input("code=ABC&state=DEF");
        assert_eq!(parsed.code.as_deref(), Some("ABC"));
        assert_eq!(parsed.state.as_deref(), Some("DEF"));

        // `code#state` shorthand.
        let parsed = parse_authorization_input("ABC#DEF");
        assert_eq!(parsed.code.as_deref(), Some("ABC"));
        assert_eq!(parsed.state.as_deref(), Some("DEF"));

        // Bare code, no state.
        let parsed = parse_authorization_input("ABC123");
        assert_eq!(parsed.code.as_deref(), Some("ABC123"));
        assert!(parsed.state.is_none());

        // Empty input.
        let parsed = parse_authorization_input("   ");
        assert!(parsed.code.is_none());
        assert!(parsed.state.is_none());
    }
}
