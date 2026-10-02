use std::io::Read;
use std::sync::Arc;

use aj_agent::bus::EventBus;
use aj_agent::events::{AgentEvent, AgentId};
use aj_agent::message::AgentMessage;
use aj_agent::tool::{ErasedToolDefinition, ToolDetails, ToolOutcome};
use aj_app::export::render_session_html;
use aj_models::types::{Message, ToolResultMessage, UserContent};
use aj_session::{
    ConversationLog, ConversationPersistence, ThreadFilter, persistence_listener, replay,
};
use aj_tools::ReadFileTool;
use aj_tools::testing::DummyToolContext;
use base64::Engine;
use flate2::read::GzDecoder;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::Mutex;

fn tool_result(id: &str, outcome: &ToolOutcome) -> ToolResultMessage {
    let mut result = ToolResultMessage::text(id, "read_file", "", outcome.is_error);
    result.content = outcome.content.clone();
    result.details = Some(serde_json::to_value(&outcome.details).expect("details serialize"));
    result
}

fn text_body(details: &Value) -> &str {
    details["body"].as_str().expect("text details body")
}

fn persisted_result<'a>(entries: &'a [Value], call_id: &str) -> &'a Value {
    entries
        .iter()
        .find(|entry| entry["message"]["tool_call_id"] == call_id)
        .unwrap_or_else(|| panic!("persisted result {call_id:?}"))
}

fn projected_body<'a>(messages: &'a [AgentMessage], call_id: &str) -> &'a str {
    messages
        .iter()
        .find_map(|message| match message.as_stored_wire() {
            Some(Message::ToolResult(result)) if result.tool_call_id == call_id => {
                result.details.as_ref().map(text_body)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("projected result {call_id:?}"))
}

fn wire_body<'a>(messages: &'a [Message], call_id: &str) -> &'a str {
    messages
        .iter()
        .find_map(|message| match message {
            Message::ToolResult(result) if result.tool_call_id == call_id => {
                result.details.as_ref().map(text_body)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("wire result {call_id:?}"))
}

fn replayed_body<'a>(events: &'a [AgentEvent], call_id: &str) -> &'a str {
    events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ToolExecutionEnd {
                call_id: found,
                result: ToolDetails::Text { body, .. },
                ..
            } if found == call_id => Some(body.as_str()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("replayed result {call_id:?}"))
}

fn replayed_message_bodies<'a>(events: &'a [AgentEvent], call_id: &str) -> Vec<&'a str> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::MessageStart { message, .. } | AgentEvent::MessageEnd { message, .. } => {
                match message.as_stored_wire() {
                    Some(Message::ToolResult(result)) if result.tool_call_id == call_id => {
                        result.details.as_ref().map(text_body)
                    }
                    _ => None,
                }
            }
            _ => None,
        })
        .collect()
}

fn count_string(value: &Value, target: &str) -> usize {
    match value {
        Value::String(value) => usize::from(value == target),
        Value::Array(values) => values.iter().map(|value| count_string(value, target)).sum(),
        Value::Object(values) => values
            .values()
            .map(|value| count_string(value, target))
            .sum(),
        Value::Null | Value::Bool(_) | Value::Number(_) => 0,
    }
}

fn decoded_export(html: &str) -> Value {
    let payload = html
        .split_once("id=\"session-data\">")
        .and_then(|(_, rest)| rest.split_once("</script>"))
        .map(|(payload, _)| payload)
        .expect("session data island");
    let compressed = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .expect("data island is base64");
    let mut decoded = String::new();
    GzDecoder::new(&compressed[..])
        .read_to_string(&mut decoded)
        .expect("data island gunzips");
    serde_json::from_str(&decoded).expect("export data parses")
}

fn exported_details<'a>(export: &'a Value, call_id: &str) -> &'a Value {
    export["entries"]
        .as_array()
        .expect("export entries")
        .iter()
        .find(|entry| entry["message"]["tool_call_id"] == call_id)
        .map(|entry| &entry["message"]["details"])
        .unwrap_or_else(|| panic!("exported result {call_id:?}"))
}

