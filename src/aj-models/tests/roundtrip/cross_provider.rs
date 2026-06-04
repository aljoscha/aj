//! Cross-provider transform tests.
//!
//! Per `docs/models-spec.md` §12 step 11b.iii, each cross-provider
//! direction in the supported pairs (`anthropic-messages` ↔
//! `openai-completions`, `anthropic-messages` ↔ `openai-responses`)
//! gets one end-to-end transform test. Each test feeds a multi-turn
//! history into [`transform_messages`] with a target [`ModelInfo`]
//! and asserts every §8.1 rule fires correctly on a single, realistic
//! transcript:
//!
//! 1. **Same-model passthrough** (rule 1) — a leading assistant turn
//!    from the target model itself, with signatures and redacted
//!    thinking that must survive untouched.
//! 2. **Cross-model thinking handling** (rule 2) — a cross-model
//!    assistant turn that mixes redacted thinking, empty-with-signature
//!    thinking, visible thinking, signed text. Asserts the redacted
//!    block is dropped, the empty-with-sig block is dropped, the visible
//!    thinking is demoted to plain text, and `text_signature` is
//!    stripped.
//! 3. **Tool-call ID normalization** (rule 3) — at least one cross-model
//!    `ToolCall` whose ID requires sanitization, truncation, composite
//!    handling, or all three.
//! 4. **Orphan synthesis** (rule 4) — a cross-model assistant turn whose
//!    second tool call has no matching `ToolResultMessage`. Asserts a
//!    synthetic `is_error: true` result is inserted at the turn boundary.
//! 5. **Errored / aborted skip** (rule 5) — an assistant turn marked
//!    `StopReason::Error` (or `Aborted`) followed by a tool result that
//!    references one of its tool calls. Asserts both the assistant and
//!    its result are dropped from the output.
//!
//! Each test asserts the expected output sequence position-by-position
//! so cross-talk between rules — e.g. an orphan synthesis firing for a
//! tool call that was supposed to be dropped under rule 5 — is caught
//! at the right offset.

use aj_models::registry::{InputModality, ModelCost, ModelInfo};
use aj_models::transform::{ORPHAN_TOOL_RESULT_TEXT, transform_messages};
use aj_models::types::{
    AssistantContent, AssistantMessage, Message, StopReason, TextContent, ThinkingContent,
    ToolCall, ToolResultMessage, Usage, UserContent, UserMessage,
};
use serde_json::json;

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

/// Build a synthetic [`ModelInfo`] for use as a transform target.
///
/// Cross-provider transforms only consult `provider`, `api`, `id`, and
/// `input` (for the §8.2 image downgrade), so the cost/context numbers
/// are fixed to harmless defaults.
fn target_model(provider: &str, api: &str, id: &str) -> ModelInfo {
    ModelInfo {
        id: id.into(),
        name: id.into(),
        api: api.into(),
        provider: provider.into(),
        base_url: "https://example.test".into(),
        reasoning: false,
        supports_adaptive_thinking: false,
        // Vision-on so the §8.2 image downgrade never fires here —
        // these tests focus on §8.1.
        input: vec![InputModality::Text, InputModality::Image],
        cost: ModelCost::default(),
        context_window: 100_000,
        max_tokens: 8_192,
        headers: None,
    }
}

/// Build an [`AssistantMessage`] with the given provider/api/model
/// stamps, content, and stop reason. Other metadata is irrelevant for
/// the transform layer and zeroed.
fn assistant_msg(
    provider: &str,
    api: &str,
    model_id: &str,
    content: Vec<AssistantContent>,
    stop_reason: StopReason,
) -> AssistantMessage {
    AssistantMessage {
        content,
        api: api.into(),
        provider: provider.into(),
        model: model_id.into(),
        response_id: None,
        usage: Usage::default(),
        stop_reason,
        error: None,
        timestamp: 0,
    }
}

fn text_block(text: &str) -> AssistantContent {
    AssistantContent::Text(TextContent {
        text: text.into(),
        text_signature: None,
    })
}

fn signed_text_block(text: &str, signature: &str) -> AssistantContent {
    AssistantContent::Text(TextContent {
        text: text.into(),
        text_signature: Some(signature.into()),
    })
}

fn thinking_block(thinking: &str, signature: Option<&str>, redacted: bool) -> AssistantContent {
    AssistantContent::Thinking(ThinkingContent {
        thinking: thinking.into(),
        thinking_signature: signature.map(str::to_string),
        redacted,
    })
}

fn tool_call_block(id: &str, name: &str) -> AssistantContent {
    AssistantContent::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: json!({}),
    })
}

fn tool_result_msg(call_id: &str, name: &str, body: &str) -> Message {
    Message::ToolResult(ToolResultMessage::text(call_id, name, body, false))
}

