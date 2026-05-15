//! Replay a persisted [`ConversationLog`](crate::log::ConversationLog)
//! as an iterator of typed [`AgentEvent`]s.
//!
//! Resuming a thread should look the same to a frontend as a live
//! run: the renderer consumes a single typed event stream regardless
//! of whether the events came from a running agent or from a
//! previously recorded log on disk. `replay` is the bridge between
//! disk and that pipeline.
//!
//! See `docs/aj-next-plan.md` §2.5 for the binary-side wiring (the
//! `aj` binary opens a log, registers persistence and
//! renderer listeners on the agent, then drains `replay(...)` into
//! the renderer pipeline before entering its input loop).
//!
//! ## Mapping
//!
//! Each persisted [`ConversationEntryKind`] maps to zero or more
//! [`AgentEvent`]s, tagged with an [`AgentId`] derived from the
//! entry's [`ThreadKind`] / `agent_id` framing so the bridge listener
//! routes main-agent and sub-agent activity to the right renderers:
//!
//! - [`ConversationEntryKind::SystemPrompt`]: model-facing metadata,
//!   not user-visible. No event.
//! - [`ConversationEntryKind::Message`] (assistant role): one
//!   [`AgentEvent::StreamChunk`] `Start`/`Stop` pair on
//!   [`StreamChannel::Thinking`] for each `ThinkingBlock` /
//!   `RedactedThinkingBlock`, then one pair on
//!   [`StreamChannel::Text`] for the joined visible text. Each
//!   `ToolUseBlock` updates an internal `tool_use_id ↦ tool_name`
//!   map used to label the matching `ToolResultBlock` later.
//! - [`ConversationEntryKind::Message`] (user role): one
//!   [`AgentEvent::ToolExecutionEnd`] for each `ToolResultBlock`
//!   keyed by `tool_use_id` (with the tool name looked up from the
//!   prior assistant turn) and, if the message also carried free
//!   text, a [`StreamChannel::User`] `Start`/`Stop` pair so the
//!   renderer paints the user input pane.
//! - [`ConversationEntryKind::UserOutput`]: each variant maps onto
//!   the closest matching live event ([`AgentEvent::Notice`] /
//!   [`AgentEvent::Error`] for textual notices,
//!   [`AgentEvent::ToolExecutionEnd`] for tool-flavoured outputs,
//!   [`AgentEvent::TurnUsage`] for token-usage snapshots). Variants
//!   without a live equivalent ([`UserOutput::TokenUsageSummary`])
//!   are end-of-session presentational entries that the binary
//!   renders separately on shutdown, so they are skipped here.
//!
//! The mapping is deliberately conservative: replay produces only
//! events the live agent already emits today, so the bridge
//! listener doesn't need any replay-only handling beyond the new
//! [`StreamChannel::User`] case.

use std::collections::HashMap;

use aj_agent::events::{AgentEvent, AgentId, StreamAction, StreamChannel};
use aj_agent::tool::ToolDetails;
use aj_agent::types::UserOutput;
use aj_models::messages::{ContentBlockParam, Role};

use crate::log::{ConversationEntry, ConversationEntryKind, ConversationLog, ThreadKind};

/// Walk `log` in append order and yield one or more [`AgentEvent`]s
/// per persisted entry.
///
/// The returned iterator owns its events (entries are projected at
/// build time) so the caller can drain it into a listener without
/// holding a borrow on the log.
pub fn replay(log: &ConversationLog) -> impl Iterator<Item = AgentEvent> {
    let mut state = ReplayState::default();
    let mut events: Vec<AgentEvent> = Vec::new();
    for entry in log.entries_in_order() {
        state.project_entry(entry, &mut events);
    }
    events.into_iter()
}

/// Per-walk projection state.
#[derive(Default)]
struct ReplayState {
    /// Map of `tool_use_id` ↦ `tool_name` populated from each
    /// `ToolUseBlock` we see on assistant messages. Used to label
    /// the synthesized [`AgentEvent::ToolExecutionEnd`] event for
    /// the matching `ToolResultBlock` later in the log. A persisted
    /// log can in principle contain a tool_result whose tool_use was
    /// truncated off the front (legacy logs that pre-date this
    /// crate), in which case the tool name falls back to a generic
    /// placeholder.
    tool_uses: HashMap<String, String>,
}

