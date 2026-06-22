//! OpenAI Responses round-trip tests (`docs/models-spec.md` §1.10,
//! §12 step 11b.iv).
//!
//! Three scenarios cover each `AssistantContent` variant the provider
//! emits:
//!
//! - `text_only`: a single message item with one text part.
//! - `thinking_text`: a reasoning item followed by a message item, the
//!   §7.3.3 round-trip case where reasoning rides through
//!   `thinking_signature` for multi-turn replay.
//! - `tool_call`: a message item plus a function_call item, exercising
//!   the §7.3.5 composite `{call_id}|{item_id}` ID and the streaming
//!   arguments parser.
//!
//! For each scenario we run the standard parse / serialize / semantic
//! round-trip shape used by the other providers' suites.

use openai_sdk::types::responses::{
    ItemStatus, MessagePhase, ReasoningSummary, ResponseInputItem, ResponseStreamEvent,
};
use serde_json::Value;

use aj_models::openai::responses::{
    TextSignatureV1, assistant_message_to_input_items, parse_assistant_input_items,
    replay_sse_events,
};
use aj_models::registry::{InputModality, ModelCost, ModelInfo};
use aj_models::types::{
    AssistantContent, AssistantMessage, ErrorCategory, StopReason, TextContent, ThinkingContent,
    ToolCall, Usage,
};

use crate::common::{assert_content_eq, parse_sse, read_fixture, read_fixture_json};

const FIXTURE_DIR: &str = "openai-responses";

// ---------------------------------------------------------------------------
// Fixture model
// ---------------------------------------------------------------------------

fn fixture_model() -> ModelInfo {
    ModelInfo {
        id: "gpt-5".into(),
        name: "GPT-5".into(),
        api: "openai-responses".into(),
        provider: "openai".into(),
        base_url: "https://api.openai.com/v1".into(),
        reasoning: true,
        supports_adaptive_thinking: false,
        supports_verbosity: false,
        input: vec![InputModality::Text],
        cost: ModelCost {
            input: 1.25,
            output: 10.0,
            cache_read: 0.125,
            cache_write: 0.0,
        },
        context_window: 200_000,
        max_tokens: 16_000,
        headers: None,
    }
}

// ---------------------------------------------------------------------------
// Fixture loaders
// ---------------------------------------------------------------------------

/// Decode an SSE fixture into a vector of typed [`ResponseStreamEvent`]s
/// using the same JSON shape the live SDK feeds the streaming state
/// machine.
fn load_sse(scenario: &str) -> Vec<ResponseStreamEvent> {
    let path = format!("{FIXTURE_DIR}/{scenario}.sse");
    let raw = read_fixture(&path);
    parse_sse(&raw)
        .into_iter()
        .map(|frame| {
            serde_json::from_str(&frame.data).unwrap_or_else(|err| {
                panic!(
                    "fixture {path}: data line failed to deserialize as \
                     ResponseStreamEvent: {err}\n\
                     event={}, data={}",
                    frame.event, frame.data
                )
            })
        })
        .collect()
}

fn load_request_json(scenario: &str) -> Value {
    read_fixture_json(format!("{FIXTURE_DIR}/{scenario}.request.json"))
}

// ---------------------------------------------------------------------------
// Canonical assistant messages — source of truth per scenario
// ---------------------------------------------------------------------------

fn text_signature(id: &str, phase: Option<MessagePhase>) -> String {
    serde_json::to_string(&TextSignatureV1 {
        v: 1,
        id: id.to_string(),
        phase,
    })
    .expect("serialize TextSignatureV1")
}

/// JSON-encoded reasoning signature carried in `thinking_signature`.
/// Built from the typed [`ResponseInputItem::Reasoning`] struct so key
/// ordering matches what the parser writes (struct-field order, not
/// alphabetical).
fn reasoning_signature() -> String {
    let item = ResponseInputItem::Reasoning {
        id: "rs_1".to_string(),
        summary: vec![ReasoningSummary {
            text: "Considering the user request.".to_string(),
            r#type: "summary_text".to_string(),
        }],
        content: None,
        encrypted_content: Some("opaque-blob".to_string()),
        status: Some(ItemStatus::Completed),
    };
    serde_json::to_string(&item).expect("serialize reasoning signature")
}

