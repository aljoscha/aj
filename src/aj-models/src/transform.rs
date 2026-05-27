//! Cross-provider message transformation.
//!
//! When a conversation is replayed against a different provider/model than
//! the one that produced part of its history, content blocks must be
//! rewritten so the target API accepts them: encrypted reasoning tied to
//! the source model is dropped or demoted, tool-call IDs are coerced into
//! the target's character class and length limits, orphaned tool calls
//! get synthetic error results, and incomplete (errored/aborted) turns
//! are skipped along with their dangling tool results.
//!
//! Capability downgrade (§8.2) follows the same call: images on a
//! non-vision target collapse into a fixed placeholder string.
//!
//! See `docs/models-spec.md` §8 for the full design.

use std::collections::{HashMap, HashSet};

use crate::registry::{InputModality, ModelInfo};
use crate::types::{
    AssistantContent, AssistantMessage, Message, StopReason, TextContent, ToolCall,
    ToolResultMessage, UserContent, UserMessage,
};

// ---------------------------------------------------------------------------
// §8.2 placeholder strings (publicly observable — downstream consumers may
// match against these exact values)
// ---------------------------------------------------------------------------

/// Placeholder substituted for images in [`UserMessage`]s when the target
/// model lacks image input support.
pub const NON_VISION_USER_IMAGE_PLACEHOLDER: &str =
    "(image omitted: model does not support images)";

/// Placeholder substituted for images in [`ToolResultMessage`]s when the
/// target model lacks image input support.
pub const NON_VISION_TOOL_IMAGE_PLACEHOLDER: &str =
    "(tool image omitted: model does not support images)";

/// Placeholder substituted for images in [`UserMessage`]s when the
/// `image_block` config flag is `true`. Distinct from the non-vision
/// placeholder so consumers can tell the two failure modes apart in
/// logs and persisted transcripts.
pub const BLOCKED_USER_IMAGE_PLACEHOLDER: &str = "(image omitted: blocked by config)";

/// Placeholder substituted for images in [`ToolResultMessage`]s when
/// the `image_block` config flag is `true`.
pub const BLOCKED_TOOL_IMAGE_PLACEHOLDER: &str = "(tool image omitted: blocked by config)";

/// Synthetic error text inserted as the body of a tool result when an
/// assistant tool call has no corresponding `ToolResultMessage` in the
/// history (§8.1 rule 4).
pub const ORPHAN_TOOL_RESULT_TEXT: &str = "No result provided";

// ---------------------------------------------------------------------------
// Public API (§8.1)
// ---------------------------------------------------------------------------

/// Transform a message history so it can be replayed against `target`.
///
/// Applies §8.1 (cross-provider rewrites: thinking blocks, signatures,
/// tool-call IDs, orphans, errored turns) followed by §8.2 (capability
/// downgrade: images on non-vision targets).
///
/// The function never mutates the input slice; the returned vector
/// contains owned, freshly-constructed messages.
pub fn transform_messages(messages: &[Message], target: &ModelInfo) -> Vec<Message> {
    // Pass 1: walk every message and rewrite assistant content (rule 2),
    // collecting the original→normalized tool-call ID map (rule 3) for
    // pass 2 to consume.
    let mut id_map: HashMap<String, String> = HashMap::new();
    let pass1: Vec<Message> = messages
        .iter()
        .map(|m| match m {
            Message::Assistant(a) => {
                Message::Assistant(transform_assistant(a, target, &mut id_map))
            }
            Message::User(_) | Message::ToolResult(_) => m.clone(),
        })
        .collect();

    // Pass 2: rewrite tool-result IDs via the map, drop errored/aborted
    // assistants and any tool results that referenced their tool calls
    // (rule 5), and synthesize error results for orphaned tool calls
    // (rule 4).
    let pass2 = align_tool_results(pass1, &id_map);

    // §8.2: downgrade images when the target does not accept image input.
    downgrade_unsupported_images(pass2, target)
}

// ---------------------------------------------------------------------------
// Pass 1: assistant rewrites + tool-call ID normalization (§8.1 rules 2–3)
// ---------------------------------------------------------------------------

/// True when `a` was produced by exactly the target model.
fn is_same_model(a: &AssistantMessage, target: &ModelInfo) -> bool {
    a.provider == target.provider && a.api == target.api && a.model == target.id
}