impl ReplayState {
    /// Translate one entry into zero or more events, appending them
    /// to `out`.
    fn project_entry(&mut self, entry: &ConversationEntry, out: &mut Vec<AgentEvent>) {
        let agent_id = match agent_id_for(entry) {
            Some(id) => id,
            // [`ThreadKind::Meta`] is structural framing (system
            // prompt root) that doesn't render as a user-facing
            // event. Skip silently.
            None => return,
        };

        match &entry.entry {
            ConversationEntryKind::SystemPrompt { .. } => {
                // Model-facing metadata; not user-visible.
            }
            ConversationEntryKind::Message(msg) => match msg.role {
                Role::Assistant => self.project_assistant(agent_id, &msg.content, out),
                Role::User => self.project_user(agent_id, &msg.content, out),
            },
            ConversationEntryKind::UserOutput(output) => {
                project_user_output(agent_id, output, out);
            }
        }
    }

    /// Project an assistant-role message's content blocks. Order
    /// follows the legacy CLI's history rendering: thinking first,
    /// then text. Tool-use blocks update the tracking map but emit
    /// nothing on their own — the matching `tool_result` block in a
    /// subsequent user message is what triggers the
    /// [`AgentEvent::ToolExecutionEnd`].
    fn project_assistant(
        &mut self,
        agent_id: AgentId,
        content: &[ContentBlockParam],
        out: &mut Vec<AgentEvent>,
    ) {
        for block in content {
            match block {
                ContentBlockParam::ThinkingBlock { thinking, .. } => {
                    push_stream_pair(out, agent_id, StreamChannel::Thinking, thinking);
                }
                ContentBlockParam::RedactedThinkingBlock { data } => {
                    let snapshot = format!("[Redacted thinking: {data}]");
                    push_stream_pair(out, agent_id, StreamChannel::Thinking, &snapshot);
                }
                _ => {}
            }
        }

        let text = collect_text(content);
        if !text.is_empty() {
            push_stream_pair(out, agent_id, StreamChannel::Text, &text);
        }

        // Track tool_use blocks so subsequent tool_result blocks
        // can name the tool that produced them. We do this last so
        // any thinking/text replay precedes the synthesized tool
        // events that live on the next user-role message.
        for block in content {
            if let ContentBlockParam::ToolUseBlock { id, name, .. } = block {
                self.tool_uses.insert(id.clone(), name.clone());
            }
        }
    }

    /// Project a user-role message's content blocks. Tool results
    /// produce synthesized `ToolExecutionEnd` events; free text
    /// produces a single `StreamChannel::User` Start/Stop pair so
    /// the renderer can paint the user-input pane. Anything else
    /// (images, documents) is currently not user-rendered on
    /// resume.
    fn project_user(
        &self,
        agent_id: AgentId,
        content: &[ContentBlockParam],
        out: &mut Vec<AgentEvent>,
    ) {
        for block in content {
            if let ContentBlockParam::ToolResultBlock {
                tool_use_id,
                content,
                is_error,
            } = block
            {
                let tool_name = self
                    .tool_uses
                    .get(tool_use_id)
                    .cloned()
                    .unwrap_or_else(|| "tool".to_string());
                let body = content.text();
                out.push(AgentEvent::ToolExecutionEnd {
                    agent_id,
                    call_id: tool_use_id.clone(),
                    tool: tool_name.clone(),
                    result: ToolDetails::Text {
                        summary: tool_name,
                        body,
                    },
                    is_error: *is_error,
                });
            }
        }

        let text = collect_text(content);
        if !text.is_empty() {
            push_stream_pair(out, agent_id, StreamChannel::User, &text);
        }
    }
}

