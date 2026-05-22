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
//!   [`AgentEvent::MessageStart`] / [`AgentEvent::MessageEnd`] pair
//!   wrapping the projected [`AssistantMessage`]. Renderers walk the
//!   finalized content blocks on `MessageEnd` to paint
//!   text/thinking/tool-call blocks; no per-block streaming events
//!   are synthesized (replay has no deltas to stream). Each
//!   `ToolUseBlock` updates an internal `tool_use_id ↦ tool_name`
//!   map used to label the matching `ToolResultBlock` later.
//! - [`ConversationEntryKind::Message`] (user role) and
//!   [`ConversationEntryKind::ToolResult`]: one
//!   [`AgentEvent::ToolExecutionStart`] / [`ToolExecutionEnd`] pair
//!   per `ToolResultBlock` (the structured variant pulls
//!   per-`tool_use_id` payloads from `details`; legacy logs fall
//!   back to a text-only [`ToolDetails::Text`] synthesis), plus, if
//!   the message also carried free user text, a
//!   [`MessageStart`] / [`MessageEnd`] pair wrapping a synthesized
//!   [`UserMessage`].
//! - [`ConversationEntryKind::UserOutput`]: each variant maps onto
//!   the closest matching live event ([`AgentEvent::Notice`] /
//!   [`AgentEvent::Error`] for textual notices,
//!   [`AgentEvent::ToolExecutionEnd`] for tool-flavoured outputs,
//!   [`AgentEvent::TurnUsage`] for token-usage snapshots). Variants
//!   without a live equivalent ([`UserOutput::TokenUsageSummary`])
//!   are end-of-session presentational entries that the binary
//!   renders separately on shutdown, so they are skipped here.
//!
//! The mapping aligns with the unified event protocol per
//! `docs/aj-next-plan.md` §1.1: a `MessageStart`/`MessageEnd` pair
//! brackets every user, assistant, and tool-result message, with
//! `MessageUpdate` skipped because replay carries no deltas to
//! stream. Renderers that handle live streaming work seamlessly on
//! replayed content by reading the finalized message off
//! `MessageEnd`.

use std::collections::HashMap;

