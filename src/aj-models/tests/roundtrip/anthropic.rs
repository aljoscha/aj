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
    AssistantContent, AssistantMessage, StopReason, TextContent, ThinkingContent, ToolCall, Usage,
};

use crate::common::{parse_sse, read_fixture, read_fixture_json};

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
        supports_xhigh: false,
        supports_adaptive_thinking: true,
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
// Assertion helpers
// ---------------------------------------------------------------------------

/// Compare two assistant messages on the fields the round-trip invariant
/// requires preserving (`docs/models-spec.md` §1.10) and ignore the rest.
///
/// Specifically: response metadata (`api`, `provider`, `model`,
/// `response_id`, `usage`, `timestamp`) is *not* a multi-turn-significant
/// property — the next turn carries its own — so we don't enforce it on
/// the re-parse leg of a semantic round-trip. We do compare `content`
/// block-for-block, including all signatures.
fn assert_content_eq(expected: &AssistantMessage, actual: &AssistantMessage, ctx: &str) {
    assert_eq!(
        actual.content.len(),
        expected.content.len(),
        "{ctx}: content length mismatch (expected {:?}, got {:?})",
        expected.content,
        actual.content
    );
    for (i, (exp, got)) in expected
        .content
        .iter()
        .zip(actual.content.iter())
        .enumerate()
    {
        match (exp, got) {
            (AssistantContent::Text(e), AssistantContent::Text(g)) => {
                assert_eq!(e.text, g.text, "{ctx}: content[{i}] text body mismatch");
                assert_eq!(
                    e.text_signature, g.text_signature,
                    "{ctx}: content[{i}] text_signature mismatch"
                );
            }
            (AssistantContent::Thinking(e), AssistantContent::Thinking(g)) => {
                assert_eq!(
                    e.thinking, g.thinking,
                    "{ctx}: content[{i}] thinking body mismatch"
                );
                assert_eq!(
                    e.thinking_signature, g.thinking_signature,
                    "{ctx}: content[{i}] thinking_signature mismatch"
                );
                assert_eq!(
                    e.redacted, g.redacted,
                    "{ctx}: content[{i}] redacted flag mismatch"
                );
            }
            (AssistantContent::ToolCall(e), AssistantContent::ToolCall(g)) => {
                assert_eq!(e.id, g.id, "{ctx}: content[{i}] tool_call.id mismatch");
                assert_eq!(
                    e.name, g.name,
                    "{ctx}: content[{i}] tool_call.name mismatch"
                );
                assert_eq!(
                    e.arguments, g.arguments,
                    "{ctx}: content[{i}] tool_call.arguments mismatch"
                );
            }
            (e, g) => panic!("{ctx}: content[{i}] kind mismatch: expected {e:?}, got {g:?}"),
        }
    }
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