/// Apply §8.1 rules 2 and 3 to a single assistant message. Same-model
/// messages pass through unchanged so signatures and redacted thinking
/// survive the round-trip.
fn transform_assistant(
    a: &AssistantMessage,
    target: &ModelInfo,
    id_map: &mut HashMap<String, String>,
) -> AssistantMessage {
    if is_same_model(a, target) {
        return a.clone();
    }

    let mut new_content: Vec<AssistantContent> = Vec::with_capacity(a.content.len());
    for block in &a.content {
        match block {
            AssistantContent::Thinking(th) => {
                // Redacted blocks are an opaque encrypted payload bound
                // to the source model — drop unconditionally on cross-
                // model replay.
                if th.redacted {
                    continue;
                }
                // Empty thinking with or without a signature: nothing
                // useful to round-trip, drop. Signed-but-empty falls
                // here because the signature is invalid against a
                // different model anyway.
                if th.thinking.is_empty() {
                    continue;
                }
                // Non-empty visible reasoning: demote to plain text so
                // the target sees it as ordinary speech (signatures
                // dropped per rule 2).
                new_content.push(AssistantContent::Text(TextContent::new(
                    th.thinking.clone(),
                )));
            }
            AssistantContent::Text(t) => {
                // Strip text_signature: it's bound to the source model.
                new_content.push(AssistantContent::Text(TextContent {
                    text: t.text.clone(),
                    text_signature: None,
                }));
            }
            AssistantContent::ToolCall(tc) => {
                let new_id = normalize_tool_call_id(&target.api, &a.provider, &tc.id);
                if new_id != tc.id {
                    id_map.insert(tc.id.clone(), new_id.clone());
                }
                new_content.push(AssistantContent::ToolCall(ToolCall {
                    id: new_id,
                    name: tc.name.clone(),
                    arguments: tc.arguments.clone(),
                }));
            }
        }
    }

    AssistantMessage {
        content: new_content,
        api: a.api.clone(),
        provider: a.provider.clone(),
        model: a.model.clone(),
        response_id: a.response_id.clone(),
        usage: a.usage.clone(),
        stop_reason: a.stop_reason.clone(),
        error: a.error.clone(),
        timestamp: a.timestamp,
    }
}

// ---------------------------------------------------------------------------
// Tool-call ID normalization (§8.1 rule 3, §7.3.5)
// ---------------------------------------------------------------------------

/// Rewrite a tool-call ID for `target_api`. `source_provider` controls
/// the foreign-origin branch for `openai-responses` targets (§7.3.5).
fn normalize_tool_call_id(target_api: &str, source_provider: &str, id: &str) -> String {
    match target_api {
        "openai-completions" => {
            // Composite Responses IDs ({call_id}|{item_id}) collapse to
            // the call_id half — Completions has no item_id slot. Plain
            // IDs sanitize+truncate as-is.
            let call_id = match id.find('|') {
                Some(i) => &id[..i],
                None => id,
            };
            truncate(&sanitize(call_id), 40)
        }
        "openai-responses" => {
            let foreign_origin = source_provider != "openai";
            match id.find('|') {
                Some(i) => {
                    let (call_id, rest) = (&id[..i], &id[i + 1..]);
                    let normalized_call = truncate(&sanitize(call_id), 64);
                    // Foreign-origin item_ids get a stable hash rewrite
                    // (the upstream value is provider-specific and won't
                    // round-trip); same-provider item_ids just sanitize.
                    let mut normalized_item = if foreign_origin {
                        format!("fc_{}", short_hash(rest))
                    } else {
                        truncate(&sanitize(rest), 64)
                    };
                    // Responses requires the item_id to start with
                    // `fc_`; inject the prefix when the upstream
                    // half-ID was missing it.
                    if !normalized_item.starts_with("fc_") {
                        normalized_item =
                            truncate(&sanitize(&format!("fc_{}", normalized_item)), 64);
                    } else {
                        normalized_item = truncate(&normalized_item, 64);
                    }
                    format!("{}|{}", normalized_call, normalized_item)
                }
                // Non-composite (e.g. an Anthropic toolu_xxx or a
                // Completions call_xxx): treat as call_id only. The
                // serializer will emit `function_call.id = undefined`.
                None => truncate(&sanitize(id), 64),
            }
        }
        // Default branch covers `anthropic-messages` and any
        // unrecognized future APIs that follow the same character class.
        _ => truncate(&sanitize(id), 64),
    }
}