fn user_msg(text: &str) -> Message {
    Message::User(UserMessage::text(text))
}

// ---------------------------------------------------------------------------
// Assertion helpers shared across the four directions.
//
// Each direction's test composes a multi-turn history hitting all five
// §8.1 rules at once, then we walk the output once and check each
// assertion against its known-good position. Rule-specific helpers keep
// the per-direction tests focused on the bits unique to that direction
// (which IDs to normalize, which signatures to expect dropped).
// ---------------------------------------------------------------------------

/// Assert the kept-thinking block in a cross-model assistant message
/// has been demoted to plain text, with no `text_signature`. Used for
/// rule 2's "non-empty thinking → text" leg.
fn assert_demoted_text(content: &[AssistantContent], index: usize, expected_text: &str) {
    match &content[index] {
        AssistantContent::Text(t) => {
            assert_eq!(t.text, expected_text, "demoted thinking text body mismatch");
            assert!(
                t.text_signature.is_none(),
                "demoted thinking should have no text_signature"
            );
        }
        other => panic!("expected demoted text at index {index}, got {:?}", other),
    }
}

/// Assert a content block at `index` is a stripped (no signature) text
/// block with the given body. Used for the "text passes through, signature
/// stripped" leg of rule 2.
fn assert_stripped_text(content: &[AssistantContent], index: usize, expected_text: &str) {
    match &content[index] {
        AssistantContent::Text(t) => {
            assert_eq!(t.text, expected_text);
            assert!(
                t.text_signature.is_none(),
                "cross-model text should have text_signature stripped"
            );
        }
        other => panic!("expected stripped text at index {index}, got {:?}", other),
    }
}

/// Extract the (id, name) of a `ToolCall` block at a specific index,
/// panicking if the variant doesn't match.
fn tool_call_at(content: &[AssistantContent], index: usize) -> (&str, &str) {
    match &content[index] {
        AssistantContent::ToolCall(tc) => (tc.id.as_str(), tc.name.as_str()),
        other => panic!("expected tool_call at index {index}, got {:?}", other),
    }
}

/// Pull an `AssistantMessage` reference out at a specific position in
/// the transformed history, panicking on mismatch.
fn expect_assistant<'a>(messages: &'a [Message], index: usize) -> &'a AssistantMessage {
    match &messages[index] {
        Message::Assistant(a) => a,
        other => panic!("expected assistant at index {index}, got {:?}", other),
    }
}

/// Pull a `ToolResultMessage` reference out at a specific position,
/// panicking on mismatch.
fn expect_tool_result<'a>(messages: &'a [Message], index: usize) -> &'a ToolResultMessage {
    match &messages[index] {
        Message::ToolResult(tr) => tr,
        other => panic!("expected tool_result at index {index}, got {:?}", other),
    }
}

/// Pull a `UserMessage` reference out at a specific position, panicking
/// on mismatch.
fn expect_user<'a>(messages: &'a [Message], index: usize) -> &'a UserMessage {
    match &messages[index] {
        Message::User(u) => u,
        other => panic!("expected user at index {index}, got {:?}", other),
    }
}

/// Assert a tool result is the synthetic orphan placeholder per rule 4.
fn assert_synthetic_orphan(result: &ToolResultMessage, expected_call_id: &str) {
    assert_eq!(
        result.tool_call_id, expected_call_id,
        "synthetic orphan should reference the unmatched call id"
    );
    assert!(result.is_error, "synthetic orphan must be marked is_error");
    match result.content.first() {
        Some(UserContent::Text(t)) => assert_eq!(t.text, ORPHAN_TOOL_RESULT_TEXT),
        other => panic!(
            "synthetic orphan body should be the §8.1 placeholder, got {:?}",
            other
        ),
    }
}

// ---------------------------------------------------------------------------
// Direction 1: anthropic-messages → openai-completions
//
// What we check end-to-end:
//
// - Rule 1: An Anthropic→Anthropic same-model turn? Not applicable here
//   because the target is Completions. We instead exercise rule 1 by
//   having a same-target (`openai-completions`) assistant turn ride
//   through with a tool-call whose ID remains exactly as authored.
// - Rule 2: an Anthropic turn carrying redacted_thinking + empty-signed
//   thinking + visible thinking + signed text. Redacted+empty-signed
//   are dropped, visible thinking demotes, text loses its signature.
// - Rule 3: an Anthropic-style `toolu_…` ID with characters outside
//   the permitted class and >40 chars total — sanitized and truncated
//   to 40 for Completions. The matching `ToolResultMessage` follows
//   the rewritten ID via the pass-2 map.
// - Rule 4: that same Anthropic turn carries a second tool call with
//   no matching `ToolResultMessage`. After the user message arrives,
//   a synthetic orphan result must be emitted before it.
// - Rule 5: a third assistant turn marked `StopReason::Aborted`, plus
//   a tool_result that references its tool call — both vanish.
// ---------------------------------------------------------------------------

