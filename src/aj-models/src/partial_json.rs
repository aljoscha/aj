//! Partial / streaming JSON parsing.
//!
//! Tool-call arguments arrive as a stream of bytes that's only valid JSON
//! once the final delta lands. UIs and the agent loop want a usable
//! `Value` snapshot every time we get more bytes — even when the stream is
//! mid-string or has unmatched brackets.
//!
//! [`parse_streaming_json`] is the public entry point. It tries strict
//! [`serde_json::from_str`] first, then escalates through a small chain of
//! repair / completion strategies, and falls back to an empty object so
//! callers always receive a value.

use serde_json::Value;

/// Parse potentially incomplete JSON.
///
/// Returns the most complete value we can recover. Falls back to an empty
/// object (`{}`) when no strategy succeeds — never panics, never returns
/// `null`. The empty-object fallback matches the streaming-tool-call
/// invariant: callers always observe a
/// usable `arguments` value, even before the final byte arrives.
///
/// Strategy chain:
///
/// 1. Strict [`serde_json::from_str`] of the input as-is.
/// 2. [`repair_json`] to escape stray control chars / fix invalid
///    backslash escapes, then strict parse.
/// 3. [`complete_partial_json`] — close unmatched strings, objects, and
///    arrays; trim dangling commas / colons — then strict parse.
/// 4. Repair + complete combined, then strict parse.
/// 5. Empty object.
pub fn parse_streaming_json(input: &str) -> Value {
    if input.trim().is_empty() {
        return Value::Object(serde_json::Map::new());
    }

    // 1. Strict parse.
    if let Ok(v) = serde_json::from_str::<Value>(input) {
        return v;
    }

    // 2. Repair + strict parse. Skip if `repair_json` was a no-op so we
    //    don't pay for a redundant parse on the common path where the
    //    input only has structural (not lexical) damage.
    let repaired = repair_json(input);
    if repaired != input
        && let Ok(v) = serde_json::from_str::<Value>(&repaired)
    {
        return v;
    }

    // 3. Complete (close brackets etc.) + strict parse.
    let completed = complete_partial_json(input);
    if let Ok(v) = serde_json::from_str::<Value>(&completed) {
        return v;
    }

    // 4. Repair, then complete, then parse — handles inputs that have
    //    *both* lexical damage (control chars, bad escapes) and missing
    //    closers.
    let completed_repaired = complete_partial_json(&repaired);
    if let Ok(v) = serde_json::from_str::<Value>(&completed_repaired) {
        return v;
    }

    // 5. Total failure — empty object so callers can still render.
    Value::Object(serde_json::Map::new())
}

/// JSON characters that may follow a backslash inside a string literal.
const VALID_JSON_ESCAPES: &[char] = &['"', '\\', '/', 'b', 'f', 'n', 'r', 't', 'u'];

