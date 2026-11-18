//! Persistence codec for structured tool-result details.
//!
//! A tool result's display details often repeat its model-facing content: a
//! text body that is the content, a command's output that the content starts
//! with. The log append codec replaces such a copy with a reference into the
//! content of its owned message. Projections, replay, and export resolve the
//! reference before exposing the details to agent state or renderers.
//!
//! A reference names the content's text blocks joined into one string. Only
//! the exact shapes written here are resolved. Anything else is left as it
//! is and decoded as ordinary details.

use aj_agent::message::{AgentMessage, AgentMessageKind};
use aj_agent::tool::ToolDetails;
use aj_models::types::{Message, ToolResultMessage, UserContent};
use serde_json::{Map, Value, json};

const CONTENT_TEXT_SOURCE: &str = "content_text";

/// The command-output fields of bash details that can be references.
const BASH_STREAMS: [(&str, &str); 2] = [("stdout", "stdout_ref"), ("stderr", "stderr_ref")];

/// Compacts duplicated details in an owned message before persistence.
pub(crate) fn compact_message(message: &mut AgentMessage) {
    let (AgentMessageKind::Wire(Message::ToolResult(result))
    | AgentMessageKind::ToolActivity(aj_agent::message::ToolActivity {
        message: Message::ToolResult(result),
        ..
    })) = &mut message.kind
    else {
        return;
    };
    compact_tool_result(result);
}

fn compact_tool_result(result: &mut ToolResultMessage) {
    let Some(object) = result.details.as_ref().and_then(Value::as_object) else {
        return;
    };
    let compacted = match object.get("kind").and_then(Value::as_str) {
        Some("text") => compact_text(object, &result.content),
        Some("bash") => compact_bash(object, &result.content),
        _ => None,
    };
    let Some(compacted) = compacted else {
        return;
    };
    let (Ok(full_len), Ok(compact_len)) = (
        serde_json::to_vec(object).map(|bytes| bytes.len()),
        serde_json::to_vec(&compacted).map(|bytes| bytes.len()),
    ) else {
        return;
    };
    if compact_len < full_len {
        result.details = Some(compacted);
    }
}

/// A text body that is the content: its blocks run together or joined by a
/// newline, with at most one newline appended.
fn compact_text(object: &Map<String, Value>, content: &[UserContent]) -> Option<Value> {
    // We only rewrite the exact schema we produced. An extra field may carry
    // semantics from a newer writer, so preserving the whole object is safer.
    if object.len() != 3 {
        return None;
    }
    let summary = object.get("summary")?.as_str()?;
    let body = object.get("body")?.as_str()?;
    let blocks = text_blocks(content)?;
    let (join, append_newline) = ["", "\n"].into_iter().find_map(|join| {
        let text = blocks.join(join);
        if body == text {
            Some((join, false))
        } else if body.strip_suffix('\n') == Some(text.as_str()) {
            Some((join, true))
        } else {
            None
        }
    })?;
    let mut body_ref = json!({
        "source": CONTENT_TEXT_SOURCE,
        "append_newline": append_newline,
    });
    // Blocks run together is the form older builds read, so it stays as it was.
    if !join.is_empty() {
        body_ref["join"] = json!(join);
    }
    Some(json!({ "kind": "text", "summary": summary, "body_ref": body_ref }))
}

/// A command's output that the content holds verbatim: stdout at its start,
/// stderr somewhere after it.
fn compact_bash(object: &Map<String, Value>, content: &[UserContent]) -> Option<Value> {
    if BASH_STREAMS
        .iter()
        .any(|(_, reference)| object.contains_key(*reference))
    {
        return None;
    }
    let text = text_blocks(content)?.concat();
    let mut compacted = object.clone();
    let mut from = 0;
    for (field, reference) in BASH_STREAMS {
        let Some(stream) = object.get(field).and_then(Value::as_str) else {
            return None;
        };
        if stream.is_empty() {
            continue;
        }
        let start = if field == "stdout" {
            text.starts_with(stream).then_some(0)
        } else {
            text[from..].find(stream).map(|at| from + at)
        };
        let Some(start) = start else {
            continue;
        };
        from = start + stream.len();
        compacted.remove(field);
        compacted.insert(
            reference.to_string(),
            json!({ "source": CONTENT_TEXT_SOURCE, "start": start, "len": stream.len() }),
        );
    }
    Some(Value::Object(compacted))
}

