//! OpenAI Chat Completions round-trip tests.
//!
//! For each scenario covering an `AssistantContent` variant the provider
//! emits, runs the test shapes from `docs/models-spec.md` §12 step 11b:
//!
//! - **Parse**: fixture SSE → unified `AssistantMessage`. Asserts the
//!   structural fields the spec requires preserving (text, tool-call
//!   ids/names/arguments, reasoning capture from compatible providers)
//!   against the canonical, hand-built message.
//! - **Serialize**: hand-built `AssistantMessage` → request item JSON.
//!   Compares against the golden `<scenario>.request.json` file. The
//!   comparison is performed on `serde_json::Value`s, not byte streams,
//!   so field ordering and whitespace don't break the assertion.
//! - **Semantic round-trip**: parsed `AssistantMessage` → request item
//!   → re-parsed back to `AssistantMessage`. Asserts the content blocks
//!   round-trip field-equal modulo metadata that the §1.10 invariant
//!   explicitly does not require preserving.
//!
//! For `openai-completions`, §1.10 explicitly lists reasoning under the
//! deliberately-not-preserved set (the public API does not accept
//! `reasoning_content` on input — see §7.2). The `reasoning_text`
//! scenario asserts that drop behavior directly: the parsed message has
//! a `Thinking` block, the request item drops it, and the reparsed
//! message contains only the text.

use openai_sdk::types::chat_completions::{
    ChatCompletionRequestMessage, CreateChatCompletionStreamResponse,
};
use serde_json::Value;

use aj_models::openai::provider::{
    assistant_message_to_request_item, parse_assistant_request_item, replay_sse_events,
};
use aj_models::registry::{InputModality, ModelCost, ModelInfo};
use aj_models::types::{
    AssistantContent, AssistantMessage, StopReason, TextContent, ThinkingContent, ToolCall, Usage,
};

use crate::common::{assert_content_eq, parse_sse, read_fixture, read_fixture_json};

const FIXTURE_DIR: &str = "openai-completions";

// ---------------------------------------------------------------------------
// Fixture model
// ---------------------------------------------------------------------------

/// Synthetic catalog entry used for replay.
///
/// The spec's round-trip invariant doesn't depend on cost or context
/// window, but [`replay_sse_events`] still needs a real [`ModelInfo`] to
/// stamp `provider`/`model` on the partial. We mirror a non-reasoning
/// GPT-4o-shaped entry so cost / finalize logic also exercises sensible
/// numbers.
fn fixture_model() -> ModelInfo {
    ModelInfo {
        id: "gpt-4o".into(),
        name: "GPT-4o".into(),
        api: "openai-completions".into(),
        provider: "openai".into(),
        base_url: "https://api.openai.com/v1".into(),
        reasoning: false,
        supports_xhigh: false,
        supports_adaptive_thinking: false,
        input: vec![InputModality::Text],
        cost: ModelCost {
            input: 2.5,
            output: 10.0,
            cache_read: 1.25,
            cache_write: 0.0,
        },
        context_window: 128_000,
        max_tokens: 16_000,
        headers: None,
    }
}

// ---------------------------------------------------------------------------
// Fixture loader
// ---------------------------------------------------------------------------

