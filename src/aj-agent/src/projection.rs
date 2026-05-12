//! Projection of the agent's in-memory transcript onto the unified
//! [`aj_models::types::Message`] shape used by the [`Provider`]
//! trait, plus the reverse projection from unified
//! [`aj_models::types::AssistantMessage`] back onto a
//! [`aj_models::wire::MessageParam`] so the existing transcript
//! and persistence flow stays on the wire on-disk format.
//!
//! The agent's `transcript: Vec<MessageParam>` field is the
//! authoritative on-disk + replay shape (`aj-session` round-trips
//! exactly this). When the agent runs an inference it needs the
//! tagged-union [`aj_models::types::Message`] shape; when the
//! finalized [`aj_models::types::AssistantMessage`] comes back off
//! the stream it must be re-projected onto a flat-content
//! [`MessageParam`] for the transcript and the
//! [`crate::events::PersistedMessageKind::Assistant`] event the
//! persistence listener subscribes to.
//!
//! See `docs/aj-next-progress.md` Phase 6 step 6.5 for the full
//! design (and the rationale for keeping `MessageParam` as the
//! transcript shape rather than switching to `Vec<Message>` outright).
//!
//! [`Provider`]: aj_models::provider::Provider

use aj_models::types::{
    AssistantContent, AssistantMessage, ImageContent, Message, TextContent, ThinkingContent,
    ToolCall, ToolResultMessage, Usage as UnifiedUsage, UserContent, UserMessage,
};
use aj_models::wire::{
    ContentBlockParam, ImageSource, MessageParam, Role, ToolResultContent, Usage as LegacyUsage,
};

/// Project the agent's in-memory transcript onto the unified
/// [`Message`] sequence the [`aj_models::provider::Provider`] trait
/// consumes.
///
/// The mapping is mechanical:
/// - An assistant-role [`MessageParam`] becomes a single
///   [`Message::Assistant`] whose `content` mirrors the source
///   blocks (text / thinking / tool-call). Identity fields
///   (`api`, `provider`, `model`, `response_id`, `usage`,
///   `stop_reason`) are left at default — the provider stamps its
///   own identity on the freshly-emitted [`AssistantMessage`] each
///   turn, and the historical transcript entries are only used as
///   *input* context where these fields don't matter.
/// - A user-role [`MessageParam`] is split: any
///   [`ContentBlockParam::ToolResultBlock`] entries become
///   stand-alone [`Message::ToolResult`] rows (one per block),
///   and the remaining text/image blocks coalesce into a single
///   [`Message::User`]. The split preserves the order of blocks
///   within the source message (text-then-tool-result and
///   tool-result-then-text both survive, even though the agent
///   itself never produces that mix today).
pub(crate) fn transcript_to_messages(transcript: &[MessageParam]) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::with_capacity(transcript.len());
    for entry in transcript {
        match entry.role {
            Role::Assistant => {
                out.push(Message::Assistant(project_assistant_param(entry)));
            }
            Role::User => {
                append_user_or_tool_results(&mut out, entry);
            }
        }
    }
    out
}

/// Split a user-role [`MessageParam`] into the [`Message::User`] /
/// [`Message::ToolResult`] sequence the unified protocol expects.
fn append_user_or_tool_results(out: &mut Vec<Message>, entry: &MessageParam) {
    let mut pending_user: Vec<UserContent> = Vec::new();
    for block in &entry.content {
        match block {
            ContentBlockParam::ToolResultBlock {
                tool_use_id,
                content,
                is_error,
            } => {
                // Flush any accumulated text/image blocks first so
                // the on-wire ordering matches the source.
                if !pending_user.is_empty() {
                    out.push(Message::User(UserMessage {
                        content: std::mem::take(&mut pending_user),
                        timestamp: 0,
                    }));
                }
                out.push(Message::ToolResult(ToolResultMessage {
                    tool_call_id: tool_use_id.clone(),
                    // `tool_name` is informational only — providers
                    // identify a tool_result purely by `tool_call_id`.
                    // We keep it empty since the legacy
                    // `ToolResultBlock` doesn't carry the name.
                    tool_name: String::new(),
                    content: project_tool_result_content(content),
                    details: None,
                    is_error: *is_error,
                    timestamp: 0,
                }));
            }
            other => {
                if let Some(c) = project_block_to_user_content(other) {
                    pending_user.push(c);
                }
            }
        }
    }
    if !pending_user.is_empty() {
        out.push(Message::User(UserMessage {
            content: pending_user,
            timestamp: 0,
        }));
    }
}

