//! Persistence codec for structured tool-result details.
//!
//! Text tool results often repeat model-facing content in their display body.
//! The log append codec replaces that duplicate with a reference in its owned
//! message. Projections, replay, and export resolve the reference before
//! exposing the details to agent state or renderers.

use aj_agent::message::{AgentMessage, AgentMessageKind};
use aj_agent::tool::ToolDetails;
use aj_models::types::{Message, ToolResultMessage, UserContent};
use serde::Serialize;
use serde_json::Value;

const CONTENT_TEXT_SOURCE: &str = "content_text";

#[derive(Serialize)]
struct PersistedTextDetails<'a> {
    kind: &'static str,
    summary: &'a str,
    body_ref: PersistedBodyRef,
}

#[derive(Serialize)]
struct PersistedBodyRef {
    source: &'static str,
    append_newline: bool,
}

/// Compacts duplicated text details in an owned message before persistence.
pub(crate) fn compact_message(message: &mut AgentMessage) {
    let AgentMessageKind::Wire(Message::ToolResult(result)) = &mut message.kind else {
        return;
    };
    compact_tool_result(result);
}

fn compact_tool_result(result: &mut ToolResultMessage) {
    let Some(details) = result.details.as_ref() else {
        return;
    };
    let Some(object) = details.as_object() else {
        return;
    };

    // We only rewrite the exact schema we produced. An extra field may carry
    // semantics from a newer writer, so preserving the whole object is safer.
    if object.len() != 3
        || object.get("kind").and_then(Value::as_str) != Some("text")
        || !object.contains_key("summary")
        || !object.contains_key("body")
    {
        return;
    }
    let (Some(summary), Some(body)) = (
        object.get("summary").and_then(Value::as_str),
        object.get("body").and_then(Value::as_str),
    ) else {
        return;
    };
    let Some(content_text) = concatenated_text(&result.content) else {
        return;
    };

    let append_newline = if body == content_text {
        false
    } else if body
        .strip_suffix('\n')
        .is_some_and(|without_newline| without_newline == content_text)
    {
        true
    } else {
        return;
    };

    let marker = PersistedTextDetails {
        kind: "text",
        summary,
        body_ref: PersistedBodyRef {
            source: CONTENT_TEXT_SOURCE,
            append_newline,
        },
    };
    let Ok(marker) = serde_json::to_value(marker) else {
        return;
    };
    let (Ok(full_len), Ok(marker_len)) = (
        serde_json::to_vec(details).map(|bytes| bytes.len()),
        serde_json::to_vec(&marker).map(|bytes| bytes.len()),
    ) else {
        return;
    };
    if marker_len < full_len {
        result.details = Some(marker);
    }
}

/// Expands persisted text references in an owned message projection.
///
/// Only exact persisted markers are normalized. This keeps ordinary details
/// and their unknown fields unchanged while ensuring a valid body reference
/// becomes the full `ToolDetails::Text` shape expected outside storage.
pub(crate) fn expand_message(mut message: AgentMessage) -> AgentMessage {
    let AgentMessageKind::Wire(Message::ToolResult(result)) = &mut message.kind else {
        return message;
    };
    let Some(raw) = result.details.as_ref() else {
        return message;
    };
    let Some(details) = resolve_text_reference(raw, &result.content) else {
        return message;
    };
    if let Ok(details) = serde_json::to_value(details) {
        result.details = Some(details);
    }
    message
}

/// Resolves persisted tool details against their model-facing content.
///
/// Exact text body references are hydrated first. Every other value, including
/// a valid inline text object with an unrelated `body_ref` field, falls through
/// to ordinary `ToolDetails` deserialization. A malformed marker-only value or
/// non-text content returns `None`, allowing callers to use their existing
/// content fallback without panicking.
pub fn resolve_tool_details(raw: &Value, content: &[UserContent]) -> Option<ToolDetails> {
    resolve_text_reference(raw, content).or_else(|| serde_json::from_value(raw.clone()).ok())
}

fn resolve_text_reference(raw: &Value, content: &[UserContent]) -> Option<ToolDetails> {
    let object = raw.as_object()?;
    if object.len() != 3
        || object.get("kind").and_then(Value::as_str) != Some("text")
        || !object.contains_key("summary")
        || !object.contains_key("body_ref")
    {
        return None;
    }
    let summary = object.get("summary")?.as_str()?;
    let body_ref = object.get("body_ref")?.as_object()?;
    if body_ref.len() != 2
        || body_ref.get("source").and_then(Value::as_str) != Some(CONTENT_TEXT_SOURCE)
        || !body_ref.contains_key("append_newline")
    {
        return None;
    }
    let append_newline = body_ref.get("append_newline")?.as_bool()?;
    let mut body = concatenated_text(content)?;
    if append_newline {
        body.push('\n');
    }
    Some(ToolDetails::Text {
        summary: summary.to_string(),
        body,
    })
}

fn concatenated_text(content: &[UserContent]) -> Option<String> {
    let mut text = String::new();
    for block in content {
        match block {
            UserContent::Text(block) => text.push_str(&block.text),
            UserContent::Image(_) => return None,
        }
    }
    Some(text)
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
}