/// Load an OpenAI Chat Completions SSE fixture and decode each non-
/// terminal frame into a [`CreateChatCompletionStreamResponse`].
///
/// Skips `data: [DONE]` frames the way the live SDK's SSE filter does
/// (see `openai_sdk::client::Client::chat_completions_stream`); they
/// signal end-of-stream rather than a structural chunk.
fn load_sse(scenario: &str) -> Vec<CreateChatCompletionStreamResponse> {
    let path = format!("{FIXTURE_DIR}/{scenario}.sse");
    let raw = read_fixture(&path);
    parse_sse(&raw)
        .into_iter()
        .filter(|frame| frame.data.trim() != "[DONE]")
        .map(|frame| {
            serde_json::from_str(&frame.data).unwrap_or_else(|err| {
                panic!(
                    "fixture {path}: data line failed to deserialize as \
                     CreateChatCompletionStreamResponse: {err}\n\
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
//
// The `reasoning_text` scenario is special: §7.2 drops `Thinking` blocks
// on outbound, so the canonical here is the *parsed* shape (with the
// thinking block) and the serialized form omits it. The semantic
// round-trip test asserts that drop directly.
// ---------------------------------------------------------------------------

fn canonical_text_only() -> AssistantMessage {
    let mut msg = AssistantMessage::empty();
    msg.api = "openai-completions".into();
    msg.provider = "openai".into();
    msg.model = "gpt-4o".into();
    msg.response_id = Some("chatcmpl_round_trip_text".into());
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

fn canonical_tool_call() -> AssistantMessage {
    let mut msg = AssistantMessage::empty();
    msg.api = "openai-completions".into();
    msg.provider = "openai".into();
    msg.model = "gpt-4o".into();
    msg.response_id = Some("chatcmpl_round_trip_tool".into());
    msg.content = vec![
        AssistantContent::Text(TextContent {
            text: "I'll read that file.".into(),
            text_signature: None,
        }),
        AssistantContent::ToolCall(ToolCall {
            id: "call_abc".into(),
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

fn canonical_reasoning_text() -> AssistantMessage {
    let mut msg = AssistantMessage::empty();
    msg.api = "openai-completions".into();
    msg.provider = "openai".into();
    msg.model = "gpt-4o-r".into();
    msg.response_id = Some("chatcmpl_round_trip_reasoning".into());
    msg.content = vec![
        AssistantContent::Thinking(ThinkingContent {
            thinking: "let me think about this...".into(),
            thinking_signature: None,
            redacted: false,
        }),
        AssistantContent::Text(TextContent {
            text: "The answer is 42.".into(),
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

// ---------------------------------------------------------------------------
// Per-scenario test shapes
//
// Each scenario produces three top-level test functions so failures in
// one shape don't mask failures in another.
// ---------------------------------------------------------------------------

fn run_parse_test(scenario: &str, canonical: AssistantMessage) {
    let events = load_sse(scenario);
    let parsed = replay_sse_events(&fixture_model(), events);
    assert_eq!(
        parsed.api, "openai-completions",
        "{scenario}: parsed api should mark openai-completions"
    );
    assert_eq!(
        parsed.stop_reason, canonical.stop_reason,
        "{scenario}: stop_reason mismatch"
    );
    assert_content_eq(&canonical, &parsed, &format!("parse:{scenario}"));
}

fn run_serialize_test(scenario: &str, canonical: AssistantMessage) {
    let param: ChatCompletionRequestMessage = assistant_message_to_request_item(&canonical)
        .expect("non-empty canonical assistant message must serialize to Some");
    let actual =
        serde_json::to_value(&param).expect("ChatCompletionRequestMessage serializes to JSON");
    let expected = load_request_json(scenario);
    assert_eq!(
        actual,
        expected,
        "serialize:{scenario}: ChatCompletionRequestMessage JSON does not match golden file.\n\
         actual:   {}\nexpected: {}",
        serde_json::to_string_pretty(&actual).unwrap(),
        serde_json::to_string_pretty(&expected).unwrap()
    );
}

fn run_semantic_roundtrip_test(scenario: &str, canonical: AssistantMessage) {
    let events = load_sse(scenario);
    let parsed = replay_sse_events(&fixture_model(), events);
    let param =
        assistant_message_to_request_item(&parsed).expect("parsed assistant message non-empty");
    let reparsed = parse_assistant_request_item(&param);
    // Semantic round-trip: the canonical message defines the structural
    // ground truth. Both `parsed` and `reparsed` should match it on the
    // §1.10 preserved fields.
    assert_content_eq(&canonical, &parsed, &format!("rt-parse:{scenario}"));
    assert_content_eq(&canonical, &reparsed, &format!("rt-reparse:{scenario}"));
    // The `api` tag survives the projection so listeners can still tell
    // which provider produced the message after a round-trip.
    assert_eq!(reparsed.api, "openai-completions");
}

/// Variant of [`run_semantic_roundtrip_test`] that asserts §7.2 / §1.10's
/// explicit drop of reasoning content.
///
/// Used only by the `reasoning_text` scenario: the parsed message holds
/// a `Thinking` block, but `assistant_message_to_request_item` strips it
/// (the Chat Completions request shape has no `reasoning_content` field
/// the API will accept), and the reparsed message therefore contains
/// only the non-thinking blocks. Tests this drop is the spec's primary
/// motivation for marking reasoning as "deliberately not preserved" on
/// this provider.
fn run_semantic_roundtrip_drops_reasoning(scenario: &str, canonical: AssistantMessage) {
    let events = load_sse(scenario);
    let parsed = replay_sse_events(&fixture_model(), events);
    assert_content_eq(&canonical, &parsed, &format!("rt-parse:{scenario}"));

    let param =
        assistant_message_to_request_item(&parsed).expect("parsed assistant message non-empty");
    let reparsed = parse_assistant_request_item(&param);

    // The expected post-round-trip content is the canonical content with
    // every `Thinking` block filtered out. Any other block kind must
    // survive untouched, so we synthesize a stripped-canonical message
    // and compare against it block-for-block.
    let mut stripped = canonical.clone();
    stripped
        .content
        .retain(|b| !matches!(b, AssistantContent::Thinking(_)));
    assert!(
        !stripped.content.is_empty(),
        "fixture must keep at least one non-thinking block so the drop is observable"
    );
    assert_content_eq(&stripped, &reparsed, &format!("rt-reparse:{scenario}"));

    // And confirm that `parsed` actually carried a thinking block — if
    // the streaming parser ever stops surfacing `reasoning_content`, the
    // drop assertion above would silently turn into a tautology.
    assert!(
        parsed
            .content
            .iter()
            .any(|b| matches!(b, AssistantContent::Thinking(_))),
        "parsed message should contain a Thinking block from reasoning_content"
    );
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
// reasoning_text scenario
//
// Reasoning is explicitly listed under §1.10's deliberately-not-preserved
// set for this provider; see §7.2 for the API-level limitation. The
// three test shapes here verify (a) the streaming parser does capture
// reasoning_content into a Thinking block, (b) the request projection
// drops it, and (c) the round-trip degrades gracefully — no panic, no
// orphan blocks, just the non-thinking content surviving.
// ---------------------------------------------------------------------------

#[test]
fn parse_reasoning_text() {
    run_parse_test("reasoning_text", canonical_reasoning_text());
}

#[test]
fn serialize_reasoning_text() {
    run_serialize_test("reasoning_text", canonical_reasoning_text());
}

#[test]
fn semantic_roundtrip_reasoning_text() {
    run_semantic_roundtrip_drops_reasoning("reasoning_text", canonical_reasoning_text());
}