#[test]
fn transform_anthropic_to_completions_full_history() {
    let target = target_model("openai", "openai-completions", "gpt-4o");

    // Rule 1: a same-target assistant turn whose tool call ID must NOT
    // change, and whose matching tool result must keep the same id.
    let same_model_id = "call_same_target_42";
    let same_target_assistant = assistant_msg(
        "openai",
        "openai-completions",
        "gpt-4o",
        vec![
            text_block("Sure, I can help."),
            tool_call_block(same_model_id, "ls"),
        ],
        StopReason::ToolUse,
    );

    // Rule 2 + 3 + 4: a cross-model Anthropic turn carrying every
    // thinking-block shape, plus two tool calls (one will be matched,
    // one orphaned).
    let raw_anthropic_id = "toolu_call!with/odd:chars-".to_string() + &"x".repeat(60);
    let orphan_id = "toolu_orphan_id";
    let cross_anthropic_assistant = assistant_msg(
        "anthropic",
        "anthropic-messages",
        "claude-sonnet-4",
        vec![
            // Redacted: dropped.
            thinking_block("", Some("opaque-redacted-payload"), true),
            // Empty with signature: dropped.
            thinking_block("", Some("invalid-elsewhere"), false),
            // Visible thinking with signature: demoted to plain text,
            // signature dropped.
            thinking_block("let me reason about this", Some("sig-bytes"), false),
            // Signed text: text_signature stripped on cross-model.
            signed_text_block("Reading the file now.", "anthropic-text-sig"),
            // Tool call with characters outside [a-zA-Z0-9_-] and longer
            // than 40 chars — must sanitize + truncate for Completions.
            tool_call_block(&raw_anthropic_id, "read_file"),
            // Second tool call with no matching tool result — orphaned.
            tool_call_block(orphan_id, "read_file"),
        ],
        StopReason::ToolUse,
    );
    // ToolResultMessage references the *original* (pre-normalization)
    // ID; the transform map rewrites it to whatever the assistant's
    // tool_call now uses.
    let cross_tool_result = tool_result_msg(&raw_anthropic_id, "read_file", "file contents");

    // Rule 5: aborted assistant + its tool result. Both should vanish.
    let aborted_assistant_call_id = "toolu_aborted_call";
    let aborted_assistant = assistant_msg(
        "anthropic",
        "anthropic-messages",
        "claude-sonnet-4",
        vec![tool_call_block(aborted_assistant_call_id, "bash")],
        StopReason::Aborted,
    );
    let aborted_tool_result =
        tool_result_msg(aborted_assistant_call_id, "bash", "this should disappear");

    let history = vec![
        user_msg("Hi, what's in /tmp/x?"),
        Message::Assistant(same_target_assistant),
        tool_result_msg(same_model_id, "ls", "ok"),
        user_msg("now read it please"),
        Message::Assistant(cross_anthropic_assistant),
        cross_tool_result,
        // The orphan from the cross-model assistant must be synthesized
        // *before* this user message, by rule 4.
        user_msg("ok thanks"),
        Message::Assistant(aborted_assistant),
        aborted_tool_result,
        // Trailing user message confirms the rule-5 drops cascade
        // through to here without the aborted turn's tool result
        // sneaking back in.
        user_msg("are you done?"),
    ];

    let out = transform_messages(&history, &target);

    // Expected sequence:
    //   0. user "Hi, what's in /tmp/x?"
    //   1. same-target assistant (untouched)
    //   2. tool_result for same_model_id
    //   3. user "now read it please"
    //   4. cross-model assistant (rules 2, 3 applied, second tool call kept)
    //   5. tool_result for the *normalized* anthropic id (rule 3 via id_map)
    //   6. synthetic orphan tool_result for the second (unmatched) call
    //   7. user "ok thanks"
    //   8. user "are you done?"   (rule 5 dropped the aborted turn + its result)
    assert_eq!(out.len(), 9, "unexpected output length: {:?}", out);

    // 0: leading user passes through.
    assert!(matches!(out[0], Message::User(_)));

    // 1: rule 1 — same-target assistant survives without modification.
    let same = expect_assistant(&out, 1);
    assert_eq!(same.api, "openai-completions");
    assert_eq!(same.content.len(), 2);
    let (same_tc_id, _) = tool_call_at(&same.content, 1);
    assert_eq!(
        same_tc_id, same_model_id,
        "rule 1: same-target tool_call id must not change"
    );

    // 2: matching tool_result for the same-target call — id unchanged.
    let same_result = expect_tool_result(&out, 2);
    assert_eq!(same_result.tool_call_id, same_model_id);

    // 3: user message between turns.
    assert!(matches!(out[3], Message::User(_)));

    // 4: rule 2 — cross-model assistant rewritten.
    let cross = expect_assistant(&out, 4);
    assert_eq!(
        cross.content.len(),
        4,
        "expected 4 surviving blocks (visible thinking → text, signed text \
         → stripped text, two tool calls), got {:?}",
        cross.content
    );
    // [0] = demoted thinking text
    assert_demoted_text(&cross.content, 0, "let me reason about this");
    // [1] = original text, signature stripped
    assert_stripped_text(&cross.content, 1, "Reading the file now.");
    // [2] = first tool call, ID rewritten to ≤40 chars and sanitized
    let (matched_id, _) = tool_call_at(&cross.content, 2);
    assert!(
        matched_id.len() <= 40,
        "rule 3: completions IDs must be ≤40 chars, got {} ({})",
        matched_id.len(),
        matched_id
    );
    assert!(
        matched_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
        "rule 3: completions IDs must be sanitized, got {}",
        matched_id
    );
    assert_ne!(
        matched_id, raw_anthropic_id,
        "rule 3 should have rewritten the raw anthropic id"
    );
    // [3] = orphan tool call survives in-place (only its result is missing).
    let (orphan_normalized_id, _) = tool_call_at(&cross.content, 3);
    assert!(orphan_normalized_id.len() <= 40);
    assert_ne!(orphan_normalized_id, matched_id);

    // 5: matched tool_result, ID rewritten to track the assistant's normalized id.
    let matched_result = expect_tool_result(&out, 5);
    assert_eq!(
        matched_result.tool_call_id, matched_id,
        "rule 3: tool_result must follow the assistant's normalized ID"
    );

    // 6: synthetic orphan for the second tool call.
    let orphan_result = expect_tool_result(&out, 6);
    assert_synthetic_orphan(orphan_result, orphan_normalized_id);

    // 7-8: user messages, no aborted turn or its tool result in between.
    let _ = expect_user(&out, 7);
    let _ = expect_user(&out, 8);
    assert!(
        out.iter().all(|m| match m {
            Message::ToolResult(tr) => tr.tool_call_id != aborted_assistant_call_id,
            _ => true,
        }),
        "rule 5: aborted turn's tool result must be dropped"
    );
}

