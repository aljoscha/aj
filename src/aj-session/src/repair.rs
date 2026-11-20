//! Answers for the tool calls an interrupted turn left open.
//!
//! A process killed between writing the assistant message and writing
//! the matching tool_result message leaves the log in a state that
//! both Anthropic and OpenAI APIs reject on resume. A `tool_call`
//! block must be answered by a `tool_result` message before the next
//! inference. [`repair_interrupted_tool_uses`] walks a linearized
//! [`BranchView`], detects dangling `tool_call` ids, and synthesizes
//! one error-flagged tool_result message per dangling id anchored at
//! the branch's current head.
//!
//! Scope: this heals exactly the dangling-`tool_call` shape, the one a
//! crash mid-tool-use produces and the one the providers reject. It does
//! not flag or trim an assistant message whose content was truncated but
//! whose line is well-formed. That message is replayed as authoritative (see
//! [`crate::log::SessionLog::append`]). The in-memory context build
//! (`aj_models::transform::transform_messages`) is the other half: it
//! drops `Error`/`Aborted` assistant messages before each inference, so a
//! truncated message that finalized as an error never reaches the model even
//! though it stays in the log and the scrollback.
//!
//! Owned by `aj-session` because it operates on log entries and
//! writes through `AgentLog`. Two callers use it: the function
//! that settles a session when it is opened, for the main agent and each
//! sub-agent, and a head switch, whose fork point can sit between a tool
//! call and its result.

use std::collections::HashSet;

use aj_agent::events::AgentId;
use aj_agent::message::AgentMessage;
use aj_models::types::{AssistantContent, Message, StopReason, ToolResultMessage};

use crate::log::{AgentLog, BranchView, LogEntryKind, SessionError, SessionLog};

/// The tool calls in the branch's model context that have no
/// result, as `(id, tool name)` in call order.
///
/// Calls in errored or aborted assistant responses are abandoned provider
/// output, not interrupted executions. They are excluded along with those
/// responses by model-context construction and need no result. Neither
/// does a call the last compaction summarized away: it is not in the
/// model's context, and a result would have nothing to answer there.
pub fn unanswered_tool_calls(branch: &BranchView) -> Vec<(String, String)> {
    let mut used: Vec<(String, String)> = Vec::new();
    let mut resolved: HashSet<String> = HashSet::new();
    for entry in branch.context_entries() {
        let msg = match &entry.entry {
            LogEntryKind::Message { message } => message,
            _ => continue,
        };
        match msg.as_stored_wire() {
            Some(Message::Assistant(a))
                if matches!(a.stop_reason, StopReason::Error | StopReason::Aborted) => {}
            Some(Message::Assistant(a)) => {
                for c in &a.content {
                    if let AssistantContent::ToolCall(tc) = c
                        && !used.iter().any(|(id, _)| *id == tc.id)
                    {
                        used.push((tc.id.clone(), tc.name.clone()));
                    }
                }
            }
            Some(Message::ToolResult(tr)) => {
                resolved.insert(tr.tool_call_id.clone());
            }
            _ => {}
        }
    }
    used.retain(|(id, _)| !resolved.contains(id));
    used
}

/// The unanswered tool calls of `branch` that spawned a sub-agent,
/// as `(call id, tool name, sub-agent)` in call order. The parent's call
/// waited for that sub-agent's report and did not get it.
///
/// A spawn record names the call that spawned its sub-agent, which is the
/// only tie between the two. An entry from before it did names none, and
/// its call is not found here.
pub fn unanswered_spawns(log: &SessionLog, branch: &BranchView) -> Vec<(String, String, usize)> {
    let entries = log.entries_in_order();
    let spawned = |call: &str| {
        entries
            .iter()
            .find_map(|entry| match (&entry.entry, entry.agent_id) {
                (LogEntryKind::SubAgentSpawn { call_id, .. }, agent)
                    if call_id.as_deref() == Some(call) =>
                {
                    agent
                }
                _ => None,
            })
    };
    let open = unanswered_tool_calls(branch).into_iter();
    open.filter_map(|(call, tool)| {
        let agent = spawned(&call)?;
        Some((call, tool, agent))
    })
    .collect()
}

/// Append `result` at the end of `agent`'s thread, the point its next real
/// turn would append at. For a caller that answers a call
/// [`unanswered_tool_calls`] named with something better than
/// [`repair_interrupted_tool_uses`] would.
pub fn answer_tool_call(
    log: &mut SessionLog,
    agent: AgentId,
    result: ToolResultMessage,
) -> Result<(), SessionError> {
    AgentLog::of(log, agent)?.add_message(AgentMessage::wire(Message::ToolResult(result)))?;
    Ok(())
}

