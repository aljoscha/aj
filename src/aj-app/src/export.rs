//! Self-contained HTML export of a conversation session.
//!
//! [`render_session_html`] assembles a single static HTML document that
//! renders the session entirely client-side. The exporter ships the
//! whole session as JSON plus a vendored renderer: it does not render
//! the transcript server-side. The browser parses the embedded entries
//! and builds the view (messages, tool results, sub-agent runs,
//! markdown, syntax highlighting), so the same file is both a readable
//! transcript and the source data used by the page.
//!
//! What gets embedded:
//! - the on-disk entries (`ConversationEntry`), with session environment
//!   values redacted, valid diff details normalized, and valid text body
//!   references expanded one entry at a time,
//! - the page renderer (`template.js`),
//! - `marked` (markdown), vendored under `assets/export/vendor` (see its
//!   `PROVENANCE.md`).
//!
//! Security: the session rides in a `<script type="application/octet-stream">`
//! block as gzip-compressed, base64-encoded bytes. The base64 alphabet
//! has no `<`, so the payload cannot open or close a tag and needs no
//! further escaping. The browser inflates it with the native
//! `DecompressionStream` and `JSON.parse`s the result. The renderer treats raw
//! HTML in prose as inert text and restricts link/image URLs to a scheme
//! allow-list, so a shared transcript cannot inject markup or scripts.

use std::io::Write;

use aj_agent::events::AgentEvent;
use aj_agent::tool::ToolDetails;
use aj_models::types::{Message, ToolResultMessage, UserContent};
use aj_session::{
    ConversationEntry, ConversationEntryKind, ConversationLog, EntryId, replay,
    resolve_tool_details,
};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use flate2::Compression;
use flate2::write::GzEncoder;
use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};

/// The HTML shell with `{{KEY}}` placeholders, filled by
/// [`fill_template`].
const TEMPLATE: &str = include_str!("../assets/export/template.html");
const CSS: &str = include_str!("../assets/export/template.css");
const APP_JS: &str = include_str!("../assets/export/template.js");
const MARKED_JS: &str = include_str!("../assets/export/vendor/marked.min.js");
const REDACTED_ENV_VALUE: &str = "[redacted]";

/// Full license text for the vendored library, embedded in the export so
/// every shared copy carries the notice the MIT license requires to
/// travel with a redistribution.
const MARKED_LICENSE: &str = include_str!("../assets/export/vendor/marked.LICENSE");

/// The embedded session envelope. Environment values are redacted, valid diff
/// details are canonicalized, and valid text body references are expanded
/// during serialization. All other entry data keeps its on-disk shape.
#[derive(Serialize)]
struct ExportData<'a> {
    session_id: &'a str,
    /// The active user-thread tip, so the page opens on the same branch
    /// a resumed session would. `None` for a session with no user
    /// messages yet.
    leaf_id: Option<EntryId>,
    entries: Vec<ExportEntry<'a>>,
}

struct ExportEntry<'a>(&'a ConversationEntry);

#[derive(Serialize)]
struct ExportToolResultMessage<'a> {
    role: &'static str,
    tool_call_id: &'a str,
    tool_name: &'a str,
    content: &'a [UserContent],
    details: &'a ToolDetails,
    is_error: bool,
    timestamp: i64,
}

impl Serialize for ExportEntry<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let entry = self.0;
        if matches!(entry.entry, ConversationEntryKind::EnvChange { .. }) {
            let mut redacted = entry.clone();
            let ConversationEntryKind::EnvChange { env } = &mut redacted.entry else {
                unreachable!("entry kind was checked above");
            };
            for value in env.values_mut() {
                *value = REDACTED_ENV_VALUE.to_string();
            }
            return redacted.serialize(serializer);
        }
        let ConversationEntryKind::Message { message } = &entry.entry else {
            return entry.serialize(serializer);
        };
        let Some(Message::ToolResult(result)) = message.as_stored_wire() else {
            return entry.serialize(serializer);
        };
        let Some(raw_details) = result.details.as_ref() else {
            return entry.serialize(serializer);
        };
        let kind = raw_details.get("kind").and_then(|kind| kind.as_str());
        let project_details =
            kind == Some("diff") || (kind == Some("text") && raw_details.get("body_ref").is_some());
        if !project_details {
            return entry.serialize(serializer);
        }
        let Some(details) = resolve_tool_details(raw_details, &result.tool_name, &result.content)
        else {
            return entry.serialize(serializer);
        };

        serialize_normalized_tool_result(entry, result, &details, serializer)
    }
}

