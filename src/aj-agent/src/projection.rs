//! Helpers projecting the agent's in-memory transcript onto the
//! unified [`aj_models::types::Message`] sequence the [`Provider`]
//! trait consumes.
//!
//! After the wire-format flip the agent's `transcript:
//! Vec<AgentMessage>` already carries unified messages, so the
//! projection is a single linear walk pulling the inner
//! [`aj_models::types::Message`] out of each
//! [`crate::message::AgentMessage`].
//!
//! [`Provider`]: aj_models::provider::Provider

use aj_models::types::Message;

use crate::message::AgentMessage;

/// Project the agent's in-memory transcript onto the unified
/// [`Message`] sequence the [`aj_models::provider::Provider`] trait
/// consumes.
///
/// Today every [`AgentMessage`] carries a wire [`Message`], so the
/// projection is just a clone-and-collect. The helper is retained
/// (rather than inlined at the call site) so any future agent-only
/// [`crate::message::AgentMessageKind`] variant has one obvious
/// place to project / drop on the way to inference.
pub fn transcript_to_messages(transcript: &[AgentMessage]) -> Vec<Message> {
    transcript
        .iter()
        .filter_map(|m| m.as_wire().cloned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aj_models::types::{
        AssistantContent, AssistantMessage, TextContent, ToolCall, UserContent, UserMessage,
    };
    use serde_json::json;

    #[test]
    fn transcript_with_unified_messages_round_trips() {
        let transcript = vec![
            AgentMessage::wire(Message::User(UserMessage::text("hello"))),
            AgentMessage::wire(Message::Assistant(AssistantMessage {
                content: vec![
                    AssistantContent::Text(TextContent {
                        text: "hi".into(),
                        text_signature: None,
                    }),
                    AssistantContent::ToolCall(ToolCall {
                        id: "tu-1".into(),
                        name: "ping".into(),
                        arguments: json!({}),
                    }),
                ],
                ..AssistantMessage::empty()
            })),
        ];

        let messages = transcript_to_messages(&transcript);
        assert_eq!(messages.len(), 2);
        match &messages[0] {
            Message::User(u) => match &u.content[0] {
                UserContent::Text(t) => assert_eq!(t.text, "hello"),
                other => panic!("expected text, got {other:?}"),
            },
            other => panic!("expected User, got {other:?}"),
        }
        match &messages[1] {
            Message::Assistant(a) => {
                assert_eq!(a.content.len(), 2);
            }
            other => panic!("expected Assistant, got {other:?}"),
        }
    }
}