fn canonical_text_only() -> AssistantMessage {
    let mut msg = AssistantMessage::empty();
    msg.api = "openai-responses".into();
    msg.provider = "openai".into();
    msg.model = "gpt-5".into();
    msg.response_id = Some("resp_text_1".into());
    msg.content = vec![AssistantContent::Text(TextContent {
        text: "Hello, world!".into(),
        text_signature: Some(text_signature("msg_text_1", None)),
    })];
    msg.usage = Usage {
        input: 12,
        output: 5,
        cache_read: 0,
        cache_write: 0,
        total_tokens: 0,
        cost: Default::default(),
    };
    msg.stop_reason = StopReason::Stop;
    msg
}

fn canonical_thinking_text() -> AssistantMessage {
    let mut msg = AssistantMessage::empty();
    msg.api = "openai-responses".into();
    msg.provider = "openai".into();
    msg.model = "gpt-5".into();
    msg.response_id = Some("resp_think_1".into());
    msg.content = vec![
        AssistantContent::Thinking(ThinkingContent {
            thinking: "Considering the user request.".into(),
            thinking_signature: Some(reasoning_signature()),
            redacted: false,
        }),
        AssistantContent::Text(TextContent {
            text: "The answer is 42.".into(),
            text_signature: Some(text_signature("msg_2", None)),
        }),
    ];
    msg.usage = Usage {
        input: 30,
        output: 18,
        cache_read: 10,
        cache_write: 0,
        total_tokens: 0,
        cost: Default::default(),
    };
    msg.stop_reason = StopReason::Stop;
    msg
}

fn canonical_tool_call() -> AssistantMessage {
    let mut msg = AssistantMessage::empty();
    msg.api = "openai-responses".into();
    msg.provider = "openai".into();
    msg.model = "gpt-5".into();
    msg.response_id = Some("resp_tool_1".into());
    msg.content = vec![
        AssistantContent::Text(TextContent {
            text: "I'll read that file.".into(),
            text_signature: Some(text_signature("msg_3", None)),
        }),
        AssistantContent::ToolCall(ToolCall {
            id: "call_1|fc_1".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({"path": "/tmp/x"}),
        }),
    ];
    msg.usage = Usage {
        input: 30,
        output: 22,
        cache_read: 0,
        cache_write: 0,
        total_tokens: 0,
        cost: Default::default(),
    };
    msg.stop_reason = StopReason::ToolUse;
    msg
}

// ---------------------------------------------------------------------------
// Test shapes
// ---------------------------------------------------------------------------

fn run_parse_test(scenario: &str, canonical: AssistantMessage) {
    let events = load_sse(scenario);
    let parsed = replay_sse_events(&fixture_model(), events, None);
    assert_eq!(parsed.api, "openai-responses");
    assert_eq!(parsed.stop_reason, canonical.stop_reason);
    assert_content_eq(&canonical, &parsed, &format!("parse:{scenario}"));
}

fn run_serialize_test(scenario: &str, canonical: AssistantMessage) {
    let items = assistant_message_to_input_items(&canonical);
    let actual = serde_json::to_value(&items).expect("input items serialize");
    let expected = load_request_json(scenario);
    assert_eq!(
        actual,
        expected,
        "serialize:{scenario}: input items do not match golden file.\n\
         actual:   {}\nexpected: {}",
        serde_json::to_string_pretty(&actual).unwrap(),
        serde_json::to_string_pretty(&expected).unwrap()
    );
}

fn run_semantic_roundtrip_test(scenario: &str, canonical: AssistantMessage) {
    let events = load_sse(scenario);
    let parsed = replay_sse_events(&fixture_model(), events, None);

    // §1.10: parsed unified message must serialize to the request item
    // shape and re-parse back into a structurally equivalent message.
    let items: Vec<ResponseInputItem> = assistant_message_to_input_items(&parsed);
    let reparsed = parse_assistant_input_items(&items);

    assert_content_eq(&canonical, &parsed, &format!("rt-parse:{scenario}"));
    assert_content_eq(&canonical, &reparsed, &format!("rt-reparse:{scenario}"));
    assert_eq!(reparsed.api, "openai-responses");
}