fn project_assistant_param(entry: &MessageParam) -> AssistantMessage {
    let mut content: Vec<AssistantContent> = Vec::with_capacity(entry.content.len());
    for block in &entry.content {
        if let Some(c) = project_block_to_assistant_content(block) {
            content.push(c);
        }
    }
    let mut msg = AssistantMessage::empty();
    msg.content = content;
    msg
}

fn project_block_to_user_content(block: &ContentBlockParam) -> Option<UserContent> {
    match block {
        ContentBlockParam::TextBlock {
            text, signature, ..
        } => Some(UserContent::Text(TextContent {
            text: text.clone(),
            text_signature: signature.clone(),
        })),
        ContentBlockParam::ImageBlock {
            source: ImageSource::Base64 { data, media_type },
        } => Some(UserContent::Image(ImageContent {
            data: data.clone(),
            mime_type: media_type.clone(),
        })),
        // URL / file / document / non-image blocks aren't representable
        // in the unified `UserContent` set today. The agent doesn't
        // produce these for its own input, so dropping is safe; if
        // resumed transcripts contain them they're silently elided
        // from the projected `Message::User`.
        _ => None,
    }
}

fn project_block_to_assistant_content(block: &ContentBlockParam) -> Option<AssistantContent> {
    match block {
        ContentBlockParam::TextBlock {
            text, signature, ..
        } => Some(AssistantContent::Text(TextContent {
            text: text.clone(),
            text_signature: signature.clone(),
        })),
        ContentBlockParam::ThinkingBlock {
            signature,
            thinking,
        } => Some(AssistantContent::Thinking(ThinkingContent {
            thinking: thinking.clone(),
            thinking_signature: empty_to_none(signature),
            redacted: false,
        })),
        ContentBlockParam::RedactedThinkingBlock { data } => {
            Some(AssistantContent::Thinking(ThinkingContent {
                thinking: String::new(),
                thinking_signature: empty_to_none(data),
                redacted: true,
            }))
        }
        ContentBlockParam::ToolUseBlock {
            id,
            input,
            name,
            caller: _,
        } => Some(AssistantContent::ToolCall(ToolCall {
            id: id.clone(),
            name: name.clone(),
            arguments: input.clone(),
        })),
        // Tool-result, image, document, and server-side tool blocks
        // never appear on an assistant-role message in the agent's
        // transcript (assistant messages are pure text / thinking /
        // tool_use). Drop on the projection to keep the
        // unified-side `AssistantContent` set small.
        _ => None,
    }
}

fn project_tool_result_content(content: &ToolResultContent) -> Vec<UserContent> {
    match content {
        ToolResultContent::Text(s) => vec![UserContent::Text(TextContent {
            text: s.clone(),
            text_signature: None,
        })],
        ToolResultContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(project_block_to_user_content)
            .collect(),
    }
}

