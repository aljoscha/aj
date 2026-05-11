//! Event pump — translates each [`AgentEvent`] into a layout
//! mutation.
//!
//! The interactive mode subscribes to the agent's bus through
//! [`aj_agent::Agent::subscribe_channel`] and pulls events off the
//! receiver in its `tokio::select!` loop. For each event the
//! [`EventPump`] looks up (or creates) the matching component in
//! the chat / status slots and forwards the update. Sub-agent
//! events ride on the same pump (per `docs/aj-next-plan.md` §1.6
//! sub-agents share the parent's bus) — for now they render
//! identically to the main agent, with their `agent_id` surfaced
//! through the existing chat container; richer sub-agent grouping
//! lands alongside the `Ctrl+O` expand affordance in a follow-up.
//!
//! See `docs/aj-next-plan.md` §1.1 (event protocol) and §4
//! (event-pump shape).

use std::collections::HashMap;

use aj_agent::events::{AgentEvent, AgentId, PersistedMessageKind, StreamAction, StreamChannel};
use aj_agent::types::TokenUsage;
use aj_models::messages::ContentBlockParam;
use aj_tui::components::editor::Editor;
use aj_tui::components::spacer::Spacer;
use aj_tui::components::text::Text;
use aj_tui::container::Container;
use aj_tui::tui::Tui;

use crate::config::theme::ChatTheme;
use crate::modes::interactive::components::assistant_message::AssistantMessageComponent;
use crate::modes::interactive::components::loader_status::LoaderStatus;
use crate::modes::interactive::components::tool_execution::ToolExecutionComponent;
use crate::modes::interactive::components::user_message::UserMessageComponent;
use crate::modes::interactive::layout::SlotIndex;

/// Translates [`AgentEvent`]s into TUI mutations.
///
/// The pump owns no view state of its own — every component lives
/// inside the `Tui`'s slot tree. It only tracks the small amount of
/// per-turn metadata needed to route streaming events to the
/// right place: the index of the in-flight assistant message
/// component (so `StreamChunk` updates land on the right widget)
/// and the index map for tool-call ids → chat-container index.
pub struct EventPump {
    theme: ChatTheme,
    /// Index, inside the chat container, of the current
    /// in-flight assistant message component. `None` between
    /// turns; set when the first assistant chunk arrives, cleared
    /// when the assistant message persists or the turn ends.
    current_assistant: Option<usize>,
    /// Map of `tool_use_id` → index inside the chat container of
    /// the matching [`ToolExecutionComponent`]. Indices stay
    /// stable for the lifetime of the chat session because the
    /// container only ever appends.
    tool_index: HashMap<String, usize>,
}

impl EventPump {
    /// Build a fresh pump bound to the supplied [`ChatTheme`]
    /// (used when constructing assistant / user message
    /// components on the fly).
    pub fn new(theme: ChatTheme) -> Self {
        Self {
            theme,
            current_assistant: None,
            tool_index: HashMap::new(),
        }
    }