/// Expands persisted references in an owned message projection.
///
/// Only exact persisted references are resolved. This keeps ordinary details
/// and their unknown fields unchanged while ensuring a valid reference
/// becomes the full details shape expected outside storage.
pub(crate) fn expand_message(mut message: AgentMessage) -> AgentMessage {
    let (AgentMessageKind::Wire(Message::ToolResult(result))
    | AgentMessageKind::ToolActivity(aj_agent::message::ToolActivity {
        message: Message::ToolResult(result),
        ..
    })) = &mut message.kind
    else {
        return message;
    };
    let Some(raw) = result.details.as_ref() else {
        return message;
    };
    if let Some(details) = resolve_references(raw, &result.content) {
        result.details = Some(details);
    }
    message
}

/// Resolves persisted tool details against their model-facing content.
///
/// Exact references are hydrated first. Every other value, including a valid
/// inline text object with an unrelated `body_ref` field, falls through to
/// ordinary `ToolDetails` deserialization. A malformed reference or non-text
/// content returns `None`, allowing callers to use their existing content
/// fallback without panicking.
pub fn resolve_tool_details(raw: &Value, content: &[UserContent]) -> Option<ToolDetails> {
    let hydrated = resolve_references(raw, content);
    serde_json::from_value(hydrated.unwrap_or_else(|| raw.clone())).ok()
}

/// The details with their references replaced by what they name, `None` for
/// details that hold no valid reference.
fn resolve_references(raw: &Value, content: &[UserContent]) -> Option<Value> {
    let object = raw.as_object()?;
    match object.get("kind").and_then(Value::as_str) {
        Some("text") => resolve_text_reference(object, content),
        Some("bash") => resolve_bash_references(object, content),
        _ => None,
    }
}

fn resolve_text_reference(object: &Map<String, Value>, content: &[UserContent]) -> Option<Value> {
    if object.len() != 3 || !object.contains_key("body_ref") {
        return None;
    }
    let summary = object.get("summary")?.as_str()?;
    let body_ref = object.get("body_ref")?.as_object()?;
    if body_ref.get("source").and_then(Value::as_str) != Some(CONTENT_TEXT_SOURCE) {
        return None;
    }
    let append_newline = body_ref.get("append_newline")?.as_bool()?;
    let join = match body_ref.len() {
        2 => "",
        3 => body_ref.get("join")?.as_str()?,
        _ => return None,
    };
    let mut body = text_blocks(content)?.join(join);
    if append_newline {
        body.push('\n');
    }
    Some(json!({ "kind": "text", "summary": summary, "body": body }))
}

fn resolve_bash_references(object: &Map<String, Value>, content: &[UserContent]) -> Option<Value> {
    if !BASH_STREAMS
        .iter()
        .any(|(_, reference)| object.contains_key(*reference))
    {
        return None;
    }
    let text = text_blocks(content)?.concat();
    let mut hydrated = object.clone();
    for (field, reference) in BASH_STREAMS {
        let Some(stream_ref) = hydrated.remove(reference) else {
            continue;
        };
        let stream_ref = stream_ref.as_object()?;
        if stream_ref.len() != 3
            || object.contains_key(field)
            || stream_ref.get("source").and_then(Value::as_str) != Some(CONTENT_TEXT_SOURCE)
        {
            return None;
        }
        let start = usize::try_from(stream_ref.get("start")?.as_u64()?).ok()?;
        let len = usize::try_from(stream_ref.get("len")?.as_u64()?).ok()?;
        let stream = text.get(start..start.checked_add(len)?)?;
        hydrated.insert(field.to_string(), json!(stream));
    }
    Some(Value::Object(hydrated))
}