// ---------------------------------------------------------------------------
// Direction 2: openai-completions → anthropic-messages
//
// What we check end-to-end:
//
// - Rule 1: same-target Anthropic assistant whose signed thinking and
//   redacted thinking blocks survive verbatim, and whose `toolu_*` ID
//   is preserved.
// - Rule 2: a Completions-source turn (no thinking blocks possible —
//   §1.10 / §7.2 — but text with a `text_signature` and a tool_call
//   ID that needs sanitization).
// - Rule 3: a `call_…` ID that contains characters outside the allowed
//   class is sanitized and truncated to ≤64 for Anthropic.
// - Rule 4: a second tool call from the Completions turn has no
//   matching tool result — orphan synthesized.
// - Rule 5: aborted Completions assistant + its tool result are
//   dropped.
// ---------------------------------------------------------------------------

#[test]
fn transform_completions_to_anthropic_full_history() {
    let target = target_model("anthropic", "anthropic-messages", "claude-sonnet-4");

    // Rule 1: same-target Anthropic assistant. Signed thinking, redacted
    // thinking, and a `toolu_*` ID all survive.
    let same_target_call_id = "toolu_same_target_xyz";
    let same_target_assistant = assistant_msg(
        "anthropic",
        "anthropic-messages",
        "claude-sonnet-4",
        vec![
            thinking_block("internal reasoning", Some("anthropic-sig-bytes"), false),
            thinking_block("", Some("anthropic-redacted-blob"), true),
            text_block("Reading now."),
            tool_call_block(same_target_call_id, "read_file"),
        ],
        StopReason::ToolUse,
    );

    // Rule 2 + 3 + 4: cross-model Completions turn.
    // Completions never produces signed thinking (§1.10 / §7.2), so the
    // only cross-model rewrites that fire are: text_signature strip,
    // tool-id sanitize/truncate, and orphan synthesis on the second
    // tool call.
    let raw_completions_id = "call/with:bad chars".to_string() + &"y".repeat(80);
    let orphan_completions_id = "call_orphan_x";
    let cross_completions_assistant = assistant_msg(
        "openai",
        "openai-completions",
        "gpt-4o",
        vec![
            signed_text_block("Sure, will do.", "completions-text-sig"),
            tool_call_block(&raw_completions_id, "read_file"),
            tool_call_block(orphan_completions_id, "ls"),
        ],
        StopReason::ToolUse,
    );
    let cross_tool_result = tool_result_msg(&raw_completions_id, "read_file", "file contents");

    // Rule 5: errored Completions assistant + its tool result.
    let errored_call_id = "call_errored_x";
    let errored_assistant = assistant_msg(
        "openai",
        "openai-completions",
        "gpt-4o",
        vec![tool_call_block(errored_call_id, "bash")],
        StopReason::Error,
    );
    let errored_tool_result = tool_result_msg(errored_call_id, "bash", "should disappear");

    let history = vec![
        user_msg("read file please"),
        Message::Assistant(same_target_assistant),
        tool_result_msg(same_target_call_id, "read_file", "ok"),
        user_msg("again, on a different file"),
        Message::Assistant(cross_completions_assistant),
        cross_tool_result,
        user_msg("thanks"),
        Message::Assistant(errored_assistant),
        errored_tool_result,
        user_msg("done?"),
    ];

    let out = transform_messages(&history, &target);

    // Expected sequence:
    //   0. user "read file please"
    //   1. same-target assistant (rule 1: untouched)
    //   2. tool_result for the same-target call (id preserved)
    //   3. user "again..."
    //   4. cross-model completions assistant (rule 2/3 applied)
    //   5. tool_result for the *rewritten* call id
    //   6. synthetic orphan for the second cross-model call
    //   7. user "thanks"
    //   8. user "done?"   (rule 5 dropped the errored turn + result)
    assert_eq!(out.len(), 9, "unexpected output length: {:?}", out);

    // 1: rule 1 — same-target assistant preserves signatures + redacted.
    let same = expect_assistant(&out, 1);
    assert_eq!(same.content.len(), 4, "all 4 blocks should survive");
    match &same.content[0] {
        AssistantContent::Thinking(th) => {
            assert_eq!(th.thinking, "internal reasoning");
            assert_eq!(
                th.thinking_signature.as_deref(),
                Some("anthropic-sig-bytes")
            );
            assert!(!th.redacted);
        }
        _ => panic!("expected signed thinking"),
    }
    match &same.content[1] {
        AssistantContent::Thinking(th) => {
            assert!(th.redacted);
            assert_eq!(
                th.thinking_signature.as_deref(),
                Some("anthropic-redacted-blob")
            );
        }
        _ => panic!("expected redacted thinking"),
    }
    let (same_id, _) = tool_call_at(&same.content, 3);
    assert_eq!(same_id, same_target_call_id);

    // 2: matching tool_result, id preserved.
    let same_result = expect_tool_result(&out, 2);
    assert_eq!(same_result.tool_call_id, same_target_call_id);

    // 4: cross-model rewrite.
    let cross = expect_assistant(&out, 4);
    assert_eq!(
        cross.content.len(),
        3,
        "all blocks survive (no thinking on completions source)"
    );
    // Text: signature stripped.
    assert_stripped_text(&cross.content, 0, "Sure, will do.");
    // First tool call: sanitize + ≤64 truncation for Anthropic target.
    let (matched_id, _) = tool_call_at(&cross.content, 1);
    assert!(
        matched_id.len() <= 64,
        "rule 3: anthropic IDs must be ≤64 chars, got {}",
        matched_id.len()
    );
    assert!(
        matched_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
        "rule 3: anthropic IDs must be sanitized, got {}",
        matched_id
    );
    assert_ne!(matched_id, raw_completions_id);
    let (orphan_normalized_id, _) = tool_call_at(&cross.content, 2);
    assert_eq!(
        orphan_normalized_id, orphan_completions_id,
        "rule 3: already-clean ID under target's char class & length passes through"
    );

    // 5: matching tool_result rewritten to follow the assistant's id.
    let matched_result = expect_tool_result(&out, 5);
    assert_eq!(matched_result.tool_call_id, matched_id);

    // 6: synthetic orphan for the second cross-model call.
    let orphan_result = expect_tool_result(&out, 6);
    assert_synthetic_orphan(orphan_result, orphan_normalized_id);

    // 7-8: user messages, no errored turn or its tool result.
    let _ = expect_user(&out, 7);
    let _ = expect_user(&out, 8);
    assert!(
        out.iter().all(|m| match m {
            Message::ToolResult(tr) => tr.tool_call_id != errored_call_id,
            _ => true,
        }),
        "rule 5: errored turn's tool result must be dropped"
    );
}