/// Replace any character outside `[A-Za-z0-9_-]` with `_`.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Truncate a string to at most `max` characters, slicing on UTF-8
/// boundaries (safe here because [`sanitize`] strips non-ASCII).
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        s[..max].to_string()
    }
}

/// Short stable hash used to rewrite foreign item_ids into the
/// `fc_<hash>` form Responses requires. FNV-1a over the bytes,
/// truncated to 12 hex digits — deterministic across runs and
/// platforms, which is all the spec asks for.
fn short_hash(s: &str) -> String {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    let mut h: u64 = FNV_OFFSET;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    let hex = format!("{:016x}", h);
    hex[..12].to_string()
}

// ---------------------------------------------------------------------------
// Pass 2: tool-result alignment (§8.1 rules 4–5)
// ---------------------------------------------------------------------------

/// Walk pass-1 output to:
///   - rewrite `ToolResultMessage.tool_call_id` via the id map,
///   - skip errored/aborted assistants and drop their tool results,
///   - emit synthetic error results for orphaned tool calls when the
///     assistant turn closes (next non-tool-result message or EOF).
fn align_tool_results(messages: Vec<Message>, id_map: &HashMap<String, String>) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::with_capacity(messages.len());
    // Tool calls from the most recent kept assistant message that are
    // still waiting for a result. Cleared when the turn closes.
    let mut pending: Vec<ToolCall> = Vec::new();
    // Result IDs we've already emitted for the current pending set.
    let mut seen_results: HashSet<String> = HashSet::new();
    // Tool-call IDs (post-normalization) belonging to assistants we
    // dropped under rule 5; matching tool results disappear too.
    let mut dropped_call_ids: HashSet<String> = HashSet::new();

    for msg in messages {
        match msg {
            Message::Assistant(a) => {
                close_pending(&mut out, &mut pending, &mut seen_results);
                if matches!(a.stop_reason, StopReason::Error | StopReason::Aborted) {
                    // Rule 5: drop the message and remember its tool
                    // call IDs so any later results referencing them
                    // are dropped too. Pass-1 already normalized those
                    // IDs to match what pass-2 will rewrite results
                    // into, so the comparison works directly.
                    for block in &a.content {
                        if let AssistantContent::ToolCall(tc) = block {
                            dropped_call_ids.insert(tc.id.clone());
                        }
                    }
                    continue;
                }
                pending = a
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        AssistantContent::ToolCall(tc) => Some(tc.clone()),
                        _ => None,
                    })
                    .collect();
                seen_results.clear();
                out.push(Message::Assistant(a));
            }
            Message::ToolResult(mut tr) => {
                if let Some(new_id) = id_map.get(&tr.tool_call_id) {
                    tr.tool_call_id = new_id.clone();
                }
                if dropped_call_ids.contains(&tr.tool_call_id) {
                    continue;
                }
                seen_results.insert(tr.tool_call_id.clone());
                out.push(Message::ToolResult(tr));
            }
            Message::User(u) => {
                close_pending(&mut out, &mut pending, &mut seen_results);
                out.push(Message::User(u));
            }
        }
    }
    close_pending(&mut out, &mut pending, &mut seen_results);
    out
}

/// Emit synthetic error results for any pending tool calls that didn't
/// see a real result, then clear the per-turn state.
fn close_pending(
    out: &mut Vec<Message>,
    pending: &mut Vec<ToolCall>,
    seen_results: &mut HashSet<String>,
) {
    for tc in pending.drain(..) {
        if !seen_results.contains(&tc.id) {
            out.push(Message::ToolResult(ToolResultMessage {
                tool_call_id: tc.id.clone(),
                tool_name: tc.name.clone(),
                content: vec![UserContent::text(ORPHAN_TOOL_RESULT_TEXT)],
                details: None,
                is_error: true,
                timestamp: chrono::Utc::now().timestamp_millis(),
            }));
        }
    }
    seen_results.clear();
}

// ---------------------------------------------------------------------------
// §8.2 Capability downgrade (images on non-vision models)
// ---------------------------------------------------------------------------

fn downgrade_unsupported_images(messages: Vec<Message>, target: &ModelInfo) -> Vec<Message> {
    if target.input.contains(&InputModality::Image) {
        return messages;
    }
    messages
        .into_iter()
        .map(|m| match m {
            Message::User(u) => Message::User(UserMessage {
                content: replace_images_with_placeholder(
                    u.content,
                    NON_VISION_USER_IMAGE_PLACEHOLDER,
                ),
                timestamp: u.timestamp,
            }),
            Message::ToolResult(tr) => Message::ToolResult(ToolResultMessage {
                content: replace_images_with_placeholder(
                    tr.content,
                    NON_VISION_TOOL_IMAGE_PLACEHOLDER,
                ),
                ..tr
            }),
            Message::Assistant(_) => m,
        })
        .collect()
}