/// Project a freestanding [`UserOutput`] entry onto the bus.
///
/// Each variant maps onto the closest live equivalent so the bridge
/// listener can render it through its existing handlers:
///
/// - [`UserOutput::Notice`] / [`UserOutput::Error`]: textual
///   notices fold onto [`AgentEvent::Notice`] / [`AgentEvent::Error`].
/// - [`UserOutput::ToolResult`] / [`UserOutput::ToolResultDiff`] /
///   [`UserOutput::ToolError`]: tool-flavoured payloads fold onto
///   [`AgentEvent::ToolExecutionEnd`] with the matching
///   [`ToolDetails`] variant. The `tool_use_id` correlation is
///   unavailable for these legacy entries, so we synthesize an
///   empty `call_id`; the bridge listener doesn't read it.
/// - [`UserOutput::TokenUsage`]: per-turn snapshot folds onto
///   [`AgentEvent::TurnUsage`].
/// - [`UserOutput::TokenUsageSummary`]: end-of-session totals are
///   rendered by the binary on shutdown, not as a bus event during
///   replay.
fn project_user_output(agent_id: AgentId, output: &UserOutput, out: &mut Vec<AgentEvent>) {
    match output {
        UserOutput::Notice(text) => {
            out.push(AgentEvent::Notice {
                agent_id,
                text: text.clone(),
            });
        }
        UserOutput::Error(text) => {
            out.push(AgentEvent::Error {
                agent_id,
                text: text.clone(),
            });
        }
        UserOutput::ToolResult {
            tool_name,
            input,
            output,
        } => {
            out.push(AgentEvent::ToolExecutionEnd {
                agent_id,
                call_id: String::new(),
                tool: tool_name.clone(),
                result: ToolDetails::Text {
                    summary: input.clone(),
                    body: output.clone(),
                },
                is_error: false,
            });
        }
        UserOutput::ToolResultDiff {
            tool_name,
            input,
            before,
            after,
        } => {
            out.push(AgentEvent::ToolExecutionEnd {
                agent_id,
                call_id: String::new(),
                tool: tool_name.clone(),
                result: ToolDetails::Diff {
                    path: input.clone(),
                    before: before.clone(),
                    after: after.clone(),
                },
                is_error: false,
            });
        }
        UserOutput::ToolError {
            tool_name,
            input,
            error,
        } => {
            out.push(AgentEvent::ToolExecutionEnd {
                agent_id,
                call_id: String::new(),
                tool: tool_name.clone(),
                result: ToolDetails::Text {
                    summary: input.clone(),
                    body: error.clone(),
                },
                is_error: true,
            });
        }
        UserOutput::TokenUsage(usage) => {
            out.push(AgentEvent::TurnUsage {
                agent_id,
                usage: usage.clone(),
            });
        }
        UserOutput::TokenUsageSummary(_) => {
            // Surfaced by the binary on shutdown via the agent's
            // accumulated usage counters; there is no live event
            // equivalent to replay.
        }
    }
}

/// Map an entry's [`ThreadKind`] / `agent_id` framing onto an
/// [`AgentId`]. Returns `None` for [`ThreadKind::Meta`], whose
/// entries (the system-prompt root) carry no user-visible payload.
fn agent_id_for(entry: &ConversationEntry) -> Option<AgentId> {
    match entry.thread {
        ThreadKind::User => Some(AgentId::Main),
        ThreadKind::Subagent => entry.agent_id.map(AgentId::Sub),
        ThreadKind::Meta => None,
    }
}

