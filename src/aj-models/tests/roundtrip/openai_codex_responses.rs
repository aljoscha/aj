//! OpenAI Codex Responses round-trip tests (`docs/models-spec.md`
//! §1.10, §7.4, §12 step 8c.v).
//!
//! Four scenarios pin the §1.10 round-trip invariant for the Codex
//! deployment of the Responses API:
//!
//! - `text_only`: a single message item with one text part. Mirrors the
//!   public Responses suite's `text_only` and locks the `api`-tagging
//!   path through the Codex parse helpers.
//! - `thinking_text`: a reasoning item followed by a message item, the
//!   §7.3.3 reasoning round-trip exercised under the Codex API
//!   identity.
//! - `tool_call`: a message item plus a function_call item, exercising
//!   the §7.3.5 composite `{call_id}|{item_id}` ID and the streaming
//!   arguments parser on the Codex code path.
//! - `legacy_done`: a text-only flow whose terminal event is the older
//!   `response.done` name the Codex backend still emits in places. The
//!   §7.4.5 normalization layer must rewrite it to
//!   `response.completed` before the shared §7.3.6 state machine sees
//!   it; otherwise the final message's `stop_reason` lands as the
//!   default `Stop` from the state machine's unterminated path.
//!
//! For each scenario we run the standard parse / serialize / semantic
//! round-trip shape used by the other providers' suites, plus an extra
//! `terminal_status_completed` assertion on `legacy_done` to verify the
//! normalization happened.

use openai_sdk::types::responses::{
    ItemStatus, MessagePhase, ReasoningSummary, ResponseInputItem, ResponseStreamEvent,
};
use serde_json::Value;

use aj_models::openai::codex::{
    assistant_message_to_input_items, parse_assistant_input_items, replay_sse_events,
};
use aj_models::openai::responses::TextSignatureV1;
use aj_models::registry::{InputModality, ModelCost, ModelInfo};
use aj_models::types::{
    AssistantContent, AssistantMessage, StopReason, TextContent, ThinkingContent, ToolCall, Usage,
};

use crate::common::{assert_content_eq, parse_sse, read_fixture, read_fixture_json};

const FIXTURE_DIR: &str = "openai-codex-responses";
const API_NAME: &str = "openai-codex-responses";
const PROVIDER_ID: &str = "openai-codex";

// ---------------------------------------------------------------------------
// Fixture model
// ---------------------------------------------------------------------------

fn fixture_model() -> ModelInfo {
    ModelInfo {
        id: "gpt-5.1".into(),
        name: "GPT-5.1".into(),
        api: API_NAME.into(),
        provider: PROVIDER_ID.into(),
        base_url: "https://chatgpt.com/backend-api".into(),
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
        context_window: 272_000,
        max_tokens: 128_000,
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
        id: "rs_codex_1".to_string(),
        summary: vec![ReasoningSummary {
            text: "Planning the response.".to_string(),
            r#type: "summary_text".to_string(),
        }],
        content: None,
        encrypted_content: Some("codex-opaque".to_string()),
        status: Some(ItemStatus::Completed),
    };
    serde_json::to_string(&item).expect("serialize reasoning signature")
}