fn serialize_normalized_tool_result<S>(
    entry: &ConversationEntry,
    result: &ToolResultMessage,
    details: &ToolDetails,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    // Keep these patterns exhaustive so new persisted fields cannot disappear
    // from normalized exports without a compile error.
    let ConversationEntry {
        id,
        parent_id,
        timestamp,
        thread,
        agent_id,
        entry: _,
    } = entry;
    let ToolResultMessage {
        tool_call_id,
        tool_name,
        content,
        details: _,
        is_error,
        timestamp: message_timestamp,
    } = result;

    let mut map = serializer.serialize_map(None)?;
    map.serialize_entry("id", id)?;
    if let Some(parent_id) = parent_id {
        map.serialize_entry("parent_id", parent_id)?;
    }
    map.serialize_entry("timestamp", timestamp)?;
    map.serialize_entry("thread", thread)?;
    if let Some(agent_id) = agent_id {
        map.serialize_entry("agent_id", agent_id)?;
    }
    map.serialize_entry("type", "message")?;
    map.serialize_entry(
        "message",
        &ExportToolResultMessage {
            role: "tool_result",
            tool_call_id,
            tool_name,
            content,
            details,
            is_error: *is_error,
            timestamp: *message_timestamp,
        },
    )?;
    map.end()
}

fn export_data(log: &ConversationLog) -> ExportData<'_> {
    ExportData {
        session_id: log.session_id(),
        leaf_id: log.head().cloned(),
        entries: log
            .entries_in_order()
            .into_iter()
            .map(ExportEntry)
            .collect(),
    }
}