// ---------------------------------------------------------------------------
// text_only scenario
// ---------------------------------------------------------------------------

#[test]
fn parse_text_only() {
    run_parse_test("text_only", canonical_text_only());
}

#[test]
fn serialize_text_only() {
    run_serialize_test("text_only", canonical_text_only());
}

#[test]
fn semantic_roundtrip_text_only() {
    run_semantic_roundtrip_test("text_only", canonical_text_only());
}

// ---------------------------------------------------------------------------
// thinking_text scenario — §7.3.3 reasoning round-trip
// ---------------------------------------------------------------------------

#[test]
fn parse_thinking_text() {
    run_parse_test("thinking_text", canonical_thinking_text());
}

#[test]
fn serialize_thinking_text() {
    run_serialize_test("thinking_text", canonical_thinking_text());
}

#[test]
fn semantic_roundtrip_thinking_text() {
    run_semantic_roundtrip_test("thinking_text", canonical_thinking_text());
}

// ---------------------------------------------------------------------------
// tool_call scenario — §7.3.5 composite IDs + streaming arguments
// ---------------------------------------------------------------------------

#[test]
fn parse_tool_call() {
    run_parse_test("tool_call", canonical_tool_call());
}

#[test]
fn serialize_tool_call() {
    run_serialize_test("tool_call", canonical_tool_call());
}

#[test]
fn semantic_roundtrip_tool_call() {
    run_semantic_roundtrip_test("tool_call", canonical_tool_call());
}

// ---------------------------------------------------------------------------
// Error / truncation scenarios
//
// An errored or truncated turn is never serialized back into a request
// item, so these scenarios only assert the terminal classification
// (`stop_reason` + `error.category`) and that partial content survives.
// They pin the §10.3 / §7.3.8 terminal legs against captured wire
// fixtures.
// ---------------------------------------------------------------------------

/// Replay an SSE fixture and return the finalized message.
fn replay_fixture(scenario: &str) -> AssistantMessage {
    replay_sse_events(&fixture_model(), load_sse(scenario), None)
}

fn first_text(msg: &AssistantMessage) -> &str {
    match msg.content.first() {
        Some(AssistantContent::Text(t)) => &t.text,
        other => panic!("expected leading Text block, got {other:?}"),
    }
}

#[test]
fn truncated_stream_is_transient_error() {
    // A stream that ends before any terminal lifecycle event
    // (`response.completed` / `.incomplete` / `.failed`) is a mid-flight
    // drop: a retryable transient error with partial deltas preserved.
    let parsed = replay_fixture("truncated");
    assert_eq!(parsed.stop_reason, StopReason::Error);
    assert_eq!(
        parsed.error.as_ref().map(|e| e.category),
        Some(ErrorCategory::Transient)
    );
    assert_eq!(first_text(&parsed), "This answer was cut o");
}

#[test]
fn incomplete_length_is_clean_done() {
    // §7.3.8: a `response.incomplete` with `incomplete_details.reason:
    // max_output_tokens` is a *length cutoff* — a clean `Done(Length)`,
    // not an error. This is the positive control that distinguishes a
    // real length stop from a transport truncation.
    let parsed = replay_fixture("incomplete_length");
    assert_eq!(parsed.stop_reason, StopReason::Length);
    assert!(parsed.error.is_none());
    assert_eq!(first_text(&parsed), "A long answer that ran out of room");
}

#[test]
fn response_failed_is_classified_error() {
    // A `response.failed` terminates with an error whose category derives
    // from the wire error code. A bare `server_error` carries no HTTP
    // status on the SSE frame, so it lands in `Unknown` (deliberately not
    // auto-retried) rather than `Transient`.
    let parsed = replay_fixture("response_failed");
    assert_eq!(parsed.stop_reason, StopReason::Error);
    assert_eq!(
        parsed.error.as_ref().map(|e| e.category),
        Some(ErrorCategory::Unknown)
    );
}