/// Repair common malformations inside JSON string literals.
///
/// - Raw control characters (codepoint <= 0x1F) inside strings are
///   replaced with their `\\b` / `\\f` / `\\n` / `\\r` / `\\t` escape, or
///   a generic `\\u00XX` for other control codes.
/// - Backslashes followed by anything other than a JSON-legal escape
///   character (or a non-hex `\\u...` sequence) are doubled, so that a
///   stray `"\z"` becomes `"\\z"` rather than triggering a parse error.
/// - A trailing dangling backslash at end-of-input is doubled too, so we
///   don't leave the next parse stage thinking it's mid-escape.
///
/// Outside of strings the input is passed through unchanged. The intent
/// is to normalize bytes that the model emitted with imperfect JSON
/// hygiene; structural repairs (unclosed brackets etc.) are
/// [`complete_partial_json`]'s job.
pub fn repair_json(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut in_string = false;
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];

        if !in_string {
            out.push(c);
            if c == '"' {
                in_string = true;
            }
            i += 1;
            continue;
        }

        if c == '"' {
            out.push(c);
            in_string = false;
            i += 1;
            continue;
        }

        if c == '\\' {
            // Trailing backslash with no following char: double it so the
            // next parse stage doesn't see a dangling escape.
            let Some(&next) = chars.get(i + 1) else {
                out.push_str("\\\\");
                i += 1;
                continue;
            };

            if next == 'u' {
                // `\uXXXX` requires 4 hex digits. Keep it intact only if
                // all four are present; otherwise double the backslash so
                // the (possibly incomplete) sequence doesn't fail parse.
                let digits: String = chars
                    .iter()
                    .skip(i + 2)
                    .take(4)
                    .copied()
                    .collect::<String>();
                if digits.len() == 4 && digits.chars().all(|d| d.is_ascii_hexdigit()) {
                    out.push('\\');
                    out.push('u');
                    out.push_str(&digits);
                    i += 6;
                    continue;
                }
                // Fall through to "double the backslash" path.
            } else if VALID_JSON_ESCAPES.contains(&next) {
                out.push('\\');
                out.push(next);
                i += 2;
                continue;
            }

            // Invalid escape — treat the backslash as a literal.
            out.push_str("\\\\");
            i += 1;
            continue;
        }

        if is_control_character(c) {
            out.push_str(&escape_control_character(c));
        } else {
            out.push(c);
        }
        i += 1;
    }

    out
}

/// True for ASCII control characters (codepoint <= 0x1F) which must be
/// escaped inside a JSON string literal.
fn is_control_character(c: char) -> bool {
    u32::from(c) <= 0x1f
}

/// Render a control character as a JSON escape sequence.
fn escape_control_character(c: char) -> String {
    match c {
        '\u{08}' => "\\b".to_string(),
        '\u{0c}' => "\\f".to_string(),
        '\n' => "\\n".to_string(),
        '\r' => "\\r".to_string(),
        '\t' => "\\t".to_string(),
        other => format!("\\u{:04x}", u32::from(other)),
    }
}

