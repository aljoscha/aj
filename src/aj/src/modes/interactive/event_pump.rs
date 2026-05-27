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

use std::collections::{HashMap, HashSet};

use aj_agent::events::{AgentEvent, AgentId};
use aj_agent::message::{AgentMessage, AgentMessageKind};
use aj_agent::types::TokenUsage;
use aj_models::streaming::AssistantMessageEvent;
use aj_models::types::{AssistantContent, Message, UserContent};
use aj_tui::components::editor::Editor;
use aj_tui::components::spacer::Spacer;
use aj_tui::components::text::Text;
use aj_tui::container::Container;
use aj_tui::tui::Tui;

use crate::config::theme::ChatTheme;
use crate::modes::interactive::components::assistant_message::{
    AssistantMessageComponent, BlockKind,
};
use crate::modes::interactive::components::footer::Footer;
use crate::modes::interactive::components::loader_status::LoaderStatus;
use crate::modes::interactive::components::tool_execution::ToolExecutionComponent;
use crate::modes::interactive::components::user_message::UserMessageComponent;
use crate::modes::interactive::footer_data::FooterData;
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
    /// Identifiers of agents that have emitted [`AgentEvent::AgentStart`]
    /// without a matching [`AgentEvent::AgentEnd`] yet.
    ///
    /// Sub-agents share the parent's bus and emit their own
    /// `AgentStart` / `AgentEnd` pair, so a single main turn that
    /// spawns sub-agents produces nested events on this listener.
    /// The set is the refcount that decides when the working
    /// spinner runs: the loader starts on the 0→1 transition and
    /// stops on the 1→0 transition, so the spinner is visible for
    /// the entire span between the *first* `AgentStart` and the
    /// *last* `AgentEnd`, regardless of how the events nest.
    ///
    /// Main's `AgentEnd` is additionally treated as authoritative:
    /// when it fires, the set is drained and the loader stops even
    /// if a sub-agent's `AgentEnd` was lost. This is sound because
    /// `Agent::run_top_level_turn` only emits Main's `AgentEnd`
    /// after every spawned sub-agent's future has been awaited (or
    /// dropped); anything still in the set at that point is a
    /// stale entry, not a sub-agent that's actually still running,
    /// and waiting for its `AgentEnd` would pin the spinner
    /// (and the render loop) forever on an idle session.
    running_agents: HashSet<AgentId>,
    /// Whether new and existing assistant-message components
    /// should render thinking blocks as a single italic
    /// `Thinking…` placeholder line instead of the full expanded
    /// markdown widget. Toggled at runtime by
    /// [`Self::set_hide_thinking_block`]; see
    /// `docs/aj-next-plan.md` §4.4.
    hide_thinking_block: bool,
    /// Whether new and existing tool-execution components should
    /// render their bodies fully (`true`) or in the compact head/
    /// tail-truncated form (`false`). Toggled at runtime by
    /// [`Self::set_tools_expanded`] in response to the
    /// `aj.tools.expand` keybinding; defaults to `false` so the
    /// scrollback stays compact across long sessions.
    tools_expanded: bool,
    /// Whether tool-execution components should render their
    /// image attachments inline. Sourced from the
    /// `image_show_in_terminal` config key at startup; threaded
    /// into every freshly-constructed [`ToolExecutionComponent`]
    /// via [`ToolExecutionComponent::with_show_image_in_terminal`].
    show_image_in_terminal: bool,
    /// Snapshot fed to the [`Footer`] component's context-usage
    /// indicator. Updated from main-agent
    /// [`AgentEvent::TurnUsage`] events and on model swaps; the
    /// pump pushes a fresh view into the footer after each
    /// mutation. Sub-agent turns are tracked through the
    /// scrollback row only and do not move the main footer (their
    /// context windows are independent).
    footer_data: FooterData,
}

impl EventPump {
    /// Build a fresh pump bound to the supplied [`ChatTheme`]
    /// (used when constructing assistant / user message
    /// components on the fly). `hide_thinking_block` is the
    /// initial mode for the thinking channel; the host loads it
    /// from `~/.aj/config.toml` (`hide_thinking_block` key) on
    /// startup and can flip it at runtime via
    /// [`Self::set_hide_thinking_block`]. `tools_expanded` is the
    /// initial mode for tool-output bubbles; today the host
    /// always starts collapsed and the user flips it at runtime
    /// via [`Self::set_tools_expanded`].
    ///
    /// `context_window` seeds the footer's context-usage
    /// denominator (in tokens). Pass `agent.model_info().context_window`;
    /// keep it in sync across model swaps via
    /// [`Self::set_context_window`].
    pub fn new(
        theme: ChatTheme,
        hide_thinking_block: bool,
        tools_expanded: bool,
        show_image_in_terminal: bool,
        context_window: u64,
    ) -> Self {
        Self {
            theme,
            current_assistant: None,
            tool_index: HashMap::new(),
            running_agents: HashSet::new(),
            hide_thinking_block,
            tools_expanded,
            show_image_in_terminal,
            footer_data: FooterData::new(context_window),
        }
    }

    /// Push the current footer snapshot into the [`Footer`]
    /// component. Called after every mutation that affects the
    /// rendered indicator so the row stays in sync.
    pub fn sync_footer(&self, tui: &mut Tui) {
        let snapshot = self.footer_data.context_usage();
        if let Some(footer) = tui.get_mut_as::<Footer>(SlotIndex::Footer.idx()) {
            footer.set_context_usage(Some(snapshot));
        }
        tui.request_render();
    }

    /// Swap the model context window used as the indicator's
    /// denominator. Pushes the refreshed snapshot into the footer
    /// so the user sees the change immediately rather than after
    /// the next turn lands.
    pub fn set_context_window(&mut self, tui: &mut Tui, context_window: u64) {
        self.footer_data.set_context_window(context_window);
        self.sync_footer(tui);
    }

    /// Current thinking-block render mode. Exposed so the host's
    /// `aj.thinking.toggle` handler can flip the state without first reading
    /// it back through a separate getter.
    pub fn hide_thinking_block(&self) -> bool {
        self.hide_thinking_block
    }

    /// Current inline-image render mode. Surface so thread-swap /
    /// new-thread paths can preserve the user's
    /// `image_show_in_terminal` choice across pump re-creation.
    pub fn show_image_in_terminal(&self) -> bool {
        self.show_image_in_terminal
    }

    /// Update the thinking-block render mode for this session.
    ///
    /// Updates the pump's own flag so freshly-created assistant
    /// message components pick up the new mode, then walks every
    /// existing child of the chat container and calls
    /// [`AssistantMessageComponent::set_hide_thinking_block`] on
    /// each one so the next render reflects the new mode for both
    /// finalized history and any in-flight streaming message.
    /// Finally invalidates the TUI's cached render output so the
    /// next paint actually picks the change up — without this the
    /// chat container would re-emit its memoised lines from
    /// before the toggle.
    pub fn set_hide_thinking_block(&mut self, tui: &mut Tui, hide: bool) {
        if self.hide_thinking_block == hide {
            return;
        }
        self.hide_thinking_block = hide;
        if let Some(chat) = tui.get_mut_as::<Container>(SlotIndex::Chat.idx()) {
            for i in 0..chat.len() {
                if let Some(msg) = chat.get_mut_as::<AssistantMessageComponent>(i) {
                    msg.set_hide_thinking_block(hide);
                }
            }
        }
        tui.invalidate();
        tui.request_render();
    }

    /// Current tool-output render mode. Exposed so the host's
    /// `aj.tools.expand` handler can flip the state without first
    /// reading it back through a separate getter.
    pub fn tools_expanded(&self) -> bool {
        self.tools_expanded
    }