    /// Dispatch one [`AgentEvent`] onto `tui`'s slot tree. Returns
    /// nothing — every effect is a side effect on the layout.
    /// Callers that want a render afterwards should call
    /// [`Tui::request_render`] (the pump itself does so for the
    /// events that mutate visible state).
    pub fn handle(&mut self, tui: &mut Tui, event: &AgentEvent) {
        match event {
            // ---- Lifecycle: start / stop the working spinner. ----
            AgentEvent::AgentStart { .. } => {
                self.with_loader(tui, |l| l.start());
            }
            AgentEvent::AgentEnd { .. } => {
                self.with_loader(tui, |l| l.stop());
                self.current_assistant = None;
                self.tool_index.clear();
            }
            AgentEvent::TurnStart { .. } => {
                // Each new turn starts with a fresh assistant
                // message component; the previous turn's component
                // (if any) was already finalized at `MessageEnd` /
                // `MessagePersisted::Assistant`.
                self.current_assistant = None;
            }

            // ---- Streaming: assistant text and thinking. ----
            AgentEvent::StreamChunk {
                channel, action, ..
            } => self.handle_stream_chunk(tui, *channel, action),

            // ---- Persisted messages: user and assistant turn finals. ----
            AgentEvent::MessagePersisted { kind, .. } => match kind {
                PersistedMessageKind::User { content } => {
                    self.append_user_message(tui, content);
                }
                PersistedMessageKind::Assistant { .. } => {
                    // Streaming already painted the assistant
                    // content; finalising just unbinds the
                    // streaming target so the next turn starts a
                    // fresh component.
                    self.current_assistant = None;
                }
                PersistedMessageKind::ToolResult { .. }
                | PersistedMessageKind::UserOutput { .. } => {
                    // Tool-result content is rendered through
                    // `ToolExecutionEnd` (which carries the
                    // structured `ToolDetails`); the persisted
                    // record is the wire-side projection. Free-
                    // standing `UserOutput::ToolError` records
                    // similarly already came through as a
                    // `ToolExecutionEnd { is_error: true }`. No
                    // visible work here.
                }
            },

            // ---- Tool execution: header + result. ----
            AgentEvent::ToolExecutionStart {
                call_id,
                tool,
                args,
                ..
            } => self.append_tool_execution(tui, call_id, tool, args),
            AgentEvent::ToolExecutionUpdate {
                call_id, partial, ..
            } => {
                self.update_tool_execution_partial(tui, call_id, partial);
            }
            AgentEvent::ToolExecutionEnd {
                call_id,
                tool,
                result,
                is_error,
                ..
            } => {
                self.update_tool_execution_result(tui, call_id, tool, result, *is_error);
            }

            // ---- Notices / warnings / errors. ----
            AgentEvent::Notice { text, .. } => {
                self.append_notice(tui, text);
            }
            AgentEvent::Warning { text, .. } => {
                self.append_styled_notice(tui, text, aj_tui::style::yellow);
            }
            AgentEvent::Error { text, .. } => {
                self.append_styled_notice(tui, text, aj_tui::style::red);
            }
            AgentEvent::StreamRetry {
                attempt,
                delay,
                error,
                ..
            } => {
                let msg = format!(
                    "Retrying inference (attempt {attempt}, in {}ms): {error}",
                    delay.as_millis()
                );
                self.append_styled_notice(tui, &msg, aj_tui::style::yellow);
            }

            // ---- Per-turn token usage. ----
            AgentEvent::TurnUsage { agent_id, usage } => {
                self.append_turn_usage(tui, *agent_id, usage);
            }

            // ---- Placeholders: events whose UI work isn't yet wired. ----
            AgentEvent::SubAgentStart { .. }
            | AgentEvent::SubAgentEnd { .. }
            | AgentEvent::TurnEnd { .. }
            | AgentEvent::QueueUpdate { .. }
            | AgentEvent::MessageStart { .. }
            | AgentEvent::MessageUpdate { .. }
            | AgentEvent::MessageEnd { .. } => {
                // Sub-agent grouping, queue indicators, and the
                // unified message-event variants (which the agent
                // doesn't emit yet) all land in follow-up commits.
                // Holding the arms here keeps the exhaustiveness
                // check active so a newly-emitted event variant
                // shows up as a compile error.
            }
        }

        tui.request_render();
    }

    // ---- Helpers ---------------------------------------------------------

    /// Mutate the [`LoaderStatus`] component in the status slot.
    /// Centralised so callers don't repeat the slot/container
    /// nesting; cheap because it's a couple of downcasts.
    fn with_loader<F: FnOnce(&mut LoaderStatus)>(&self, tui: &mut Tui, f: F) {
        let Some(status) = tui.get_mut_as::<Container>(SlotIndex::Status.idx()) else {
            return;
        };
        let Some(loader) = status.get_mut_as::<LoaderStatus>(0) else {
            return;
        };
        f(loader);
    }

