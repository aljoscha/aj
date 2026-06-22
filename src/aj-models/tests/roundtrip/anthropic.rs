//! Anthropic Messages round-trip tests.
//!
//! For each scenario covering an `AssistantContent` variant the provider
//! emits, runs the three test shapes from `docs/models-spec.md` §12 step
//! 11b:
//!
//! - **Parse**: fixture SSE → unified `AssistantMessage`. Asserts the
//!   structural fields the spec requires preserving (text, signatures,
//!   tool-call ids, …) against the canonical, hand-built message.
//! - **Serialize**: hand-built `AssistantMessage` → request item JSON.
//!   Compares against the golden `<scenario>.request.json` file.
//! - **Semantic round-trip**: parsed `AssistantMessage` → request item
//!   → re-parsed back to `AssistantMessage`. Asserts the content blocks
//!   round-trip field-equal modulo metadata that the §1.10 invariant
//!   explicitly does not require preserving (model, usage, response_id,
//!   stop_reason, timestamp).

use anthropic_sdk::messages::{MessageParam, ServerSentEvent};
use serde_json::{Value, json};

use aj_models::anthropic::provider::{
    assistant_message_to_request_item, parse_assistant_request_item, replay_sse_events,
};
use aj_models::registry::{InputModality, ModelCost, ModelInfo};
use aj_models::types::{
    AssistantContent, AssistantMessage, ErrorCategory, StopReason, TextContent, ThinkingContent,
    ToolCall, Usage,
};

use crate::common::{assert_content_eq, parse_sse, read_fixture, read_fixture_json};

const FIXTURE_DIR: &str = "anthropic-messages";

// ---------------------------------------------------------------------------
// Fixture model
// ---------------------------------------------------------------------------

/// Synthetic catalog entry used for replay.
///
/// The spec's round-trip invariant doesn't depend on cost or context
/// window, but [`replay_sse_events`] still needs a real [`ModelInfo`] to
/// stamp `provider`/`model` on the partial. We mirror Sonnet 4's basic
/// shape so any cost/finalize logic also exercises sensible numbers.
fn fixture_model() -> ModelInfo {
    ModelInfo {
        id: "claude-sonnet-4-20250514".into(),
        name: "Claude Sonnet 4".into(),
        api: "anthropic-messages".into(),
        provider: "anthropic".into(),
        base_url: "https://api.anthropic.com".into(),
        reasoning: true,
        supports_adaptive_thinking: true,
        supports_verbosity: false,
        input: vec![InputModality::Text],
        cost: ModelCost {
            input: 3.0,
            output: 15.0,
            cache_read: 0.3,
            cache_write: 3.75,
        },
        context_window: 200_000,
        max_tokens: 64_000,
        headers: None,
    }
}

// ---------------------------------------------------------------------------
// Fixture loader
// ---------------------------------------------------------------------------