    /// Update the tool-output render mode for this session.
    ///
    /// Updates the pump's own flag so freshly-created tool
    /// components pick up the new mode, then walks every existing
    /// child of the chat container and calls
    /// [`ToolExecutionComponent::set_expanded`] on each one so the
    /// next render reflects the new mode for both finalized
    /// history and any in-flight streaming tool. Finally
    /// invalidates the TUI's cached render output so the next
    /// paint actually picks the change up.
    pub fn set_tools_expanded(&mut self, tui: &mut Tui, expanded: bool) {
        if self.tools_expanded == expanded {
            return;
        }
        self.tools_expanded = expanded;
        if let Some(chat) = tui.get_mut_as::<Container>(SlotIndex::Chat.idx()) {
            for i in 0..chat.len() {
                if let Some(tool) = chat.get_mut_as::<ToolExecutionComponent>(i) {
                    tool.set_expanded(expanded);
                }
            }
        }
        tui.invalidate();
        tui.request_render();
    }

    /// Dispatch one [`AgentEvent`] onto `tui`'s slot tree. Returns
    /// nothing — every effect is a side effect on the layout.
    /// Callers that want a render afterwards should call
    /// [`Tui::request_render`] (the pump itself does so for the
    /// events that mutate visible state).
    pub fn handle(&mut self, tui: &mut Tui, event: &AgentEvent) {
        match event {
            // ---- Lifecycle: start / stop the working spinner. ----
            //
            // Sub-agents share the parent's bus and emit their own
            // `AgentStart` / `AgentEnd` bracket inside the main
            // agent's turn, so we can't naively start/stop the
            // loader on each event — a sub-agent's `AgentEnd` would
            // otherwise turn the spinner off while the main turn
            // is still mid-execution, and (worse) leave the
            // loader's animation pump running unmatched if the
            // sub-agent's `AgentStart` had already cancelled and
            // re-spawned it. Track the set of in-flight agents
            // instead and only start/stop on the boundary
            // transitions.
            AgentEvent::AgentStart { agent_id } => {
                // A top-level `AgentStart(Main)` is a hard resync
                // point: any state left over from a previous turn
                // (e.g. a sub-agent whose `AgentEnd` never made it
                // through because the agent task panicked) would
                // otherwise pin the loader on forever, so we drop
                // the stale set before inserting.
                if *agent_id == AgentId::Main {
                    self.running_agents.clear();
                }
                let was_idle = self.running_agents.is_empty();
                self.running_agents.insert(*agent_id);
                if was_idle {
                    self.with_loader(tui, |l| l.start());
                }
            }
            AgentEvent::AgentEnd { agent_id, .. } => {
                let was_present = self.running_agents.remove(agent_id);
                if was_present && self.running_agents.is_empty() {
                    self.with_loader(tui, |l| l.stop());
                }
                // Streaming-target and tool-index bookkeeping is
                // scoped to the main turn: a sub-agent's end
                // leaves the main agent's pending `agent` tool
                // call (the one that *invoked* the sub-agent)
                // mid-flight, so clearing `tool_index` here would
                // strand the lookup the upcoming
                // `ToolExecutionEnd` for that call needs. Only
                // the main agent's `AgentEnd` ends the turn from
                // the chat-scrollback's perspective.
                //
                // Main's end is also the authoritative "agent
                // activity has stopped" signal for the loader.
                // Stop it unconditionally and drain the set: any
                // sub-agent ID still in `running_agents` is a
                // stale entry whose `AgentEnd` was dropped (most
                // often because the parent's `spawn_agent`
                // future was cancelled mid-await) — by the time
                // Main's `AgentEnd` fires, every sub-agent's
                // future has been driven to completion or
                // dropped, so the entry doesn't correspond to a
                // task that's still running. Without this drain
                // the loader's animation pump keeps requesting
                // renders every 80 ms on what is, from the
                // user's POV, an idle session.
                if *agent_id == AgentId::Main {
                    self.current_assistant = None;
                    self.tool_index.clear();
                    if !self.running_agents.is_empty() {
                        self.running_agents.clear();
                        self.with_loader(tui, |l| l.stop());
                    }
                }
            }
            AgentEvent::TurnStart { .. } => {
                // Each new turn starts with a fresh assistant
                // message component; the previous turn's component
                // (if any) was already finalized at `MessageEnd` /
                // `MessagePersisted::Assistant`.
                self.current_assistant = None;
            }

            // ---- Streaming: unified message lifecycle. ----
            //
            // The agent emits a `MessageStart` / `MessageEnd` pair
            // around every message (user, assistant, tool-result)
            // and, for assistant streaming, one `MessageUpdate` per
            // provider [`AssistantMessageEvent`]. Renderers consume
            // the embedded event directly to drive in-flight text /
            // thinking / tool-call blocks; the finalized payload on
            // `MessageEnd` is the authoritative snapshot, used for
            // resume (which has no deltas) and to confirm streaming
            // results.
            AgentEvent::MessageStart { message, .. } => {
                self.handle_message_start(tui, message);
            }
            AgentEvent::MessageUpdate { event, .. } => {
                self.handle_message_update(tui, event);
            }
            AgentEvent::MessageEnd { message, .. } => {
                self.handle_message_end(tui, message);
            }

            // ---- Tool execution: header + result. ----
            AgentEvent::ToolExecutionStart {
                call_id,
                tool,
                args,
                ..
            } => self.append_tool_execution(tui, call_id, tool, args),
            AgentEvent::ToolExecutionUpdate {
                call_id,
                partial,
                content,
                ..
            } => {
                self.update_tool_execution_partial(tui, call_id, partial, content);
            }
            AgentEvent::ToolExecutionEnd {
                call_id,
                tool,
                result,
                content,
                is_error,
                ..
            } => {
                self.update_tool_execution_result(tui, call_id, tool, result, content, *is_error);
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
                // Only main-agent turns drive the footer's
                // context-occupancy indicator. Sub-agents run in
                // their own context window and surface their
                // usage through the scrollback row above; mixing
                // their counts into the main footer would make
                // the percentage jump unpredictably during a turn.
                if *agent_id == AgentId::Main {
                    self.footer_data.record_turn_usage(usage);
                    self.sync_footer(tui);
                }
            }

            // ---- Placeholders: events whose UI work isn't yet wired. ----
            AgentEvent::SubAgentStart { .. }
            | AgentEvent::SubAgentEnd { .. }
            | AgentEvent::TurnEnd { .. }
            | AgentEvent::QueueUpdate { .. } => {
                // Sub-agent grouping, queue indicators, and the
                // `TurnEnd` summary all land in follow-up commits.
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
    /// Append a `UserMessageComponent` for a user message that
    /// landed on the bus. Used by the live readline path (via the
    /// agent's `MessageEnd { User }` event) and the resume path
    /// (via the same event synthesized by `aj_session::replay`).
    /// Multiple [`UserContent::Text`] blocks are joined with `\n`
    /// so legacy multi-block user messages collapse into one
    /// rendered component — live user prompts are always
    /// single-block, but resumed threads may carry multi-block
    /// shapes from older formats.
    fn append_user_message(&self, tui: &mut Tui, content: &[UserContent]) {
        let text = content
            .iter()
            .filter_map(|b| match b {
                UserContent::Text(t) => Some(t.text.as_str()),
                UserContent::Image(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        if text.is_empty() {
            return;
        }
        let component = UserMessageComponent::new(&text, &self.theme);
        self.push_chat_child(tui, Box::new(component));
    }

    /// Handle [`AgentEvent::MessageStart`].
    ///
    /// All variants are no-ops on the TUI surface:
    ///
    /// * User / tool-result: the authoritative payload lands on
    ///   the matching [`AgentEvent::MessageEnd`], which is where
    ///   the rendering happens.
    /// * Assistant: the [`AssistantMessageComponent`] slot is
    ///   materialised lazily — by the first painting
    ///   `MessageUpdate` (`TextStart` / `ThinkingStart`) on the
    ///   live path, or by [`Self::handle_message_end`] on the
    ///   replay path (which emits `MessageStart` + `MessageEnd`
    ///   with no `MessageUpdate` in between).
    ///
    /// Earlier versions of this method pre-created the assistant
    /// component here so the first `MessageUpdate` didn't have to
    /// materialise it. That left an orphan empty component in the
    /// chat container on tool-use-only turns (the model called a
    /// tool without emitting any text / thinking first): the
    /// component rendered zero lines, but the leading
    /// [`Spacer`] inserted by [`Self::push_chat_child`] stuck
    /// around and doubled up with the next chat row's leading
    /// spacer, producing two visible blank rows where one was
    /// intended. Deferring creation to the first event that
    /// actually paints into the component removes the orphan
    /// without changing visible behaviour for turns that DO paint
    /// into the slot.
    ///
    /// The function is retained (rather than collapsing the
    /// dispatch arm to an inline no-op) so the design rationale
    /// has somewhere to live and a future hook for Start-time
    /// work has an obvious home. The `&mut self` / `&mut Tui`
    /// signature is preserved for the same reason: future Start-
    /// time work will almost certainly need both.
    #[allow(clippy::needless_pass_by_ref_mut)]
    fn handle_message_start(&mut self, _tui: &mut Tui, _message: &AgentMessage) {}

    /// Handle [`AgentEvent::MessageUpdate`] for an assistant
    /// streaming inference. Drives the in-flight
    /// [`AssistantMessageComponent`] off the embedded
    /// [`AssistantMessageEvent`].
    fn handle_message_update(&mut self, tui: &mut Tui, event: &AssistantMessageEvent) {
        // Early-out for events that don't paint into the assistant
        // component, BEFORE calling `ensure_assistant_message`:
        //
        // * `ToolCall{Start,Delta,End}` — tool calls render through
        //   the dedicated `ToolExecutionStart` / `ToolExecutionEnd`
        //   events, not through this component (the agent collects
        //   them off the finalized `Done { message }` payload and
        //   brackets them with their own events).
        // * `Start` / `Done` / `Error` — agent-side lifecycle
        //   markers; the matching `MessageStart` / `MessageEnd`
        //   are the authoritative bookends.
        //
        // Returning here is what keeps tool-use-only turns from
        // materialising an empty assistant slot whose leading
        // auto-spacer would orphan into the next row's gap. See
        // [`Self::handle_message_start`] for the full rationale.
        if !matches!(
            event,
            AssistantMessageEvent::TextStart { .. }
                | AssistantMessageEvent::TextDelta { .. }
                | AssistantMessageEvent::TextEnd { .. }
                | AssistantMessageEvent::ThinkingStart { .. }
                | AssistantMessageEvent::ThinkingDelta { .. }
                | AssistantMessageEvent::ThinkingEnd { .. }
        ) {
            return;
        }

        let idx = self.ensure_assistant_message(tui);
        let Some(chat) = tui.get_mut_as::<Container>(SlotIndex::Chat.idx()) else {
            return;
        };
        let Some(c) = chat.get_mut_as::<AssistantMessageComponent>(idx) else {
            return;
        };
        match event {
            AssistantMessageEvent::TextStart { .. } => {
                c.open_block(BlockKind::Text, String::new());
            }
            AssistantMessageEvent::TextDelta { delta, .. } => {
                c.append_delta(BlockKind::Text, delta);
            }
            AssistantMessageEvent::TextEnd { content, .. } => {
                // The text block's canonical final bytes; pass
                // through so the block matches the model's
                // authoritative content even if individual deltas
                // got dropped.
                c.close_block(BlockKind::Text, Some(content.clone()));
            }
            AssistantMessageEvent::ThinkingStart { .. } => {
                c.open_block(BlockKind::Thinking, String::new());
            }
            AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                c.append_delta(BlockKind::Thinking, delta);
            }
            AssistantMessageEvent::ThinkingEnd { content, .. } => {
                let payload = if content.is_empty() {
                    None
                } else {
                    Some(content.clone())
                };
                c.close_block(BlockKind::Thinking, payload);
            }
            // All non-painting variants returned early above.
            AssistantMessageEvent::ToolCallStart { .. }
            | AssistantMessageEvent::ToolCallDelta { .. }
            | AssistantMessageEvent::ToolCallEnd { .. }
            | AssistantMessageEvent::Start { .. }
            | AssistantMessageEvent::Done { .. }
            | AssistantMessageEvent::Error { .. } => {
                unreachable!("non-painting AssistantMessageEvent variants are filtered above")
            }
        }
    }

    /// Handle [`AgentEvent::MessageEnd`]. Assistant messages
    /// finalize their in-flight component (next turn opens a fresh
    /// one); user / tool-result messages append a fresh component
    /// from the authoritative payload — this is the rendering path
    /// for both live user prompts and replayed user threads.
    fn handle_message_end(&mut self, tui: &mut Tui, message: &AgentMessage) {
        match &message.kind {
            AgentMessageKind::Wire(Message::User(u)) => {
                self.append_user_message(tui, &u.content);
            }
            AgentMessageKind::Wire(Message::Assistant(a)) => {
                // Two cases share this arm:
                //
                // 1. Live streaming has already painted the content
                //    through `handle_message_update`; the finalized
                //    event just unbinds the streaming target so the
                //    next turn starts fresh.
                //
                // 2. Replay emits `MessageStart` + `MessageEnd` with
                //    no `MessageUpdate` in between (see
                //    `aj_session::replay`), so the slot was never
                //    materialised by a painting event. We have to
                //    create it here and synthesize per-block
                //    open/close pairs from the finalized content so
                //    the component lands in the same shape live
                //    streaming would have produced.
                //
                // We only materialise the slot when the finalized
                // payload carries at least one Text / Thinking
                // block. Tool-use-only turns render entirely
                // through the [`ToolExecutionComponent`]; creating
                // an empty assistant slot for them would leave an
                // orphan component whose leading auto-spacer
                // doubles the gap to the next row (see
                // [`Self::handle_message_start`] for the full
                // rationale).
                let has_renderable = a.content.iter().any(|b| {
                    matches!(b, AssistantContent::Text(_) | AssistantContent::Thinking(_))
                });
                if has_renderable {
                    let idx = self.ensure_assistant_message(tui);
                    if let Some(chat) = tui.get_mut_as::<Container>(SlotIndex::Chat.idx())
                        && let Some(c) = chat.get_mut_as::<AssistantMessageComponent>(idx)
                        && c.is_empty()
                    {
                        // Replay synthesis. Live streaming has
                        // already populated the blocks, so this
                        // branch is a no-op when `c.is_empty()` is
                        // false.
                        for block in &a.content {
                            match block {
                                AssistantContent::Thinking(t) => {
                                    c.open_block(BlockKind::Thinking, String::new());
                                    c.close_block(
                                        BlockKind::Thinking,
                                        Some(if t.redacted {
                                            format!("[Redacted thinking: {}]", t.thinking)
                                        } else {
                                            t.thinking.clone()
                                        }),
                                    );
                                }
                                AssistantContent::Text(t) => {
                                    c.open_block(BlockKind::Text, String::new());
                                    c.close_block(BlockKind::Text, Some(t.text.clone()));
                                }
                                AssistantContent::ToolCall(_) => {
                                    // Tool calls surface as
                                    // ToolExecutionStart/End in
                                    // replay; nothing to render here.
                                }
                            }
                        }
                    }
                }
                self.current_assistant = None;
            }
            AgentMessageKind::Wire(Message::ToolResult(_)) => {
                // Tool results render through the dedicated
                // `ToolExecutionEnd` event (which carries the
                // structured `ToolDetails`). The unified
                // `MessageEnd { ToolResult }` is structural framing.
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
        let component = AssistantMessageComponent::new(&self.theme, self.hide_thinking_block);
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
        let cell_pixel_size = tui.terminal().cell_pixel_size();
        let component = ToolExecutionComponent::with_cell_pixel_size(
            tool.to_string(),
            args,
            &self.theme,
            self.tools_expanded,
            cell_pixel_size,
        )
        .with_show_image_in_terminal(self.show_image_in_terminal);
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
        content: &[aj_models::types::UserContent],
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
        c.update_partial(partial, content);
    }

    /// Finalize a tool execution with its result.
    fn update_tool_execution_result(
        &mut self,
        tui: &mut Tui,
        call_id: &str,
        tool: &str,
        result: &aj_agent::tool::ToolDetails,
        content: &[aj_models::types::UserContent],
        is_error: bool,
    ) {
        // If we never saw `ToolExecutionStart` (replay path), build
        // a component now so the result is visible. Args aren't
        // available on the End event, so we render with an empty
        // object.
        //
        // The live path runs `append_tool_execution` on
        // `ToolExecutionStart`, which clears `current_assistant`
        // so the next streaming chunk opens a fresh assistant
        // component *after* the tool. The replay fallback below
        // builds an equivalent component but must replicate the
        // same bookkeeping — otherwise a subsequent assistant
        // text `StreamChunk` would attach to the previous turn's
        // assistant component (created for the thinking block
        // that *preceded* the tool), and the tool would appear
        // visually below the next assistant message instead of
        // between them. See the "Resume fidelity follow-up"
        // section in `docs/aj-next-progress.md` for the trace.
        let idx = match self.tool_index.get(call_id) {
            Some(idx) => *idx,
            None => {
                let cell_pixel_size = tui.terminal().cell_pixel_size();
                let component = ToolExecutionComponent::with_cell_pixel_size(
                    tool.to_string(),
                    &serde_json::json!({}),
                    &self.theme,
                    self.tools_expanded,
                    cell_pixel_size,
                )
                .with_show_image_in_terminal(self.show_image_in_terminal);
                let idx = self.push_chat_child(tui, Box::new(component));
                self.tool_index.insert(call_id.to_string(), idx);
                self.current_assistant = None;
                idx
            }
        };
        let Some(chat) = tui.get_mut_as::<Container>(SlotIndex::Chat.idx()) else {
            return;
        };
        let Some(c) = chat.get_mut_as::<ToolExecutionComponent>(idx) else {
            return;
        };
        c.update_result(result, content, is_error);
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
    let cache_creation_str = format_tokens(usage.accumulated_cache_write, usage.turn_cache_write);
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

    /// Build a TokenUsage carrying the supplied per-turn deltas
    /// and the running accumulator state observed *before* this
    /// turn was folded in (`already`). Matches the wire semantic
    /// on [`aj_agent::events::AgentEvent::TurnUsage`]: each
    /// `accumulated_*` field is the running total before this
    /// turn, and `turn_*` is the delta the turn is contributing
    /// on top.
    fn token_usage(turn: [u64; 4], already: [u64; 4]) -> TokenUsage {
        TokenUsage {
            accumulated_input: already[0],
            turn_input: turn[0],
            accumulated_output: already[1],
            turn_output: turn[1],
            accumulated_cache_write: already[2],
            turn_cache_write: turn[2],
            accumulated_cache_read: already[3],
            turn_cache_read: turn[3],
        }
    }

    /// Build a synthetic `AssistantMessage` partial with the
    /// scripted-provider identity stamped onto it. Used by the
    /// tests below to construct `AssistantMessageEvent` payloads
    /// that drive the pump's message-update path.
    fn empty_assistant_partial() -> aj_models::types::AssistantMessage {
        aj_models::types::AssistantMessage {
            content: Vec::new(),
            api: "scripted".into(),
            provider: "scripted".into(),
            model: "scripted".into(),
            response_id: None,
            usage: aj_models::types::Usage::default(),
            stop_reason: aj_models::types::StopReason::Stop,
            error: None,
            timestamp: 0,
        }
    }

    /// Build an `AgentEvent::MessageUpdate` carrying the given
    /// streaming-protocol event. Threading an empty partial through
    /// keeps each call site short.
    fn message_update_event(event: AssistantMessageEvent) -> AgentEvent {
        AgentEvent::MessageUpdate {
            agent_id: AgentId::Main,
            message: AgentMessage::wire(Message::Assistant(empty_assistant_partial())),
            event,
        }
    }

    /// Build an `AgentEvent::MessageStart` for an assistant turn.
    fn assistant_message_start_event() -> AgentEvent {
        AgentEvent::MessageStart {
            agent_id: AgentId::Main,
            message: AgentMessage::wire(Message::Assistant(empty_assistant_partial())),
        }
    }

    /// Build an `AgentEvent::MessageEnd` carrying a user message
    /// with the given text. Mirrors what the agent emits for a
    /// freshly-submitted readline prompt and what
    /// `aj_session::replay` synthesizes for a resumed user thread.
    fn user_message_end_event(text: &str) -> AgentEvent {
        AgentEvent::MessageEnd {
            agent_id: AgentId::Main,
            message: AgentMessage::wire(Message::User(aj_models::types::UserMessage::text(text))),
        }
    }

    #[test]
    fn format_turn_usage_line_emits_acc_plus_turn_for_main_agent() {
        // First turn: the accumulator is still zero, so each
        // field prints `0+turn` — "there was nothing before, and
        // this turn is contributing `turn`".
        let usage = token_usage([100, 50, 30, 5], [0, 0, 0, 0]);
        let line = format_turn_usage_line(AgentId::Main, &usage);
        assert_eq!(
            line,
            "Token Usage - Input: 0+100 | Output: 0+50 | Cache Creation: 0+30 | Cache Read: 0+5",
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
        // rows distinguishable in the shared scrollback. This is
        // the sub-agent's first turn, so the accumulator is still
        // zero (each field prints `0+turn`); cache_read's turn
        // delta is zero so the `+turn` suffix drops off there.
        let usage = token_usage([10, 5, 1, 0], [0, 0, 0, 0]);
        let line = format_turn_usage_line(AgentId::Sub(2), &usage);
        assert_eq!(
            line,
            "(sub agent 2) Token Usage - Input: 0+10 | Output: 0+5 | Cache Creation: 0+1 | Cache Read: 0",
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
        // Test-only: 200k matches the canonical Sonnet window so
        // any incidental "context_window" expectations in future
        // tests don't need to know about a synthetic value.
        let pump = EventPump::new(chat.clone(), false, false, true, 200_000);
        (tui, pump, chat)
    }

    /// Render the footer and return the rendered row as a single
    /// string, ANSI escape codes stripped. Test helper for the
    /// context-occupancy assertions below.
    fn rendered_footer(tui: &mut Tui) -> String {
        use crate::modes::interactive::components::footer::Footer;
        let footer = tui
            .get_mut_as::<Footer>(SlotIndex::Footer.idx())
            .expect("footer slot");
        let lines = footer.render(120);
        let joined = lines.join("\n");
        // Strip the SGR escape sequences so tests assert against
        // visible content only. Matches the pattern used elsewhere
        // in this module ("\x1b[2m" + "\x1b[22m" dim wrap).
        let mut s = joined;
        for code in [
            "\x1b[2m", "\x1b[22m", "\x1b[33m", "\x1b[31m", "\x1b[39m", "\x1b[0m",
        ] {
            s = s.replace(code, "");
        }
        s
    }

    #[test]
    fn main_agent_turn_usage_drives_the_footer_indicator() {
        // Sum of the prompt-side fields is the numerator:
        // 1_000 + 200 + 50 = 1_250 tokens; window is 200k -> 0.6%.
        let (mut tui, mut pump, _theme) = fresh_tui_with_layout();
        pump.sync_footer(&mut tui);
        // Before any turn lands the indicator is "?/200k".
        assert!(rendered_footer(&mut tui).contains("?/200k"));

        pump.handle(
            &mut tui,
            &AgentEvent::TurnUsage {
                agent_id: AgentId::Main,
                usage: token_usage([1_000, 999, 50, 200], [0, 0, 0, 0]),
            },
        );

        let line = rendered_footer(&mut tui);
        assert!(
            line.contains("1.2k/200k"),
            "expected `1.2k/200k` numerator/denominator in {line:?}",
        );
        assert!(
            line.contains("(0.6%)"),
            "expected `(0.6%)` percentage in {line:?}",
        );
    }

    #[test]
    fn sub_agent_turn_usage_leaves_the_footer_alone() {
        // Sub-agents share the bus but run in their own context
        // window; their usage must not move the main footer.
        let (mut tui, mut pump, _theme) = fresh_tui_with_layout();
        pump.sync_footer(&mut tui);
        let before = rendered_footer(&mut tui);

        pump.handle(
            &mut tui,
            &AgentEvent::TurnUsage {
                agent_id: AgentId::Sub(1),
                usage: token_usage([5_000, 1_000, 200, 100], [0, 0, 0, 0]),
            },
        );

        assert_eq!(
            rendered_footer(&mut tui),
            before,
            "sub-agent TurnUsage should not move the main footer indicator",
        );
    }

    #[test]
    fn set_context_window_repaints_with_new_denominator() {
        // Swap the model and confirm the denominator updates
        // immediately rather than waiting for the next turn.
        let (mut tui, mut pump, _theme) = fresh_tui_with_layout();
        pump.handle(
            &mut tui,
            &AgentEvent::TurnUsage {
                agent_id: AgentId::Main,
                usage: token_usage([10_000, 0, 0, 0], [0, 0, 0, 0]),
            },
        );
        assert!(rendered_footer(&mut tui).contains("10k/200k"));

        pump.set_context_window(&mut tui, 100_000);
        assert!(
            rendered_footer(&mut tui).contains("10k/100k"),
            "denominator should follow the new context window",
        );
    }

    #[test]
    fn replay_tool_result_without_start_does_not_steal_next_assistant_message() {
        // Regression for the resume-fidelity reorder bug. On a
        // resumed thread the disk shape is:
        //   user prompt -> assistant (thinking + tool_use) ->
        //   tool_result -> assistant (final text)
        // Replay walks these in append order and (today) emits a
        // `ToolExecutionEnd` without a matching
        // `ToolExecutionStart`, so this listener falls into the
        // build-on-miss branch of `update_tool_execution_result`.
        // That branch must clear `current_assistant`, otherwise
        // the *next* assistant message's `StreamChunk(Text, ...)`
        // attaches to the assistant component that was already
        // appended for the thinking block (which sits *above* the
        // tool in the chat container), and the visible scrollback
        // ends up with the final assistant text rendered *before*
        // the tool execution. Pin the correct order with a
        // canonical replay event sequence.
        let (mut tui, mut pump, _theme) = fresh_tui_with_layout();

        let chat_len = |tui: &mut Tui| -> usize {
            tui.get_mut_as::<Container>(SlotIndex::Chat.idx())
                .expect("chat slot")
                .len()
        };
        let chat_baseline = chat_len(&mut tui);

        // User prompt → user message component.
        pump.handle(&mut tui, &user_message_end_event("please run a tool"));

        // Assistant message lifecycle: MessageStart opens the slot.
        pump.handle(&mut tui, &assistant_message_start_event());

        // Assistant thinking block → drives the slot.
        pump.handle(
            &mut tui,
            &message_update_event(AssistantMessageEvent::ThinkingStart {
                content_index: 0,
                partial: empty_assistant_partial(),
            }),
        );
        pump.handle(
            &mut tui,
            &message_update_event(AssistantMessageEvent::ThinkingEnd {
                content_index: 0,
                content: "let me think".into(),
                partial: empty_assistant_partial(),
            }),
        );

        // Tool result lands without a preceding ToolExecutionStart
        // (replay path). The build-on-miss branch creates the
        // tool component AND must drop `current_assistant`.
        pump.handle(
            &mut tui,
            &AgentEvent::ToolExecutionEnd {
                agent_id: AgentId::Main,
                call_id: "call-1".into(),
                tool: "bash".into(),
                result: aj_agent::tool::ToolDetails::Text {
                    summary: "bash".into(),
                    body: "hello from aj".into(),
                },
                content: std::sync::Arc::from(Vec::<aj_models::types::UserContent>::new()),
                is_error: false,
            },
        );

        // Final assistant text (next persisted assistant message).
        // After the fix this must open a *fresh* assistant
        // component appended *after* the tool component, not
        // reuse the thinking-block component that was appended
        // before the tool.
        pump.handle(&mut tui, &assistant_message_start_event());
        pump.handle(
            &mut tui,
            &message_update_event(AssistantMessageEvent::TextStart {
                content_index: 0,
                partial: empty_assistant_partial(),
            }),
        );
        pump.handle(
            &mut tui,
            &message_update_event(AssistantMessageEvent::TextEnd {
                content_index: 0,
                content: "Done. Anything specific?".into(),
                partial: empty_assistant_partial(),
            }),
        );

        // Walk the chat container and verify (a) the last child
        // is the final assistant text, and (b) a
        // `ToolExecutionComponent` sits strictly before it. With
        // the regression in place the tool would be the last
        // child instead.
        let chat = tui
            .get_mut_as::<Container>(SlotIndex::Chat.idx())
            .expect("chat slot");
        let total = chat.len();
        assert!(total > chat_baseline + 6, "got {total} children");
        let last_idx = total - 1;
        let last_is_assistant = chat
            .get_mut_as::<AssistantMessageComponent>(last_idx)
            .is_some();
        assert!(
            last_is_assistant,
            "last chat child should be the final assistant message; \
             a regression would leave the tool execution at the tail",
        );
        let mut tool_idx: Option<usize> = None;
        for i in chat_baseline..last_idx {
            if chat.get_mut_as::<ToolExecutionComponent>(i).is_some() {
                tool_idx = Some(i);
                break;
            }
        }
        assert!(
            tool_idx.is_some(),
            "expected a ToolExecutionComponent before the final \
             assistant message; got chat layout with {total} children",
        );
    }

    #[test]
    fn tool_use_only_turn_does_not_leave_an_empty_assistant_slot() {
        // Regression: a turn where the model emitted only a
        // `tool_use` block (no text, no thinking) used to leave
        // an empty `AssistantMessageComponent` in the chat
        // container. The component rendered zero lines but the
        // leading auto-spacer that `push_chat_child` inserts
        // before every new child stuck around and doubled up
        // with the next sibling's leading spacer, producing two
        // visible blank rows where one was intended.
        //
        // The lifecycle below mirrors what the agent actually
        // emits for a tool-use-only turn: `MessageStart{Assistant}`
        // → `ToolCallStart/Delta/End` `MessageUpdate`s →
        // `MessageEnd{Assistant}` carrying a single `ToolCall`
        // block → `ToolExecutionStart` → `ToolExecutionEnd`. None
        // of those events should materialise an
        // `AssistantMessageComponent`.
        let (mut tui, mut pump, _theme) = fresh_tui_with_layout();

        pump.handle(&mut tui, &user_message_end_event("please run a tool"));
        pump.handle(&mut tui, &assistant_message_start_event());

        // Tool-call deltas; none of these paint into the assistant
        // component, so the slot must NOT be materialised here.
        pump.handle(
            &mut tui,
            &message_update_event(AssistantMessageEvent::ToolCallStart {
                content_index: 0,
                partial: empty_assistant_partial(),
            }),
        );
        pump.handle(
            &mut tui,
            &message_update_event(AssistantMessageEvent::ToolCallDelta {
                content_index: 0,
                delta: "{\"cmd\":\"ls\"}".into(),
                partial: empty_assistant_partial(),
            }),
        );
        pump.handle(
            &mut tui,
            &message_update_event(AssistantMessageEvent::ToolCallEnd {
                content_index: 0,
                tool_call: aj_models::types::ToolCall {
                    id: "call-1".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({"cmd": "ls"}),
                },
                partial: empty_assistant_partial(),
            }),
        );

        // `MessageEnd` carrying a tool-only assistant payload. The
        // replay-synthesis branch must skip slot materialisation
        // because there's no Text / Thinking content to paint.
        let assistant_tool_only = aj_models::types::AssistantMessage {
            content: vec![AssistantContent::ToolCall(aj_models::types::ToolCall {
                id: "call-1".into(),
                name: "bash".into(),
                arguments: serde_json::json!({"cmd": "ls"}),
            })],
            api: "scripted".into(),
            provider: "scripted".into(),
            model: "scripted".into(),
            response_id: None,
            usage: aj_models::types::Usage::default(),
            stop_reason: aj_models::types::StopReason::ToolUse,
            error: None,
            timestamp: 0,
        };
        pump.handle(
            &mut tui,
            &AgentEvent::MessageEnd {
                agent_id: AgentId::Main,
                message: AgentMessage::wire(Message::Assistant(assistant_tool_only)),
            },
        );

        // Tool runs.
        pump.handle(
            &mut tui,
            &AgentEvent::ToolExecutionStart {
                agent_id: AgentId::Main,
                call_id: "call-1".into(),
                tool: "bash".into(),
                args: serde_json::json!({"cmd": "ls"}),
            },
        );
        pump.handle(
            &mut tui,
            &AgentEvent::ToolExecutionEnd {
                agent_id: AgentId::Main,
                call_id: "call-1".into(),
                tool: "bash".into(),
                result: aj_agent::tool::ToolDetails::Text {
                    summary: "bash".into(),
                    body: "ok".into(),
                },
                content: std::sync::Arc::from(Vec::<aj_models::types::UserContent>::new()),
                is_error: false,
            },
        );

        // No `AssistantMessageComponent` anywhere in the chat
        // container — the regression would put an empty one
        // between the user message and the tool execution.
        let chat = tui
            .get_mut_as::<Container>(SlotIndex::Chat.idx())
            .expect("chat slot");
        let assistant_count = (0..chat.len())
            .filter(|&i| chat.get_mut_as::<AssistantMessageComponent>(i).is_some())
            .count();
        assert_eq!(
            assistant_count,
            0,
            "tool-use-only turn must not leave an AssistantMessageComponent in chat; \
             chat has {} children",
            chat.len(),
        );
    }

    #[test]
    fn thinking_stream_survives_empty_snapshot_stop_event() {
        // Regression: the agent emits `ThinkingStop` with an
        // empty snapshot (the streaming layer doesn't accumulate
        // a canonical thinking-channel snapshot the way it does
        // for text). A naive pump would feed that empty string
        // back through `set_thinking_snapshot` and wipe the
        // accumulated deltas, making the entire thinking block
        // disappear the instant streaming finished.
        //
        // We assert that the rendered output still contains the
        // streamed thinking content after the Stop event lands.
        let (mut tui, mut pump, _theme) = fresh_tui_with_layout();

        // Open a thinking stream and append a non-trivial body.
        pump.handle(
            &mut tui,
            &message_update_event(AssistantMessageEvent::ThinkingStart {
                content_index: 0,
                partial: empty_assistant_partial(),
            }),
        );
        pump.handle(
            &mut tui,
            &message_update_event(AssistantMessageEvent::ThinkingDelta {
                content_index: 0,
                delta: "first let me reason about the".to_string(),
                partial: empty_assistant_partial(),
            }),
        );
        pump.handle(
            &mut tui,
            &message_update_event(AssistantMessageEvent::ThinkingDelta {
                content_index: 0,
                delta: " inputs carefully".to_string(),
                partial: empty_assistant_partial(),
            }),
        );
        // The empty-content ThinkingEnd is the exact shape the
        // agent emits when the provider finalizes a thinking block
        // without an authoritative snapshot. With the regression in
        // place this wiped the buffer.
        pump.handle(
            &mut tui,
            &message_update_event(AssistantMessageEvent::ThinkingEnd {
                content_index: 0,
                content: String::new(),
                partial: empty_assistant_partial(),
            }),
        );

        let chat = tui
            .get_mut_as::<Container>(SlotIndex::Chat.idx())
            .expect("chat slot");
        let last = chat.len() - 1;
        let assistant = chat
            .get_mut_as::<AssistantMessageComponent>(last)
            .expect("assistant message at chat tail after thinking stream");
        let rendered = assistant.render(80).join("\n");
        assert!(
            rendered.contains("first let me reason about the inputs carefully"),
            "expected accumulated thinking text to survive the empty-snapshot \
             Stop event; got rendered:\n{rendered}"
        );
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
                "Token Usage - Input: 0+42 | Output: 0+17 | Cache Creation: 0 | Cache Read: 0+3"
            ),
            "row should carry the formatted usage line, got: {joined:?}",
        );
        assert!(
            joined.contains("\x1b[2m"),
            "row should be wrapped in the dim ANSI escape",
        );
    }

    #[test]
    fn nested_subagent_lifecycle_keeps_loader_running_until_main_ends() {
        // The loader's only periodic source of `request_render`
        // calls is its animation pump. If a sub-agent run's
        // `AgentStart` / `AgentEnd` events drop the loader while
        // the main turn is still in flight, the user loses the
        // spinner for the resume — and conversely, an off-by-one
        // in the lifecycle leaves the pump running indefinitely
        // and the render loop pegged at the 80 ms tick rate.
        // Pin the expected behaviour: the loader stays active
        // through nested events and only stops on the main agent's
        // `AgentEnd`.
        let (mut tui, mut pump, _theme) = fresh_tui_with_layout();

        fn is_loader_active(tui: &mut Tui) -> bool {
            tui.get_mut_as::<Container>(SlotIndex::Status.idx())
                .expect("status slot")
                .get_mut_as::<LoaderStatus>(0)
                .expect("loader status")
                .is_active()
        }

        // Fresh pump: loader is idle.
        assert!(!is_loader_active(&mut tui));

        // Main turn starts.
        pump.handle(
            &mut tui,
            &AgentEvent::AgentStart {
                agent_id: AgentId::Main,
            },
        );
        assert!(is_loader_active(&mut tui), "loader should start on main");

        // Sub-agent starts inside the main turn.
        pump.handle(
            &mut tui,
            &AgentEvent::AgentStart {
                agent_id: AgentId::Sub(1),
            },
        );
        assert!(
            is_loader_active(&mut tui),
            "loader should stay active across the sub-agent start",
        );

        // Sub-agent ends — main is still running.
        pump.handle(
            &mut tui,
            &AgentEvent::AgentEnd {
                agent_id: AgentId::Sub(1),
                messages: Vec::new(),
            },
        );
        assert!(
            is_loader_active(&mut tui),
            "loader must NOT stop on the sub-agent's AgentEnd: \
             the main turn is still running and the user expects \
             a spinner during the resume",
        );

        // Main turn ends.
        pump.handle(
            &mut tui,
            &AgentEvent::AgentEnd {
                agent_id: AgentId::Main,
                messages: Vec::new(),
            },
        );
        assert!(
            !is_loader_active(&mut tui),
            "loader should stop once the main agent ends",
        );
    }

    #[test]
    fn unmatched_subagent_end_does_not_stop_the_loader() {
        // Defensive: a stray `AgentEnd` event for a sub-agent we
        // never saw start (in practice this shouldn't happen, but
        // the bus is the only source of ordering and any
        // out-of-order event must not leave the loader in a state
        // that contradicts the main agent's true running status).
        let (mut tui, mut pump, _theme) = fresh_tui_with_layout();
        pump.handle(
            &mut tui,
            &AgentEvent::AgentStart {
                agent_id: AgentId::Main,
            },
        );
        pump.handle(
            &mut tui,
            &AgentEvent::AgentEnd {
                agent_id: AgentId::Sub(7),
                messages: Vec::new(),
            },
        );
        let status = tui
            .get_mut_as::<Container>(SlotIndex::Status.idx())
            .expect("status slot");
        let loader = status.get_mut_as::<LoaderStatus>(0).expect("loader status");
        assert!(
            loader.is_active(),
            "a sub-agent AgentEnd must not stop the loader while \
             the main agent is still active",
        );
    }

    #[test]
    fn main_agent_end_drains_leaked_subagents_and_stops_loader() {
        // Authoritative-end contract: when the main agent emits
        // `AgentEnd`, the loader must stop even if a sub-agent's
        // own `AgentEnd` was dropped earlier in the turn (typical
        // cause: the parent's `spawn_agent` future was cancelled
        // mid-await, so `Agent::run_single_turn` was dropped at
        // `run_single_turn_inner.await` and the `AgentEnd` emit
        // on the following line never fired).
        //
        // Regression for a CPU-pegging bug observed on a long
        // idle session: a stale sub-agent ID kept `running_agents`
        // non-empty after the main turn ended, the loader stayed
        // active, and its animation pump kept calling
        // `request_render` every 80 ms forever — pinning one CPU
        // core through `Tui::render` over a ~25k-line scrollback.
        let (mut tui, mut pump, _theme) = fresh_tui_with_layout();

        fn is_loader_active(tui: &mut Tui) -> bool {
            tui.get_mut_as::<Container>(SlotIndex::Status.idx())
                .expect("status slot")
                .get_mut_as::<LoaderStatus>(0)
                .expect("loader status")
                .is_active()
        }

        // Main turn starts and spawns a sub-agent.
        pump.handle(
            &mut tui,
            &AgentEvent::AgentStart {
                agent_id: AgentId::Main,
            },
        );
        pump.handle(
            &mut tui,
            &AgentEvent::AgentStart {
                agent_id: AgentId::Sub(1),
            },
        );
        assert!(is_loader_active(&mut tui));

        // Note: no `AgentEnd(Sub(1))` — simulates the dropped emit.
        // Main's `AgentEnd` fires.
        pump.handle(
            &mut tui,
            &AgentEvent::AgentEnd {
                agent_id: AgentId::Main,
                messages: Vec::new(),
            },
        );

        assert!(
            !is_loader_active(&mut tui),
            "loader must stop on Main's AgentEnd even with a leaked \
             sub-agent — otherwise the animation pump pegs CPU on \
             an idle session"
        );
    }

    #[test]
    fn second_main_turn_recovers_loader_after_a_leaked_subagent() {
        // Defence-in-depth: even with the authoritative-end fix,
        // exercise the next-turn path so a future regression that
        // weakens the AgentEnd handler is still caught by the
        // top-level resync at AgentStart{Main} (which clears the
        // stale set unconditionally).
        let (mut tui, mut pump, _theme) = fresh_tui_with_layout();

        fn is_loader_active(tui: &mut Tui) -> bool {
            tui.get_mut_as::<Container>(SlotIndex::Status.idx())
                .expect("status slot")
                .get_mut_as::<LoaderStatus>(0)
                .expect("loader status")
                .is_active()
        }

        // Turn 1: main starts, sub-agent starts, but the sub's
        // `AgentEnd` never arrives (leak). Main eventually ends.
        pump.handle(
            &mut tui,
            &AgentEvent::AgentStart {
                agent_id: AgentId::Main,
            },
        );
        pump.handle(
            &mut tui,
            &AgentEvent::AgentStart {
                agent_id: AgentId::Sub(3),
            },
        );
        // Note: no AgentEnd(Sub(3)) here.
        pump.handle(
            &mut tui,
            &AgentEvent::AgentEnd {
                agent_id: AgentId::Main,
                messages: Vec::new(),
            },
        );
        // Main's `AgentEnd` is authoritative: even with the leaked
        // sub still in the set when it fires, the loader stops.
        assert!(
            !is_loader_active(&mut tui),
            "loader stops on Main's AgentEnd; the leaked sub's stale \
             entry is drained as part of that handler"
        );

        // Turn 2: a fresh `AgentStart(Main)` is also a resync point.
        // The `running_agents.clear()` on Main's start is now a
        // belt-and-suspenders guard (Main's End above already
        // drained the set); the start/stop cycle of the new turn
        // still transitions cleanly through it.
        pump.handle(
            &mut tui,
            &AgentEvent::AgentStart {
                agent_id: AgentId::Main,
            },
        );
        assert!(is_loader_active(&mut tui));
        pump.handle(
            &mut tui,
            &AgentEvent::AgentEnd {
                agent_id: AgentId::Main,
                messages: Vec::new(),
            },
        );
        assert!(
            !is_loader_active(&mut tui),
            "second main turn ends with the loader stopped",
        );
    }

    #[test]
    fn subagent_end_does_not_clear_main_tool_index() {
        // Regression: while a sub-agent is running, the main
        // agent's pending `agent` tool call is still in flight
        // (the tool body *is* the sub-agent's run). The
        // `ToolExecutionEnd` for that call lands after the
        // sub-agent's `AgentEnd`. Clearing `tool_index` on the
        // sub's `AgentEnd` would strand the lookup; the result
        // would either silently drop or — through the
        // build-on-miss fallback in `update_tool_execution_result`
        // — append a second `ToolExecutionComponent` for the same
        // call, leaving the original stuck on its `Started`
        // spinner glyph.
        let (mut tui, mut pump, _theme) = fresh_tui_with_layout();

        // Main agent fires the `agent` tool.
        pump.handle(
            &mut tui,
            &AgentEvent::AgentStart {
                agent_id: AgentId::Main,
            },
        );
        let call_id = "call-agent-1";
        pump.handle(
            &mut tui,
            &AgentEvent::ToolExecutionStart {
                agent_id: AgentId::Main,
                call_id: call_id.to_string(),
                tool: "agent".to_string(),
                args: serde_json::json!({"task": "summarise"}),
            },
        );
        let chat_len_after_tool_start = tui
            .get_mut_as::<Container>(SlotIndex::Chat.idx())
            .expect("chat slot")
            .len();

        // Sub-agent runs and ends, with its `AgentStart` /
        // `AgentEnd` bracketing.
        pump.handle(
            &mut tui,
            &AgentEvent::AgentStart {
                agent_id: AgentId::Sub(1),
            },
        );
        pump.handle(
            &mut tui,
            &AgentEvent::AgentEnd {
                agent_id: AgentId::Sub(1),
                messages: Vec::new(),
            },
        );

        // Now the agent-tool's End lands. With the old
        // `tool_index.clear()`-on-every-End behaviour, the lookup
        // would miss and a *second* `ToolExecutionComponent`
        // would be appended; with the fix the existing component
        // is updated in place and the chat length is unchanged.
        pump.handle(
            &mut tui,
            &AgentEvent::ToolExecutionEnd {
                agent_id: AgentId::Main,
                call_id: call_id.to_string(),
                tool: "agent".to_string(),
                result: aj_agent::tool::ToolDetails::Text {
                    summary: "summary".into(),
                    body: "the sub-agent's report".into(),
                },
                content: std::sync::Arc::from(Vec::<aj_models::types::UserContent>::new()),
                is_error: false,
            },
        );
        let chat_len_after_tool_end = tui
            .get_mut_as::<Container>(SlotIndex::Chat.idx())
            .expect("chat slot")
            .len();
        assert_eq!(
            chat_len_after_tool_start, chat_len_after_tool_end,
            "ToolExecutionEnd should update the existing component \
             in place, not append a second one — the sub-agent's \
             AgentEnd must not have cleared the main agent's \
             tool_index",
        );
    }

    /// Runtime regression for the CPU-pegging bug the refcount fix
    /// addresses. The four `#[test]`s above pin the bookkeeping
    /// — `running_agents` transitions, `Loader::start` / `stop`
    /// calls, `tool_index` lifetime — but they don't exercise the
    /// thing the user actually feels: the loader's animation pump
    /// spawning a fresh tokio task on every `Loader::start` and
    /// only cancelling it on the matching `stop`.
    ///
    /// Old behaviour (sub-agent's `AgentEnd` calls `Loader::stop`
    /// followed by the main turn's continuation re-triggering
    /// `Loader::start`) would, over a session with many sub-agent
    /// turns, leak animation pumps whose `request_render` ticks
    /// kept the throttle saturated even when no visible work was
    /// in flight. With the fix, `Loader::start` fires exactly once
    /// per main turn (on the 0 → 1 transition of
    /// `running_agents`) regardless of how many sub-agent starts /
    /// ends nest inside it, so the render channel should be driven
    /// at the loader's own 80 ms interval rather than at the
    /// throttle's 16 ms cap.
    ///
    /// The assertion below counts `TuiEvent::Render`s the
    /// throttle releases over a fixed window after many nested
    /// cycles. A single active pump produces roughly
    /// `window / 80 ms` renders; a regression that leaks pumps
    /// would push the count toward `window / 16 ms`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn nested_subagent_cycles_do_not_accumulate_animation_pumps() {
        use std::time::Duration;

        use aj_tui::tui::TuiEvent;
        use tokio::time::Instant as TokioInstant;

        let (mut tui, mut pump, _theme) = fresh_tui_with_layout();
        // Suppress the implicit bootstrap render so the
        // post-cycle counting window only sees renders from the
        // loader's animation pump.
        tui.set_initial_render(false);

        // Drive many nested cycles synchronously. Each cycle
        // brackets one main turn around one sub-agent turn:
        //
        //   Main start → Sub start → Sub end → Main end.
        //
        // With the fix, this produces 100 paired
        // `Loader::start` / `Loader::stop` calls and the same
        // number of animation-pump task spawns / cancellations
        // — but at any instant only one pump should be alive,
        // because each `Loader::start` cancels the prior token
        // before spawning, and each `Loader::stop` cancels
        // unconditionally.
        for _ in 0..100 {
            pump.handle(
                &mut tui,
                &AgentEvent::AgentStart {
                    agent_id: AgentId::Main,
                },
            );
            pump.handle(
                &mut tui,
                &AgentEvent::AgentStart {
                    agent_id: AgentId::Sub(1),
                },
            );
            pump.handle(
                &mut tui,
                &AgentEvent::AgentEnd {
                    agent_id: AgentId::Sub(1),
                    messages: Vec::new(),
                },
            );
            pump.handle(
                &mut tui,
                &AgentEvent::AgentEnd {
                    agent_id: AgentId::Main,
                    messages: Vec::new(),
                },
            );
        }

        // Final main turn — the loader's pump is now running. If
        // any cycle-phase pumps leaked, this is the moment their
        // background `request_render` ticks would still be
        // landing on the channel.
        pump.handle(
            &mut tui,
            &AgentEvent::AgentStart {
                agent_id: AgentId::Main,
            },
        );

        // Drain the synchronous `request_repaint` carry-over
        // from the cycle phase + the final start. We give it a
        // generous 80 ms (one loader interval) so leaked pumps
        // — if any — get at least one chance to tick before
        // the counting window starts. Without this, the
        // measurement would race against the throttle's lazy
        // initialisation on the first `next_event`.
        let drain_until = TokioInstant::now() + Duration::from_millis(80);
        while let Ok(maybe) = tokio::time::timeout_at(drain_until, tui.next_event()).await {
            // Discard whatever we drain; we only care about the
            // *rate* during the measurement window below.
            if maybe.is_none() {
                break;
            }
        }

        // Measure: how many renders does the loader drive over a
        // 320 ms window?
        //
        // - One healthy pump at 80 ms interval ≈ 4 renders.
        // - The throttle's 16 ms floor caps the worst-case
        //   regression at ≈ 20 renders.
        //
        // Assert well below the regression cap (and well above
        // the healthy baseline) so CI scheduler jitter doesn't
        // flake the test in either direction.
        let window = Duration::from_millis(320);
        let deadline = TokioInstant::now() + window;
        let mut renders = 0usize;
        while let Ok(maybe) = tokio::time::timeout_at(deadline, tui.next_event()).await {
            match maybe {
                Some(TuiEvent::Render) => renders += 1,
                Some(_) => {}
                None => break,
            }
        }

        // Tidy up so the loader's pump is cancelled before the
        // test exits (otherwise the task survives the `Tui`
        // drop until the runtime tears down — harmless, but
        // noisier than necessary).
        pump.handle(
            &mut tui,
            &AgentEvent::AgentEnd {
                agent_id: AgentId::Main,
                messages: Vec::new(),
            },
        );

        assert!(
            renders <= 10,
            "loader produced {renders} renders in {} ms; expected ≈4 \
             from a single 80 ms-interval animation pump. A count \
             approaching {} (the throttle cap of {} ms-interval \
             ticks) suggests orphaned animation pumps leaking \
             through the refcount fix.",
            window.as_millis(),
            window.as_millis() / 16,
            16,
        );
    }

    #[test]
    fn set_tools_expanded_flips_every_existing_tool_component() {
        // The pump starts in collapsed mode; appending two
        // finalized tool executions should give us two collapsed
        // components. Flipping the pump must walk the chat
        // container and expand each one.
        let (mut tui, mut pump, _theme) = fresh_tui_with_layout();
        assert!(!pump.tools_expanded());

        // Drive two finished tool calls into the chat slot.
        for (i, body_line_count) in [(0usize, 30usize), (1, 25)] {
            let body = (0..body_line_count)
                .map(|n| format!("tool-{i} line {n}"))
                .collect::<Vec<_>>()
                .join("\n");
            pump.handle(
                &mut tui,
                &AgentEvent::ToolExecutionStart {
                    agent_id: AgentId::Main,
                    call_id: format!("call-{i}"),
                    tool: "read_file".into(),
                    args: serde_json::json!({}),
                },
            );
            pump.handle(
                &mut tui,
                &AgentEvent::ToolExecutionEnd {
                    agent_id: AgentId::Main,
                    call_id: format!("call-{i}"),
                    tool: "read_file".into(),
                    result: aj_agent::tool::ToolDetails::Text {
                        summary: String::new(),
                        body,
                    },
                    content: std::sync::Arc::from(Vec::<aj_models::types::UserContent>::new()),
                    is_error: false,
                },
            );
        }

        // Helper: count tool components in the chat slot whose
        // rendered body contains the line that's *only* visible in
        // expanded mode (line index >= TEXT_COLLAPSED_LINES = 10).
        let count_expanded = |tui: &mut Tui| -> usize {
            let chat = tui
                .get_mut_as::<Container>(SlotIndex::Chat.idx())
                .expect("chat slot");
            let mut n = 0;
            for i in 0..chat.len() {
                if let Some(tool) = chat.get_mut_as::<ToolExecutionComponent>(i) {
                    let lines = tool.render(80);
                    let has_late_line = lines
                        .iter()
                        .any(|l| l.contains("line 20") || l.contains("line 24"));
                    if has_late_line {
                        n += 1;
                    }
                }
            }
            n
        };

        // Sanity: collapsed, no late lines visible.
        assert_eq!(count_expanded(&mut tui), 0);

        pump.set_tools_expanded(&mut tui, true);
        assert!(pump.tools_expanded());
        assert_eq!(
            count_expanded(&mut tui),
            2,
            "both tool components should expose their hidden tail after expand",
        );

        // A redundant set is a no-op (no panic, no rebuild churn).
        pump.set_tools_expanded(&mut tui, true);
        assert!(pump.tools_expanded());

        // And we can flip back.
        pump.set_tools_expanded(&mut tui, false);
        assert!(!pump.tools_expanded());
        assert_eq!(count_expanded(&mut tui), 0);
    }
}
