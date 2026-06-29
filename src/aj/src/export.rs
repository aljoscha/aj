//! Self-contained HTML export of a conversation session.
//!
//! [`render_session_html`] assembles a single static HTML document that
//! renders the session entirely client-side. The exporter ships the
//! whole session as JSON plus a vendored renderer: it does not render
//! the transcript server-side. The browser parses the embedded entries
//! and builds the view (messages, tool results, sub-agent runs,
//! markdown, syntax highlighting), so the same file is both a readable
//! transcript and the lossless source data.
//!
//! What gets embedded:
//! - the verbatim on-disk entries (`ConversationEntry`), under a small
//!   envelope carrying the session id and the active user-thread leaf,
//! - the page renderer (`template.js`),
//! - `marked` (markdown) and `highlight.js` (syntax highlighting),
//!   vendored under `assets/export/vendor` (see its `PROVENANCE.md`).
//!
//! Security: the JSON island lives in a `<script type="application/json">`
//! block, and every `<` is rewritten to `\u003c` so the payload cannot
//! open or close a tag (not just `</script>`, but also `<!--`/`<script`,
//! which flip the HTML script-data tokenizer). `JSON.parse` restores it
//! in the browser. The renderer treats raw HTML in prose as inert text
//! and restricts link/image URLs to a scheme allow-list, so a shared
//! transcript cannot inject markup or scripts.

use aj_agent::events::AgentEvent;
use aj_models::types::{Message, UserContent};
use aj_session::{ConversationEntry, ConversationLog, EntryId, ThreadFilter, replay};
use serde::Serialize;

/// The HTML shell with `{{KEY}}` placeholders, filled by
/// [`fill_template`].
const TEMPLATE: &str = include_str!("../assets/export/template.html");
const CSS: &str = include_str!("../assets/export/template.css");
const APP_JS: &str = include_str!("../assets/export/template.js");
const MARKED_JS: &str = include_str!("../assets/export/vendor/marked.min.js");
const HIGHLIGHT_JS: &str = include_str!("../assets/export/vendor/highlight.min.js");

/// Full license texts for the vendored libraries, embedded in the
/// export so every shared copy carries the notices both licenses
/// (MIT, BSD-3-Clause) require to travel with a redistribution.
const MARKED_LICENSE: &str = include_str!("../assets/export/vendor/marked.LICENSE");
const HIGHLIGHT_LICENSE: &str = include_str!("../assets/export/vendor/highlight.LICENSE");

/// The embedded session envelope. Entries serialize verbatim (snake_case
/// fields, `type`/`role`/`kind` tags as on disk); the renderer reads
/// them directly.
#[derive(Serialize)]
struct ExportData<'a> {
    session_id: &'a str,
    /// The active user-thread tip, so the page opens on the same branch
    /// a resumed session would. `None` for a session with no user
    /// messages yet.
    leaf_id: Option<EntryId>,
    entries: Vec<&'a ConversationEntry>,
}

/// Render a whole session to a self-contained HTML document.
///
/// Pure over the log: it reads but never mutates, so it is safe to call
/// while a turn is in flight.
pub(crate) fn render_session_html(log: &ConversationLog) -> String {
    let title = derive_title(log)
        .map(|t| truncate_title(&t))
        .unwrap_or_else(|| "aj session".to_string());

    let data = ExportData {
        session_id: log.session_id(),
        leaf_id: log.latest_leaf(ThreadFilter::USER),
        entries: log.entries_in_order(),
    };
    let session_data = embed_json(&data);
    let licenses = format!(
        "marked (MIT) https://github.com/markedjs/marked\n\n{MARKED_LICENSE}\n\n\
         highlight.js (BSD-3-Clause) https://github.com/highlightjs/highlight.js\n\n{HIGHLIGHT_LICENSE}"
    );

    // The untrusted values (title, session JSON) are filled in the same
    // single pass as the trusted assets, and `fill_template` never
    // re-scans what it inserts, so a prompt that contains a literal
    // `{{...}}` cannot be reinterpreted as a placeholder.
    fill_template(
        TEMPLATE,
        &[
            ("TITLE", &escape(&title)),
            ("CSS", CSS),
            ("MARKED_JS", MARKED_JS),
            ("HIGHLIGHT_JS", HIGHLIGHT_JS),
            ("APP_JS", APP_JS),
            ("LICENSES", &licenses),
            ("SESSION_DATA", &session_data),
        ],
    )
}

