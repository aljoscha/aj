//! Crash-recovery helpers for resumed conversation logs.
//!
//! A process killed between writing the assistant message and writing
//! the matching `tool_result` user message leaves the log in a state
//! that both Anthropic and OpenAI APIs reject on resume — `tool_use`
//! blocks must be answered by `tool_result` blocks before the next
//! inference. [`repair_interrupted_tool_uses`] walks a linearized
//! [`Conversation`], detects dangling `tool_use` ids, and synthesizes
//! a single user message with one `tool_result` block per dangling id
//! anchored at the conversation's current head.
//!
//! Owned by `aj-session` because it operates on log entries and
//! writes through [`ConversationView`]. The binary calls it once on
//! startup, after resolving the system prompt and before seeding the
//! agent's in-memory transcript (per `docs/aj-next-plan.md` §2.4b
//! and §2.5).

use std::collections::HashSet;

use aj_models::messages::{ContentBlockParam, Role};

use crate::log::{
    Conversation, ConversationEntryKind, ConversationLog, ConversationView, ThreadFilter,
};

/// Scan the linearized user thread for `tool_use` blocks that never
/// got a matching `tool_result`. If any are found, synthesize a
/// single user message with one `tool_result` block per dangling id
/// and append it to the log so the conversation is valid input for
/// the model again.
///
/// The function mutates `log` in place. Pass the linearized
/// `conversation` you've already computed for the user thread; the
/// helper does not re-linearize so the caller can reuse the snapshot
/// for replay rendering. After the call, the caller should
/// re-linearize if it needs to observe the synthesized tool_results
/// (the freshly-appended message becomes the new latest leaf).
///
/// Returns `Ok(true)` when one or more dangling tool_use ids were
/// repaired, `Ok(false)` when the conversation was already
/// internally consistent. Errors propagate from the underlying
/// [`ConversationView::add_user_message`] write.
pub fn repair_interrupted_tool_uses(
    log: &mut ConversationLog,
    conversation: &Conversation,
) -> Result<bool, anyhow::Error> {
    // Collect all tool_use ids from assistant messages and all
    // tool_result ids seen in subsequent user messages. Anything in
    // the first set that isn't in the second set is dangling.
    let mut used: HashSet<String> = HashSet::new();
    let mut resolved: HashSet<String> = HashSet::new();
    for entry in conversation.entries() {
        let ConversationEntryKind::Message(msg) = &entry.entry else {
            continue;
        };
        match msg.role {
            Role::Assistant => {
                for block in &msg.content {
                    if let ContentBlockParam::ToolUseBlock { id, .. } = block {
                        used.insert(id.clone());
                    }
                }
            }
            Role::User => {
                for block in &msg.content {
                    if let ContentBlockParam::ToolResultBlock { tool_use_id, .. } = block {
                        resolved.insert(tool_use_id.clone());
                    }
                }
            }
        }
    }

    let dangling: Vec<String> = used.difference(&resolved).cloned().collect();
    if dangling.is_empty() {
        return Ok(false);
    }

    tracing::warn!(
        "resuming past {} interrupted tool call(s); synthesizing error results",
        dangling.len()
    );

    let tool_result_contents: Vec<ContentBlockParam> = dangling
        .into_iter()
        .map(|tool_use_id| ContentBlockParam::ToolResultBlock {
            tool_use_id,
            content: "Previous session was interrupted before this tool call completed."
                .to_string()
                .into(),
            is_error: true,
        })
        .collect();

    let head = log
        .latest_leaf(ThreadFilter::USER)
        .expect("repair called with a non-empty user thread");
    let mut view = ConversationView::user(log, Some(head));
    view.add_user_message(tool_result_contents)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::{ConversationLog, ConversationView, ThreadFilter};
    use crate::persistence::ConversationPersistence;
    use aj_models::messages::ContentBlockParam;
    use serde_json::json;
    use tempfile::TempDir;

    fn fresh_log() -> (TempDir, ConversationLog) {
        let dir = TempDir::new().expect("temp dir");
        let persistence = ConversationPersistence::new(dir.path().join("threads"));
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".to_string()).expect("sp");
        (dir, log)
    }

    #[test]
    fn repair_synthesizes_tool_results_for_dangling_uses() {
        let (_dir, mut log) = fresh_log();
        {
            let mut view = ConversationView::user(&mut log, None);
            view.add_user_message(vec![ContentBlockParam::new_text_block("hi".into())])
                .expect("u");
            view.add_assistant_message(vec![ContentBlockParam::ToolUseBlock {
                id: "tu-1".to_string(),
                input: json!({}),
                name: "ping".to_string(),
                caller: None,
            }])
            .expect("a");
        }

        let head = log
            .latest_leaf(ThreadFilter::USER)
            .expect("user head exists");
        let convo = log.linearize(&head, ThreadFilter::USER);
        let repaired = repair_interrupted_tool_uses(&mut log, &convo).expect("repair");
        assert!(repaired, "should have synthesized at least one result");

        // The synthesized message is anchored at the assistant
        // message and lives on the user thread.
        let head = log
            .latest_leaf(ThreadFilter::USER)
            .expect("user head exists post-repair");
        let convo = log.linearize(&head, ThreadFilter::USER);
        let last = convo.last_message().expect("at least one message");
        assert!(matches!(last.role, aj_models::messages::Role::User));
        // Every block in the synthesized message is a tool_result.
        for block in &last.content {
            assert!(matches!(block, ContentBlockParam::ToolResultBlock { .. }));
        }
    }

    #[test]
    fn repair_is_a_noop_when_consistent() {
        let (_dir, mut log) = fresh_log();
        {
            let mut view = ConversationView::user(&mut log, None);
            view.add_user_message(vec![ContentBlockParam::new_text_block("hi".into())])
                .expect("u");
            view.add_assistant_message(vec![ContentBlockParam::new_text_block("hello".into())])
                .expect("a");
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
}