// ---------------------------------------------------------------------------
// Direction 3: anthropic-messages → openai-responses
//
// What we check end-to-end:
//
// - Rule 1: same-target Responses assistant whose composite
//   `{call_id}|{item_id}` tool ID survives unchanged (both halves
//   already legal under the Responses character class).
// - Rule 2: Anthropic-source thinking blocks (redacted, empty-with-sig,
//   visible signed) handled per rule 2.
// - Rule 3: an Anthropic-source ID that happens to contain a `|` (the
//   contrived shape that exercises the foreign-origin item_id rewrite
//   path: foreign-origin item halves get a stable `fc_<hash>`); an
//   Anthropic-source non-composite ID, which stays non-composite and
//   only gets sanitize+truncate per §7.3.5.
// - Rule 4: a second tool call from the Anthropic turn has no result —
//   synthetic orphan is emitted before the next user message.
// - Rule 5: aborted Anthropic turn + its tool result both vanish.
// ---------------------------------------------------------------------------

#[test]
fn transform_anthropic_to_responses_full_history() {
    let target = target_model("openai", "openai-responses", "gpt-5");

    // Rule 1: same-target Responses assistant.
    let same_target_id = "call_same_target|fc_item_xyz";
    let same_target_assistant = assistant_msg(
        "openai",
        "openai-responses",
        "gpt-5",
        vec![
            // Responses encodes its reasoning-item JSON into thinking_signature;
            // round-tripping the signature is the whole point of rule 1 here.
            thinking_block("planning", Some("{\"id\":\"rs_x\"}"), false),
            text_block("Acknowledged."),
            tool_call_block(same_target_id, "read_file"),
        ],
        StopReason::ToolUse,
    );

    // Rule 2 + 3 + 4: cross-model Anthropic turn.
    //
    // Two tool calls cover the two §8.1-rule-3 paths for Responses
    // targets: one composite-shaped ID (exercises the foreign-origin
    // item half rewrite to `fc_<hash>`) and one plain `toolu_*` ID
    // (stays non-composite, just sanitized).
    let composite_anthropic_id = "toolu_call_composite|some_item";
    let plain_anthropic_id = "toolu_plain_DEF";
    let cross_anthropic_assistant = assistant_msg(
        "anthropic",
        "anthropic-messages",
        "claude-sonnet-4",
        vec![
            thinking_block("", Some("opaque-redacted"), true),
            thinking_block("", Some("empty-signed-discarded"), false),
            thinking_block("reasoning visible", Some("anthropic-sig"), false),
            signed_text_block("OK reading.", "anthropic-text-sig"),
            tool_call_block(composite_anthropic_id, "read_file"),
            tool_call_block(plain_anthropic_id, "ls"),
        ],
        StopReason::ToolUse,
    );
    // Match the *composite* tool call so we can verify the id_map
    // rewrite tracks the foreign-origin substitution.
    let cross_tool_result = tool_result_msg(composite_anthropic_id, "read_file", "contents");

    // Rule 5: aborted Anthropic turn + tool result.
    let aborted_id = "toolu_aborted";
    let aborted_assistant = assistant_msg(
        "anthropic",
        "anthropic-messages",
        "claude-sonnet-4",
        vec![tool_call_block(aborted_id, "bash")],
        StopReason::Aborted,
    );
    let aborted_tool_result = tool_result_msg(aborted_id, "bash", "should disappear");

    let history = vec![
        user_msg("hello"),
        Message::Assistant(same_target_assistant),
        tool_result_msg(same_target_id, "read_file", "ok"),
        user_msg("now read /tmp/x"),
        Message::Assistant(cross_anthropic_assistant),
        cross_tool_result,
        user_msg("thanks"),
        Message::Assistant(aborted_assistant),
        aborted_tool_result,
        user_msg("anything else?"),
    ];

    let out = transform_messages(&history, &target);

    // Expected sequence (same shape as Direction 1):
    //   0. user
    //   1. same-target assistant (rule 1: composite ID + thinking sig preserved)
    //   2. tool_result for the same-target composite id
    //   3. user
    //   4. cross-model anthropic assistant (rule 2/3 applied)
    //   5. tool_result for the *rewritten* composite tool-call id
    //   6. synthetic orphan for the second (plain) tool call
    //   7. user "thanks"
    //   8. user "anything else?"   (rule 5 dropped aborted turn + result)
    assert_eq!(out.len(), 9, "unexpected output length: {:?}", out);

    // 1: rule 1 — composite ID and thinking signature survive verbatim.
    let same = expect_assistant(&out, 1);
    assert_eq!(same.content.len(), 3);
    match &same.content[0] {
        AssistantContent::Thinking(th) => {
            assert_eq!(th.thinking, "planning");
            assert_eq!(th.thinking_signature.as_deref(), Some("{\"id\":\"rs_x\"}"));
        }
        _ => panic!("expected signed thinking"),
    }
    let (same_id, _) = tool_call_at(&same.content, 2);
    assert_eq!(same_id, same_target_id, "rule 1: composite ID preserved");
    assert!(same_id.contains('|'), "responses IDs are composites");

    // 4: rule 2/3 — Anthropic content rewritten.
    let cross = expect_assistant(&out, 4);
    assert_eq!(
        cross.content.len(),
        4,
        "redacted + empty-signed dropped, visible thinking demoted, text + 2 tool calls survive"
    );
    assert_demoted_text(&cross.content, 0, "reasoning visible");
    assert_stripped_text(&cross.content, 1, "OK reading.");

    // First tool call — composite Anthropic-origin ID. Foreign-origin path
    // leaves the call_id half alone (just sanitize+truncate) and rewrites
    // the item_id half to `fc_<short_hash>`.
    let (matched_id, _) = tool_call_at(&cross.content, 2);
    let (matched_call, matched_item) = matched_id.split_once('|').unwrap_or_else(|| {
        panic!(
            "rule 3: composite source ID should stay composite for responses target, got {}",
            matched_id
        )
    });
    assert_eq!(matched_call, "toolu_call_composite");
    assert!(
        matched_item.starts_with("fc_"),
        "rule 3 / §7.3.5: foreign-origin item_id must be `fc_<hash>`, got {}",
        matched_item
    );
    assert!(
        matched_item.len() <= 64 && matched_call.len() <= 64,
        "rule 3: each half must be ≤64 chars"
    );
    // Foreign-origin item_id substitution is a stable hash, so re-running
    // the same transform on the same input produces the same id.
    let again = transform_messages(&[history[4].clone()], &target);
    let (again_id, _) = tool_call_at(&expect_assistant(&again, 0).content, 2);
    assert_eq!(again_id, matched_id, "foreign-origin hash must be stable");

    // Second (plain) tool call — non-composite Anthropic ID, stays
    // non-composite per §7.3.5 (no item_id half to synthesize). Sanitize
    // + truncate only.
    let (orphan_normalized_id, _) = tool_call_at(&cross.content, 3);
    assert!(
        !orphan_normalized_id.contains('|'),
        "rule 3: non-composite source IDs stay non-composite for responses targets"
    );
    assert_eq!(
        orphan_normalized_id, plain_anthropic_id,
        "rule 3: a clean non-composite Anthropic ID passes through unchanged"
    );

    // 5: matching tool_result follows the rewritten composite ID.
    let matched_result = expect_tool_result(&out, 5);
    assert_eq!(matched_result.tool_call_id, matched_id);

    // 6: synthetic orphan for the second tool call.
    let orphan_result = expect_tool_result(&out, 6);
    assert_synthetic_orphan(orphan_result, orphan_normalized_id);

    // 7-8: user messages, aborted turn + result both gone.
    let _ = expect_user(&out, 7);
    let _ = expect_user(&out, 8);
    assert!(
        out.iter().all(|m| match m {
            Message::ToolResult(tr) => !tr.tool_call_id.starts_with(aborted_id),
            _ => true,
        }),
        "rule 5: aborted turn's tool result must be dropped"
    );
}