/// Concatenate every [`ContentBlockParam::TextBlock`] in `content`
/// into a single space-separated string.
fn collect_text(content: &[ContentBlockParam]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlockParam::TextBlock { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Emit a Start/Stop streaming pair for the given snapshot. Mirrors
/// the legacy CLI's two-call rendering pattern (`*_start("")` then
/// `*_stop(text)`) so persisted text round-trips through the same
/// helpers used for live streaming.
fn push_stream_pair(
    out: &mut Vec<AgentEvent>,
    agent_id: AgentId,
    channel: StreamChannel,
    snapshot: &str,
) {
    out.push(AgentEvent::StreamChunk {
        agent_id,
        channel,
        action: StreamAction::Start {
            snapshot: String::new(),
        },
    });
    out.push(AgentEvent::StreamChunk {
        agent_id,
        channel,
        action: StreamAction::Stop {
            snapshot: snapshot.to_string(),
        },
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::{ConversationLog, ConversationView};
    use crate::persistence::ConversationPersistence;
    use aj_agent::types::UserOutput;
    use aj_models::messages::{ContentBlockParam, ToolResultContent};
    use serde_json::json;
    use std::path::PathBuf;

    fn fresh_threads_dir() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "aj-session-replay-test-{pid}-{tid:?}-{nanos}",
            pid = std::process::id(),
            tid = std::thread::current().id(),
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// Build a simple log that exercises the assistant text path,
    /// the user text path, and the tool_use → tool_result
    /// correlation. Returns the log so callers can drain `replay`
    /// against it.
    fn seeded_log() -> (PathBuf, ConversationLog) {
        let dir = fresh_threads_dir();
        let persistence = ConversationPersistence::new(dir.clone());
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("sys".into()).expect("system prompt");
        {
            let mut view = ConversationView::user(&mut log, None);
            view.add_user_message(vec![ContentBlockParam::new_text_block("hi".into())])
                .expect("user msg");
            view.add_assistant_message(vec![
                ContentBlockParam::ThinkingBlock {
                    signature: String::new(),
                    thinking: "let me think".into(),
                },
                ContentBlockParam::new_text_block("hello".into()),
                ContentBlockParam::ToolUseBlock {
                    id: "call-1".into(),
                    name: "read_file".into(),
                    input: json!({"path": "/tmp/x"}),
                    caller: None,
                },
            ])
            .expect("assistant msg");
            view.add_user_message(vec![ContentBlockParam::ToolResultBlock {
                tool_use_id: "call-1".into(),
                content: ToolResultContent::Text("result body".into()),
                is_error: false,
            }])
            .expect("tool result msg");
        }
        (dir, log)
    }

    #[test]
    fn replay_projects_assistant_thinking_text_and_tool_results() {
        // The replay walker must emit, in order, the seeded user
        // input, the assistant thinking + text Start/Stop pairs,
        // and the synthesized tool_result event keyed off the
        // earlier tool_use block.
        let (_dir, log) = seeded_log();
        let events: Vec<AgentEvent> = replay(&log).collect();

        // Expected order:
        //   StreamChunk(User, Start)
        //   StreamChunk(User, Stop "hi")
        //   StreamChunk(Thinking, Start)
        //   StreamChunk(Thinking, Stop "let me think")
        //   StreamChunk(Text, Start)
        //   StreamChunk(Text, Stop "hello")
        //   ToolExecutionEnd { tool: "read_file", call_id: "call-1" }
        assert_eq!(events.len(), 7, "got events: {events:#?}");

        // First user message → StreamChannel::User pair.
        match &events[0] {
            AgentEvent::StreamChunk {
                channel: StreamChannel::User,
                action: StreamAction::Start { snapshot },
                ..
            } => assert!(snapshot.is_empty()),
            other => panic!("expected user Start, got {other:?}"),
        }
        match &events[1] {
            AgentEvent::StreamChunk {
                channel: StreamChannel::User,
                action: StreamAction::Stop { snapshot },
                ..
            } => assert_eq!(snapshot, "hi"),
            other => panic!("expected user Stop with text, got {other:?}"),
        }

        // Assistant thinking pair.
        match &events[2] {
            AgentEvent::StreamChunk {
                channel: StreamChannel::Thinking,
                action: StreamAction::Start { .. },
                ..
            } => {}
            other => panic!("expected thinking Start, got {other:?}"),
        }
        match &events[3] {
            AgentEvent::StreamChunk {
                channel: StreamChannel::Thinking,
                action: StreamAction::Stop { snapshot },
                ..
            } => assert_eq!(snapshot, "let me think"),
            other => panic!("expected thinking Stop, got {other:?}"),
        }

        // Assistant text pair.
        match &events[4] {
            AgentEvent::StreamChunk {
                channel: StreamChannel::Text,
                action: StreamAction::Start { .. },
                ..
            } => {}
            other => panic!("expected text Start, got {other:?}"),
        }
        match &events[5] {
            AgentEvent::StreamChunk {
                channel: StreamChannel::Text,
                action: StreamAction::Stop { snapshot },
                ..
            } => assert_eq!(snapshot, "hello"),
            other => panic!("expected text Stop, got {other:?}"),
        }

        // Tool result event keyed off the prior tool_use block.
        match &events[6] {
            AgentEvent::ToolExecutionEnd {
                agent_id,
                call_id,
                tool,
                result,
                is_error,
            } => {
                assert_eq!(*agent_id, AgentId::Main);
                assert_eq!(call_id, "call-1");
                assert_eq!(tool, "read_file");
                assert!(!is_error);
                match result {
                    ToolDetails::Text { summary, body } => {
                        assert_eq!(summary, "read_file");
                        assert_eq!(body, "result body");
                    }
                    other => panic!("expected Text details, got {other:?}"),
                }
            }
            other => panic!("expected tool execution end, got {other:?}"),
        }
    }

    #[test]
    fn replay_skips_system_prompt_and_handles_empty_log() {
        // A log with only the system-prompt root produces zero
        // events: meta entries are structural framing that the
        // binary projects elsewhere (it freezes the prompt and
        // hands it to the agent), not user-facing output.
        let dir = fresh_threads_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into())
            .expect("set system prompt");

        let events: Vec<AgentEvent> = replay(&log).collect();
        assert!(events.is_empty(), "got: {events:#?}");
    }

    #[test]
    fn replay_projects_user_output_tool_error_with_is_error_flag() {
        // `UserOutput::ToolError` is the predominant on-disk shape
        // (see the §2.0 reconnaissance findings): every error a
        // tool surfaced before the §2.4a migration ended up here.
        // Replay must fold them onto `ToolExecutionEnd` with
        // `is_error: true` so the bridge listener routes the event
        // to `display_tool_error` instead of `display_tool_result`.
        let dir = fresh_threads_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        {
            let mut view = ConversationView::user(&mut log, None);
            view.add_user_message(vec![ContentBlockParam::new_text_block("hi".into())])
                .expect("user msg");
            view.add_user_output(UserOutput::ToolError {
                tool_name: "bash".into(),
                input: "{ \"command\": \"false\" }".into(),
                error: "exit 1".into(),
            })
            .expect("user output");
        }

        let events: Vec<AgentEvent> = replay(&log).collect();
        // Two user-stream events for "hi" plus one tool-error event.
        assert_eq!(events.len(), 3, "got events: {events:#?}");

        match &events[2] {
            AgentEvent::ToolExecutionEnd {
                tool,
                result,
                is_error,
                ..
            } => {
                assert_eq!(tool, "bash");
                assert!(*is_error);
                match result {
                    ToolDetails::Text { summary, body } => {
                        assert_eq!(summary, "{ \"command\": \"false\" }");
                        assert_eq!(body, "exit 1");
                    }
                    other => panic!("expected Text details, got {other:?}"),
                }
            }
            other => panic!("expected tool execution end, got {other:?}"),
        }
    }

    #[test]
    fn replay_routes_subagent_entries_to_sub_agent_id() {
        // Sub-agent threads share the parent's bus per
        // `docs/aj-next-plan.md` §1.6; replay must carry through
        // the `Sub(n)` tagging so the bridge listener routes those
        // events to the per-sub-agent renderer.
        let dir = fresh_threads_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into())
            .expect("set system prompt");

        let user_head = {
            let mut view = ConversationView::user(&mut log, None);
            view.add_user_message(vec![ContentBlockParam::new_text_block("hi".into())])
                .expect("user msg");
            view.add_assistant_message(vec![ContentBlockParam::new_text_block(
                "delegating".into(),
            )])
            .expect("assistant msg");
            view.head().cloned().expect("head present")
        };

        {
            let mut view = ConversationView::subagent(&mut log, user_head, 1);
            view.add_user_message(vec![ContentBlockParam::new_text_block("subtask".into())])
                .expect("subagent prompt");
            view.add_assistant_message(vec![ContentBlockParam::new_text_block(
                "subagent reply".into(),
            )])
            .expect("subagent assistant");
        }

        let events: Vec<AgentEvent> = replay(&log).collect();
        // Verify there is at least one sub-agent-tagged event.
        let any_sub = events
            .iter()
            .any(|event| matches!(event.agent_id(), AgentId::Sub(1)));
        assert!(any_sub, "expected at least one Sub(1) event in {events:#?}");

        // And at least one main-agent event (for the parent thread).
        let any_main = events
            .iter()
            .any(|event| matches!(event.agent_id(), AgentId::Main));
        assert!(any_main, "expected at least one Main event in {events:#?}");
    }

    #[test]
    fn replay_falls_back_when_tool_use_id_is_not_tracked() {
        // A truncated/legacy log can carry a tool_result block
        // whose `tool_use_id` was never seen on a preceding
        // assistant message. Replay must still emit a sensible
        // event rather than panicking — generic "tool" name and
        // empty tool body that the renderer prints as plain text.
        let dir = fresh_threads_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        {
            let mut view = ConversationView::user(&mut log, None);
            // No prior assistant tool_use; jump straight to a
            // tool_result block on a user message. This mirrors a
            // log file that's been truncated to drop the assistant
            // turn but kept the response.
            view.add_user_message(vec![ContentBlockParam::ToolResultBlock {
                tool_use_id: "orphan".into(),
                content: ToolResultContent::Text("body".into()),
                is_error: false,
            }])
            .expect("orphan tool result");
        }

        let events: Vec<AgentEvent> = replay(&log).collect();
        assert_eq!(events.len(), 1, "got: {events:#?}");
        match &events[0] {
            AgentEvent::ToolExecutionEnd { tool, .. } => {
                assert_eq!(tool, "tool", "fallback tool name");
            }
            other => panic!("expected tool execution end, got {other:?}"),
        }
    }
}