    /// Append a [`UserMessageComponent`] for `content` to the
    /// chat slot. Walks the wire content blocks and concatenates
    /// every textual block into one rendered message — sub-second
    /// latency is more important than perfect block separation
    /// for live user prompts.
    fn append_user_message(&self, tui: &mut Tui, content: &[ContentBlockParam]) {
        let text = content
            .iter()
            .filter_map(|b| match b {
                ContentBlockParam::TextBlock { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        if text.is_empty() {
            return;
        }
        let component = UserMessageComponent::new(&text, &self.theme);
        self.push_chat_child(tui, Box::new(component));
    }

    /// Route a `StreamChunk` to the in-flight assistant message
    /// component, creating one if necessary.
    fn handle_stream_chunk(
        &mut self,
        tui: &mut Tui,
        channel: StreamChannel,
        action: &StreamAction,
    ) {
        match channel {
            StreamChannel::Text | StreamChannel::Thinking => {
                let idx = self.ensure_assistant_message(tui);
                let Some(chat) = tui.get_mut_as::<Container>(SlotIndex::Chat.idx()) else {
                    return;
                };
                let Some(c) = chat.get_mut_as::<AssistantMessageComponent>(idx) else {
                    return;
                };
                match (channel, action) {
                    (StreamChannel::Text, StreamAction::Start { snapshot })
                    | (StreamChannel::Text, StreamAction::Stop { snapshot }) => {
                        c.set_text_snapshot(snapshot.clone());
                    }
                    (StreamChannel::Text, StreamAction::Update { delta }) => {
                        c.append_text_delta(delta);
                    }
                    (StreamChannel::Thinking, StreamAction::Start { snapshot })
                    | (StreamChannel::Thinking, StreamAction::Stop { snapshot }) => {
                        c.set_thinking_snapshot(snapshot.clone());
                    }
                    (StreamChannel::Thinking, StreamAction::Update { delta }) => {
                        c.append_thinking_delta(delta);
                    }
                    (StreamChannel::User, _) => unreachable!("guarded above"),
                }
            }
            StreamChannel::User => {
                // Replay path: a persisted user-thread message was
                // surfaced as Start/Update/Stop. We only consume the
                // `Stop` because that's the variant carrying the full
                // snapshot; intermediate updates would just be partial
                // copies of the same text.
                if let StreamAction::Stop { snapshot } = action {
                    let component = UserMessageComponent::new(snapshot, &self.theme);
                    self.push_chat_child(tui, Box::new(component));
                }
            }
        }
    }

    /// Ensure the chat slot's tail child is an
    /// [`AssistantMessageComponent`]. Returns its container index.
    /// Creates a new component (and remembers its index) if the
    /// current turn doesn't have one yet.
    fn ensure_assistant_message(&mut self, tui: &mut Tui) -> usize {
        if let Some(idx) = self.current_assistant {
            return idx;
        }
        let component = AssistantMessageComponent::new(self.theme.markdown.clone());
        let idx = self.push_chat_child(tui, Box::new(component));
        self.current_assistant = Some(idx);
        idx
    }

    /// Append a tool-execution component for a freshly-started
    /// tool call. Records the chat-container index in
    /// `tool_index` so subsequent `ToolExecutionUpdate` /
    /// `ToolExecutionEnd` events can find it.
    fn append_tool_execution(
        &mut self,
        tui: &mut Tui,
        call_id: &str,
        tool: &str,
        args: &serde_json::Value,
    ) {
        let component = ToolExecutionComponent::new(tool.to_string(), args, &self.theme);
        let idx = self.push_chat_child(tui, Box::new(component));
        self.tool_index.insert(call_id.to_string(), idx);
        // A tool call that arrives mid-turn means the assistant
        // message that emitted it is finished as far as the
        // stream is concerned. Drop the streaming target so the
        // next assistant turn opens a fresh component.
        self.current_assistant = None;
    }

    /// Update an in-flight tool's body with a partial snapshot.
    fn update_tool_execution_partial(
        &self,
        tui: &mut Tui,
        call_id: &str,
        partial: &aj_agent::tool::ToolDetails,
    ) {
        let Some(&idx) = self.tool_index.get(call_id) else {
            return;
        };
        let Some(chat) = tui.get_mut_as::<Container>(SlotIndex::Chat.idx()) else {
            return;
        };
        let Some(c) = chat.get_mut_as::<ToolExecutionComponent>(idx) else {
            return;
        };
        c.update_partial(partial);
    }

    /// Finalize a tool execution with its result.
    fn update_tool_execution_result(
        &mut self,
        tui: &mut Tui,
        call_id: &str,
        tool: &str,
        result: &aj_agent::tool::ToolDetails,
        is_error: bool,
    ) {
        // If we never saw `ToolExecutionStart` (replay path), build
        // a component now so the result is visible. Args aren't
        // available on the End event, so we render with an empty
        // object.
        let idx = match self.tool_index.get(call_id) {
            Some(idx) => *idx,
            None => {
                let component = ToolExecutionComponent::new(
                    tool.to_string(),
                    &serde_json::json!({}),
                    &self.theme,
                );
                let idx = self.push_chat_child(tui, Box::new(component));
                self.tool_index.insert(call_id.to_string(), idx);
                idx
            }
        };
        let Some(chat) = tui.get_mut_as::<Container>(SlotIndex::Chat.idx()) else {
            return;
        };
        let Some(c) = chat.get_mut_as::<ToolExecutionComponent>(idx) else {
            return;
        };
        c.update_result(result, is_error);
    }

    /// Append a plain dim-styled notice line. The auto-spacer
    /// inserted by [`Self::push_chat_child`] handles separation
    /// from neighbouring chat elements, so the text component
    /// itself uses `padding_y = 0` to keep the row compact.
    fn append_notice(&self, tui: &mut Tui, text: &str) {
        let styled = aj_tui::style::dim(text);
        self.push_chat_child(tui, Box::new(Text::new(&styled, 1, 0)));
    }

    /// Append a styled notice using the supplied colour function
    /// (yellow for warnings, red for errors). Mirrors
    /// [`Self::append_notice`]'s zero internal padding; the
    /// surrounding auto-spacer provides the visible gap.
    fn append_styled_notice(&self, tui: &mut Tui, text: &str, style: fn(&str) -> String) {
        let styled = style(text);
        self.push_chat_child(tui, Box::new(Text::new(&styled, 1, 0)));
    }

    /// Append a dim `Token Usage - ...` row for a freshly-completed
    /// turn. Sub-agents get a leading `(sub agent N)` tag so their
    /// per-turn counts stay distinguishable when they share the
    /// parent's scrollback (per `docs/aj-next-plan.md` §1.6
    /// sub-agents share the parent's bus). The format matches the
    /// legacy `display_token_usage` line byte-for-byte (modulo the
    /// ANSI dim escape sequence) so users with eyes trained on the
    /// old format don't have to re-learn the layout.
    fn append_turn_usage(&self, tui: &mut Tui, agent_id: AgentId, usage: &TokenUsage) {
        let line = format_turn_usage_line(agent_id, usage);
        let styled = aj_tui::style::dim(&line);
        self.push_chat_child(tui, Box::new(Text::new(&styled, 1, 0)));
    }

    /// Append `child` to the chat container slot and return its
    /// index. Centralises the slot lookup so callers don't have to
    /// know about [`SlotIndex::Chat`].
    ///
    /// When the chat container already has at least one child this
    /// helper inserts a [`Spacer`] of one blank row immediately
    /// before `child`. Each chat-scrollback component (user
    /// messages, assistant messages, tool executions, notices)
    /// can therefore stay focused on its own internal layout — the
    /// vertical breathing room between siblings is owned by the
    /// container side.
    ///
    /// The returned index is the *child's* slot in the container,
    /// not the spacer's. Callers that key follow-up lookups by
    /// this index ([`Self::ensure_assistant_message`],
    /// [`Self::append_tool_execution`]) continue to find the right
    /// component after the spacer is inserted.
    fn push_chat_child(
        &self,
        tui: &mut Tui,
        child: Box<dyn aj_tui::component::Component>,
    ) -> usize {
        let Some(chat) = tui.get_mut_as::<Container>(SlotIndex::Chat.idx()) else {
            // The chat slot must exist for the lifetime of the TUI;
            // if it doesn't, the layout was torn down and there's
            // nothing useful to do but drop the child.
            return 0;
        };
        if !chat.is_empty() {
            chat.add_child(Box::new(Spacer::new(1)));
        }
        let idx = chat.len();
        chat.add_child(child);
        idx
    }
}

/// Render the `Token Usage - ...` line for a single `TurnUsage`
/// event. Sub-agents are tagged with their `(sub agent N)` prefix
/// so their per-turn counts stand apart from the main agent's in
/// the shared scrollback. Visible for testing.
fn format_turn_usage_line(agent_id: AgentId, usage: &TokenUsage) -> String {
    // `format_tokens` keeps the legacy convention: render the
    // accumulated total bare when the turn contributed nothing
    // (e.g. a cached read of an existing tool result), or
    // `acc+turn` so the per-turn delta is visible at a glance.
    let format_tokens = |acc: u64, turn: u64| -> String {
        if turn == 0 {
            format!("{acc}")
        } else {
            format!("{acc}+{turn}")
        }
    };
    let input_str = format_tokens(usage.accumulated_input, usage.turn_input);
    let output_str = format_tokens(usage.accumulated_output, usage.turn_output);
    let cache_creation_str =
        format_tokens(usage.accumulated_cache_creation, usage.turn_cache_creation);
    let cache_read_str = format_tokens(usage.accumulated_cache_read, usage.turn_cache_read);
    let body = format!(
        "Token Usage - Input: {input_str} | Output: {output_str} | Cache Creation: {cache_creation_str} | Cache Read: {cache_read_str}",
    );
    match agent_id {
        AgentId::Main => body,
        AgentId::Sub(n) => format!("(sub agent {n}) {body}"),
    }
}

/// Pull and clear the editor's submitted text. Returns `Some` at
/// most once per editor submission; the host's main loop calls
/// this after every input event so a freshly-submitted prompt
/// doesn't sit in the editor's buffer waiting for the next event.
pub fn take_submitted_prompt(tui: &mut Tui) -> Option<String> {
    let editor = tui.get_mut_as::<Editor>(SlotIndex::Editor.idx())?;
    editor.take_submitted()
}

/// Toggle the editor's `disable_submit` flag. Used by the main
/// loop to gate a second submission while the agent is still
/// running the previous prompt.
pub fn set_editor_submit_enabled(tui: &mut Tui, enabled: bool) {
    if let Some(editor) = tui.get_mut_as::<Editor>(SlotIndex::Editor.idx()) {
        editor.disable_submit = !enabled;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use aj_tui::component::Component;
    use aj_tui::terminal::ProcessTerminal;

    use crate::config::theme::{ChatTheme, ThemeHandle, chat_theme};
    use crate::modes::interactive::layout::build_layout;

    /// Build a TokenUsage with the supplied turn deltas and the
    /// accumulated totals set to `turn + already`, so the resulting
    /// snapshot looks like an agent that ran one prior turn worth
    /// `already` plus this turn's contribution.
    fn token_usage(turn: [u64; 4], already: [u64; 4]) -> TokenUsage {
        TokenUsage {
            accumulated_input: already[0] + turn[0],
            turn_input: turn[0],
            accumulated_output: already[1] + turn[1],
            turn_output: turn[1],
            accumulated_cache_creation: already[2] + turn[2],
            turn_cache_creation: turn[2],
            accumulated_cache_read: already[3] + turn[3],
            turn_cache_read: turn[3],
        }
    }

    #[test]
    fn format_turn_usage_line_emits_acc_plus_turn_for_main_agent() {
        // First turn: accumulated == turn. Each `acc+turn` field
        // should print `acc+turn` since the turn delta is nonzero.
        let usage = token_usage([100, 50, 30, 5], [0, 0, 0, 0]);
        let line = format_turn_usage_line(AgentId::Main, &usage);
        assert_eq!(
            line,
            "Token Usage - Input: 100+100 | Output: 50+50 | Cache Creation: 30+30 | Cache Read: 5+5",
        );
    }

    #[test]
    fn format_turn_usage_line_drops_turn_part_when_turn_is_zero() {
        // The legacy renderer hides the `+turn` suffix when the
        // turn contributed nothing — a cached read of an existing
        // tool result, for example. Pin that behaviour so we don't
        // start showing `+0` rows for routine cache hits.
        let usage = token_usage([0, 0, 0, 0], [200, 80, 0, 14]);
        let line = format_turn_usage_line(AgentId::Main, &usage);
        assert_eq!(
            line,
            "Token Usage - Input: 200 | Output: 80 | Cache Creation: 0 | Cache Read: 14",
        );
    }

    #[test]
    fn format_turn_usage_line_prefixes_sub_agent_id() {
        // Sub-agents share the parent's bus, so their per-turn
        // usage line is tagged with `(sub agent N)` to keep the
        // rows distinguishable in the shared scrollback.
        let usage = token_usage([10, 5, 1, 0], [0, 0, 0, 0]);
        let line = format_turn_usage_line(AgentId::Sub(2), &usage);
        assert_eq!(
            line,
            "(sub agent 2) Token Usage - Input: 10+10 | Output: 5+5 | Cache Creation: 1+1 | Cache Read: 0",
        );
    }

    /// Build a fresh `Tui` + layout pair for event-pump tests.
    /// Returns the populated `Tui` and a paired `EventPump` so the
    /// caller can dispatch events and inspect the chat container's
    /// contents afterwards.
    fn fresh_tui_with_layout() -> (aj_tui::tui::Tui, EventPump, ChatTheme) {
        let mut tui = aj_tui::tui::Tui::new(Box::new(ProcessTerminal::new()));
        let theme = ThemeHandle::new(crate::config::theme::Theme::bundled_dark());
        build_layout(&mut tui, &theme);
        let chat = chat_theme(&theme);
        let pump = EventPump::new(chat.clone());
        (tui, pump, chat)
    }

    #[test]
    fn turn_usage_event_appends_one_chat_row() {
        // End-to-end: dispatch a `TurnUsage` event and verify a
        // new child landed in the chat container with the
        // formatted line inside it (the dim escape wraps the
        // visible body verbatim, so `.contains` is enough).
        let (mut tui, mut pump, _theme) = fresh_tui_with_layout();
        let chat_len_before = {
            let chat = tui
                .get_mut_as::<aj_tui::container::Container>(SlotIndex::Chat.idx())
                .expect("chat slot");
            chat.len()
        };
        let usage = token_usage([42, 17, 0, 3], [0, 0, 0, 0]);
        pump.handle(
            &mut tui,
            &AgentEvent::TurnUsage {
                agent_id: AgentId::Main,
                usage,
            },
        );
        let chat = tui
            .get_mut_as::<aj_tui::container::Container>(SlotIndex::Chat.idx())
            .expect("chat slot");
        assert_eq!(
            chat.len(),
            chat_len_before + 1,
            "TurnUsage should append exactly one chat row",
        );
        let row = chat
            .get_mut_as::<aj_tui::components::text::Text>(chat_len_before)
            .expect("the appended row should be a Text component");
        // `render` walks the styled string; the dim escape sequence
        // brackets the body, so a `contains` check on the visible
        // payload is robust to trailing/leading ANSI bytes.
        let lines = row.render(120);
        let joined = lines.join("\n");
        assert!(
            joined.contains(
                "Token Usage - Input: 42+42 | Output: 17+17 | Cache Creation: 0 | Cache Read: 3+3"
            ),
            "row should carry the formatted usage line, got: {joined:?}",
        );
        assert!(
            joined.contains("\x1b[2m"),
            "row should be wrapped in the dim ANSI escape",
        );
    }
}