// ---------------------------------------------------------------------------
// Direction 4: openai-responses → anthropic-messages
//
// What we check end-to-end:
//
// - Rule 1: same-target Anthropic assistant whose `toolu_*` ID and
//   signed/redacted thinking ride through verbatim.
// - Rule 2: a Responses-source assistant with a thinking block carrying
//   a (Responses-shaped) signature — signature dropped, visible
//   thinking demoted to plain text. Empty redacted thinking dropped.
// - Rule 3: composite `{call_id}|{item_id}` collapses to the `call_id`
//   half on Anthropic targets, stays under the ≤64 char limit, and
//   gets sanitized.
// - Rule 4: a second composite tool call from the Responses turn has
//   no matching result — synthetic orphan emitted with the *collapsed*
//   single-half id.
// - Rule 5: aborted Responses turn + its tool result both vanish.
// ---------------------------------------------------------------------------

#[test]
fn transform_responses_to_anthropic_full_history() {
    let target = target_model("anthropic", "anthropic-messages", "claude-sonnet-4");

    // Rule 1: same-target Anthropic assistant.
    let same_target_id = "toolu_same_target";
    let same_target_assistant = assistant_msg(
        "anthropic",
        "anthropic-messages",
        "claude-sonnet-4",
        vec![
            thinking_block("inner", Some("anthropic-sig"), false),
            thinking_block("", Some("anthropic-redacted"), true),
            text_block("Hi."),
            tool_call_block(same_target_id, "ls"),
        ],
        StopReason::ToolUse,
    );

    // Rule 2 + 3 + 4: cross-model Responses turn.
    // Composite IDs include characters that need sanitizing in the
    // call_id half, plus `fc_*` item halves that get discarded entirely.
    let raw_responses_id = "call_with:bad/chars|fc_item_part_xyz";
    let orphan_responses_id = "call_other|fc_item_two";
    let cross_responses_assistant = assistant_msg(
        "openai",
        "openai-responses",
        "gpt-5",
        vec![
            // Empty redacted thinking (drop).
            thinking_block("", Some("redacted"), true),
            // Empty-with-signature (drop).
            thinking_block("", Some("{\"id\":\"rs_empty\"}"), false),
            // Visible thinking + signature (demote, signature dropped).
            thinking_block("planning", Some("{\"id\":\"rs_visible\"}"), false),
            // Signed text (signature dropped).
            signed_text_block("Will do.", "responses-text-sig"),
            tool_call_block(raw_responses_id, "read_file"),
            tool_call_block(orphan_responses_id, "ls"),
        ],
        StopReason::ToolUse,
    );
    let cross_tool_result = tool_result_msg(raw_responses_id, "read_file", "contents");

    // Rule 5: aborted Responses turn.
    let aborted_id = "call_aborted|fc_item_aborted";
    let aborted_assistant = assistant_msg(
        "openai",
        "openai-responses",
        "gpt-5",
        vec![tool_call_block(aborted_id, "bash")],
        StopReason::Error,
    );
    let aborted_tool_result = tool_result_msg(aborted_id, "bash", "vanish");

    let history = vec![
        user_msg("hi"),
        Message::Assistant(same_target_assistant),
        tool_result_msg(same_target_id, "ls", "ok"),
        user_msg("read it"),
        Message::Assistant(cross_responses_assistant),
        cross_tool_result,
        user_msg("ok"),
        Message::Assistant(aborted_assistant),
        aborted_tool_result,
        user_msg("done?"),
    ];

    let out = transform_messages(&history, &target);

    // Expected: same shape as the other directions.
    assert_eq!(out.len(), 9, "unexpected output length: {:?}", out);

    // 1: rule 1 passthrough.
    let same = expect_assistant(&out, 1);
    assert_eq!(same.content.len(), 4);
    match &same.content[0] {
        AssistantContent::Thinking(th) => {
            assert_eq!(th.thinking, "inner");
            assert_eq!(th.thinking_signature.as_deref(), Some("anthropic-sig"));
        }
        _ => panic!("expected signed thinking"),
    }
    match &same.content[1] {
        AssistantContent::Thinking(th) => {
            assert!(th.redacted);
            assert_eq!(th.thinking_signature.as_deref(), Some("anthropic-redacted"));
        }
        _ => panic!("expected redacted thinking"),
    }
    let (same_id, _) = tool_call_at(&same.content, 3);
    assert_eq!(same_id, same_target_id);

    // 4: cross-model Responses → Anthropic rewrite.
    let cross = expect_assistant(&out, 4);
    assert_eq!(
        cross.content.len(),
        4,
        "redacted + empty-signed dropped; visible thinking demoted; text + 2 tool calls"
    );
    assert_demoted_text(&cross.content, 0, "planning");
    assert_stripped_text(&cross.content, 1, "Will do.");

    // First tool call: composite collapses to a single sanitized id ≤64 chars.
    let (matched_id, _) = tool_call_at(&cross.content, 2);
    assert!(
        !matched_id.contains('|'),
        "rule 3: anthropic IDs must not carry a composite separator, got {}",
        matched_id
    );
    assert!(
        matched_id.len() <= 64,
        "rule 3: anthropic IDs must be ≤64 chars, got {}",
        matched_id.len()
    );
    assert!(
        matched_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
        "rule 3: anthropic IDs must be sanitized, got {}",
        matched_id
    );

    // The orphan id similarly collapses.
    let (orphan_normalized_id, _) = tool_call_at(&cross.content, 3);
    assert!(!orphan_normalized_id.contains('|'));
    assert_ne!(orphan_normalized_id, matched_id);

    // 5: matched tool_result follows the rewritten id.
    let matched_result = expect_tool_result(&out, 5);
    assert_eq!(matched_result.tool_call_id, matched_id);

    // 6: synthetic orphan for the second tool call (using the collapsed id).
    let orphan_result = expect_tool_result(&out, 6);
    assert_synthetic_orphan(orphan_result, orphan_normalized_id);

    // 7-8: user messages, aborted turn + result both gone.
    let _ = expect_user(&out, 7);
    let _ = expect_user(&out, 8);
    assert!(
        out.iter().all(|m| match m {
            Message::ToolResult(tr) => {
                // The aborted turn's id was a composite; after rule 3 it
                // would have collapsed to "call_aborted". Make sure no
                // tool result with either form survives.
                tr.tool_call_id != aborted_id && tr.tool_call_id != "call_aborted"
            }
            _ => true,
        }),
        "rule 5: aborted turn's tool result must be dropped"
    );
}
