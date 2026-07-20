//! Helpers projecting the agent's in-memory transcript onto the
//! unified [`aj_models::types::Message`] sequence the [`Provider`]
//! trait consumes.
//!
//! The projection is a single linear walk that asks each
//! [`crate::message::AgentMessage`] for its provider-facing wire form
//! via [`AgentMessage::to_projected_wire`]. A stored wire message
//! projects as itself; an agent-only kind (a task notification)
//! synthesizes its wire framing here, so this is the one place a
//! notice becomes a wire message on the way to inference.
//!
//! [`Provider`]: aj_models::provider::Provider

use aj_models::types::Message;

use crate::message::AgentMessage;

/// Project the agent's in-memory transcript onto the unified
/// [`Message`] sequence the [`aj_models::provider::Provider`] trait
/// consumes.
///
/// Each entry projects through [`AgentMessage::to_projected_wire`], so
/// a task notification reaches the provider as its framed user message
/// and any future never-projecting kind drops out here.
pub fn transcript_to_messages(transcript: &[AgentMessage]) -> Vec<Message> {
    transcript
        .iter()
        .filter_map(|m| m.to_projected_wire())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{TaskNotification, TaskNotificationKind, TaskOutcome};
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

    #[test]
    fn task_notification_projects_to_framed_user_message() {
        // A notice has no stored wire form, but the provider must see
        // it framed so the model knows the turn was harness-injected.
        let transcript = vec![AgentMessage::task_notification(TaskNotification::new(
            "sleep 1".into(),
            TaskNotificationKind::Bash,
            TaskOutcome::Succeeded,
            "exit code 0".into(),
        ))];
        let messages = transcript_to_messages(&transcript);
        assert_eq!(messages.len(), 1);
        match &messages[0] {
            Message::User(u) => match &u.content[0] {
                UserContent::Text(t) => assert_eq!(
                    t.text,
                    "<task-notification>\nexit code 0\n</task-notification>"
                ),
                other => panic!("expected text, got {other:?}"),
            },
            other => panic!("expected User, got {other:?}"),
        }
    }
}