/// Answer every tool call of `agent`'s linearized thread that has no
/// result (see [`unanswered_tool_calls`]) with an error-flagged
/// `Message::ToolResult`, so the branch is valid input for the
/// model again.
///
/// The function mutates `log` in place. Pass the linearized
/// `branch` you've already computed for the agent's thread; the
/// helper does not re-linearize so the caller can reuse the snapshot
/// for replay rendering. After the call, the caller should
/// re-linearize if it needs to observe the synthesized tool_results.
///
/// Returns `Ok(true)` when one or more dangling tool_call ids were
/// repaired, `Ok(false)` when the branch was already
/// internally consistent.
pub fn repair_interrupted_tool_uses(
    log: &mut SessionLog,
    agent: AgentId,
    branch: &BranchView,
) -> Result<bool, SessionError> {
    let dangling = unanswered_tool_calls(branch);
    if dangling.is_empty() {
        return Ok(false);
    }

    tracing::warn!(
        "resuming past {} interrupted tool call(s); synthesizing error results",
        dangling.len()
    );

    for (id, name) in dangling {
        let result = ToolResultMessage::text(
            id,
            name,
            "Previous session was interrupted before this tool call completed.",
            true,
        );
        answer_tool_call(log, agent, result)?;
    }
    Ok(true)
}

