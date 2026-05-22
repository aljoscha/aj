//! Crash-recovery helpers for resumed conversation logs.
//!
//! A process killed between writing the assistant message and writing
//! the matching tool_result message leaves the log in a state that
//! both Anthropic and OpenAI APIs reject on resume — `tool_call`
//! blocks must be answered by `tool_result` messages before the next
//! inference. [`repair_interrupted_tool_uses`] walks a linearized
//! [`Conversation`], detects dangling `tool_call` ids, and synthesizes
//! one error-flagged tool_result message per dangling id anchored at
//! the conversation's current head.
//!
//! Owned by `aj-session` because it operates on log entries and
//! writes through [`ConversationView`]. The binary calls it once on
//! startup, after resolving the system prompt and before seeding the
//! agent's in-memory transcript (per `docs/aj-next-plan.md` §2.4b
//! and §2.5).

use std::collections::HashSet;

use aj_agent::message::AgentMessage;
use aj_models::types::{AssistantContent, Message, ToolResultMessage};

use crate::log::{
    Conversation, ConversationEntryKind, ConversationLog, ConversationView, ThreadFilter,
};

/// Scan the linearized user thread for `tool_call` blocks that never
/// got a matching `tool_result`. If any are found, synthesize one
/// error-flagged `Message::ToolResult` per dangling id and append
/// them to the log so the conversation is valid input for the model
/// again.
///
/// The function mutates `log` in place. Pass the linearized
/// `conversation` you've already computed for the user thread; the
/// helper does not re-linearize so the caller can reuse the snapshot
/// for replay rendering. After the call, the caller should
/// re-linearize if it needs to observe the synthesized tool_results.
///
/// Returns `Ok(true)` when one or more dangling tool_call ids were
/// repaired, `Ok(false)` when the conversation was already
/// internally consistent.
pub fn repair_interrupted_tool_uses(
    log: &mut ConversationLog,
    conversation: &Conversation,
) -> Result<bool, anyhow::Error> {
    let mut used: HashSet<(String, String)> = HashSet::new();
    let mut resolved: HashSet<String> = HashSet::new();
    for entry in conversation.entries() {
        let ConversationEntryKind::Message { message: msg } = &entry.entry else {
            continue;
        };
        match msg.as_wire() {
            Some(Message::Assistant(a)) => {
                for c in &a.content {
                    if let AssistantContent::ToolCall(tc) = c {
                        used.insert((tc.id.clone(), tc.name.clone()));
                    }
                }
            }
            Some(Message::ToolResult(tr)) => {
                resolved.insert(tr.tool_call_id.clone());
            }
            _ => {}
        }
    }

    let dangling: Vec<(String, String)> = used
        .into_iter()
        .filter(|(id, _)| !resolved.contains(id))
        .collect();
    if dangling.is_empty() {
        return Ok(false);
    }

    tracing::warn!(
        "resuming past {} interrupted tool call(s); synthesizing error results",
        dangling.len()
    );

    let head = log
        .latest_leaf(ThreadFilter::USER)
        .expect("repair called with a non-empty user thread");
    let mut view = ConversationView::user(log, Some(head));
    for (id, name) in dangling {
        let tr = ToolResultMessage::text(
            id,
            name,
            "Previous session was interrupted before this tool call completed.",
            true,
        );
        view.add_message(AgentMessage::wire(Message::ToolResult(tr)))?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::{ConversationLog, ConversationView, ThreadFilter};
    use crate::persistence::ConversationPersistence;
    use aj_models::types::{
        AssistantContent, AssistantMessage, TextContent, ToolCall, UserMessage,
    };
    use serde_json::json;
    use tempfile::TempDir;

    fn fresh_log() -> (TempDir, ConversationLog) {
        let dir = TempDir::new().expect("temp dir");
        let persistence = ConversationPersistence::new(dir.path().join("threads"));
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".to_string()).expect("sp");
        (dir, log)
    }

    fn user(text: &str) -> AgentMessage {
        AgentMessage::wire(Message::User(UserMessage::text(text)))
    }

    fn assistant_tool_call(id: &str, name: &str) -> AgentMessage {
        AgentMessage::wire(Message::Assistant(AssistantMessage {
            content: vec![AssistantContent::ToolCall(ToolCall {
                id: id.to_string(),
                name: name.to_string(),
                arguments: json!({}),
            })],
            ..AssistantMessage::empty()
        }))
    }

    fn assistant_text(text: &str) -> AgentMessage {
        AgentMessage::wire(Message::Assistant(AssistantMessage {
            content: vec![AssistantContent::Text(TextContent {
                text: text.to_string(),
                text_signature: None,
            })],
            ..AssistantMessage::empty()
        }))
    }

    fn tool_result_msg(id: &str, name: &str, body: &str, is_error: bool) -> AgentMessage {
        AgentMessage::wire(Message::ToolResult(ToolResultMessage::text(
            id, name, body, is_error,
        )))
    }

    #[test]
    fn repair_synthesizes_tool_results_for_dangling_uses() {
        let (_dir, mut log) = fresh_log();
        {
            let mut view = ConversationView::user(&mut log, None);
            view.add_message(user("hi")).expect("u");
            view.add_message(assistant_tool_call("tu-1", "ping"))
                .expect("a");
        }

        let head = log
            .latest_leaf(ThreadFilter::USER)
            .expect("user head exists");
        let convo = log.linearize(&head, ThreadFilter::USER);
        let repaired = repair_interrupted_tool_uses(&mut log, &convo).expect("repair");
        assert!(repaired, "should have synthesized at least one result");

        let head = log
            .latest_leaf(ThreadFilter::USER)
            .expect("user head exists post-repair");
        let convo = log.linearize(&head, ThreadFilter::USER);
        let last = convo.last_message().expect("at least one message");
        match last {
            Message::ToolResult(tr) => {
                assert_eq!(tr.tool_call_id, "tu-1");
                assert!(tr.is_error);
            }
            other => panic!("expected synthetic ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn repair_is_a_noop_when_consistent() {
        let (_dir, mut log) = fresh_log();
        {
            let mut view = ConversationView::user(&mut log, None);
            view.add_message(user("hi")).expect("u");
            view.add_message(assistant_text("hello")).expect("a");
        }

        let head = log
            .latest_leaf(ThreadFilter::USER)
            .expect("user head exists");
        let convo = log.linearize(&head, ThreadFilter::USER);
        let entry_count_before = convo.len();
        let repaired = repair_interrupted_tool_uses(&mut log, &convo).expect("repair");
        assert!(!repaired, "should not have synthesized anything");

        let head = log
            .latest_leaf(ThreadFilter::USER)
            .expect("user head exists");
        let convo = log.linearize(&head, ThreadFilter::USER);
        assert_eq!(convo.len(), entry_count_before);
    }

    #[test]
    fn repair_recognises_resolved_tool_call_ids() {
        let (_dir, mut log) = fresh_log();
        {
            let mut view = ConversationView::user(&mut log, None);
            view.add_message(user("hi")).expect("u");
            view.add_message(assistant_tool_call("tu-1", "ping"))
                .expect("a");
            view.add_message(tool_result_msg("tu-1", "ping", "ok", false))
                .expect("tr");
        }

        let head = log
            .latest_leaf(ThreadFilter::USER)
            .expect("user head exists");
        let convo = log.linearize(&head, ThreadFilter::USER);
        let repaired = repair_interrupted_tool_uses(&mut log, &convo).expect("repair");
        assert!(!repaired, "resolved tool_call ids must not trigger repair",);
    }
}