/// Render a whole session to a self-contained HTML document.
///
/// Pure over the log: it reads but never mutates, so it is safe to call
/// while a turn is in flight.
pub fn render_session_html(log: &ConversationLog) -> String {
    let title = derive_title(log)
        .map(|t| truncate_title(&t))
        .unwrap_or_else(|| "aj session".to_string());

    let data = export_data(log);
    let session_data = embed_session(&data);
    let licenses = format!("marked (MIT) https://github.com/markedjs/marked\n\n{MARKED_LICENSE}");

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

/// Serialize the export envelope, gzip-compress it, and base64-encode
/// the result for embedding in a `<script>` element.
///
/// Compression keeps large text tool results and long sessions compact. The
/// base64 alphabet contains no `<`, so the payload is inert inside its
/// surrounding element without any further escaping. The browser reverses
/// both steps with `DecompressionStream` and `JSON.parse`.
fn embed_session(data: &ExportData) -> String {
    let json = serde_json::to_vec(data).expect("ExportData always serializes");
    let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
    // Gzipping to an in-memory buffer is infallible, so a failure here is
    // a bug rather than something to paper over with empty output (which
    // would inflate to "" and leave the page unable to parse its data).
    let gzipped = encoder
        .write_all(&json)
        .and_then(|()| encoder.finish())
        .expect("gzip to a Vec cannot fail");
    BASE64.encode(gzipped)
}

/// The first user prompt's text, used for the page `<title>`. Derived
/// from the replay stream so it tracks whatever the live view would show
/// as the opening message.
fn derive_title(log: &ConversationLog) -> Option<String> {
    for event in replay(log) {
        if let AgentEvent::MessageEnd { message, .. } = event
            && let Some(Message::User(u)) = message.as_stored_wire()
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
    use std::io::Read;

    use aj_session::{ConversationLog, ConversationPersistence};
    use flate2::read::GzDecoder;
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

    /// The raw (base64) payload from the embedded data island.
    fn data_island(html: &str) -> &str {
        html.split_once("id=\"session-data\">")
            .and_then(|(_, rest)| rest.split_once("</script>"))
            .map(|(payload, _)| payload)
            .expect("data island present")
    }

    /// Decode the data island back to its JSON text (reverse of
    /// [`embed_session`]: base64-decode then gunzip), so tests can assert
    /// on the embedded session content.
    fn decoded_island(html: &str) -> String {
        let gzipped = BASE64.decode(data_island(html)).expect("island is base64");
        let mut json = String::new();
        GzDecoder::new(&gzipped[..])
            .read_to_string(&mut json)
            .expect("island gunzips to utf-8");
        json
    }

    const SYSTEM: &str = r#"{"id":"root0001","timestamp":"2024-01-01T00:00:00Z","thread":"meta","type":"system_prompt","text":"You are aj."}"#;
    const USER: &str = r#"{"id":"u0000001","parent_id":"root0001","timestamp":"2024-01-01T00:00:01Z","thread":"user","type":"message","message":{"role":"user","content":[{"type":"text","text":"Hello **world**"}],"timestamp":1704067201000}}"#;
    const ASSISTANT: &str = r#"{"id":"a0000001","parent_id":"u0000001","timestamp":"2024-01-01T00:00:02Z","thread":"user","type":"message","message":{"role":"assistant","content":[{"type":"text","text":"Reading the file."},{"type":"tool_call","id":"call-1","name":"read_file","arguments":{"path":"/tmp/x"}}],"api":"anthropic-messages","provider":"anthropic","model":"claude-test","usage":{"input":10,"output":5,"cache_read":0,"cache_write":0,"total_tokens":15,"cost":{"input":0.0,"output":0.0,"cache_read":0.0,"cache_write":0.0,"total":0.0}},"stop_reason":"ToolUse","timestamp":1704067202000}}"#;
    const TOOL_RESULT: &str = r#"{"id":"t0000001","parent_id":"a0000001","timestamp":"2024-01-01T00:00:03Z","thread":"user","type":"message","message":{"role":"tool_result","tool_call_id":"call-1","tool_name":"read_file","content":[{"type":"text","text":"the file body"}],"details":{"kind":"text","summary":"read_file /tmp/x","body":"the file body"},"is_error":false,"timestamp":1704067203000}}"#;
    // A task-completion notice on the user thread, in the new on-disk
    // shape (`role:"task_notification"` with structured fields).
    const NOTIFICATION_ROOT: &str = r#"{"id":"n0000001","parent_id":"root0001","timestamp":"2024-01-01T00:00:01Z","thread":"user","type":"message","message":{"role":"task_notification","label":"cargo build","kind":"bash","outcome":{"status":"succeeded"},"body":"exit code 0"}}"#;
    const USER_AFTER_NOTICE: &str = r#"{"id":"u0000009","parent_id":"n0000001","timestamp":"2024-01-01T00:00:02Z","thread":"user","type":"message","message":{"role":"user","content":[{"type":"text","text":"typed after notice"}],"timestamp":1704067202000}}"#;
    const NOTIFICATION_TAIL: &str = r#"{"id":"n0000002","parent_id":"t0000001","timestamp":"2024-01-01T00:00:04Z","thread":"user","type":"message","message":{"role":"task_notification","label":"cargo build","kind":"bash","outcome":{"status":"succeeded"},"body":"exit code 0"}}"#;

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
        // Renderer and vendored library are inlined.
        assert!(html.contains("marked"), "marked not embedded");
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
    fn derive_title_skips_task_notifications() {
        // A notice precedes the first typed prompt. `derive_title` keys
        // on the stored-wire accessor, which yields `None` for a notice,
        // so the title comes from the real prompt after it.
        let (_dir, log) = log_from_jsonl(&[SYSTEM, NOTIFICATION_ROOT, USER_AFTER_NOTICE]);
        assert_eq!(derive_title(&log).as_deref(), Some("typed after notice"));
    }

    #[test]
    fn task_notification_serializes_with_its_role_and_fields() {
        // The raw entry serializer falls through to the on-disk shape,
        // so a notice rides the island as `role:"task_notification"`
        // with its structured fields (not a user bubble).
        let (_dir, log) =
            log_from_jsonl(&[SYSTEM, USER, ASSISTANT, TOOL_RESULT, NOTIFICATION_TAIL]);
        let html = render_session_html(&log);
        let data = decoded_island(html.as_str());
        assert!(
            data.contains("\"role\":\"task_notification\""),
            "notice role not embedded: {data}"
        );
        assert!(data.contains("\"label\":\"cargo build\""), "label missing");
        assert!(data.contains("\"status\":\"succeeded\""), "outcome missing");
        assert!(data.contains("\"body\":\"exit code 0\""), "body missing");
    }

    #[test]
    fn embeds_entries_and_leaf() {
        let (_dir, log) = log_from_jsonl(&[SYSTEM, USER, ASSISTANT, TOOL_RESULT]);
        let html = render_session_html(&log);
        let data = decoded_island(html.as_str());
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
    fn export_redacts_env_values_in_the_embedded_entries_without_mutating_the_log() {
        let env = r#"{"id":"e0000001","parent_id":"root0001","timestamp":"2024-01-01T00:00:00Z","thread":"meta","type":"env_change","env":{"BEADS_ACTOR":"session-actor","SECRET_TOKEN":"hunter2"}}"#;
        let user = r#"{"id":"u0000001","parent_id":"root0001","timestamp":"2024-01-01T00:00:01Z","thread":"user","type":"message","message":{"role":"user","content":[{"type":"text","text":"Hello"}],"timestamp":1704067201000}}"#;
        let (dir, log) = log_from_jsonl(&[SYSTEM, env, user]);
        let source_path = dir.path().join("test-session.jsonl");
        let source_bytes = fs::read(&source_path).expect("read source log before export");
        let source_entry = serde_json::to_value(log.entries_in_order()[1])
            .expect("serialize in-memory source entry before export");

        let html = render_session_html(&log);
        let decoded = decoded_island(&html);
        let data: serde_json::Value =
            serde_json::from_str(&decoded).expect("embedded export data parses");
        let mut expected_entry: serde_json::Value =
            serde_json::from_str(env).expect("source env fixture parses");
        for value in expected_entry["env"]
            .as_object_mut()
            .expect("source env is an object")
            .values_mut()
        {
            *value = serde_json::json!(REDACTED_ENV_VALUE);
        }
        assert_eq!(
            data["entries"][1], expected_entry,
            "the decoded embedded entry must preserve all framing and redact only env values",
        );
        assert!(
            decoded.contains("BEADS_ACTOR"),
            "the decoded entry lost its key"
        );
        assert!(
            decoded.contains(REDACTED_ENV_VALUE),
            "the decoded entry lost the redaction marker"
        );
        assert!(
            !decoded.contains("hunter2"),
            "the env value leaked into the decoded embedded entries"
        );
        assert!(
            !html.contains("hunter2"),
            "the env value leaked into the raw HTML artifact"
        );
        assert_eq!(
            serde_json::to_value(log.entries_in_order()[1])
                .expect("serialize in-memory source entry after export"),
            source_entry,
            "export redaction mutated the in-memory log entry",
        );
        assert_eq!(
            fs::read(&source_path).expect("read source log after export"),
            source_bytes,
            "export changed the source log bytes",
        );

        drop(log);
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let reread = ConversationLog::resume(&persistence, "test-session")
            .expect("re-read source log after export");
        let ConversationEntryKind::EnvChange { env } = &reread.entries_in_order()[1].entry else {
            panic!("source log lost its env entry");
        };
        assert_eq!(
            env,
            &std::collections::BTreeMap::from([
                ("BEADS_ACTOR".to_string(), "session-actor".to_string()),
                ("SECRET_TOKEN".to_string(), "hunter2".to_string()),
            ]),
            "export redaction wrote back into the source log",
        );
    }

    #[test]
    fn export_normalizes_valid_diff_details_without_mutating_log() {
        let legacy = r#"{"id":"t0000001","parent_id":"a0000001","thread":"user","type":"message","message":{"role":"tool_result","tool_call_id":"call-1","tool_name":"edit_file","content":[{"type":"text","text":"legacy model result"}],"details":{"kind":"diff","path":"legacy.rs","before":"same\nold\n","after":"same\nnew\n"},"is_error":false,"timestamp":0}}"#;
        let compact = r#"{"id":"t0000002","parent_id":"t0000001","thread":"user","type":"message","message":{"role":"tool_result","tool_call_id":"call-2","tool_name":"edit_file","content":[{"type":"text","text":"compact model result"}],"details":{"kind":"diff","format":"display-v1","path":"src/\u001b[31mlib.rs","lines":["--- a/src/\u001b[31mlib.rs","+++ b/src/\u001b[31mlib.rs","- old","+ new"]},"is_error":false,"timestamp":0}}"#;
        let (_dir, log) = log_from_jsonl(&[SYSTEM, USER, ASSISTANT, legacy, compact]);

        let html = render_session_html(&log);
        let data: serde_json::Value =
            serde_json::from_str(&decoded_island(&html)).expect("export data parses");
        let legacy = &data["entries"][3]["message"]["details"];
        assert_eq!(legacy["format"], "display-v1");
        assert_eq!(legacy["path"], "legacy.rs");
        assert!(legacy.get("before").is_none());
        assert!(legacy.get("after").is_none());
        assert_eq!(legacy["lines"][2], "  same");
        assert_eq!(legacy["lines"][3], "- old");
        assert_eq!(legacy["lines"][4], "+ new");

        let compact = &data["entries"][4]["message"]["details"];
        assert_eq!(compact["format"], "display-v1");
        assert_eq!(compact["path"], "src/lib.rs");
        assert_eq!(compact["lines"][0], "--- a/src/lib.rs");
        assert!(
            compact["lines"]
                .as_array()
                .is_some_and(|lines| { lines.iter().all(serde_json::Value::is_string) })
        );

        let entries = log.entries_in_order();
        let ConversationEntryKind::Message { message } = &entries[3].entry else {
            panic!("expected message entry");
        };
        let Some(Message::ToolResult(result)) = message.as_stored_wire() else {
            panic!("expected tool result");
        };
        let original = result.details.as_ref().expect("original details");
        assert!(original.get("before").is_some());
        assert!(original.get("after").is_some());
    }

    #[test]
    fn export_expands_compact_text_details_without_mutating_log() {
        let compact_text = r#"{"id":"t0000001","parent_id":"a0000001","thread":"user","type":"message","message":{"role":"tool_result","tool_call_id":"call-1","tool_name":"read_file","content":[{"type":"text","text":"first block"},{"type":"text","text":"second block"}],"details":{"kind":"text","summary":"read_file x.rs","body_ref":{"source":"content_text","append_newline":true}},"is_error":false,"timestamp":0}}"#;
        let (_dir, log) = log_from_jsonl(&[SYSTEM, USER, ASSISTANT, compact_text]);

        let html = render_session_html(&log);
        let data: serde_json::Value =
            serde_json::from_str(&decoded_island(&html)).expect("export data parses");
        assert_eq!(
            data["entries"][3]["message"]["details"],
            serde_json::json!({
                "kind": "text",
                "summary": "read_file x.rs",
                "body": "first blocksecond block\n",
            })
        );

        let entries = log.entries_in_order();
        let ConversationEntryKind::Message { message } = &entries[3].entry else {
            panic!("expected message entry");
        };
        let Some(Message::ToolResult(result)) = message.as_stored_wire() else {
            panic!("expected tool result");
        };
        let original = result.details.as_ref().expect("original details");
        assert_eq!(original["body_ref"]["source"], "content_text");
        assert!(original.get("body").is_none());
    }

    #[test]
    fn export_preserves_malformed_text_references() {
        let malformed = r#"{"id":"t0000001","parent_id":"a0000001","thread":"user","type":"message","message":{"role":"tool_result","tool_call_id":"call-1","tool_name":"read_file","content":[{"type":"text","text":"model result"}],"details":{"kind":"text","summary":"read_file x.rs","body_ref":{"source":"content_text","append_newline":"yes"}},"is_error":false,"timestamp":0}}"#;
        let (_dir, log) = log_from_jsonl(&[SYSTEM, USER, ASSISTANT, malformed]);

        let source: serde_json::Value = serde_json::from_str(malformed).expect("source entry");
        let html = render_session_html(&log);
        let exported: serde_json::Value =
            serde_json::from_str(&decoded_island(&html)).expect("export data parses");

        assert_eq!(
            exported["entries"][3]["message"]["details"],
            source["message"]["details"]
        );
    }

    #[test]
    fn export_preserves_non_diff_detail_extensions() {
        let text = r#"{"id":"t0000001","parent_id":"a0000001","thread":"user","type":"message","message":{"role":"tool_result","tool_call_id":"call-1","tool_name":"read_file","content":[{"type":"text","text":"model result"}],"details":{"kind":"text","summary":"read_file x.rs","body":"body","extension":{"version":2,"enabled":true}},"is_error":false,"timestamp":0}}"#;
        let (_dir, log) = log_from_jsonl(&[SYSTEM, USER, ASSISTANT, text]);

        let source: serde_json::Value = serde_json::from_str(text).expect("source entry parses");
        let html = render_session_html(&log);
        let exported: serde_json::Value =
            serde_json::from_str(&decoded_island(&html)).expect("export data parses");
        assert_eq!(
            exported["entries"][3]["message"]["details"],
            source["message"]["details"]
        );
    }

    #[test]
    fn data_island_is_inert_base64() {
        // The base64 payload contains no `<`, so it cannot open or close a
        // tag inside its surrounding script element, no matter what a
        // prompt contains. The original tag-like text round-trips once
        // decoded.
        let user = r#"{"id":"u0000001","parent_id":"root0001","thread":"user","type":"message","message":{"role":"user","content":[{"type":"text","text":"</script><!--<script>x"}],"timestamp":1704067201000}}"#;
        let (_dir, log) = log_from_jsonl(&[SYSTEM, user]);
        let html = render_session_html(&log);
        assert!(
            !data_island(html.as_str()).contains('<'),
            "raw '<' leaked into the data island"
        );
        assert!(
            decoded_island(html.as_str()).contains("</script><!--<script>x"),
            "prompt text did not round-trip through the island"
        );
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
        // The vendored library and the renderer are inlined raw into
        // `<script>` elements. A literal `</script` is the one sequence
        // that would terminate the element early and break the page, so
        // no embedded asset may contain it (case-insensitive). The
        // `<!--`/`<script` script-data escape only bites when a
        // `</script` follows, so guarding this sequence is sufficient.
        for (name, js) in [("marked", MARKED_JS), ("app", APP_JS)] {
            assert!(
                !js.to_ascii_lowercase().contains("</script"),
                "{name} contains a script-closing sequence"
            );
        }

        // The document has exactly three script elements (data island,
        // marked, renderer). A drift here means an asset leaked an extra
        // terminator.
        let (_dir, log) = log_from_jsonl(&[SYSTEM, USER, ASSISTANT, TOOL_RESULT]);
        let html = render_session_html(&log);
        assert_eq!(
            html.matches("</script>").count(),
            3,
            "script element count drifted"
        );

        // The license text sits in an HTML comment, so it must not
        // contain `-->` (which would end the comment early).
        assert!(
            !MARKED_LICENSE.contains("-->"),
            "marked license ends the comment"
        );
    }

    #[test]
    fn island_round_trips_export_envelope() {
        let (_dir, log) = log_from_jsonl(&[SYSTEM, USER, ASSISTANT, TOOL_RESULT]);
        let html = render_session_html(&log);
        // Decoding the island yields exactly the canonical export data before
        // the gzip and base64 transport steps.
        let data = export_data(&log);
        let expected = serde_json::to_string(&data).expect("serialize envelope");
        assert_eq!(decoded_island(html.as_str()), expected);
    }

    #[test]
    fn embeds_license_text() {
        let (_dir, log) = log_from_jsonl(&[SYSTEM, USER]);
        let html = render_session_html(&log);
        assert!(
            html.contains("Permission is hereby granted"),
            "MIT text missing"
        );
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
