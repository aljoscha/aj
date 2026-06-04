//! Replay a persisted [`ConversationLog`](crate::log::ConversationLog)
//! as an iterator of typed [`AgentEvent`]s.
//!
//! Resuming a session should look the same to a frontend as a live
//! run: the renderer consumes a single typed event stream regardless
//! of whether the events came from a running agent or from a
//! previously recorded log on disk. `replay` is the bridge between
//! disk and that pipeline.
//!
//! See `docs/aj-next-plan.md` §2.5 for the binary-side wiring (the
//! `aj` binary opens a log, registers persistence and renderer
//! listeners on the agent, then drains `replay(...)` into the
//! renderer pipeline before entering its input loop).
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
//! - [`ConversationEntryKind::Message`] (assistant): one
//!   [`AgentEvent::MessageStart`] / [`AgentEvent::MessageEnd`] pair
//!   wrapping the projected [`AssistantMessage`], followed by an
//!   [`AgentEvent::TurnUsage`] carrying the per-turn `usage`
//!   recorded on the assistant message and a running
//!   accumulated total. Listeners (the TUI footer, end-of-session
//!   summaries) therefore see the same shape on resume as on a
//!   live turn. Renderers walk the finalized content blocks on
//!   `MessageEnd` to paint text/thinking/tool-call blocks; no
//!   per-block streaming events are synthesized (replay has no
//!   deltas to stream). Each tool_call updates an internal
//!   `tool_call_id ↦ (tool_name, args)` map used to label the
//!   matching tool result later.
//! - [`ConversationEntryKind::Message`] (user): one
//!   [`AgentEvent::MessageStart`] / [`AgentEvent::MessageEnd`] pair.
//! - [`ConversationEntryKind::Message`] (tool_result): one
//!   [`AgentEvent::ToolExecutionStart`] / [`ToolExecutionEnd`] pair
//!   pulling the tool name & input args from the tracking map. The
//!   structured `ToolDetails` payload is reconstructed by
//!   deserializing [`ToolResultMessage::details`] (falling back to a
//!   text-only synthesis when absent or corrupt). The
//!   [`AgentEvent::MessageStart`] / [`AgentEvent::MessageEnd`] pair
//!   around the tool_result is also emitted so persistence listeners
//!   replaying the stream see the same shape live runs produce.

use std::collections::HashMap;

use aj_agent::events::{AgentEvent, AgentId};
use aj_agent::message::AgentMessage;
use aj_agent::tool::ToolDetails;
use aj_agent::types::TokenUsage;
use aj_models::types::{AssistantContent, Message, Usage, UserContent};
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
        state.bracket_subagent(entry, &mut events);
        state.project_entry(entry, &mut events);
    }
    // Close a sub-agent run still open at end-of-log.
    state.close_open_sub(&mut events);
    events.into_iter()
}

/// Per-walk projection state.
#[derive(Default)]
struct ReplayState {
    /// Map of `tool_call_id` ↦ (`tool_name`, `arguments`) populated
    /// from each `ToolCall` we see on assistant messages. Used to
    /// synthesize a matching [`AgentEvent::ToolExecutionStart`]
    /// (carrying the args) and label the
    /// [`AgentEvent::ToolExecutionEnd`] for the corresponding
    /// tool result later in the log.
    tool_calls: HashMap<String, (String, Value)>,
    /// Per-agent accumulated [`Usage`] running totals, used to
    /// build the `accumulated_*` fields on synthesized
    /// [`AgentEvent::TurnUsage`] events. The map starts empty and
    /// grows on demand the first time we see an assistant message
    /// for an [`AgentId`]; the value stored at `agent_id` is the
    /// accumulator *as observed before* the next turn is emitted,
    /// matching the live agent's event order (see
    /// `aj_agent::Agent::prompt`: `TurnUsage` carries the
    /// pre-add total, and the per-turn delta is added afterwards).
    usage_accumulators: HashMap<AgentId, Usage>,
    /// The `Sub(n)` index of the sub-agent run currently being
    /// walked, if any. Sub-agent entries are contiguous in append
    /// order (a sub-agent runs fully within one parent tool call),
    /// so a single open run is enough to bracket each sub run with
    /// [`AgentEvent::SubAgentStart`] / [`AgentEvent::SubAgentEnd`].
    open_sub: Option<usize>,
    /// Concatenated text of the most recent `Sub` assistant message
    /// seen during the open run. After the run's last assistant
    /// message this holds the final report carried on the closing
    /// [`AgentEvent::SubAgentEnd`].
    open_sub_report: String,
}