use aj_agent::events::{AgentEvent, AgentId};
use aj_agent::message::AgentMessage;
use aj_agent::projection::transcript_to_messages;
use aj_agent::tool::ToolDetails;
use aj_agent::types::UserOutput;
use aj_models::types::{Message, UserContent, UserMessage};
use aj_models::wire::{ContentBlockParam, MessageParam, Role};
use serde_json::Value;

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
    /// Map of `tool_use_id` ↦ (`tool_name`, `input`) populated from
    /// each `ToolUseBlock` we see on assistant messages. Used to
    /// synthesize a matching [`AgentEvent::ToolExecutionStart`]
    /// (carrying the args) and label the
    /// [`AgentEvent::ToolExecutionEnd`] for the corresponding
    /// `ToolResultBlock` later in the log. A persisted log can in
    /// principle contain a tool_result whose tool_use was truncated
    /// off the front (legacy logs that pre-date this crate), in
    /// which case the tool name falls back to a generic placeholder
    /// and the args fall back to an empty JSON object.
    tool_uses: HashMap<String, (String, Value)>,
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
                // Legacy user-role messages predate the structured
                // [`ConversationEntryKind::ToolResult`] variant and
                // never carry a `details` map on disk, so the
                // synthesized text-only [`ToolDetails::Text`]
                // fallback inside `project_user` is the only
                // payload available to the renderer here.
                Role::User => self.project_user(agent_id, &msg.content, None, out),
            },
            ConversationEntryKind::ToolResult { content, details } => {
                // Structured tool-result entries ship the per-call
                // [`ToolDetails`] payload alongside the wire content
                // (see `add_tool_result` in `aj_session::log`).
                // Threading the map through `project_user` lets
                // each [`ToolResultBlock`] pull its structured
                // result keyed by `tool_use_id`; missing entries
                // (or empty maps from callers that didn't have
                // structured details to ship, e.g. the
                // repair-on-resume walker that synthesizes
                // interrupted tool-call markers) keep the
                // text-only synthesis as the fallback so legacy
                // logs still render.
                self.project_user(agent_id, content, Some(details), out);
            }
            ConversationEntryKind::UserOutput(output) => {
                project_user_output(agent_id, output, out);
            }
        }
    }

    /// Project an assistant-role message's content blocks. Replays
    /// the finalized assistant message as a [`MessageStart`] /
    /// [`MessageEnd`] pair carrying the projected
    /// [`AssistantMessage`]; renderers walk the message's content
    /// blocks on `MessageEnd` to paint text/thinking/tool-call
    /// blocks. No per-block streaming events are synthesized.
    fn project_assistant(
        &mut self,
        agent_id: AgentId,
        content: &[ContentBlockParam],
        out: &mut Vec<AgentEvent>,
    ) {
        // Reuse the agent's wire-to-unified projection so the
        // renderer sees the same `AssistantMessage` shape it would
        // on a live `MessageEnd`. Wrapping the param in a single-
        // entry transcript is the simplest way to drive
        // `transcript_to_messages`; the helper returns one
        // `Message::Assistant` element in this case.
        let param = MessageParam {
            role: Role::Assistant,
            content: content.to_vec(),
        };
        let mut projected = transcript_to_messages(&[param]);
        let assistant = match projected.pop() {
            Some(Message::Assistant(m)) => m,
            // Empty assistant content projects to nothing; skip the
            // entry entirely rather than emit a malformed pair.
            _ => return,
        };

        // MessageStart carries an empty placeholder (with identity
        // stamped from the projected message) so renderers can open
        // their assistant slot without seeing the full content
        // twice; MessageEnd is the authoritative finalized snapshot.
        // This mirrors the live-streaming shape where MessageStart
        // fires before any content arrives.
        let empty_start = aj_models::types::AssistantMessage {
            content: Vec::new(),
            api: assistant.api.clone(),
            provider: assistant.provider.clone(),
            model: assistant.model.clone(),
            response_id: assistant.response_id.clone(),
            usage: assistant.usage.clone(),
            stop_reason: assistant.stop_reason.clone(),
            error: assistant.error.clone(),
            timestamp: assistant.timestamp,
        };
        out.push(AgentEvent::MessageStart {
            agent_id,
            message: AgentMessage::wire(Message::Assistant(empty_start)),
        });
        out.push(AgentEvent::MessageEnd {
            agent_id,
            message: AgentMessage::wire(Message::Assistant(assistant)),
        });

        // Track tool_use blocks so subsequent tool_result blocks
        // can synthesize a matching `ToolExecutionStart` (with
        // captured args) and `ToolExecutionEnd`. We do this after
        // the pair so any in-pair ordering invariants stay clear.
        for block in content {
            if let ContentBlockParam::ToolUseBlock {
                id, name, input, ..
            } = block
            {
                self.tool_uses
                    .insert(id.clone(), (name.clone(), input.clone()));
            }
        }
    }

    /// Project a user-role message's content blocks. Tool results
    /// produce synthesized `ToolExecutionStart` / `ToolExecutionEnd`
    /// events; free text produces a [`MessageStart`] /
    /// [`MessageEnd`] pair wrapping a [`UserMessage`] so renderers
    /// can paint the user-input pane the same way they handle live
    /// user prompts.
    ///
    /// `details` is `Some` for structured
    /// [`ConversationEntryKind::ToolResult`] entries and `None` for
    /// legacy user-role [`ConversationEntryKind::Message`] entries
    /// that predate the structured shape. When supplied, the
    /// payload for the `ToolExecutionEnd` event is taken from the
    /// map keyed by `tool_use_id`; entries missing from the map
    /// (e.g. an empty map from the repair walker) keep the
    /// text-only synthesis off the wire content so the renderer
    /// still has something to paint.
    fn project_user(
        &self,
        agent_id: AgentId,
        content: &[ContentBlockParam],
        details: Option<&HashMap<String, ToolDetails>>,
        out: &mut Vec<AgentEvent>,
    ) {
        for block in content {
            if let ContentBlockParam::ToolResultBlock {
                tool_use_id,
                content,
                is_error,
            } = block
            {
                // Look up the tool name and input args captured
                // from the preceding assistant message's tool_use
                // block. Missing entries (truncated/legacy logs)
                // fall back to a generic name and empty args; the
                // renderer copes with both.
                let (tool_name, args) = self
                    .tool_uses
                    .get(tool_use_id)
                    .cloned()
                    .unwrap_or_else(|| ("tool".to_string(), Value::Object(Default::default())));
                // Emit a synthetic Start so the renderer can paint
                // the `tool(args)` header with the real input
                // arguments. Without this the renderer's
                // build-on-miss fallback in
                // `update_tool_execution_result` constructs a
                // component with `json!({})` and the header reads
                // as `bash()` instead of
                // `bash(command="...", ...)`.
                out.push(AgentEvent::ToolExecutionStart {
                    agent_id,
                    call_id: tool_use_id.clone(),
                    tool: tool_name.clone(),
                    args,
                });
                // Prefer the persisted structured details payload
                // when the entry shipped one; fall back to a
                // text-only synthesis off the wire content for
                // legacy logs (or for tool_use_ids the producer
                // never recorded details for). This is what lets
                // a resumed session render diffs, bash exit codes,
                // todo snapshots, and sub-agent reports the same
                // way a live run does.
                let result = details
                    .and_then(|map| map.get(tool_use_id))
                    .cloned()
                    .unwrap_or_else(|| ToolDetails::Text {
                        summary: tool_name.clone(),
                        body: content.text(),
                    });
                out.push(AgentEvent::ToolExecutionEnd {
                    agent_id,
                    call_id: tool_use_id.clone(),
                    tool: tool_name,
                    result,
                    is_error: *is_error,
                });
            }
        }

        let text = collect_text(content);
        if !text.is_empty() {
            let user_message = AgentMessage::wire(Message::User(UserMessage {
                content: vec![UserContent::text(text)],
                timestamp: 0,
            }));
            out.push(AgentEvent::MessageStart {
                agent_id,
                message: user_message.clone(),
            });
            out.push(AgentEvent::MessageEnd {
                agent_id,
                message: user_message,
            });
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::{ConversationLog, ConversationView};
    use crate::persistence::ConversationPersistence;
    use aj_agent::types::UserOutput;
    use aj_models::wire::{ContentBlockParam, ToolResultContent};
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
        // input, the assistant message (Start/End wrapping the
        // projected AssistantMessage with thinking + text + tool_use
        // blocks), and the synthesized tool_result event keyed off
        // the earlier tool_use block.
        let (_dir, log) = seeded_log();
        let events: Vec<AgentEvent> = replay(&log).collect();

        // Expected order:
        //   MessageStart(User "hi")
        //   MessageEnd(User "hi")
        //   MessageStart(Assistant empty)
        //   MessageEnd(Assistant {thinking, text, tool_use})
        //   ToolExecutionStart { tool: "read_file", call_id: "call-1", args }
        //   ToolExecutionEnd   { tool: "read_file", call_id: "call-1" }
        assert_eq!(events.len(), 6, "got events: {events:#?}");

        // First user message → MessageStart/MessageEnd pair carrying
        // a UserMessage with the seeded text.
        match &events[0] {
            AgentEvent::MessageStart { message, .. } => match message.as_wire() {
                Some(Message::User(u)) => {
                    assert_eq!(u.content.len(), 1);
                    match &u.content[0] {
                        UserContent::Text(t) => assert_eq!(t.text, "hi"),
                        other => panic!("expected text content, got {other:?}"),
                    }
                }
                other => panic!("expected user message, got {other:?}"),
            },
            other => panic!("expected user MessageStart, got {other:?}"),
        }
        match &events[1] {
            AgentEvent::MessageEnd { message, .. } => match message.as_wire() {
                Some(Message::User(_)) => {}
                other => panic!("expected user message, got {other:?}"),
            },
            other => panic!("expected user MessageEnd, got {other:?}"),
        }

        // Assistant message → MessageStart with empty content,
        // MessageEnd carrying the finalized content blocks.
        match &events[2] {
            AgentEvent::MessageStart { message, .. } => match message.as_wire() {
                Some(Message::Assistant(a)) => assert!(a.content.is_empty()),
                other => panic!("expected assistant message, got {other:?}"),
            },
            other => panic!("expected assistant MessageStart, got {other:?}"),
        }
        match &events[3] {
            AgentEvent::MessageEnd { message, .. } => match message.as_wire() {
                Some(Message::Assistant(a)) => {
                    // The projected message must carry both the
                    // thinking text and the visible text. Tool calls
                    // are surfaced on the next event pair, not
                    // inline in the assistant content (though they
                    // do appear on the AssistantMessage itself for
                    // anyone inspecting it directly).
                    let thinking_count = a
                        .content
                        .iter()
                        .filter(|c| matches!(c, aj_models::types::AssistantContent::Thinking(_)))
                        .count();
                    let text_count = a
                        .content
                        .iter()
                        .filter(|c| matches!(c, aj_models::types::AssistantContent::Text(_)))
                        .count();
                    let tool_call_count = a
                        .content
                        .iter()
                        .filter(|c| matches!(c, aj_models::types::AssistantContent::ToolCall(_)))
                        .count();
                    assert_eq!(thinking_count, 1);
                    assert_eq!(text_count, 1);
                    assert_eq!(tool_call_count, 1);
                }
                other => panic!("expected assistant message, got {other:?}"),
            },
            other => panic!("expected assistant MessageEnd, got {other:?}"),
        }

        // Tool start event with the captured input args.
        match &events[4] {
            AgentEvent::ToolExecutionStart {
                agent_id,
                call_id,
                tool,
                args,
            } => {
                assert_eq!(*agent_id, AgentId::Main);
                assert_eq!(call_id, "call-1");
                assert_eq!(tool, "read_file");
                // The args must round-trip the input JSON from the
                // seeded tool_use block — this is what makes the
                // renderer print `read_file(path="/tmp/x")` on resume
                // instead of an empty `read_file()`.
                assert_eq!(args, &json!({"path": "/tmp/x"}));
            }
            other => panic!("expected tool execution start, got {other:?}"),
        }

        // Tool result event keyed off the prior tool_use block.
        match &events[5] {
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
        // Both a synthetic ToolExecutionStart (with empty args
        // because we never saw the tool_use block) and a
        // ToolExecutionEnd (with the result body) get emitted; the
        // renderer copes with the missing args fine.
        assert_eq!(events.len(), 2, "got: {events:#?}");
        match &events[0] {
            AgentEvent::ToolExecutionStart { tool, args, .. } => {
                assert_eq!(tool, "tool", "fallback tool name");
                assert_eq!(args, &serde_json::Value::Object(Default::default()));
            }
            other => panic!("expected tool execution start, got {other:?}"),
        }
        match &events[1] {
            AgentEvent::ToolExecutionEnd { tool, .. } => {
                assert_eq!(tool, "tool", "fallback tool name");
            }
            other => panic!("expected tool execution end, got {other:?}"),
        }
    }

    #[test]
    fn replay_projects_structured_tool_result_entries_as_user_thread_events() {
        // The structured [`ConversationEntryKind::ToolResult`] variant
        // must project onto the same Start/End event pair that the
        // legacy [`Role::User`] tool-result message path produces.
        // This test pins the empty-`details` fallback: callers that
        // didn't have structured details to ship (e.g. the
        // repair-on-resume walker that synthesizes interrupted
        // tool-call markers) get a text-only synthesis off the wire
        // content instead, so the renderer still has something to
        // paint. The non-empty-details path is covered separately
        // by `replay_projects_persisted_tool_details_on_resume`.
        let dir = fresh_threads_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        {
            let mut view = ConversationView::user(&mut log, None);
            view.add_user_message(vec![ContentBlockParam::new_text_block("hi".into())])
                .expect("u");
            view.add_assistant_message(vec![ContentBlockParam::ToolUseBlock {
                id: "tu-1".into(),
                name: "ping".into(),
                input: json!({"x": 1}),
                caller: None,
            }])
            .expect("a");
            view.add_tool_result(
                vec![ContentBlockParam::ToolResultBlock {
                    tool_use_id: "tu-1".into(),
                    content: ToolResultContent::Text("pong".into()),
                    is_error: false,
                }],
                HashMap::new(),
            )
            .expect("structured tool result");
        }

        let events: Vec<AgentEvent> = replay(&log).collect();
        // Walk the events list to find the synthesized
        // ToolExecutionStart/End pair for `tu-1`. With an empty
        // `details` map the End event falls back to a `Text` body
        // off the wire content (the assertion below).
        let start = events
            .iter()
            .find(|e| matches!(e, AgentEvent::ToolExecutionStart { call_id, .. } if call_id == "tu-1"))
            .expect("ToolExecutionStart fires for the structured tool result");
        match start {
            AgentEvent::ToolExecutionStart { tool, args, .. } => {
                assert_eq!(tool, "ping");
                assert_eq!(args, &json!({"x": 1}));
            }
            _ => unreachable!(),
        }

        let end = events
            .iter()
            .find(
                |e| matches!(e, AgentEvent::ToolExecutionEnd { call_id, .. } if call_id == "tu-1"),
            )
            .expect("ToolExecutionEnd fires for the structured tool result");
        match end {
            AgentEvent::ToolExecutionEnd {
                tool,
                result,
                is_error,
                ..
            } => {
                assert_eq!(tool, "ping");
                assert!(!is_error);
                // Empty `details` map: text-only synthesis off the
                // wire content.
                match result {
                    ToolDetails::Text { body, .. } => assert_eq!(body, "pong"),
                    other => panic!("expected Text fallback, got {other:?}"),
                }
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn replay_projects_persisted_tool_details_on_resume() {
        // When the producer (the agent's persistence listener)
        // shipped structured [`ToolDetails`] alongside the wire
        // content, replay must surface that payload on the
        // synthesized [`AgentEvent::ToolExecutionEnd`] event so a
        // resumed session renders diffs, bash exit codes, todo
        // snapshots, and sub-agent reports the same way a live run
        // does. This is the core of Step 4 of the resume-fidelity
        // follow-up plan.
        let dir = fresh_threads_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        {
            let mut view = ConversationView::user(&mut log, None);
            view.add_user_message(vec![ContentBlockParam::new_text_block("edit it".into())])
                .expect("u");
            // Two tool calls in one assistant batch: one with a
            // structured `Diff` detail (mirroring write_file /
            // edit_file), one with a `Bash` detail (mirroring the
            // bash tool). Exercising two distinct ToolDetails
            // variants in the same batch pins the
            // tool_use_id-keyed lookup so we don't accidentally
            // collapse to the first or last entry.
            view.add_assistant_message(vec![
                ContentBlockParam::ToolUseBlock {
                    id: "tu-edit".into(),
                    name: "edit_file".into(),
                    input: json!({"path": "/tmp/x", "old_string": "a", "new_string": "b"}),
                    caller: None,
                },
                ContentBlockParam::ToolUseBlock {
                    id: "tu-bash".into(),
                    name: "bash".into(),
                    input: json!({"command": "echo hi"}),
                    caller: None,
                },
            ])
            .expect("a");

            let mut details = HashMap::new();
            details.insert(
                "tu-edit".into(),
                ToolDetails::Diff {
                    path: "/tmp/x".into(),
                    before: "a".into(),
                    after: "b".into(),
                },
            );
            details.insert(
                "tu-bash".into(),
                ToolDetails::Bash {
                    command: "echo hi".into(),
                    stdout: "hi\n".into(),
                    stderr: String::new(),
                    exit_code: Some(0),
                    truncated: false,
                    full_output_path: None,
                },
            );

            view.add_tool_result(
                vec![
                    ContentBlockParam::ToolResultBlock {
                        tool_use_id: "tu-edit".into(),
                        content: ToolResultContent::Text("edited".into()),
                        is_error: false,
                    },
                    ContentBlockParam::ToolResultBlock {
                        tool_use_id: "tu-bash".into(),
                        content: ToolResultContent::Text("hi".into()),
                        is_error: false,
                    },
                ],
                details,
            )
            .expect("structured tool result with details");
        }

        let events: Vec<AgentEvent> = replay(&log).collect();

        // Find both End events and verify each carries the
        // persisted `ToolDetails` variant verbatim, not the
        // text-only fallback.
        let edit_end = events
            .iter()
            .find(
                |e| matches!(e, AgentEvent::ToolExecutionEnd { call_id, .. } if call_id == "tu-edit"),
            )
            .expect("ToolExecutionEnd for tu-edit");
        match edit_end {
            AgentEvent::ToolExecutionEnd { tool, result, .. } => {
                assert_eq!(tool, "edit_file");
                match result {
                    ToolDetails::Diff {
                        path,
                        before,
                        after,
                    } => {
                        assert_eq!(path, "/tmp/x");
                        assert_eq!(before, "a");
                        assert_eq!(after, "b");
                    }
                    other => {
                        panic!("expected persisted Diff details on resume, got {other:?}")
                    }
                }
            }
            _ => unreachable!(),
        }

        let bash_end = events
            .iter()
            .find(
                |e| matches!(e, AgentEvent::ToolExecutionEnd { call_id, .. } if call_id == "tu-bash"),
            )
            .expect("ToolExecutionEnd for tu-bash");
        match bash_end {
            AgentEvent::ToolExecutionEnd { tool, result, .. } => {
                assert_eq!(tool, "bash");
                match result {
                    ToolDetails::Bash {
                        command,
                        stdout,
                        exit_code,
                        ..
                    } => {
                        assert_eq!(command, "echo hi");
                        assert_eq!(stdout, "hi\n");
                        assert_eq!(*exit_code, Some(0));
                    }
                    other => {
                        panic!("expected persisted Bash details on resume, got {other:?}")
                    }
                }
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn replay_falls_back_to_text_for_tool_use_ids_missing_from_details() {
        // A structured [`ConversationEntryKind::ToolResult`] entry
        // can carry a `details` map that's partially populated —
        // e.g. one block has structured details, another doesn't
        // (the producer might have failed to capture the details
        // for some calls, or this might be a future repair walker
        // synthesizing partial entries). For every `tool_use_id`
        // missing from the map, the projection must fall back to
        // a text-only synthesis off the wire content so the
        // renderer still has something to paint. Pinning this
        // guarantees we never drop a tool result on the floor
        // just because the structured payload is incomplete.
        let dir = fresh_threads_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        {
            let mut view = ConversationView::user(&mut log, None);
            view.add_user_message(vec![ContentBlockParam::new_text_block("hi".into())])
                .expect("u");
            view.add_assistant_message(vec![
                ContentBlockParam::ToolUseBlock {
                    id: "tu-with".into(),
                    name: "edit_file".into(),
                    input: json!({"path": "/tmp/y"}),
                    caller: None,
                },
                ContentBlockParam::ToolUseBlock {
                    id: "tu-without".into(),
                    name: "read_file".into(),
                    input: json!({"path": "/tmp/z"}),
                    caller: None,
                },
            ])
            .expect("a");

            // Only `tu-with` has structured details; `tu-without`
            // exercises the fallback.
            let mut details = HashMap::new();
            details.insert(
                "tu-with".into(),
                ToolDetails::Diff {
                    path: "/tmp/y".into(),
                    before: "old".into(),
                    after: "new".into(),
                },
            );

            view.add_tool_result(
                vec![
                    ContentBlockParam::ToolResultBlock {
                        tool_use_id: "tu-with".into(),
                        content: ToolResultContent::Text("edited".into()),
                        is_error: false,
                    },
                    ContentBlockParam::ToolResultBlock {
                        tool_use_id: "tu-without".into(),
                        content: ToolResultContent::Text("file body".into()),
                        is_error: false,
                    },
                ],
                details,
            )
            .expect("structured tool result with mixed details");
        }

        let events: Vec<AgentEvent> = replay(&log).collect();

        // tu-with: structured Diff comes through.
        let with_end = events
            .iter()
            .find(
                |e| matches!(e, AgentEvent::ToolExecutionEnd { call_id, .. } if call_id == "tu-with"),
            )
            .expect("ToolExecutionEnd for tu-with");
        match with_end {
            AgentEvent::ToolExecutionEnd { result, .. } => match result {
                ToolDetails::Diff { .. } => {}
                other => panic!("expected structured Diff for tu-with, got {other:?}"),
            },
            _ => unreachable!(),
        }

        // tu-without: text-only fallback, body matches the wire
        // content, summary matches the resolved tool name.
        let without_end = events
            .iter()
            .find(
                |e| matches!(e, AgentEvent::ToolExecutionEnd { call_id, .. } if call_id == "tu-without"),
            )
            .expect("ToolExecutionEnd for tu-without");
        match without_end {
            AgentEvent::ToolExecutionEnd { result, .. } => match result {
                ToolDetails::Text { summary, body } => {
                    assert_eq!(summary, "read_file");
                    assert_eq!(body, "file body");
                }
                other => panic!("expected Text fallback for tu-without, got {other:?}"),
            },
            _ => unreachable!(),
        }
    }
}