/// Append the closing tokens needed to make `s` a syntactically complete
/// JSON value.
///
/// Tracks string state, escapes, and `{`/`[` nesting; trims trailing
/// whitespace and dangling commas; appends ` null` after a dangling `:`
/// so the object remains parseable. Doesn't try to repair partial
/// keywords (`tru`, `fals`, `nul`) or partial numbers — those fall
/// through to the empty-object fallback in [`parse_streaming_json`].
pub fn complete_partial_json(s: &str) -> String {
    let mut stack: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escape = false;
    let mut out = String::with_capacity(s.len() + 8);

    for c in s.chars() {
        out.push(c);
        if escape {
            escape = false;
            continue;
        }
        if in_string {
            match c {
                '\\' => escape = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' => {
                if let Some(top) = stack.last()
                    && *top == c
                {
                    stack.pop();
                }
            }
            _ => {}
        }
    }

    if in_string {
        out.push('"');
    }

    // Drop trailing whitespace and any dangling separator.
    while let Some(c) = out.chars().last() {
        if c.is_whitespace() {
            out.pop();
        } else {
            break;
        }
    }
    if out.ends_with(',') {
        out.pop();
    }
    // If we ended on `:` (no value yet for the current key), append a
    // null so the object remains parseable.
    if out.ends_with(':') {
        out.push_str(" null");
    }

    while let Some(closer) = stack.pop() {
        out.push(closer);
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ----- parse_streaming_json: structural completion -----

    #[test]
    fn empty_input_returns_empty_object() {
        let v = parse_streaming_json("");
        assert!(v.is_object());
        assert_eq!(v.as_object().unwrap().len(), 0);
    }

    #[test]
    fn whitespace_only_returns_empty_object() {
        let v = parse_streaming_json("   \n\t");
        assert!(v.is_object());
        assert_eq!(v.as_object().unwrap().len(), 0);
    }

    #[test]
    fn complete_object_parses_strict() {
        let v = parse_streaming_json("{\"a\": 1}");
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn unclosed_brace() {
        let v = parse_streaming_json("{\"a\": 1");
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn unclosed_string() {
        let v = parse_streaming_json("{\"a\": \"hel");
        assert_eq!(v["a"], "hel");
    }

    #[test]
    fn dangling_colon_yields_null_value() {
        let v = parse_streaming_json("{\"a\":");
        assert!(v["a"].is_null());
    }

    #[test]
    fn trailing_comma_is_dropped() {
        let v = parse_streaming_json("{\"a\": 1,");
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn nested_array_partial() {
        let v = parse_streaming_json("{\"xs\": [1, 2, 3");
        assert_eq!(v["xs"][2], 3);
    }

    #[test]
    fn pure_array_partial() {
        let v = parse_streaming_json("[1, 2, 3");
        assert_eq!(v[0], 1);
        assert_eq!(v[2], 3);
    }

    #[test]
    fn garbage_falls_back_to_empty_object() {
        let v = parse_streaming_json("not json");
        assert!(v.is_object());
        assert_eq!(v.as_object().unwrap().len(), 0);
    }

    // ----- parse_streaming_json: repair path -----

    #[test]
    fn repair_raw_newline_in_string() {
        let raw = "{\"msg\": \"line1\nline2\"}";
        let v = parse_streaming_json(raw);
        assert_eq!(v["msg"], "line1\nline2");
    }

    #[test]
    fn repair_raw_tab_in_string() {
        let raw = "{\"msg\": \"a\tb\"}";
        let v = parse_streaming_json(raw);
        assert_eq!(v["msg"], "a\tb");
    }

    #[test]
    fn repair_invalid_escape() {
        // `\z` is not a valid JSON escape; repair turns it into `\\z`.
        let raw = "{\"msg\": \"hello\\zworld\"}";
        let v = parse_streaming_json(raw);
        assert_eq!(v["msg"], "hello\\zworld");
    }

    #[test]
    fn repair_then_complete_combined() {
        // Both lexical (control char) and structural (unclosed) damage.
        let raw = "{\"msg\": \"line1\nline2";
        let v = parse_streaming_json(raw);
        assert_eq!(v["msg"], "line1\nline2");
    }

    #[test]
    fn repair_dangling_backslash() {
        // Trailing `\` with nothing after it would otherwise look like a
        // mid-escape; repair doubles it so parsing succeeds.
        let raw = "{\"path\": \"a\\";
        let v = parse_streaming_json(raw);
        // The repaired-then-completed value contains the literal
        // backslash; assert via the JSON string round-trip rather than
        // hard-coding the byte sequence.
        assert!(v["path"].as_str().unwrap().contains('\\'));
    }

    // ----- repair_json -----

    #[test]
    fn repair_outside_strings_is_passthrough() {
        let raw = "{\"a\":\n1}";
        // Newline outside a string is fine for JSON; repair shouldn't
        // touch it.
        assert_eq!(repair_json(raw), raw);
    }

    #[test]
    fn repair_keeps_valid_unicode_escapes() {
        let raw = "{\"a\": \"\\u00e9\"}";
        assert_eq!(repair_json(raw), raw);
    }

    #[test]
    fn repair_breaks_invalid_unicode_escape() {
        // `\u00z9` is malformed — repair should double the backslash.
        let raw = "{\"a\": \"\\u00z9\"}";
        let repaired = repair_json(raw);
        assert!(repaired.contains("\\\\u00z9"));
    }

    // ----- complete_partial_json -----

    #[test]
    fn complete_closes_object() {
        assert_eq!(complete_partial_json("{\"a\": 1"), "{\"a\": 1}");
    }

    #[test]
    fn complete_closes_string_then_object() {
        assert_eq!(complete_partial_json("{\"a\": \"hi"), "{\"a\": \"hi\"}");
    }

    #[test]
    fn complete_handles_dangling_colon() {
        assert_eq!(complete_partial_json("{\"a\":"), "{\"a\": null}");
    }
}