fn empty_to_none(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Convert a finalized unified [`AssistantMessage`] back into the
/// flat-content [`MessageParam`] shape the agent's transcript and
/// persistence flow use.
///
/// Inverse of [`project_assistant_param`]: each [`AssistantContent`]
/// becomes the matching [`ContentBlockParam`] variant. Identity /
/// usage / stop_reason fields are intentionally dropped — the
/// transcript only round-trips content, and the per-turn usage rides
/// on the [`crate::events::AgentEvent::TurnUsage`] event separately.
pub(crate) fn assistant_message_to_message_param(msg: &AssistantMessage) -> MessageParam {
    let content = msg
        .content
        .iter()
        .map(project_assistant_content_to_block)
        .collect();
    MessageParam {
        role: Role::Assistant,
        content,
    }
}

fn project_assistant_content_to_block(c: &AssistantContent) -> ContentBlockParam {
    match c {
        AssistantContent::Text(t) => ContentBlockParam::TextBlock {
            text: t.text.clone(),
            citations: None,
            signature: t.text_signature.clone(),
        },
        AssistantContent::Thinking(t) => {
            if t.redacted {
                ContentBlockParam::RedactedThinkingBlock {
                    data: t.thinking_signature.clone().unwrap_or_default(),
                }
            } else {
                ContentBlockParam::ThinkingBlock {
                    signature: t.thinking_signature.clone().unwrap_or_default(),
                    thinking: t.thinking.clone(),
                }
            }
        }
        AssistantContent::ToolCall(tc) => ContentBlockParam::ToolUseBlock {
            id: tc.id.clone(),
            input: tc.arguments.clone(),
            name: tc.name.clone(),
            caller: None,
        },
    }
}

/// Map a unified [`UnifiedUsage`] onto the legacy
/// [`LegacyUsage`] the agent's session-state accumulator and the
/// [`crate::events::AgentEvent::TurnUsage`] event still use.
///
/// Counters carry over; the unified shape doesn't model the legacy
/// `iterations` / `server_tool_use` / `service_tier` / `speed`
/// sidecar fields, so those stay at their defaults. The legacy
/// `cache_creation_input_tokens` / `cache_read_input_tokens` fields
/// take the unified `cache_write` / `cache_read` counts respectively
/// when non-zero, matching how the legacy providers emitted them.
pub(crate) fn usage_unified_to_legacy(u: &UnifiedUsage) -> LegacyUsage {
    LegacyUsage {
        input_tokens: u.input,
        output_tokens: u.output,
        cache_creation_input_tokens: option_when_nonzero(u.cache_write),
        cache_read_input_tokens: option_when_nonzero(u.cache_read),
        cache_creation: None,
        iterations: None,
        server_tool_use: None,
        service_tier: None,
        speed: None,
    }
}

fn option_when_nonzero(n: u64) -> Option<u64> {
    if n == 0 {
        None
    } else {
        Some(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aj_models::wire::{ImageSource, MessageParam, Role};
    use serde_json::json;

    #[test]
    fn transcript_with_text_user_message_projects_to_message_user() {
        let transcript = vec![MessageParam::new_user_message(vec![
            ContentBlockParam::new_text_block("hello".into()),
        ])];
        let messages = transcript_to_messages(&transcript);
        assert_eq!(messages.len(), 1);
        match &messages[0] {
            Message::User(u) => {
                assert_eq!(u.content.len(), 1);
                match &u.content[0] {
                    UserContent::Text(t) => assert_eq!(t.text, "hello"),
                    other => panic!("expected text content, got {other:?}"),
                }
            }
            other => panic!("expected Message::User, got {other:?}"),
        }
    }

    #[test]
    fn transcript_with_assistant_text_projects_to_message_assistant() {
        let transcript = vec![MessageParam {
            role: Role::Assistant,
            content: vec![ContentBlockParam::TextBlock {
                text: "hi".into(),
                citations: None,
                signature: None,
            }],
        }];
        let messages = transcript_to_messages(&transcript);
        assert_eq!(messages.len(), 1);
        match &messages[0] {
            Message::Assistant(a) => {
                assert_eq!(a.content.len(), 1);
                match &a.content[0] {
                    AssistantContent::Text(t) => assert_eq!(t.text, "hi"),
                    other => panic!("expected text content, got {other:?}"),
                }
            }
            other => panic!("expected Message::Assistant, got {other:?}"),
        }
    }

    #[test]
    fn transcript_with_tool_use_and_tool_result_splits_correctly() {
        // assistant tool_use → Message::Assistant with ToolCall;
        // user tool_result → Message::ToolResult.
        let transcript = vec![
            MessageParam {
                role: Role::Assistant,
                content: vec![ContentBlockParam::ToolUseBlock {
                    id: "tu-1".into(),
                    name: "ping".into(),
                    input: json!({}),
                    caller: None,
                }],
            },
            MessageParam::new_user_message(vec![ContentBlockParam::ToolResultBlock {
                tool_use_id: "tu-1".into(),
                content: "pong".to_string().into(),
                is_error: false,
            }]),
        ];

        let messages = transcript_to_messages(&transcript);
        assert_eq!(messages.len(), 2);
        match &messages[0] {
            Message::Assistant(a) => match &a.content[0] {
                AssistantContent::ToolCall(tc) => {
                    assert_eq!(tc.id, "tu-1");
                    assert_eq!(tc.name, "ping");
                }
                other => panic!("expected ToolCall, got {other:?}"),
            },
            other => panic!("expected Assistant, got {other:?}"),
        }
        match &messages[1] {
            Message::ToolResult(t) => {
                assert_eq!(t.tool_call_id, "tu-1");
                assert!(!t.is_error);
                assert_eq!(t.content.len(), 1);
                match &t.content[0] {
                    UserContent::Text(s) => assert_eq!(s.text, "pong"),
                    other => panic!("expected text result, got {other:?}"),
                }
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn user_message_with_mixed_text_and_tool_result_splits_in_order() {
        // text-then-tool-result blocks in a single user MessageParam
        // produce a User followed by a ToolResult (preserving order).
        let transcript = vec![MessageParam::new_user_message(vec![
            ContentBlockParam::new_text_block("preamble".into()),
            ContentBlockParam::ToolResultBlock {
                tool_use_id: "tu-2".into(),
                content: "ok".to_string().into(),
                is_error: false,
            },
        ])];
        let messages = transcript_to_messages(&transcript);
        assert_eq!(messages.len(), 2);
        assert!(matches!(&messages[0], Message::User(_)));
        assert!(matches!(&messages[1], Message::ToolResult(_)));
    }

    #[test]
    fn assistant_message_round_trips_through_message_param() {
        // Build an AssistantMessage with one text + one tool_call,
        // round-trip through `assistant_message_to_message_param`,
        // re-project, assert content equivalence.
        let mut msg = AssistantMessage::empty();
        msg.content = vec![
            AssistantContent::Text(TextContent {
                text: "hello".into(),
                text_signature: None,
            }),
            AssistantContent::ToolCall(ToolCall {
                id: "tu-rt".into(),
                name: "echo".into(),
                arguments: json!({"x": 1}),
            }),
        ];

        let param = assistant_message_to_message_param(&msg);
        assert_eq!(param.content.len(), 2);
        match &param.content[0] {
            ContentBlockParam::TextBlock { text, .. } => assert_eq!(text, "hello"),
            other => panic!("expected TextBlock, got {other:?}"),
        }
        match &param.content[1] {
            ContentBlockParam::ToolUseBlock {
                id, name, input, ..
            } => {
                assert_eq!(id, "tu-rt");
                assert_eq!(name, "echo");
                assert_eq!(*input, json!({"x": 1}));
            }
            other => panic!("expected ToolUseBlock, got {other:?}"),
        }
    }

    #[test]
    fn thinking_block_round_trips_with_and_without_signature() {
        // Signed thinking block → AssistantContent::Thinking with
        // signature, projects back to ThinkingBlock preserving the
        // signature.
        let mut msg = AssistantMessage::empty();
        msg.content = vec![
            AssistantContent::Thinking(ThinkingContent {
                thinking: "reasoning here".into(),
                thinking_signature: Some("sig-1".into()),
                redacted: false,
            }),
            AssistantContent::Thinking(ThinkingContent {
                thinking: String::new(),
                thinking_signature: Some("redacted-data".into()),
                redacted: true,
            }),
        ];

        let param = assistant_message_to_message_param(&msg);
        match &param.content[0] {
            ContentBlockParam::ThinkingBlock {
                signature,
                thinking,
            } => {
                assert_eq!(signature, "sig-1");
                assert_eq!(thinking, "reasoning here");
            }
            other => panic!("expected ThinkingBlock, got {other:?}"),
        }
        match &param.content[1] {
            ContentBlockParam::RedactedThinkingBlock { data } => {
                assert_eq!(data, "redacted-data");
            }
            other => panic!("expected RedactedThinkingBlock, got {other:?}"),
        }
    }

    #[test]
    fn user_image_block_projects_to_user_content_image() {
        let transcript = vec![MessageParam::new_user_message(vec![
            ContentBlockParam::ImageBlock {
                source: ImageSource::Base64 {
                    data: "AAA=".into(),
                    media_type: "image/png".into(),
                },
            },
        ])];
        let messages = transcript_to_messages(&transcript);
        match &messages[0] {
            Message::User(u) => match &u.content[0] {
                UserContent::Image(img) => {
                    assert_eq!(img.data, "AAA=");
                    assert_eq!(img.mime_type, "image/png");
                }
                other => panic!("expected Image, got {other:?}"),
            },
            other => panic!("expected User, got {other:?}"),
        }
    }

    #[test]
    fn usage_unified_to_legacy_maps_counters() {
        let u = UnifiedUsage {
            input: 42,
            output: 7,
            cache_read: 5,
            cache_write: 0,
            total_tokens: 54,
            cost: Default::default(),
        };
        let legacy = usage_unified_to_legacy(&u);
        assert_eq!(legacy.input_tokens, 42);
        assert_eq!(legacy.output_tokens, 7);
        assert_eq!(legacy.cache_read_input_tokens, Some(5));
        assert_eq!(legacy.cache_creation_input_tokens, None);
    }
}