/// Whether the branch ends on input the model never answered: its last
/// message is not a completed assistant message. That is what a turn cut
/// short by a crash, a restart, a cancellation or a provider error leaves
/// behind, including the results [`repair_interrupted_tool_uses`]
/// synthesizes. Which of those it was is not in the transcript: the stored
/// [`crate::TurnState`] tells a turn its process abandoned from the others. An errored or aborted assistant message does not count as an
/// answer, because model-context construction drops it.
pub fn turn_unanswered(branch: &BranchView) -> bool {
    let answered = branch.entries().iter().rev().find_map(|entry| {
        let LogEntryKind::Message { message } = &entry.entry else {
            return None;
        };
        Some(matches!(
            message.as_stored_wire(),
            Some(Message::Assistant(a))
                if !matches!(a.stop_reason, StopReason::Error | StopReason::Aborted)
        ))
    });
    answered == Some(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::{AgentLog, SessionLog, ThreadFilter};
    use crate::persistence::ProjectSessions;
    use aj_models::types::{
        AssistantContent, AssistantMessage, TextContent, ToolCall, UserMessage,
    };
    use serde_json::json;
    use tempfile::TempDir;

    fn fresh_log() -> (TempDir, SessionLog) {
        let dir = TempDir::new().expect("temp dir");
        let persistence = ProjectSessions::new(dir.path().join("sessions"));
        let mut log = SessionLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".to_string()).expect("sp");
        (dir, log)
    }

    fn user(text: &str) -> AgentMessage {
        AgentMessage::wire(Message::User(UserMessage::text(text)))
    }

    fn assistant_tool_call(id: &str, name: &str) -> AgentMessage {
        AgentMessage::wire(Message::Assistant(AssistantMessage {
            content: vec![AssistantContent::ToolCall(ToolCall {
                is_raw: false,
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
    fn a_turn_is_unanswered_until_a_completed_assistant_message_ends_it() {
        let (_dir, mut log) = fresh_log();
        let mut ended = |message: AgentMessage| {
            AgentLog::main(&mut log)
                .add_message(message)
                .expect("append");
            let head = log.latest_leaf(ThreadFilter::MAIN).expect("head");
            let branch = log.linearize(&head, ThreadFilter::MAIN);
            turn_unanswered(&branch)
        };
        assert!(ended(user("hi")));
        assert!(!ended(assistant_tool_call("tu-1", "ping")));
        assert!(ended(tool_result_msg("tu-1", "ping", "interrupted", true)));
        let mut aborted = AssistantMessage::empty();
        aborted.stop_reason = StopReason::Aborted;
        assert!(ended(AgentMessage::wire(Message::Assistant(aborted))));
        assert!(!ended(assistant_text("done")));
    }

    #[test]
    fn repair_synthesizes_tool_results_for_dangling_uses() {
        let (_dir, mut log) = fresh_log();
        {
            let mut view = AgentLog::main(&mut log);
            view.add_message(user("hi")).expect("u");
            view.add_message(assistant_tool_call("tu-1", "ping"))
                .expect("a");
        }

        let head = log
            .latest_leaf(ThreadFilter::MAIN)
            .expect("user head exists");
        let convo = log.linearize(&head, ThreadFilter::MAIN);
        let repaired =
            repair_interrupted_tool_uses(&mut log, AgentId::Main, &convo).expect("repair");
        assert!(repaired, "should have synthesized at least one result");

        let head = log
            .latest_leaf(ThreadFilter::MAIN)
            .expect("user head exists post-repair");
        let convo = log.linearize(&head, ThreadFilter::MAIN);
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
            let mut view = AgentLog::main(&mut log);
            view.add_message(user("hi")).expect("u");
            view.add_message(assistant_text("hello")).expect("a");
        }

        let head = log
            .latest_leaf(ThreadFilter::MAIN)
            .expect("user head exists");
        let convo = log.linearize(&head, ThreadFilter::MAIN);
        let entry_count_before = convo.entries().len();
        let repaired =
            repair_interrupted_tool_uses(&mut log, AgentId::Main, &convo).expect("repair");
        assert!(!repaired, "should not have synthesized anything");

        let head = log
            .latest_leaf(ThreadFilter::MAIN)
            .expect("user head exists");
        let convo = log.linearize(&head, ThreadFilter::MAIN);
        assert_eq!(convo.entries().len(), entry_count_before);
    }

    #[test]
    fn repair_leaves_abandoned_provider_calls_unanswered() {
        for stop_reason in [StopReason::Error, StopReason::Aborted] {
            for arguments in [json!({}), json!({"command": "printf hello"})] {
                let (_dir, mut log) = fresh_log();
                {
                    let mut view = AgentLog::main(&mut log);
                    view.add_message(user("hi")).expect("user");
                    view.add_message(AgentMessage::wire(Message::Assistant(AssistantMessage {
                        content: vec![AssistantContent::ToolCall(ToolCall {
                            is_raw: false,
                            id: "abandoned".into(),
                            name: "bash".into(),
                            arguments,
                        })],
                        stop_reason: stop_reason.clone(),
                        ..AssistantMessage::empty()
                    })))
                    .expect("failed response");
                    view.add_message(assistant_text("Recovered after retry"))
                        .expect("retry");
                }
                let head = log.latest_leaf(ThreadFilter::MAIN).expect("head");
                let convo = log.linearize(&head, ThreadFilter::MAIN);
                let count_before = log.entries_in_order().len();
                assert!(
                    !repair_interrupted_tool_uses(&mut log, AgentId::Main, &convo).expect("repair")
                );
                assert_eq!(log.entries_in_order().len(), count_before);
                assert_eq!(log.latest_leaf(ThreadFilter::MAIN), Some(head));

                // A real unfinished execution must still be repaired even when
                // abandoned provider output exists in the same history.
                AgentLog::main(&mut log)
                    .add_message(assistant_tool_call("interrupted", "bash"))
                    .expect("unfinished execution");
                let head = log.latest_leaf(ThreadFilter::MAIN).expect("head");
                let convo = log.linearize(&head, ThreadFilter::MAIN);
                assert!(
                    repair_interrupted_tool_uses(&mut log, AgentId::Main, &convo).expect("repair")
                );
                assert_eq!(log.entries_in_order().len(), count_before + 2);
                let head = log.latest_leaf(ThreadFilter::MAIN).expect("head");
                let convo = log.linearize(&head, ThreadFilter::MAIN);
                let Some(Message::ToolResult(result)) = convo.last_message() else {
                    panic!("expected interrupted execution result");
                };
                assert_eq!(result.tool_call_id, "interrupted");
                assert!(result.is_error);
                assert!(
                    !repair_interrupted_tool_uses(&mut log, AgentId::Main, &convo)
                        .expect("repeat repair")
                );
            }
        }
    }

    #[test]
    fn repair_recognises_resolved_tool_call_ids() {
        let (_dir, mut log) = fresh_log();
        {
            let mut view = AgentLog::main(&mut log);
            view.add_message(user("hi")).expect("u");
            view.add_message(assistant_tool_call("tu-1", "ping"))
                .expect("a");
            view.add_message(tool_result_msg("tu-1", "ping", "ok", false))
                .expect("tr");
        }

        let head = log
            .latest_leaf(ThreadFilter::MAIN)
            .expect("user head exists");
        let convo = log.linearize(&head, ThreadFilter::MAIN);
        let repaired =
            repair_interrupted_tool_uses(&mut log, AgentId::Main, &convo).expect("repair");
        assert!(!repaired, "resolved tool_call ids must not trigger repair",);
    }

    #[test]
    fn repair_adopts_synthesized_message_ids_as_entry_ids() {
        let (_dir, mut log) = fresh_log();
        {
            let mut view = AgentLog::main(&mut log);
            view.add_message(user("hi")).expect("u");
            view.add_message(assistant_tool_call("tu-1", "ping"))
                .expect("a");
        }

        let head = log
            .latest_leaf(ThreadFilter::MAIN)
            .expect("user head exists");
        let convo = log.linearize(&head, ThreadFilter::MAIN);
        let count_before = log.entries_in_order().len();

        let repaired =
            repair_interrupted_tool_uses(&mut log, AgentId::Main, &convo).expect("repair");
        assert!(repaired, "a dangling tool call must be repaired");

        // The repair appends through the same `append` path as any writer,
        // so its synthesized messages get their minted id adopted as the
        // entry id, just like the journal's writes.
        let after = log.entries_in_order();
        let synthesized = &after[count_before..];
        assert!(
            !synthesized.is_empty(),
            "repair appended at least one entry"
        );
        for entry in synthesized {
            let LogEntryKind::Message { message } = &entry.entry else {
                panic!("repair only appends message entries");
            };
            assert!(
                !message.id().is_empty(),
                "synthesized message carries an id"
            );
            assert_eq!(message.id(), entry.id, "adopted as the entry id");
        }
    }
}