fn load_sse(scenario: &str) -> Vec<ServerSentEvent> {
    let path = format!("{FIXTURE_DIR}/{scenario}.sse");
    let raw = read_fixture(&path);
    parse_sse(&raw)
        .into_iter()
        .map(|frame| {
            serde_json::from_str(&frame.data).unwrap_or_else(|err| {
                panic!(
                    "fixture {path}: data line failed to deserialize as ServerSentEvent: {err}\n\
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
// Canonical assistant messages — the source of truth per scenario.
//
// Each scenario's `canonical_*` builder produces the AssistantMessage we
// expect both: (a) the parser to return after replaying the fixture
// `.sse`, and (b) the serializer to project onto the fixture
// `.request.json`. Keeping a single in-test source of truth catches
// "fixture and code drifted" bugs that two separate expected values
// would mask.
// ---------------------------------------------------------------------------

fn canonical_text_only() -> AssistantMessage {
    let mut msg = AssistantMessage::empty();
    msg.api = "anthropic-messages".into();
    msg.provider = "anthropic".into();
    msg.model = "claude-sonnet-4-20250514".into();
    msg.response_id = Some("msg_round_trip_text".into());
    msg.content = vec![AssistantContent::Text(TextContent {
        text: "Hello, world!".into(),
        text_signature: None,
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
    msg.api = "anthropic-messages".into();
    msg.provider = "anthropic".into();
    msg.model = "claude-sonnet-4-20250514".into();
    msg.response_id = Some("msg_round_trip_thinking".into());
    msg.content = vec![
        AssistantContent::Thinking(ThinkingContent {
            thinking: "let me think about this".into(),
            thinking_signature: Some("base64-signature-bytes".into()),
            redacted: false,
        }),
        AssistantContent::Text(TextContent {
            text: "Sure, here's the answer.".into(),
            text_signature: None,
        }),
    ];
    msg.usage = Usage {
        input: 25,
        output: 18,
        cache_read: 0,
        cache_write: 0,
        total_tokens: 0,
        cost: Default::default(),
    };
    msg.stop_reason = StopReason::Stop;
    msg
}

fn canonical_tool_call() -> AssistantMessage {
    let mut msg = AssistantMessage::empty();
    msg.api = "anthropic-messages".into();
    msg.provider = "anthropic".into();
    msg.model = "claude-sonnet-4-20250514".into();
    msg.response_id = Some("msg_round_trip_tool".into());
    msg.content = vec![
        AssistantContent::Text(TextContent {
            text: "I'll read that file.".into(),
            text_signature: None,
        }),
        AssistantContent::ToolCall(ToolCall {
            id: "toolu_01abcDEF".into(),
            name: "read_file".into(),
            arguments: json!({"path": "/tmp/x"}),
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

fn canonical_redacted_thinking() -> AssistantMessage {
    let mut msg = AssistantMessage::empty();
    msg.api = "anthropic-messages".into();
    msg.provider = "anthropic".into();
    msg.model = "claude-sonnet-4-20250514".into();
    msg.response_id = Some("msg_round_trip_redacted".into());
    msg.content = vec![
        AssistantContent::Thinking(ThinkingContent {
            thinking: String::new(),
            thinking_signature: Some("encrypted-redacted-payload".into()),
            redacted: true,
        }),
        AssistantContent::Text(TextContent {
            text: "I cannot help with that.".into(),
            text_signature: None,
        }),
    ];
    msg.usage = Usage {
        input: 40,
        output: 12,
        cache_read: 0,
        cache_write: 0,
        total_tokens: 0,
        cost: Default::default(),
    };
    msg.stop_reason = StopReason::Stop;
    msg
}

// ---------------------------------------------------------------------------
// Per-scenario test trio
//
// Each scenario produces three top-level test functions so failures in
// one shape don't mask failures in another. The body of each test is
// kept tiny so the failure surfaces the fixture or the canonical
// builder directly.
// ---------------------------------------------------------------------------

fn run_parse_test(scenario: &str, canonical: AssistantMessage) {
    let events = load_sse(scenario);
    let parsed = replay_sse_events(&fixture_model(), events);
    assert_eq!(
        parsed.api, "anthropic-messages",
        "{scenario}: parsed api should mark anthropic-messages"
    );
    assert_eq!(
        parsed.stop_reason, canonical.stop_reason,
        "{scenario}: stop_reason mismatch"
    );
    assert_content_eq(&canonical, &parsed, &format!("parse:{scenario}"));
}

fn run_serialize_test(scenario: &str, canonical: AssistantMessage) {
    let param: MessageParam = assistant_message_to_request_item(&canonical);
    let actual = serde_json::to_value(&param).expect("MessageParam serializes to JSON");
    let expected = load_request_json(scenario);
    assert_eq!(
        actual,
        expected,
        "serialize:{scenario}: MessageParam JSON does not match golden file.\n\
         actual:   {}\nexpected: {}",
        serde_json::to_string_pretty(&actual).unwrap(),
        serde_json::to_string_pretty(&expected).unwrap()
    );
}

fn run_semantic_roundtrip_test(scenario: &str, canonical: AssistantMessage) {
    let events = load_sse(scenario);
    let parsed = replay_sse_events(&fixture_model(), events);
    let param = assistant_message_to_request_item(&parsed);
    let reparsed = parse_assistant_request_item(&param);
    // Semantic round-trip: the canonical message defines the structural
    // ground truth. Both `parsed` and `reparsed` should match it on the
    // §1.10 preserved fields.
    assert_content_eq(&canonical, &parsed, &format!("rt-parse:{scenario}"));
    assert_content_eq(&canonical, &reparsed, &format!("rt-reparse:{scenario}"));
    // The `api` tag survives the projection so listeners can still tell
    // which provider produced the message after a round-trip.
    assert_eq!(reparsed.api, "anthropic-messages");
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
// thinking_text scenario
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
// tool_call scenario
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
// redacted_thinking scenario
// ---------------------------------------------------------------------------

#[test]
fn parse_redacted_thinking() {
    run_parse_test("redacted_thinking", canonical_redacted_thinking());
}

#[test]
fn serialize_redacted_thinking() {
    run_serialize_test("redacted_thinking", canonical_redacted_thinking());
}

#[test]
fn semantic_roundtrip_redacted_thinking() {
    run_semantic_roundtrip_test("redacted_thinking", canonical_redacted_thinking());
}

// ---------------------------------------------------------------------------
// Error / truncation scenarios
//
// Unlike the happy-path trio, an errored or truncated turn is never
// serialized back into a request item (the agent retries or surfaces the
// error instead), so these scenarios only assert the terminal
// classification the adapter produces — `stop_reason` plus
// `error.category` — and that any partial content accumulated before the
// failure survives. They pin the §10.3 error legs against captured wire
// fixtures rather than hand-built events, the same way the happy path is
// pinned.
// ---------------------------------------------------------------------------

/// Replay an SSE fixture and return the finalized message. Thin wrapper
/// over the happy-path loader so the error scenarios share the exact
/// same parse path.
fn replay_fixture(scenario: &str) -> AssistantMessage {
    replay_sse_events(&fixture_model(), load_sse(scenario))
}

fn first_text(msg: &AssistantMessage) -> &str {
    match msg.content.first() {
        Some(AssistantContent::Text(t)) => &t.text,
        other => panic!("expected leading Text block, got {other:?}"),
    }
}

#[test]
fn truncated_stream_is_transient_error() {
    // A byte stream that drops after content but before `message_stop`
    // must finalize as a retryable transient error, not a `Done`, and
    // must preserve the partial deltas (the R1 bug this fixture guards).
    let parsed = replay_fixture("truncated");
    assert_eq!(parsed.stop_reason, StopReason::Error);
    assert_eq!(
        parsed.error.as_ref().map(|e| e.category),
        Some(ErrorCategory::Transient)
    );
    assert_eq!(first_text(&parsed), "This answer was cut o");
}

#[test]
fn mid_stream_error_frame_is_classified_error() {
    // A mid-stream `error` frame (here `overloaded_error`) terminates the
    // turn with the classified category and keeps the partial content.
    let parsed = replay_fixture("error_frame");
    assert_eq!(parsed.stop_reason, StopReason::Error);
    assert_eq!(
        parsed.error.as_ref().map(|e| e.category),
        Some(ErrorCategory::Overloaded)
    );
    assert_eq!(first_text(&parsed), "Working on it");
}

#[test]
fn refusal_stop_reason_is_content_filter() {
    // A `message_delta` carrying `stop_reason: refusal` + refusal
    // `stop_details` finalizes as a content-filter error.
    let parsed = replay_fixture("refusal");
    assert_eq!(parsed.stop_reason, StopReason::Error);
    assert_eq!(
        parsed.error.as_ref().map(|e| e.category),
        Some(ErrorCategory::ContentFilter)
    );
    assert_eq!(first_text(&parsed), "I can't help with that.");
}