impl ReplayState {
    /// Emit [`AgentEvent::SubAgentStart`] / [`AgentEvent::SubAgentEnd`]
    /// correlation events around a sub-agent's contiguous run, before
    /// the entry's own events are projected.
    ///
    /// Transitions are keyed off `agent_id_for`: leaving an open
    /// `Sub(k)` (to `Main` or a different sub) closes it with the
    /// accumulated report; entering a `Sub(n)` with no run open emits
    /// the start, taking the task from this first entry's user text.
    /// `Meta` entries carry no agent id and never transition.
    fn bracket_subagent(&mut self, entry: &ConversationEntry, out: &mut Vec<AgentEvent>) {
        let Some(current) = agent_id_for(entry) else {
            return;
        };

        if let Some(k) = self.open_sub {
            if current != AgentId::Sub(k) {
                self.close_open_sub(out);
            }
        }

        if let AgentId::Sub(n) = current {
            if self.open_sub.is_none() {
                // The first entry of a sub run is its user message,
                // whose text is the task.
                let task = subagent_task(entry);
                out.push(AgentEvent::SubAgentStart {
                    parent: AgentId::Main,
                    child: AgentId::Sub(n),
                    task,
                });
                self.open_sub = Some(n);
                self.open_sub_report.clear();
            }
        }
    }