/// Crosses the real erased producer, persistence subscriber, JSONL codec,
/// resume projections, replay, print JSON, and HTML export. The second result
/// replaces only the display body at the post-hook boundary, proving that old
/// local-gutter bytes remain inline and are never reinterpreted by readers.
#[tokio::test]
async fn real_read_file_results_compact_and_preserve_post_hook_bodies() {
    const CANONICAL_ID: &str = "canonical-read";
    const HOOKED_ID: &str = "hooked-read";

    let dir = TempDir::new().expect("tempdir");
    let source = (1..=100)
        .map(|line| format!("source line {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(dir.path().join("source.txt"), source).expect("write source");

    let tool: ErasedToolDefinition = ReadFileTool::new().into();
    let mut context = DummyToolContext {
        working_directory: dir.path().to_path_buf(),
        ..DummyToolContext::default()
    };
    let outcome = (tool.func)(
        &mut context,
        json!({"path": "source.txt", "offset": 41, "limit": 40}),
    )
    .await
    .expect("execute read_file");

    let footer = "[20 more lines in file. Use offset=81 to continue.]";
    let absolute_lines = (41..=80)
        .map(|line| format!("{line:>5}: source line {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let expected_content = format!("{absolute_lines}\n\n{footer}");
    let expected_body = format!("{expected_content}\n");
    let canonical = tool_result(CANONICAL_ID, &outcome);
    let local_lines = (41..=80)
        .enumerate()
        .map(|(index, line)| format!("{:>5}: source line {line}", index + 1))
        .collect::<Vec<_>>()
        .join("\n");
    let legacy_local_body = format!("{local_lines}\n\n{footer}\n");
    let mut hooked = canonical.clone();
    hooked.tool_call_id = HOOKED_ID.to_string();
    hooked.details = Some(json!({
        "kind": "text",
        "summary": "source.txt 41:80",
        "body": legacy_local_body,
    }));

    let persistence = ConversationPersistence::new(dir.path().join("sessions"));
    let mut created = ConversationLog::create(&persistence).expect("create log");
    created
        .set_system_prompt("test prompt".to_string())
        .expect("set prompt");
    let session_id = created.session_id().to_string();
    let log_path = created.path().to_path_buf();
    let log = Arc::new(Mutex::new(created));
    let bus = EventBus::new();
    let listener = bus.subscribe(persistence_listener(Arc::clone(&log)));
    for result in [canonical, hooked] {
        bus.emit(AgentEvent::MessageEnd {
            agent_id: AgentId::Main,
            message: AgentMessage::wire(Message::ToolResult(result)),
        })
        .await
        .expect("persist tool result");
    }

    let raw = std::fs::read_to_string(&log_path).expect("read JSONL");
    let entries = raw
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid JSONL entry"))
        .collect::<Vec<Value>>();
    let canonical = persisted_result(&entries, CANONICAL_ID);
    let canonical_details = &canonical["message"]["details"];
    assert_eq!(canonical_details["body_ref"]["source"], "content_text");
    assert_eq!(canonical_details["body_ref"]["append_newline"], true);
    assert!(canonical_details.get("body").is_none());
    assert_eq!(count_string(canonical, &expected_content), 1);
    assert!(matches!(
        outcome.content.as_slice(),
        [UserContent::Text(text)] if text.text == expected_content
    ));
    let ToolDetails::Text { summary, body } = &outcome.details else {
        panic!("read_file returned non-text details");
    };
    assert_eq!(summary, "source.txt 41:80");
    assert_eq!(body, &expected_body);

    let hooked = persisted_result(&entries, HOOKED_ID);
    let hooked_details = &hooked["message"]["details"];
    assert_eq!(hooked_details["body"], legacy_local_body);
    assert!(hooked_details.get("body_ref").is_none());

    drop(listener);
    drop(bus);
    drop(log);
    let resumed = ConversationLog::resume(&persistence, &session_id).expect("resume log");
    let head = resumed.latest_leaf(ThreadFilter::USER).expect("user head");
    let conversation = resumed.linearize(&head, ThreadFilter::USER);
    let projected = conversation.agent_messages();
    assert_eq!(projected_body(&projected, CANONICAL_ID), expected_body);
    assert_eq!(projected_body(&projected, HOOKED_ID), legacy_local_body);
    let wire = conversation.messages();
    assert_eq!(wire_body(&wire, CANONICAL_ID), expected_body);
    assert_eq!(wire_body(&wire, HOOKED_ID), legacy_local_body);
    assert_eq!(
        wire_body(
            &[conversation.last_message().expect("last message")],
            HOOKED_ID
        ),
        legacy_local_body,
    );

    let replayed = replay(&resumed).collect::<Vec<_>>();
    assert_eq!(replayed_body(&replayed, CANONICAL_ID), expected_body);
    assert_eq!(replayed_body(&replayed, HOOKED_ID), legacy_local_body);
    assert_eq!(
        replayed_message_bodies(&replayed, CANONICAL_ID),
        [expected_body.as_str(), expected_body.as_str()],
    );
    assert_eq!(
        replayed_message_bodies(&replayed, HOOKED_ID),
        [legacy_local_body.as_str(), legacy_local_body.as_str()],
    );
    let print_json = replayed
        .iter()
        .map(|event| serde_json::to_string(event).expect("event serializes"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!print_json.contains("body_ref"), "{print_json}");

    let export = decoded_export(&render_session_html(&resumed));
    let exported_canonical = exported_details(&export, CANONICAL_ID);
    assert_eq!(text_body(exported_canonical), expected_body);
    assert!(exported_canonical.get("body_ref").is_none());
    assert_eq!(
        text_body(exported_details(&export, HOOKED_ID)),
        legacy_local_body,
    );
    assert_eq!(
        std::fs::read_to_string(log_path).expect("reread JSONL"),
        raw,
        "resume, replay, and export leave the append-only source unchanged",
    );
}