/// Replace `{{KEY}}` placeholders in one left-to-right pass.
///
/// Inserted values are never re-scanned, so untrusted content cannot
/// introduce new placeholders. An unknown `{{...}}` is emitted verbatim
/// rather than dropped, which surfaces a typo'd placeholder instead of
/// silently blanking it.
fn fill_template(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len() + 16 * 1024);
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("}}") {
            Some(end) => {
                let key = &after[..end];
                match vars.iter().find(|(k, _)| *k == key) {
                    Some((_, value)) => out.push_str(value),
                    None => {
                        out.push_str("{{");
                        out.push_str(key);
                        out.push_str("}}");
                    }
                }
                rest = &after[end + 2..];
            }
            None => {
                out.push_str("{{");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Serialize the export envelope and neutralize every `<` so the JSON is
/// inert inside its surrounding `<script>` element. In JSON `<` only
/// appears in string values, where `\u003c` is an equivalent escape, so
/// the payload stays valid and parses back unchanged.
fn embed_json(data: &ExportData) -> String {
    serde_json::to_string(data)
        .unwrap_or_default()
        .replace('<', "\\u003c")
}

/// The first user prompt's text, used for the page `<title>`. Derived
/// from the replay stream so it tracks whatever the live view would show
/// as the opening message.
fn derive_title(log: &ConversationLog) -> Option<String> {
    for event in replay(log) {
        if let AgentEvent::MessageEnd { message, .. } = event
            && let Some(Message::User(u)) = message.as_wire()
            && let Some(text) = first_text(&u.content)
        {
            return Some(text);
        }
    }
    None
}

/// The first non-empty text block of a message.
fn first_text(content: &[UserContent]) -> Option<String> {
    content.iter().find_map(|c| match c {
        UserContent::Text(t) if !t.text.trim().is_empty() => Some(t.text.clone()),
        _ => None,
    })
}

/// Collapse a prompt to a single-line title, capped at 80 characters.
fn truncate_title(text: &str) -> String {
    let line = text.split('\n').next().unwrap_or(text).trim();
    if line.chars().count() > 80 {
        let truncated: String = line.chars().take(80).collect();
        format!("{truncated}\u{2026}")
    } else {
        line.to_string()
    }
}

/// Escape the five characters that are unsafe in HTML text or
/// double-quoted attribute values.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::fs;

    use aj_session::{ConversationLog, ConversationPersistence};
    use tempfile::tempdir;

    use super::*;

    /// Open a log from a JSONL fixture written into a temp sessions
    /// directory, exercising the same `resume` path the binary uses.
    fn log_from_jsonl(lines: &[&str]) -> (tempfile::TempDir, ConversationLog) {
        let dir = tempdir().expect("tempdir");
        let id = "test-session";
        fs::write(dir.path().join(format!("{id}.jsonl")), lines.join("\n")).expect("write fixture");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let log = ConversationLog::resume(&persistence, id).expect("resume fixture");
        (dir, log)
    }

    /// The neutralized JSON payload from the embedded data island.
    fn data_island(html: &str) -> &str {
        html.split_once("id=\"session-data\">")
            .and_then(|(_, rest)| rest.split_once("</script>"))
            .map(|(payload, _)| payload)
            .expect("data island present")
    }

    const SYSTEM: &str = r#"{"id":"root0001","timestamp":"2024-01-01T00:00:00Z","thread":"meta","type":"system_prompt","text":"You are aj."}"#;
    const USER: &str = r#"{"id":"u0000001","parent_id":"root0001","timestamp":"2024-01-01T00:00:01Z","thread":"user","type":"message","message":{"role":"user","content":[{"type":"text","text":"Hello **world**"}],"timestamp":1704067201000}}"#;
    const ASSISTANT: &str = r#"{"id":"a0000001","parent_id":"u0000001","timestamp":"2024-01-01T00:00:02Z","thread":"user","type":"message","message":{"role":"assistant","content":[{"type":"text","text":"Reading the file."},{"type":"tool_call","id":"call-1","name":"read_file","arguments":{"path":"/tmp/x"}}],"api":"anthropic-messages","provider":"anthropic","model":"claude-test","usage":{"input":10,"output":5,"cache_read":0,"cache_write":0,"total_tokens":15,"cost":{"input":0.0,"output":0.0,"cache_read":0.0,"cache_write":0.0,"total":0.0}},"stop_reason":"ToolUse","timestamp":1704067202000}}"#;
    const TOOL_RESULT: &str = r#"{"id":"t0000001","parent_id":"a0000001","timestamp":"2024-01-01T00:00:03Z","thread":"user","type":"message","message":{"role":"tool_result","tool_call_id":"call-1","tool_name":"read_file","content":[{"type":"text","text":"the file body"}],"details":{"kind":"text","summary":"read_file /tmp/x","body":"the file body"},"is_error":false,"timestamp":1704067203000}}"#;

    #[test]
    fn escapes_html_special_chars() {
        assert_eq!(escape("a<b>&\"'"), "a&lt;b&gt;&amp;&quot;&#39;");
    }

    #[test]
    fn fill_template_is_single_pass() {
        // A value that itself looks like a placeholder must be inserted
        // verbatim, not expanded by a later key.
        let out = fill_template("[{{A}}][{{B}}]", &[("A", "{{B}}"), ("B", "x")]);
        assert_eq!(out, "[{{B}}][x]");
        // Unknown placeholders survive so a typo is visible.
        assert_eq!(fill_template("{{NOPE}}", &[]), "{{NOPE}}");
    }

    #[test]
    fn assembles_self_contained_document() {
        let (_dir, log) = log_from_jsonl(&[SYSTEM, USER, ASSISTANT, TOOL_RESULT]);
        let html = render_session_html(&log);

        assert!(html.starts_with("<!DOCTYPE html>"));
        // Every placeholder is filled.
        assert!(!html.contains("{{"), "unfilled placeholder remains");
        // Renderer and vendored libraries are inlined.
        assert!(html.contains("marked"), "marked not embedded");
        assert!(html.contains("hljs"), "highlight.js not embedded");
        assert!(html.contains("id=\"session-data\""), "data island missing");
        // No external assets are referenced.
        assert!(!html.contains("src=\"http"), "external script referenced");
    }

    #[test]
    fn title_derived_from_first_prompt() {
        let (_dir, log) = log_from_jsonl(&[SYSTEM, USER]);
        let html = render_session_html(&log);
        assert!(html.contains("<title>Hello **world**</title>"));
    }

    #[test]
    fn title_falls_back_without_user_prompt() {
        let (_dir, log) = log_from_jsonl(&[SYSTEM]);
        let html = render_session_html(&log);
        assert!(html.contains("<title>aj session</title>"));
    }

    #[test]
    fn embeds_entries_and_leaf() {
        let (_dir, log) = log_from_jsonl(&[SYSTEM, USER, ASSISTANT, TOOL_RESULT]);
        let html = render_session_html(&log);
        let data = data_island(html.as_str());
        // The session id, the derived leaf, and the entries all ride in
        // the island (the renderer needs them all).
        assert!(data.contains("\"session_id\":\"test-session\""));
        assert!(
            data.contains("\"leaf_id\":\"t0000001\""),
            "leaf not embedded"
        );
        assert!(
            data.contains("\"kind\":\"text\""),
            "tool details not embedded"
        );
        assert!(
            data.contains("read_file /tmp/x"),
            "entry content not embedded"
        );
    }

    #[test]
    fn data_island_neutralizes_angle_brackets() {
        // Tag-like sequences in a prompt must not be able to open or
        // close a tag inside the embedded JSON island.
        let user = r#"{"id":"u0000001","parent_id":"root0001","thread":"user","type":"message","message":{"role":"user","content":[{"type":"text","text":"</script><!--<script>x"}],"timestamp":1704067201000}}"#;
        let (_dir, log) = log_from_jsonl(&[SYSTEM, user]);
        let html = render_session_html(&log);
        let data = data_island(html.as_str());
        assert!(!data.contains('<'), "raw '<' leaked into the JSON island");
        assert!(data.contains("\\u003c"), "angle bracket not neutralized");
    }

    #[test]
    fn title_is_escaped() {
        let user = r#"{"id":"u0000001","parent_id":"root0001","thread":"user","type":"message","message":{"role":"user","content":[{"type":"text","text":"<b>hi</b>"}],"timestamp":1704067201000}}"#;
        let (_dir, log) = log_from_jsonl(&[SYSTEM, user]);
        let html = render_session_html(&log);
        assert!(html.contains("<title>&lt;b&gt;hi&lt;/b&gt;</title>"));
    }

    #[test]
    fn embedded_scripts_cannot_break_out() {
        // The vendored libraries and the renderer are inlined raw into
        // `<script>` elements. A literal `</script` is the one sequence
        // that would terminate the element early and break the page, so
        // no embedded asset may contain it (case-insensitive). The
        // `<!--`/`<script` script-data escape only bites when a
        // `</script` follows, so guarding this sequence is sufficient.
        for (name, js) in [
            ("marked", MARKED_JS),
            ("highlight", HIGHLIGHT_JS),
            ("app", APP_JS),
        ] {
            assert!(
                !js.to_ascii_lowercase().contains("</script"),
                "{name} contains a script-closing sequence"
            );
        }

        // The document has exactly four script elements (data island,
        // marked, highlight.js, renderer). A drift here means an asset
        // leaked an extra terminator.
        let (_dir, log) = log_from_jsonl(&[SYSTEM, USER, ASSISTANT, TOOL_RESULT]);
        let html = render_session_html(&log);
        assert_eq!(
            html.matches("</script>").count(),
            4,
            "script element count drifted"
        );

        // The license texts sit in an HTML comment, so they must not
        // contain `-->` (which would end the comment early).
        assert!(
            !MARKED_LICENSE.contains("-->"),
            "marked license ends the comment"
        );
        assert!(
            !HIGHLIGHT_LICENSE.contains("-->"),
            "highlight license ends the comment"
        );
    }

    #[test]
    fn embeds_license_texts() {
        let (_dir, log) = log_from_jsonl(&[SYSTEM, USER]);
        let html = render_session_html(&log);
        assert!(
            html.contains("Permission is hereby granted"),
            "MIT text missing"
        );
        assert!(html.contains("BSD 3-Clause License"), "BSD text missing");
    }

    /// Run the client-side renderer (`template.js`) against a fixture
    /// under node, gating the escaping and sanitization that only the
    /// JavaScript enforces. Skipped when node is not installed, so it
    /// covers the renderer wherever node exists without forcing it.
    #[test]
    fn renderer_smoke_test_passes() {
        use std::process::Command;
        let script = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/export/smoke_test.mjs");
        match Command::new("node").arg(script).output() {
            Ok(out) => assert!(
                out.status.success(),
                "renderer smoke test failed:\n{}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("skipping renderer smoke test: node not found");
            }
            Err(e) => panic!("failed to run node: {e}"),
        }
    }
}