    /// Close the currently open sub-agent run, if any, emitting its
    /// [`AgentEvent::SubAgentEnd`] with the accumulated report.
    fn close_open_sub(&mut self, out: &mut Vec<AgentEvent>) {
        if let Some(k) = self.open_sub.take() {
            out.push(AgentEvent::SubAgentEnd {
                parent: AgentId::Main,
                child: AgentId::Sub(k),
                report: std::mem::take(&mut self.open_sub_report),
            });
        }
    }

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
            ConversationEntryKind::Message { message: agent_msg } => {
                let Some(wire) = agent_msg.as_wire() else {
                    return;
                };
                match wire {
                    Message::User(_) => {
                        // User messages: just a MessageStart/End pair
                        // around the wire message.
                        out.push(AgentEvent::MessageStart {
                            agent_id,
                            message: agent_msg.clone(),
                        });
                        out.push(AgentEvent::MessageEnd {
                            agent_id,
                            message: agent_msg.clone(),
                        });
                    }
                    Message::Assistant(a) => {
                        self.project_assistant(agent_id, agent_msg, a, out);
                    }
                    Message::ToolResult(tr) => {
                        self.project_tool_result(agent_id, agent_msg, tr, out);
                    }
                }
            }
        }
    }

    /// Project an assistant-role message into a `MessageStart`
    /// (with an empty placeholder so renderers can open the slot)
    /// followed by a `MessageEnd` carrying the finalized message.
    /// Tracks `tool_call` blocks so the matching tool_result entry
    /// later in the log can synthesize a labeled
    /// `ToolExecutionStart`/`End` pair.
    fn project_assistant(
        &mut self,
        agent_id: AgentId,
        agent_msg: &AgentMessage,
        assistant: &aj_models::types::AssistantMessage,
        out: &mut Vec<AgentEvent>,
    ) {
        // MessageStart carries an empty placeholder (with identity
        // stamped from the finalized message) so renderers open
        // their assistant slot without painting the content twice;
        // MessageEnd is the authoritative finalized snapshot. This
        // mirrors the live-streaming shape where MessageStart fires
        // before any content arrives.
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
            message: agent_msg.clone(),
        });

        // While a sub-agent run is open, record this assistant
        // message's text as the running report; after the run's last
        // assistant message it holds the final report.
        if matches!((self.open_sub, agent_id), (Some(n), AgentId::Sub(m)) if n == m) {
            let mut report = String::new();
            for c in &assistant.content {
                if let AssistantContent::Text(t) = c {
                    report.push_str(&t.text);
                }
            }
            self.open_sub_report = report;
        }

        // Synthesize the matching `TurnUsage`. Live runs emit one
        // per assistant turn on the bus; without this resumed
        // sessions would only paint the footer's context indicator
        // (and any other usage listener) starting from the first
        // post-resume turn, even though every persisted assistant
        // message has its `usage` on disk. Ordering matches the
        // live agent: `TurnUsage.accumulated_*` reflects the total
        // *before* this turn is folded in, then we add the per-turn
        // delta into the accumulator for the next emission.
        let acc = self.usage_accumulators.entry(agent_id).or_default();
        let turn_usage = TokenUsage {
            accumulated_input: acc.input,
            turn_input: assistant.usage.input,
            accumulated_output: acc.output,
            turn_output: assistant.usage.output,
            accumulated_cache_write: acc.cache_write,
            turn_cache_write: assistant.usage.cache_write,
            accumulated_cache_read: acc.cache_read,
            turn_cache_read: assistant.usage.cache_read,
        };
        out.push(AgentEvent::TurnUsage {
            agent_id,
            usage: turn_usage,
        });
        acc.input += assistant.usage.input;
        acc.output += assistant.usage.output;
        acc.cache_write += assistant.usage.cache_write;
        acc.cache_read += assistant.usage.cache_read;

        // Track tool_call blocks so subsequent tool_result entries
        // can synthesize a matching `ToolExecutionStart` (with
        // captured args) and `ToolExecutionEnd`.
        for c in &assistant.content {
            if let aj_models::types::AssistantContent::ToolCall(tc) = c {
                self.tool_calls
                    .insert(tc.id.clone(), (tc.name.clone(), tc.arguments.clone()));
            }
        }
    }

    /// Project a tool_result message into a
    /// `ToolExecutionStart`/`End` pair (so the renderer paints the
    /// tool component) bracketed by a `MessageStart`/`End` pair (so
    /// persistence/event-tape listeners see the same shape live runs
    /// produce). The `ToolDetails` payload is recovered by
    /// deserializing the message's `details` field, falling back to
    /// a text-only synthesis off the wire content when absent or
    /// corrupt.
    fn project_tool_result(
        &self,
        agent_id: AgentId,
        agent_msg: &AgentMessage,
        tr: &aj_models::types::ToolResultMessage,
        out: &mut Vec<AgentEvent>,
    ) {
        // Look up the tool name and input args captured from the
        // preceding assistant message's tool_call block. Missing
        // entries (truncated/legacy logs) fall back to a generic
        // name and empty args; the renderer copes with both.
        let (tool_name, args) = self
            .tool_calls
            .get(&tr.tool_call_id)
            .cloned()
            .unwrap_or_else(|| ("tool".to_string(), Value::Object(Default::default())));

        // The tool result's [`ToolDetails`] payload was serialized
        // onto `tr.details` as a JSON `Value`; deserialize it back.
        // Fall back to a text-only synthesis off the wire content
        // when the field is absent (legacy logs) or corrupt
        // (deserialization fails).
        let result = match tr.details.as_ref() {
            Some(value) => serde_json::from_value::<ToolDetails>(value.clone())
                .unwrap_or_else(|_| text_fallback(&tool_name, &tr.content)),
            None => text_fallback(&tool_name, &tr.content),
        };

        out.push(AgentEvent::ToolExecutionStart {
            agent_id,
            call_id: tr.tool_call_id.clone(),
            tool: tool_name.clone(),
            args,
        });
        // MessageStart/End around the tool_result so a replay-driven
        // pump sees the same shape a live agent emits.
        out.push(AgentEvent::MessageStart {
            agent_id,
            message: agent_msg.clone(),
        });
        out.push(AgentEvent::MessageEnd {
            agent_id,
            message: agent_msg.clone(),
        });
        out.push(AgentEvent::ToolExecutionEnd {
            agent_id,
            call_id: tr.tool_call_id.clone(),
            tool: tool_name,
            result,
            content: std::sync::Arc::from(tr.content.clone().into_boxed_slice()),
            is_error: tr.is_error,
        });
    }
}