fn canonical_text_only() -> AssistantMessage {
    let mut msg = AssistantMessage::empty();
    msg.api = API_NAME.into();
    msg.provider = PROVIDER_ID.into();
    msg.model = "gpt-5.1".into();
    msg.response_id = Some("resp_codex_text_1".into());
    msg.content = vec![AssistantContent::Text(TextContent {
        text: "Hello from Codex!".into(),
        text_signature: Some(text_signature("msg_codex_text_1", None)),
    })];
    msg.usage = Usage {
        input: 14,
        output: 6,
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
    msg.api = API_NAME.into();
    msg.provider = PROVIDER_ID.into();
    msg.model = "gpt-5.1".into();
    msg.response_id = Some("resp_codex_think_1".into());
    msg.content = vec![
        AssistantContent::Thinking(ThinkingContent {
            thinking: "Planning the response.".into(),
            thinking_signature: Some(reasoning_signature()),
            redacted: false,
        }),
        AssistantContent::Text(TextContent {
            text: "Result: 1764.".into(),
            text_signature: Some(text_signature("msg_codex_2", None)),
        }),
    ];
    msg.usage = Usage {
        input: 30,
        output: 12,
        cache_read: 12,
        cache_write: 0,
        total_tokens: 0,
        cost: Default::default(),
    };
    msg.stop_reason = StopReason::Stop;
    msg
}

fn canonical_tool_call() -> AssistantMessage {
    let mut msg = AssistantMessage::empty();
    msg.api = API_NAME.into();
    msg.provider = PROVIDER_ID.into();
    msg.model = "gpt-5.1".into();
    msg.response_id = Some("resp_codex_tool_1".into());
    msg.content = vec![
        AssistantContent::Text(TextContent {
            text: "Listing the directory.".into(),
            text_signature: Some(text_signature("msg_codex_tool_1", None)),
        }),
        AssistantContent::ToolCall(ToolCall {
            id: "call_codex_1|fc_codex_1".into(),
            name: "ls".into(),
            arguments: serde_json::json!({"path": "/srv/data"}),
        }),
    ];
    msg.usage = Usage {
        input: 36,
        output: 18,
        cache_read: 0,
        cache_write: 0,
        total_tokens: 0,
        cost: Default::default(),
    };
    msg.stop_reason = StopReason::ToolUse;
    msg
}

fn canonical_legacy_done() -> AssistantMessage {
    let mut msg = AssistantMessage::empty();
    msg.api = API_NAME.into();
    msg.provider = PROVIDER_ID.into();
    msg.model = "gpt-5.1".into();
    msg.response_id = Some("resp_codex_legacy_1".into());
    msg.content = vec![AssistantContent::Text(TextContent {
        text: "Codex legacy frame.".into(),
        text_signature: Some(text_signature("msg_codex_legacy_1", None)),
    })];
    msg.usage = Usage {
        input: 10,
        output: 4,
        cache_read: 0,
        cache_write: 0,
        total_tokens: 0,
        cost: Default::default(),
    };
    msg.stop_reason = StopReason::Stop;
    msg
}

// ---------------------------------------------------------------------------
// Test shapes
// ---------------------------------------------------------------------------

fn run_parse_test(scenario: &str, canonical: AssistantMessage) {
    let events = load_sse(scenario);
    let parsed = replay_sse_events(&fixture_model(), events, None);
    assert_eq!(parsed.api, API_NAME);
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
    assert_eq!(reparsed.api, API_NAME);
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
// thinking_text scenario — §7.3.3 reasoning round-trip under Codex
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
// legacy_done scenario — §7.4.5 event normalization
// ---------------------------------------------------------------------------

#[test]
fn parse_legacy_done() {
    run_parse_test("legacy_done", canonical_legacy_done());
}

#[test]
fn serialize_legacy_done() {
    run_serialize_test("legacy_done", canonical_legacy_done());
}

#[test]
fn semantic_roundtrip_legacy_done() {
    run_semantic_roundtrip_test("legacy_done", canonical_legacy_done());
}

/// §7.4.5: the legacy `response.done` terminator must be rewritten to
/// `response.completed` so the §7.3.6 state machine's terminal arm
/// fires. If the normalization layer regresses, the state machine
/// finalizes via its "unterminated stream" path and the resulting
/// message lacks the `response.completed` usage / response-id payload.
/// This test pins the post-normalization state so a future change
/// that drops the rewrite shows up here, not as a subtle stop-reason
/// drift.
#[test]
fn legacy_done_terminator_normalized() {
    let events = load_sse("legacy_done");
    let parsed = replay_sse_events(&fixture_model(), events, None);
    assert_eq!(parsed.stop_reason, StopReason::Stop);
    assert_eq!(parsed.response_id.as_deref(), Some("resp_codex_legacy_1"));
    assert_eq!(parsed.usage.input, 10);
    assert_eq!(parsed.usage.output, 4);
}
