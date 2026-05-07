//! Shared utilities for the round-trip test suite.
//!
//! Two responsibilities:
//!
//! - **SSE wire-format parsing.** Captured fixtures live as `event:` /
//!   `data:` framed text. We slice them into per-event byte buffers and
//!   leave per-provider JSON parsing to the caller (the `data:` payload
//!   shape is provider-specific).
//! - **Fixture lookup.** Each provider's tests live next to a
//!   `fixtures/<api>/` directory; this module resolves paths relative to
//!   `CARGO_MANIFEST_DIR` so tests work both under `cargo test` and the
//!   IDE's per-test runner.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// Resolve a fixture path under `tests/roundtrip/fixtures/`.
///
/// `CARGO_MANIFEST_DIR` points at `src/aj-models/`, so the fixture
/// directory is at a fixed offset from there.
pub fn fixture_path(relative: impl AsRef<Path>) -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .join("tests")
        .join("roundtrip")
        .join("fixtures")
        .join(relative.as_ref())
}

/// Read a UTF-8 fixture file, panicking with the path on failure (so
/// missing-fixture errors point at the offender directly).
pub fn read_fixture(relative: impl AsRef<Path>) -> String {
    let path = fixture_path(relative);
    fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read fixture {}: {err}", path.display()))
}

/// Read a JSON fixture and parse it into a [`serde_json::Value`].
///
/// Used for the serialize side of the round-trip — golden files live as
/// JSON we compare value-equal against (rather than byte-equal) so
/// formatting and key ordering don't break the assertion.
pub fn read_fixture_json(relative: impl AsRef<Path>) -> Value {
    let raw = read_fixture(relative);
    serde_json::from_str(&raw).expect("fixture is valid JSON")
}

/// One frame in an SSE wire dump.
///
/// `event` is the value of the `event:` line (or `"message"` if the
/// frame omits one, per the SSE spec default). `data` is the
/// concatenated `data:` lines of the frame, joined with `\n` per the
/// SSE spec.
#[derive(Clone, Debug)]
pub struct SseFrame {
    pub event: String,
    pub data: String,
}

/// Parse SSE wire text into a sequence of frames.
///
/// Implements the subset of the SSE grammar (RFC W3C Server-Sent
/// Events) we use in fixtures:
///
/// - Frames are separated by blank lines (`\n\n`).
/// - Lines beginning with `:` are comments and ignored.
/// - `event:` and `data:` lines populate the matching field; the value
///   is the substring after the colon, with at most one leading space
///   stripped (matching the SDK's wire reader).
/// - Frames whose `data:` field is empty are skipped — Anthropic's
///   `ping` events arrive as a wire frame even though there's nothing
///   to dispatch.
///
/// Other field names (`id:`, `retry:`) are ignored — fixtures don't
/// use them and the live providers don't either.
pub fn parse_sse(text: &str) -> Vec<SseFrame> {
    // Normalize CRLF → LF so fixtures committed from Windows still parse
    // and so our blank-line splitter picks up paragraph boundaries.
    let normalized = text.replace("\r\n", "\n");
    let mut frames = Vec::new();
    for block in normalized.split("\n\n") {
        let mut event = String::new();
        let mut data_lines: Vec<&str> = Vec::new();
        for line in block.lines() {
            if line.is_empty() || line.starts_with(':') {
                continue;
            }
            if let Some(rest) = line.strip_prefix("event:") {
                event = strip_optional_space(rest).to_string();
            } else if let Some(rest) = line.strip_prefix("data:") {
                data_lines.push(strip_optional_space(rest));
            }
            // Other field names ignored.
        }
        if data_lines.is_empty() {
            continue;
        }
        frames.push(SseFrame {
            event: if event.is_empty() {
                "message".to_string()
            } else {
                event
            },
            data: data_lines.join("\n"),
        });
    }
    frames
}

fn strip_optional_space(s: &str) -> &str {
    s.strip_prefix(' ').unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sse_single_frame() {
        let text = "event: foo\ndata: {\"k\":1}\n\n";
        let frames = parse_sse(text);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event, "foo");
        assert_eq!(frames[0].data, "{\"k\":1}");
    }

    #[test]
    fn parse_sse_multiple_frames_and_comments() {
        let text = "\
: comment
event: a
data: 1

event: b
data: 2
data: 3

";
        let frames = parse_sse(text);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].event, "a");
        assert_eq!(frames[0].data, "1");
        assert_eq!(frames[1].event, "b");
        assert_eq!(frames[1].data, "2\n3");
    }

    #[test]
    fn parse_sse_empty_data_frames_skipped() {
        // Anthropic emits `event: ping` frames with no `data:` line —
        // they should not produce a frame in our output.
        let text = "event: ping\n\n";
        assert!(parse_sse(text).is_empty());
    }
}