/// Build a [`ToolDetails::Text`] off the wire content. The
/// summary is the resolved tool name; the body is the concatenation
/// of every [`UserContent::Text`] block in the result content, with
/// a `[image: <mime>]` placeholder line appended for each
/// [`UserContent::Image`] so replayed entries that lack a persisted
/// structured payload still surface a hint that an image was
/// attached.
fn text_fallback(tool_name: &str, content: &[UserContent]) -> ToolDetails {
    let mut body = String::new();
    for block in content {
        match block {
            UserContent::Text(t) => body.push_str(&t.text),
            UserContent::Image(img) => {
                if !body.is_empty() && !body.ends_with('\n') {
                    body.push('\n');
                }
                body.push_str(&format!("[image: {}]", img.mime_type));
                body.push('\n');
            }
        }
    }
    // Trim a trailing newline introduced solely by an image
    // placeholder; the renderer adds its own separation.
    if body.ends_with('\n') {
        body.pop();
    }
    ToolDetails::Text {
        summary: tool_name.to_string(),
        body,
    }
}

/// Extract a sub-agent's task from its first entry. The first entry
/// of a sub-agent run is its user message, whose concatenated text is
/// the task; any other shape yields an empty task.
fn subagent_task(entry: &ConversationEntry) -> String {
    let ConversationEntryKind::Message { message } = &entry.entry else {
        return String::new();
    };
    let Some(Message::User(u)) = message.as_wire() else {
        return String::new();
    };
    let mut task = String::new();
    for block in &u.content {
        if let UserContent::Text(t) = block {
            task.push_str(&t.text);
        }
    }
    task
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::{ConversationLog, ConversationView};
    use crate::persistence::ConversationPersistence;
    use aj_models::types::{
        AssistantContent, AssistantMessage, TextContent, ThinkingContent, ToolCall,
        ToolResultMessage, UserMessage,
    };
    use serde_json::json;
    use std::path::PathBuf;

    fn fresh_sessions_dir() -> PathBuf {
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

    fn user_msg(text: &str) -> AgentMessage {
        AgentMessage::wire(Message::User(UserMessage::text(text)))
    }

    fn assistant_msg(content: Vec<AssistantContent>) -> AgentMessage {
        AgentMessage::wire(Message::Assistant(AssistantMessage {
            content,
            ..AssistantMessage::empty()
        }))
    }

    fn tool_result_msg(
        id: &str,
        name: &str,
        body: &str,
        details: Option<&ToolDetails>,
    ) -> AgentMessage {
        let mut tr = ToolResultMessage::text(id, name, body, false);
        tr.details = details.and_then(|d| serde_json::to_value(d).ok());
        AgentMessage::wire(Message::ToolResult(tr))
    }

    /// Build a seeded log exercising assistant text, thinking, tool
    /// use, and tool result with structured details.
    fn seeded_log() -> (PathBuf, ConversationLog) {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir.clone());
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("sys".into()).expect("system prompt");
        {
            let mut view = ConversationView::user(&mut log, None);
            view.add_message(user_msg("hi")).expect("user msg");
            view.add_message(assistant_msg(vec![
                AssistantContent::Thinking(ThinkingContent {
                    thinking: "let me think".into(),
                    thinking_signature: None,
                    redacted: false,
                }),
                AssistantContent::Text(TextContent {
                    text: "hello".into(),
                    text_signature: None,
                }),
                AssistantContent::ToolCall(ToolCall {
                    id: "call-1".into(),
                    name: "read_file".into(),
                    arguments: json!({"path": "/tmp/x"}),
                }),
            ]))
            .expect("assistant msg");
            view.add_message(tool_result_msg("call-1", "read_file", "result body", None))
                .expect("tool result msg");
        }
        (dir, log)
    }

    #[test]
    fn replay_projects_assistant_thinking_text_and_tool_results() {
        let (_dir, log) = seeded_log();
        let events: Vec<AgentEvent> = replay(&log).collect();

        // Expected order:
        //   MessageStart(User "hi")
        //   MessageEnd(User "hi")
        //   MessageStart(Assistant empty)
        //   MessageEnd(Assistant {thinking, text, tool_call})
        //   TurnUsage(Main)
        //   ToolExecutionStart { tool: "read_file", call_id: "call-1", args }
        //   MessageStart(ToolResult)
        //   MessageEnd(ToolResult)
        //   ToolExecutionEnd   { tool: "read_file", call_id: "call-1" }
        assert_eq!(events.len(), 9, "got events: {events:#?}");

        match &events[0] {
            AgentEvent::MessageStart { message, .. } => match message.as_wire() {
                Some(Message::User(u)) => match &u.content[0] {
                    UserContent::Text(t) => assert_eq!(t.text, "hi"),
                    other => panic!("expected text, got {other:?}"),
                },
                other => panic!("expected user, got {other:?}"),
            },
            other => panic!("expected user MessageStart, got {other:?}"),
        }

        // Assistant MessageEnd carries the finalized content.
        match &events[3] {
            AgentEvent::MessageEnd { message, .. } => match message.as_wire() {
                Some(Message::Assistant(a)) => {
                    assert_eq!(a.content.len(), 3);
                }
                other => panic!("expected assistant, got {other:?}"),
            },
            other => panic!("expected assistant MessageEnd, got {other:?}"),
        }

        // TurnUsage immediately follows the assistant MessageEnd —
        // same shape and ordering the live agent uses on its bus.
        match &events[4] {
            AgentEvent::TurnUsage { agent_id, .. } => {
                assert_eq!(*agent_id, AgentId::Main);
            }
            other => panic!("expected TurnUsage, got {other:?}"),
        }

        match &events[5] {
            AgentEvent::ToolExecutionStart {
                agent_id,
                call_id,
                tool,
                args,
            } => {
                assert_eq!(*agent_id, AgentId::Main);
                assert_eq!(call_id, "call-1");
                assert_eq!(tool, "read_file");
                assert_eq!(args, &json!({"path": "/tmp/x"}));
            }
            other => panic!("expected tool execution start, got {other:?}"),
        }

        match &events[8] {
            AgentEvent::ToolExecutionEnd {
                call_id,
                tool,
                result,
                is_error,
                ..
            } => {
                assert_eq!(call_id, "call-1");
                assert_eq!(tool, "read_file");
                assert!(!is_error);
                match result {
                    ToolDetails::Text { summary, body } => {
                        assert_eq!(summary, "read_file");
                        assert_eq!(body, "result body");
                    }
                    other => panic!("expected Text fallback, got {other:?}"),
                }
            }
            other => panic!("expected tool execution end, got {other:?}"),
        }
    }

    #[test]
    fn replay_skips_system_prompt_and_handles_empty_log() {
        // A log with only the system-prompt root produces zero
        // events: meta entries are structural framing.
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into())
            .expect("set system prompt");

        let events: Vec<AgentEvent> = replay(&log).collect();
        assert!(events.is_empty(), "got: {events:#?}");
    }

    #[test]
    fn replay_projects_structured_tool_details_on_resume() {
        // When the producer persisted structured `ToolDetails`
        // onto the tool result message, replay deserializes the
        // payload back and surfaces it on the `ToolExecutionEnd`
        // event so resumed sessions render diffs / bash output /
        // todo snapshots / sub-agent reports the same way live
        // runs do.
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("sp");

        let diff_details = ToolDetails::Diff {
            path: "/tmp/x".into(),
            before: "a".into(),
            after: "b".into(),
        };

        {
            let mut view = ConversationView::user(&mut log, None);
            view.add_message(user_msg("edit it")).expect("u");
            view.add_message(assistant_msg(vec![AssistantContent::ToolCall(ToolCall {
                id: "tu-edit".into(),
                name: "edit_file".into(),
                arguments: json!({"path": "/tmp/x"}),
            })]))
            .expect("a");
            view.add_message(tool_result_msg(
                "tu-edit",
                "edit_file",
                "edited",
                Some(&diff_details),
            ))
            .expect("tr");
        }

        let events: Vec<AgentEvent> = replay(&log).collect();
        let end = events
            .iter()
            .find(
                |e| matches!(e, AgentEvent::ToolExecutionEnd { call_id, .. } if call_id == "tu-edit"),
            )
            .expect("ToolExecutionEnd for tu-edit");
        match end {
            AgentEvent::ToolExecutionEnd { result, .. } => match result {
                ToolDetails::Diff {
                    path,
                    before,
                    after,
                } => {
                    assert_eq!(path, "/tmp/x");
                    assert_eq!(before, "a");
                    assert_eq!(after, "b");
                }
                other => panic!("expected Diff details, got {other:?}"),
            },
            _ => unreachable!(),
        }
    }

    #[test]
    fn replay_routes_subagent_entries_to_sub_agent_id() {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("sp");

        let user_head = {
            let mut view = ConversationView::user(&mut log, None);
            view.add_message(user_msg("hi")).expect("u");
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "delegating".into(),
                text_signature: None,
            })]))
            .expect("a");
            view.head().cloned().expect("head present")
        };

        {
            let mut view = ConversationView::subagent(&mut log, user_head, 1);
            view.add_message(user_msg("subtask")).expect("u");
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "reply".into(),
                text_signature: None,
            })]))
            .expect("a");
        }

        let events: Vec<AgentEvent> = replay(&log).collect();
        let any_sub = events
            .iter()
            .any(|e| matches!(e.agent_id(), AgentId::Sub(1)));
        assert!(any_sub, "expected at least one Sub(1) event in {events:#?}");
        let any_main = events.iter().any(|e| matches!(e.agent_id(), AgentId::Main));
        assert!(any_main, "expected at least one Main event in {events:#?}");
    }

    #[test]
    fn replay_brackets_subagent_run_with_start_and_end() {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("sp");

        let user_head = {
            let mut view = ConversationView::user(&mut log, None);
            view.add_message(user_msg("hi")).expect("u");
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "delegating".into(),
                text_signature: None,
            })]))
            .expect("a");
            view.head().cloned().expect("head present")
        };

        {
            let mut view = ConversationView::subagent(&mut log, user_head, 1);
            view.add_message(user_msg("subtask")).expect("u");
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "reply".into(),
                text_signature: None,
            })]))
            .expect("a");
        }

        let events: Vec<AgentEvent> = replay(&log).collect();

        let start_idx = events
            .iter()
            .position(|e| matches!(e, AgentEvent::SubAgentStart { .. }))
            .expect("SubAgentStart present");
        let end_idx = events
            .iter()
            .position(|e| matches!(e, AgentEvent::SubAgentEnd { .. }))
            .expect("SubAgentEnd present");

        match &events[start_idx] {
            AgentEvent::SubAgentStart {
                parent,
                child,
                task,
            } => {
                assert_eq!(*parent, AgentId::Main);
                assert_eq!(*child, AgentId::Sub(1));
                assert_eq!(task, "subtask");
            }
            other => panic!("expected SubAgentStart, got {other:?}"),
        }
        match &events[end_idx] {
            AgentEvent::SubAgentEnd {
                parent,
                child,
                report,
            } => {
                assert_eq!(*parent, AgentId::Main);
                assert_eq!(*child, AgentId::Sub(1));
                assert_eq!(report, "reply");
            }
            other => panic!("expected SubAgentEnd, got {other:?}"),
        }

        let first_sub = events
            .iter()
            .position(|e| matches!(e.agent_id(), AgentId::Sub(1)))
            .expect("at least one Sub(1) event");
        let last_sub = events
            .iter()
            .rposition(|e| matches!(e.agent_id(), AgentId::Sub(1)))
            .expect("at least one Sub(1) event");

        assert!(
            start_idx < first_sub,
            "SubAgentStart must precede the first Sub(1) event"
        );
        assert!(
            end_idx > last_sub,
            "SubAgentEnd must follow the last Sub(1) event"
        );
    }

    #[test]
    fn replay_closes_subagent_before_main_resumes() {
        // A main turn that follows a sub-agent run must close the sub
        // (emit SubAgentEnd) before any of its own events appear. We
        // build the resuming main activity by appending to the user
        // thread head captured before the sub run.
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("p".into()).expect("sp");

        let user_head = {
            let mut view = ConversationView::user(&mut log, None);
            view.add_message(user_msg("hi")).expect("u");
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "delegating".into(),
                text_signature: None,
            })]))
            .expect("a");
            view.head().cloned().expect("head present")
        };

        let sub_head = {
            let mut view = ConversationView::subagent(&mut log, user_head.clone(), 1);
            view.add_message(user_msg("subtask")).expect("u");
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "reply".into(),
                text_signature: None,
            })]))
            .expect("a");
            view.head().cloned().expect("sub head present")
        };

        // Resume main activity after the sub run.
        {
            let mut view = ConversationView::user(&mut log, Some(sub_head));
            view.add_message(assistant_msg(vec![AssistantContent::Text(TextContent {
                text: "back on main".into(),
                text_signature: None,
            })]))
            .expect("a");
        }

        let events: Vec<AgentEvent> = replay(&log).collect();

        let end_idx = events
            .iter()
            .position(|e| matches!(e, AgentEvent::SubAgentEnd { .. }))
            .expect("SubAgentEnd present");
        let last_sub = events
            .iter()
            .rposition(|e| matches!(e.agent_id(), AgentId::Sub(1)))
            .expect("Sub(1) event present");
        // First Main event after the last Sub(1) event marks main
        // resuming. Skip the correlation events, whose `agent_id()`
        // reports the parent (Main).
        let main_resumes = events
            .iter()
            .enumerate()
            .skip(last_sub + 1)
            .find(|(_, e)| {
                matches!(e.agent_id(), AgentId::Main)
                    && !matches!(
                        e,
                        AgentEvent::SubAgentStart { .. } | AgentEvent::SubAgentEnd { .. }
                    )
            })
            .map(|(i, _)| i)
            .expect("Main resumes after sub run");

        assert!(end_idx > last_sub, "SubAgentEnd follows last Sub(1) event");
        assert!(
            end_idx < main_resumes,
            "SubAgentEnd must close the sub before main resumes"
        );
    }

    #[test]
    fn replay_falls_back_when_tool_call_id_is_not_tracked() {
        // A truncated/legacy log can carry a tool_result whose
        // tool_call_id was never seen on a preceding assistant
        // message. Replay still emits a sensible event with the
        // fallback "tool" name and an empty args object.
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        {
            let mut view = ConversationView::user(&mut log, None);
            // Insert the tool_result with no preceding tool_call.
            view.add_message(tool_result_msg("orphan", "", "body", None))
                .expect("orphan tr");
        }

        let events: Vec<AgentEvent> = replay(&log).collect();
        // ToolExecutionStart, MessageStart, MessageEnd, ToolExecutionEnd.
        assert_eq!(events.len(), 4, "got: {events:#?}");
        match &events[0] {
            AgentEvent::ToolExecutionStart { tool, args, .. } => {
                assert_eq!(tool, "tool", "fallback tool name");
                assert_eq!(args, &Value::Object(Default::default()));
            }
            other => panic!("expected tool execution start, got {other:?}"),
        }
        match &events[3] {
            AgentEvent::ToolExecutionEnd { tool, .. } => {
                assert_eq!(tool, "tool", "fallback tool name");
            }
            other => panic!("expected tool execution end, got {other:?}"),
        }
    }

    /// Build an assistant message whose persisted `usage` carries
    /// the supplied per-turn token counts. The other identity
    /// fields are left at their defaults — the replay path only
    /// reads `content` and `usage`.
    fn assistant_msg_with_usage(
        content: Vec<AssistantContent>,
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
    ) -> AgentMessage {
        AgentMessage::wire(Message::Assistant(AssistantMessage {
            content,
            usage: aj_models::types::Usage {
                input,
                output,
                cache_read,
                cache_write,
                ..aj_models::types::Usage::default()
            },
            ..AssistantMessage::empty()
        }))
    }

    /// Two persisted main-agent assistant turns produce two
    /// synthesized `TurnUsage` events. The first carries its
    /// per-turn deltas against a zero accumulator; the second
    /// carries its deltas against an accumulator equal to the
    /// first turn's contribution (live-agent ordering: emit
    /// before adding into the accumulator).
    #[test]
    fn replay_synthesizes_turn_usage_per_assistant_message() {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");
        {
            let mut view = ConversationView::user(&mut log, None);
            view.add_message(assistant_msg_with_usage(
                vec![AssistantContent::Text(TextContent {
                    text: "first".into(),
                    text_signature: None,
                })],
                100,
                50,
                20,
                5,
            ))
            .expect("turn 1");
            view.add_message(assistant_msg_with_usage(
                vec![AssistantContent::Text(TextContent {
                    text: "second".into(),
                    text_signature: None,
                })],
                200,
                70,
                30,
                10,
            ))
            .expect("turn 2");
        }

        let events: Vec<AgentEvent> = replay(&log).collect();
        let turn_usages: Vec<&aj_agent::types::TokenUsage> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::TurnUsage {
                    agent_id: AgentId::Main,
                    usage,
                } => Some(usage),
                _ => None,
            })
            .collect();
        assert_eq!(turn_usages.len(), 2, "one TurnUsage per assistant message");

        let first = turn_usages[0];
        assert_eq!(first.turn_input, 100);
        assert_eq!(first.turn_output, 50);
        assert_eq!(first.turn_cache_read, 20);
        assert_eq!(first.turn_cache_write, 5);
        assert_eq!(first.accumulated_input, 0, "pre-add accumulator");
        assert_eq!(first.accumulated_output, 0);
        assert_eq!(first.accumulated_cache_read, 0);
        assert_eq!(first.accumulated_cache_write, 0);

        let second = turn_usages[1];
        assert_eq!(second.turn_input, 200);
        assert_eq!(second.turn_output, 70);
        assert_eq!(second.turn_cache_read, 30);
        assert_eq!(second.turn_cache_write, 10);
        // After the first turn was emitted the accumulator
        // absorbed the first turn's deltas; the second TurnUsage
        // sees those as its `accumulated_*`.
        assert_eq!(second.accumulated_input, 100);
        assert_eq!(second.accumulated_output, 50);
        assert_eq!(second.accumulated_cache_read, 20);
        assert_eq!(second.accumulated_cache_write, 5);
    }

    /// Main and sub-agent assistants keep independent
    /// accumulators. A main-agent turn that follows a sub-agent
    /// turn must not inherit the sub-agent's totals (and vice
    /// versa).
    #[test]
    fn replay_keeps_main_and_subagent_usage_accumulators_separate() {
        let dir = fresh_sessions_dir();
        let persistence = ConversationPersistence::new(dir);
        let mut log = ConversationLog::create(&persistence).expect("create log");

        let user_head = {
            let mut view = ConversationView::user(&mut log, None);
            view.add_message(user_msg("hi")).expect("u");
            view.add_message(assistant_msg_with_usage(
                vec![AssistantContent::Text(TextContent {
                    text: "main".into(),
                    text_signature: None,
                })],
                10,
                5,
                0,
                0,
            ))
            .expect("main turn");
            view.head().cloned().expect("head present")
        };

        {
            let mut view = ConversationView::subagent(&mut log, user_head, 1);
            view.add_message(user_msg("subtask")).expect("u");
            view.add_message(assistant_msg_with_usage(
                vec![AssistantContent::Text(TextContent {
                    text: "sub".into(),
                    text_signature: None,
                })],
                40,
                20,
                0,
                0,
            ))
            .expect("sub turn");
        }

        let events: Vec<AgentEvent> = replay(&log).collect();
        let main_turn = events
            .iter()
            .find_map(|e| match e {
                AgentEvent::TurnUsage {
                    agent_id: AgentId::Main,
                    usage,
                } => Some(usage),
                _ => None,
            })
            .expect("main TurnUsage present");
        let sub_turn = events
            .iter()
            .find_map(|e| match e {
                AgentEvent::TurnUsage {
                    agent_id: AgentId::Sub(1),
                    usage,
                } => Some(usage),
                _ => None,
            })
            .expect("sub(1) TurnUsage present");

        // Each agent's first turn has a zero accumulator — they
        // don't share state.
        assert_eq!(main_turn.accumulated_input, 0);
        assert_eq!(main_turn.turn_input, 10);
        assert_eq!(sub_turn.accumulated_input, 0);
        assert_eq!(sub_turn.turn_input, 40);
    }
}