/// The content's text blocks, `None` when it holds anything else.
fn text_blocks(content: &[UserContent]) -> Option<Vec<&str>> {
    content
        .iter()
        .map(|block| match block {
            UserContent::Text(block) => Some(block.text.as_str()),
            UserContent::Image(_) => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use aj_models::types::ToolResultMessage;
    use serde_json::json;

    use super::*;

    fn result(content: Vec<UserContent>, details: Value) -> ToolResultMessage {
        ToolResultMessage {
            tool_call_id: "call-1".to_string(),
            tool_name: "read_file".to_string(),
            content,
            details: Some(details),
            is_error: false,
            timestamp: 0,
        }
    }

    fn compact(result: &mut ToolResultMessage) {
        compact_tool_result(result);
    }

    fn reference(append_newline: bool) -> Value {
        json!({
            "kind": "text",
            "summary": "read_file x",
            "body_ref": {
                "source": "content_text",
                "append_newline": append_newline,
            },
        })
    }

    #[test]
    fn compacts_body_equal_to_concatenated_text() {
        let body = "first block\n".repeat(20);
        let split = body.len() / 2;
        let mut result = result(
            vec![
                UserContent::text(&body[..split]),
                UserContent::text(&body[split..]),
            ],
            json!({"kind": "text", "summary": "read_file x", "body": body}),
        );

        compact(&mut result);

        assert_eq!(result.details, Some(reference(false)));
    }

    #[test]
    fn compacts_body_with_exactly_one_appended_newline() {
        let content = "a sufficiently long file body ".repeat(10);
        let mut result = result(
            vec![UserContent::text(&content)],
            json!({
                "kind": "text",
                "summary": "read_file x",
                "body": format!("{content}\n"),
            }),
        );

        compact(&mut result);

        assert_eq!(result.details, Some(reference(true)));
    }

    #[test]
    fn keeps_a_different_body_inline() {
        let content = "model-facing body ".repeat(20);
        let details = json!({
            "kind": "text",
            "summary": "read_file x",
            "body": "display body with different line-number gutters ".repeat(20),
        });
        let mut result = result(vec![UserContent::text(content)], details.clone());

        compact(&mut result);

        assert_eq!(result.details, Some(details));
    }

    #[test]
    fn keeps_small_and_empty_bodies_when_the_marker_is_not_smaller() {
        for body in ["", "tiny"] {
            let details = json!({"kind": "text", "summary": "x", "body": body});
            let mut result = result(vec![UserContent::text(body)], details.clone());

            compact(&mut result);

            assert_eq!(result.details, Some(details), "body {body:?}");
        }
    }

    #[test]
    fn keeps_text_details_inline_when_content_contains_an_image() {
        let body = "duplicate text ".repeat(20);
        let details = json!({"kind": "text", "summary": "x", "body": body});
        let mut result = result(
            vec![
                UserContent::text(&body),
                UserContent::image("base64", "image/png"),
            ],
            details.clone(),
        );

        compact(&mut result);

        assert_eq!(result.details, Some(details));
    }

    #[test]
    fn keeps_text_details_with_unknown_fields_untouched() {
        let body = "duplicate text ".repeat(20);
        let details = json!({
            "kind": "text",
            "summary": "x",
            "body": body,
            "extension": {"version": 2},
        });
        let mut result = result(vec![UserContent::text(&body)], details.clone());

        compact(&mut result);

        assert_eq!(result.details, Some(details));
    }

    #[test]
    fn hydrates_a_compacted_body_exactly() {
        let first = "alpha".repeat(30);
        let second = "beta".repeat(30);
        let original_body = format!("{first}{second}\n");
        let content = vec![UserContent::text(&first), UserContent::text(&second)];
        let mut result = result(
            content,
            json!({
                "kind": "text",
                "summary": "read_file x",
                "body": original_body,
            }),
        );

        compact(&mut result);
        let reference = result.details.expect("body compacted");
        let hydrated = resolve_tool_details(&reference, &result.content).expect("valid reference");

        match hydrated {
            ToolDetails::Text { summary, body } => {
                assert_eq!(summary, "read_file x");
                assert_eq!(body, original_body);
            }
            other => panic!("expected text details, got {other:?}"),
        }
    }

    #[test]
    fn legacy_content_text_marker_keeps_absolute_read_file_gutters() {
        let content = "   41: alpha\n   42: beta";
        let mut result = result(vec![UserContent::text(content)], reference(true));
        result.tool_name = "read_file".to_string();

        let expanded = expand_message(AgentMessage::wire(Message::ToolResult(result)));
        let Some(Message::ToolResult(result)) = expanded.as_stored_wire() else {
            panic!("expected tool result");
        };
        let details = result.details.as_ref().expect("expanded details");

        assert_eq!(details["body"], format!("{content}\n"));
        assert!(details.get("body_ref").is_none());
    }

    #[test]
    fn inline_text_with_a_malformed_body_ref_keeps_its_body() {
        let mixed = json!({
            "kind": "text",
            "summary": "inline summary",
            "body": "inline display body",
            "body_ref": {"source": "content_text", "append_newline": "yes"},
        });

        let resolved = resolve_tool_details(&mixed, &[UserContent::text("model body")])
            .expect("inline text details remain valid");
        match resolved {
            ToolDetails::Text { summary, body } => {
                assert_eq!(summary, "inline summary");
                assert_eq!(body, "inline display body");
            }
            other => panic!("expected inline text details, got {other:?}"),
        }
    }

    #[test]
    fn rejects_malformed_marker_only_text_references() {
        let malformed = json!({
            "kind": "text",
            "summary": "read_file x",
            "body_ref": {"source": "future_content", "append_newline": false},
        });

        assert!(resolve_tool_details(&malformed, &[UserContent::text("body")]).is_none());
    }

    /// Code Mode's `exec` joins its content blocks with a newline to make
    /// its display body.
    #[test]
    fn a_body_of_newline_joined_blocks_is_a_reference_and_hydrates_exactly() {
        for append_newline in [false, true] {
            let blocks = ["Script completed", &"{\"stdout\":\"line\\n\"}".repeat(20)];
            let mut body = blocks.join("\n");
            if append_newline {
                body.push('\n');
            }
            let details = json!({ "kind": "text", "summary": "exec", "body": body });
            let content: Vec<_> = blocks
                .iter()
                .map(|block| UserContent::text(*block))
                .collect();
            let mut result = result(content, details.clone());
            compact(&mut result);

            let stored = result.details.clone().unwrap();
            assert_eq!(
                stored["body_ref"],
                json!({
                    "source": "content_text",
                    "append_newline": append_newline,
                    "join": "\n",
                })
            );
            assert!(stored.get("body").is_none());
            let resolved = resolve_tool_details(&stored, &result.content).unwrap();
            assert_eq!(serde_json::to_value(resolved).unwrap(), details);
        }
    }

    fn bash(stdout: &str, stderr: &str) -> Value {
        serde_json::to_value(ToolDetails::Bash {
            command: "make".to_string(),
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
            exit_code: Some(2),
            truncated: false,
            full_output_path: None,
            stdout_truncation: None,
            stderr_truncation: None,
            task_id: None,
        })
        .unwrap()
    }

    /// The model-facing text of a bash result starts with stdout and holds
    /// stderr after its `STDERR:` line, with markers and a trailer around
    /// them.
    #[test]
    fn bash_streams_the_content_holds_are_references_and_hydrate_exactly() {
        let stdout = "compiling étape\n".repeat(12);
        let stderr = "warning: unused\n".repeat(12);
        let wire = format!("{stdout}STDERR:\n{stderr}Command failed with exit code: 2");
        for (out, err, text) in [
            (stdout.as_str(), stderr.as_str(), wire.as_str()),
            (stdout.as_str(), "", stdout.as_str()),
            ("", stderr.as_str(), wire.as_str()),
        ] {
            let mut details = bash(out, err);
            // A field this build does not know stays with the details.
            details["later_field"] = json!(7);
            let mut result = result(vec![UserContent::text(text)], details.clone());
            compact(&mut result);

            let stored = result.details.clone().unwrap();
            for (stream, field, reference) in
                [(out, "stdout", "stdout_ref"), (err, "stderr", "stderr_ref")]
            {
                if stream.is_empty() {
                    assert_eq!(stored[field], json!(""));
                } else {
                    assert!(stored.get(field).is_none(), "{field} is stored once");
                    assert_eq!(stored[reference]["len"], json!(stream.len()));
                }
            }
            let expanded = expand_message(AgentMessage::wire(Message::ToolResult(result.clone())));
            let AgentMessageKind::Wire(Message::ToolResult(expanded)) = &expanded.kind else {
                panic!("a tool result");
            };
            assert_eq!(expanded.details.as_ref(), Some(&details));
            let resolved = resolve_tool_details(&stored, &result.content).unwrap();
            let mut known = details.clone();
            known.as_object_mut().unwrap().remove("later_field");
            assert_eq!(serde_json::to_value(resolved).unwrap(), known);
        }
    }

    #[test]
    fn bash_output_the_content_does_not_hold_stays_inline() {
        let stdout = "output\n".repeat(20);
        // A background launch answers with a notice, not with the output.
        let details = bash(&stdout, "");
        let mut result = result(vec![UserContent::text("started task #1")], details.clone());
        compact(&mut result);
        assert_eq!(result.details, Some(details));

        // A short stream is not worth a reference.
        let details = bash("ok\n", "");
        let mut result = result_with("ok\n", details.clone());
        compact(&mut result);
        assert_eq!(result.details, Some(details));
    }

    fn result_with(text: &str, details: Value) -> ToolResultMessage {
        result(vec![UserContent::text(text)], details)
    }

    #[test]
    fn a_bash_reference_the_content_cannot_answer_resolves_to_nothing() {
        let text = "é".repeat(40);
        let reference = |start: usize, len: usize| {
            let mut details = bash("", "");
            let object = details.as_object_mut().unwrap();
            object.remove("stdout");
            object.insert(
                "stdout_ref".to_string(),
                json!({ "source": "content_text", "start": start, "len": len }),
            );
            details
        };
        let content = [UserContent::text(text.as_str())];
        assert!(resolve_tool_details(&reference(0, 4), &content).is_some());
        // Past the end, and inside a character.
        assert!(resolve_tool_details(&reference(0, 400), &content).is_none());
        assert!(resolve_tool_details(&reference(1, 2), &content).is_none());
        // A reference beside the field it stands for is not one we wrote,
        // so the inline value is what the details say.
        let mut both = reference(0, 4);
        both["stdout"] = json!("x");
        let Some(ToolDetails::Bash { stdout, .. }) = resolve_tool_details(&both, &content) else {
            panic!("inline bash details");
        };
        assert_eq!(stdout, "x");
    }
}