/// Replace each `Image` block with a single text placeholder.
/// Consecutive image runs collapse into one placeholder so the
/// transcript doesn't balloon with identical markers.
fn replace_images_with_placeholder(
    content: Vec<UserContent>,
    placeholder: &str,
) -> Vec<UserContent> {
    let mut out: Vec<UserContent> = Vec::with_capacity(content.len());
    let mut previous_was_placeholder = false;
    for block in content {
        match block {
            UserContent::Image(_) => {
                if !previous_was_placeholder {
                    out.push(UserContent::text(placeholder));
                }
                previous_was_placeholder = true;
            }
            UserContent::Text(t) => {
                previous_was_placeholder = t.text == placeholder;
                out.push(UserContent::Text(t));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Defense-in-depth: `image_block` config flag
// ---------------------------------------------------------------------------

/// Strip every [`UserContent::Image`] block from `messages` (both
/// [`Message::User`] and [`Message::ToolResult`]) and replace each
/// with a single placeholder text block. Defense-in-depth for the
/// `image_block` config flag — the model never sees the image bytes
/// regardless of its declared vision capability.
///
/// Adjacent placeholder runs collapse: multiple stripped images in
/// the same message yield exactly one placeholder block, not N.
/// Pure-image messages still get a text block so provider APIs
/// don't reject empty content. Text blocks are untouched.
///
/// User messages use [`BLOCKED_USER_IMAGE_PLACEHOLDER`]; tool result
/// messages use [`BLOCKED_TOOL_IMAGE_PLACEHOLDER`].
pub fn block_user_images(messages: &mut Vec<Message>) {
    for m in messages.iter_mut() {
        match m {
            Message::User(u) => {
                let content = std::mem::take(&mut u.content);
                u.content =
                    replace_images_with_placeholder(content, BLOCKED_USER_IMAGE_PLACEHOLDER);
            }
            Message::ToolResult(tr) => {
                let content = std::mem::take(&mut tr.content);
                tr.content =
                    replace_images_with_placeholder(content, BLOCKED_TOOL_IMAGE_PLACEHOLDER);
            }
            Message::Assistant(_) => {}
        }
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{ModelCost, ModelInfo};
    use crate::types::{
        AssistantContent, AssistantMessage, ImageContent, Message, StopReason, TextContent,
        ThinkingContent, ToolCall, ToolResultMessage, Usage, UserContent, UserMessage,
    };

    // -- Helpers -----------------------------------------------------------

    fn model(provider: &str, api: &str, id: &str, vision: bool) -> ModelInfo {
        let mut input = vec![InputModality::Text];
        if vision {
            input.push(InputModality::Image);
        }
        ModelInfo {
            id: id.into(),
            name: id.into(),
            api: api.into(),
            provider: provider.into(),
            base_url: "https://example.test".into(),
            reasoning: false,
            supports_xhigh: false,
            supports_adaptive_thinking: false,
            input,
            cost: ModelCost::default(),
            context_window: 100_000,
            max_tokens: 8_192,
            headers: None,
        }
    }

    fn assistant(
        provider: &str,
        api: &str,
        model_id: &str,
        content: Vec<AssistantContent>,
    ) -> AssistantMessage {
        AssistantMessage {
            content,
            api: api.into(),
            provider: provider.into(),
            model: model_id.into(),
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error: None,
            timestamp: 0,
        }
    }

    fn tool_call(id: &str, name: &str) -> AssistantContent {
        AssistantContent::ToolCall(ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: serde_json::json!({}),
        })
    }

    fn tool_result(call_id: &str, name: &str, body: &str) -> Message {
        Message::ToolResult(ToolResultMessage::text(call_id, name, body, false))
    }

    // -- Same-model passthrough (§8.1 rule 1) ------------------------------

    #[test]
    fn same_model_preserves_signatures_and_redacted_thinking() {
        let target = model("anthropic", "anthropic-messages", "claude-x", false);
        let asst = assistant(
            "anthropic",
            "anthropic-messages",
            "claude-x",
            vec![
                AssistantContent::Thinking(ThinkingContent {
                    thinking: "visible".into(),
                    thinking_signature: Some("sig".into()),
                    redacted: false,
                }),
                AssistantContent::Thinking(ThinkingContent {
                    thinking: String::new(),
                    thinking_signature: Some("opaque".into()),
                    redacted: true,
                }),
                AssistantContent::Text(TextContent {
                    text: "hello".into(),
                    text_signature: Some("ts".into()),
                }),
                tool_call("toolu_abc", "ls"),
            ],
        );
        let out = transform_messages(
            &[
                Message::Assistant(asst.clone()),
                tool_result("toolu_abc", "ls", "ok"),
            ],
            &target,
        );
        assert_eq!(out.len(), 2);
        let Message::Assistant(a) = &out[0] else {
            panic!("expected assistant");
        };
        assert_eq!(a.content.len(), 4);
        // Signatures preserved.
        match &a.content[0] {
            AssistantContent::Thinking(th) => {
                assert_eq!(th.thinking_signature.as_deref(), Some("sig"));
                assert!(!th.redacted);
            }
            _ => panic!(),
        }
        match &a.content[1] {
            AssistantContent::Thinking(th) => {
                assert!(th.redacted);
                assert_eq!(th.thinking_signature.as_deref(), Some("opaque"));
            }
            _ => panic!(),
        }
        match &a.content[2] {
            AssistantContent::Text(t) => assert_eq!(t.text_signature.as_deref(), Some("ts")),
            _ => panic!(),
        }
        // Tool call ID untouched.
        match &a.content[3] {
            AssistantContent::ToolCall(tc) => assert_eq!(tc.id, "toolu_abc"),
            _ => panic!(),
        }
    }

    // -- Cross-model thinking handling (§8.1 rule 2) -----------------------

    #[test]
    fn cross_model_drops_redacted_and_strips_signatures() {
        let target = model("openai", "openai-completions", "gpt-x", false);
        let asst = assistant(
            "anthropic",
            "anthropic-messages",
            "claude-x",
            vec![
                AssistantContent::Thinking(ThinkingContent {
                    thinking: String::new(),
                    thinking_signature: Some("opaque".into()),
                    redacted: true,
                }),
                AssistantContent::Thinking(ThinkingContent {
                    thinking: String::new(),
                    thinking_signature: Some("sig".into()),
                    redacted: false,
                }),
                AssistantContent::Thinking(ThinkingContent {
                    thinking: "visible".into(),
                    thinking_signature: Some("sig".into()),
                    redacted: false,
                }),
                AssistantContent::Text(TextContent {
                    text: "hello".into(),
                    text_signature: Some("ts".into()),
                }),
            ],
        );
        let out = transform_messages(&[Message::Assistant(asst)], &target);
        let Message::Assistant(a) = &out[0] else {
            panic!();
        };
        // Redacted dropped, empty-with-sig dropped, non-empty thinking
        // demoted to text, original text kept with text_signature stripped.
        assert_eq!(a.content.len(), 2);
        match &a.content[0] {
            AssistantContent::Text(t) => {
                assert_eq!(t.text, "visible");
                assert!(t.text_signature.is_none());
            }
            _ => panic!("expected demoted text"),
        }
        match &a.content[1] {
            AssistantContent::Text(t) => {
                assert_eq!(t.text, "hello");
                assert!(t.text_signature.is_none());
            }
            _ => panic!(),
        }
    }

    // -- Tool call ID normalization (§8.1 rule 3) --------------------------

    #[test]
    fn anthropic_target_sanitizes_and_truncates() {
        let target = model("anthropic", "anthropic-messages", "claude-x", false);
        let long = "x".repeat(80);
        let mixed = format!("call|with/odd:chars-{}", long);
        let out = normalize_tool_call_id(&target.api, "openai", &mixed);
        assert_eq!(out.len(), 64);
        assert!(
            out.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        );
    }

    #[test]
    fn openai_completions_target_drops_item_id_half() {
        let id = "call_abc|fc_long_item_id";
        let out = normalize_tool_call_id("openai-completions", "openai", id);
        assert_eq!(out, "call_abc");
    }

    #[test]
    fn openai_completions_target_truncates_to_40() {
        let id = "a".repeat(80);
        let out = normalize_tool_call_id("openai-completions", "openai", &id);
        assert_eq!(out.len(), 40);
    }

    #[test]
    fn openai_responses_target_keeps_same_provider_composite() {
        // Same provider (openai), composite ID — both halves sanitize+truncate
        // and item_id keeps its fc_ prefix.
        let id = "call_abc|fc_item_xyz";
        let out = normalize_tool_call_id("openai-responses", "openai", id);
        assert_eq!(out, "call_abc|fc_item_xyz");
    }

    #[test]
    fn openai_responses_target_rewrites_foreign_item_id_to_fc_hash() {
        let id = "toolu_abc|some_item";
        let out = normalize_tool_call_id("openai-responses", "anthropic", id);
        let (call, item) = out.split_once('|').unwrap();
        assert_eq!(call, "toolu_abc");
        assert!(item.starts_with("fc_"));
        // Stable hash: same input ⇒ same output.
        let again = normalize_tool_call_id("openai-responses", "anthropic", id);
        assert_eq!(out, again);
    }

    #[test]
    fn openai_responses_target_injects_fc_prefix_when_missing_same_provider() {
        // Same-provider but item_id half lacks fc_ prefix — Responses
        // requires it, so we inject.
        let id = "call_abc|item_no_prefix";
        let out = normalize_tool_call_id("openai-responses", "openai", id);
        let (_, item) = out.split_once('|').unwrap();
        assert!(item.starts_with("fc_"), "got {}", item);
    }

    #[test]
    fn openai_responses_target_non_composite_passes_through() {
        let id = "toolu_abc";
        let out = normalize_tool_call_id("openai-responses", "anthropic", id);
        // No `|` ⇒ treated as call_id only; sanitize+truncate.
        assert_eq!(out, "toolu_abc");
        assert!(!out.contains('|'));
    }

    #[test]
    fn id_map_drives_tool_result_rewriting() {
        let target = model("anthropic", "anthropic-messages", "claude-x", false);
        let foreign_id = "fc_call|item_with/special:chars";
        let asst = assistant(
            "openai",
            "openai-responses",
            "gpt-x",
            vec![tool_call(foreign_id, "ls")],
        );
        let messages = vec![
            Message::Assistant(asst),
            tool_result(foreign_id, "ls", "ok"),
        ];
        let out = transform_messages(&messages, &target);
        // Both the assistant's tool-call id and the tool result's
        // tool_call_id should agree post-transform.
        let asst_id = match &out[0] {
            Message::Assistant(a) => match &a.content[0] {
                AssistantContent::ToolCall(tc) => tc.id.clone(),
                _ => panic!(),
            },
            _ => panic!(),
        };
        let result_id = match &out[1] {
            Message::ToolResult(tr) => tr.tool_call_id.clone(),
            _ => panic!(),
        };
        assert_eq!(asst_id, result_id);
        assert!(!asst_id.contains('|'), "anthropic ids never carry pipes");
    }

    // -- Orphan synthesis (§8.1 rule 4) ------------------------------------

    #[test]
    fn orphan_tool_call_gets_synthetic_error_result() {
        let target = model("anthropic", "anthropic-messages", "claude-x", false);
        let asst = assistant(
            "anthropic",
            "anthropic-messages",
            "claude-x",
            vec![tool_call("toolu_a", "read")],
        );
        let user_followup = Message::User(UserMessage::text("anything"));
        let out = transform_messages(&[Message::Assistant(asst), user_followup], &target);
        // assistant, synthetic tool result, user
        assert_eq!(out.len(), 3);
        match &out[1] {
            Message::ToolResult(tr) => {
                assert_eq!(tr.tool_call_id, "toolu_a");
                assert!(tr.is_error);
                match &tr.content[0] {
                    UserContent::Text(t) => assert_eq!(t.text, ORPHAN_TOOL_RESULT_TEXT),
                    _ => panic!(),
                }
            }
            _ => panic!("expected synthetic tool result"),
        }
    }

    #[test]
    fn orphan_synthesis_only_for_missing_results() {
        let target = model("anthropic", "anthropic-messages", "claude-x", false);
        let asst = assistant(
            "anthropic",
            "anthropic-messages",
            "claude-x",
            vec![tool_call("toolu_a", "r"), tool_call("toolu_b", "w")],
        );
        let messages = vec![
            Message::Assistant(asst),
            tool_result("toolu_a", "r", "result"),
            Message::User(UserMessage::text("next")),
        ];
        let out = transform_messages(&messages, &target);
        // Assistant, real result for a, synthetic for b, user.
        assert_eq!(out.len(), 4);
        match &out[2] {
            Message::ToolResult(tr) => assert_eq!(tr.tool_call_id, "toolu_b"),
            _ => panic!(),
        }
    }

    #[test]
    fn trailing_orphan_synthesized_at_end_of_history() {
        let target = model("anthropic", "anthropic-messages", "claude-x", false);
        let asst = assistant(
            "anthropic",
            "anthropic-messages",
            "claude-x",
            vec![tool_call("toolu_a", "r")],
        );
        let out = transform_messages(&[Message::Assistant(asst)], &target);
        assert_eq!(out.len(), 2);
        assert!(matches!(out[1], Message::ToolResult(_)));
    }

    // -- Errored / aborted skipping (§8.1 rule 5) --------------------------

    #[test]
    fn errored_assistant_dropped_with_its_results() {
        let target = model("anthropic", "anthropic-messages", "claude-x", false);
        let mut errored = assistant(
            "anthropic",
            "anthropic-messages",
            "claude-x",
            vec![tool_call("toolu_a", "r")],
        );
        errored.stop_reason = StopReason::Error;
        let mut aborted = assistant(
            "anthropic",
            "anthropic-messages",
            "claude-x",
            vec![tool_call("toolu_b", "w")],
        );
        aborted.stop_reason = StopReason::Aborted;
        let messages = vec![
            Message::Assistant(errored),
            tool_result("toolu_a", "r", "should disappear"),
            Message::Assistant(aborted),
            tool_result("toolu_b", "w", "also disappears"),
            Message::User(UserMessage::text("next")),
        ];
        let out = transform_messages(&messages, &target);
        // Both assistants and both their results should be gone.
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], Message::User(_)));
    }

    // -- Two-pass interaction (rule 5 + cross-model id rewrite) ------------

    #[test]
    fn errored_assistant_with_normalized_ids_drops_pre_normalization_results() {
        let target = model("anthropic", "anthropic-messages", "claude-x", false);
        let bad_id = "fc_call|item_a";
        let mut errored = assistant(
            "openai",
            "openai-responses",
            "gpt-x",
            vec![tool_call(bad_id, "r")],
        );
        errored.stop_reason = StopReason::Error;
        let messages = vec![
            Message::Assistant(errored),
            tool_result(bad_id, "r", "should be dropped"),
            Message::User(UserMessage::text("next")),
        ];
        let out = transform_messages(&messages, &target);
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], Message::User(_)));
    }

    // -- Image downgrade (§8.2) --------------------------------------------

    #[test]
    fn image_downgrade_replaces_runs_with_single_placeholder() {
        let target = model("anthropic", "anthropic-messages", "claude-x", false);
        let user = UserMessage {
            content: vec![
                UserContent::text("before"),
                UserContent::Image(ImageContent {
                    data: "AAA".into(),
                    mime_type: "image/png".into(),
                }),
                UserContent::Image(ImageContent {
                    data: "BBB".into(),
                    mime_type: "image/png".into(),
                }),
                UserContent::text("after"),
            ],
            timestamp: 0,
        };
        let out = transform_messages(&[Message::User(user)], &target);
        let Message::User(u) = &out[0] else {
            panic!();
        };
        assert_eq!(u.content.len(), 3);
        match &u.content[1] {
            UserContent::Text(t) => assert_eq!(t.text, NON_VISION_USER_IMAGE_PLACEHOLDER),
            _ => panic!(),
        }
    }

    #[test]
    fn image_downgrade_uses_distinct_placeholder_for_tool_results() {
        let target = model("anthropic", "anthropic-messages", "claude-x", false);
        let tr = ToolResultMessage {
            tool_call_id: "toolu_a".into(),
            tool_name: "screenshot".into(),
            content: vec![UserContent::Image(ImageContent {
                data: "AAA".into(),
                mime_type: "image/png".into(),
            })],
            details: None,
            is_error: false,
            timestamp: 0,
        };
        // Need the assistant message that owns toolu_a so the result
        // isn't filtered as an orphan from a non-existent assistant.
        let asst = assistant(
            "anthropic",
            "anthropic-messages",
            "claude-x",
            vec![tool_call("toolu_a", "screenshot")],
        );
        let out = transform_messages(
            &[Message::Assistant(asst), Message::ToolResult(tr)],
            &target,
        );
        let Message::ToolResult(tr) = &out[1] else {
            panic!();
        };
        assert_eq!(tr.content.len(), 1);
        match &tr.content[0] {
            UserContent::Text(t) => assert_eq!(t.text, NON_VISION_TOOL_IMAGE_PLACEHOLDER),
            _ => panic!(),
        }
    }

    #[test]
    fn vision_target_keeps_images() {
        let target = model("anthropic", "anthropic-messages", "claude-x", true);
        let user = UserMessage {
            content: vec![UserContent::Image(ImageContent {
                data: "AAA".into(),
                mime_type: "image/png".into(),
            })],
            timestamp: 0,
        };
        let out = transform_messages(&[Message::User(user)], &target);
        let Message::User(u) = &out[0] else {
            panic!();
        };
        assert!(matches!(u.content[0], UserContent::Image(_)));
    }

    // -- short_hash determinism --------------------------------------------

    #[test]
    fn short_hash_is_stable() {
        assert_eq!(short_hash("hello"), short_hash("hello"));
        assert_ne!(short_hash("hello"), short_hash("world"));
        assert_eq!(short_hash("hello").len(), 12);
    }

    // -- block_user_images (image_block config) ----------------------------

    fn img(data: &str) -> UserContent {
        UserContent::Image(ImageContent {
            data: data.into(),
            mime_type: "image/png".into(),
        })
    }

    #[test]
    fn block_user_images_collapses_adjacent_images_to_one_placeholder() {
        let user = UserMessage {
            content: vec![
                UserContent::text("before"),
                img("AAA"),
                img("BBB"),
                UserContent::text("after"),
            ],
            timestamp: 0,
        };
        let mut msgs = vec![Message::User(user)];
        block_user_images(&mut msgs);
        let Message::User(u) = &msgs[0] else { panic!() };
        assert_eq!(u.content.len(), 3);
        match &u.content[1] {
            UserContent::Text(t) => assert_eq!(t.text, BLOCKED_USER_IMAGE_PLACEHOLDER),
            _ => panic!("expected placeholder text block"),
        }
    }

    #[test]
    fn block_user_images_pure_image_tool_result_keeps_one_block() {
        let tr = ToolResultMessage {
            tool_call_id: "toolu_a".into(),
            tool_name: "screenshot".into(),
            content: vec![img("AAA")],
            details: None,
            is_error: false,
            timestamp: 0,
        };
        let mut msgs = vec![Message::ToolResult(tr)];
        block_user_images(&mut msgs);
        let Message::ToolResult(tr) = &msgs[0] else {
            panic!()
        };
        assert_eq!(tr.content.len(), 1);
        match &tr.content[0] {
            UserContent::Text(t) => assert_eq!(t.text, BLOCKED_TOOL_IMAGE_PLACEHOLDER),
            _ => panic!("expected placeholder text block"),
        }
    }

    #[test]
    fn block_user_images_text_then_image_tool_result() {
        let tr = ToolResultMessage {
            tool_call_id: "toolu_a".into(),
            tool_name: "screenshot".into(),
            content: vec![UserContent::text("before"), img("AAA")],
            details: None,
            is_error: false,
            timestamp: 0,
        };
        let mut msgs = vec![Message::ToolResult(tr)];
        block_user_images(&mut msgs);
        let Message::ToolResult(tr) = &msgs[0] else {
            panic!()
        };
        assert_eq!(tr.content.len(), 2);
        match &tr.content[1] {
            UserContent::Text(t) => assert_eq!(t.text, BLOCKED_TOOL_IMAGE_PLACEHOLDER),
            _ => panic!(),
        }
    }

    #[test]
    fn block_user_images_text_only_messages_untouched() {
        let user = UserMessage {
            content: vec![UserContent::text("hello"), UserContent::text("world")],
            timestamp: 0,
        };
        let mut msgs = vec![Message::User(user)];
        block_user_images(&mut msgs);
        let Message::User(u) = &msgs[0] else { panic!() };
        assert_eq!(u.content.len(), 2);
        match &u.content[0] {
            UserContent::Text(t) => assert_eq!(t.text, "hello"),
            _ => panic!(),
        }
    }

    #[test]
    fn block_user_images_empty_content_untouched() {
        let user = UserMessage {
            content: Vec::new(),
            timestamp: 0,
        };
        let mut msgs = vec![Message::User(user)];
        block_user_images(&mut msgs);
        let Message::User(u) = &msgs[0] else { panic!() };
        assert!(u.content.is_empty());
    }
}
