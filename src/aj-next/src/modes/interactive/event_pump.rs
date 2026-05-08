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

use aj_agent::events::{AgentEvent, PersistedMessageKind, StreamAction, StreamChannel};
use aj_models::messages::ContentBlockParam;
use aj_tui::components::editor::Editor;
use aj_tui::components::markdown::MarkdownTheme;
use aj_tui::components::text::Text;
use aj_tui::container::Container;
use aj_tui::tui::Tui;

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
    theme: MarkdownTheme,
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
    /// Build a fresh pump bound to the supplied markdown theme
    /// (used when constructing assistant / user message
    /// components on the fly).
    pub fn new(theme: MarkdownTheme) -> Self {
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

            // ---- Placeholders: events whose UI work isn't yet wired. ----
            AgentEvent::SubAgentStart { .. }
            | AgentEvent::SubAgentEnd { .. }
            | AgentEvent::TurnEnd { .. }
            | AgentEvent::TurnUsage { .. }
            | AgentEvent::QueueUpdate { .. }
            | AgentEvent::MessageStart { .. }
            | AgentEvent::MessageUpdate { .. }
            | AgentEvent::MessageEnd { .. } => {
                // Sub-agent grouping, per-turn usage display, queue
                // indicators, and the unified message-event variants
                // (which the agent doesn't emit yet) all land in
                // follow-up commits. Holding the arms here keeps the
                // exhaustiveness check active so a newly-emitted
                // event variant shows up as a compile error.
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
        let component = UserMessageComponent::new(&text, self.theme.clone());
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
                    let component = UserMessageComponent::new(snapshot, self.theme.clone());
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
        let component = AssistantMessageComponent::new(self.theme.clone());
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
        let component = ToolExecutionComponent::new(tool.to_string(), args);
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
                let component =
                    ToolExecutionComponent::new(tool.to_string(), &serde_json::json!({}));
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

    /// Append a plain dim-styled notice line.
    fn append_notice(&self, tui: &mut Tui, text: &str) {
        let styled = aj_tui::style::dim(text);
        self.push_chat_child(tui, Box::new(Text::new(&styled, 1, 0)));
    }

    /// Append a styled notice using the supplied colour function
    /// (yellow for warnings, red for errors).
    fn append_styled_notice(&self, tui: &mut Tui, text: &str, style: fn(&str) -> String) {
        let styled = style(text);
        self.push_chat_child(tui, Box::new(Text::new(&styled, 1, 0)));
    }

    /// Append `child` to the chat container slot and return its
    /// index. Centralises the slot lookup so callers don't have to
    /// know about [`SlotIndex::Chat`].
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
        let idx = chat.len();
        chat.add_child(child);
        idx
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
