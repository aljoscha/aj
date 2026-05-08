//! Bus listener that drives the legacy [`AjUi`] rendering off of
//! [`AgentEvent`]s.
//!
//! Per `docs/aj-next-plan.md` §2.3, the agent emits typed events
//! through its internal bus and the binary registers a listener that
//! translates those events back into the existing
//! [`crate::cli::AjCli`] / [`crate::cli_sub_agent::SubAgentCli`]
//! rendering helpers. Once the bus drives rendering end-to-end, the
//! agent no longer touches `AjUi` from inside `execute_turn`.
//!
//! The listener:
//!
//! - Owns one `Box<dyn AjUi>` for the main agent (the parent's
//!   [`crate::cli::AjCli`] clone) and lazily creates one
//!   [`crate::cli_sub_agent::SubAgentCli`] per `Sub(n)` agent id the
//!   first time an event for that sub-agent arrives. Sub-agents
//!   share the parent's bus per `docs/aj-next-plan.md` §1.6, so all
//!   their events flow here too.
//! - Reuses [`aj_tools::bridge::render_details_via_ui`] for
//!   [`AgentEvent::ToolExecutionEnd`] so the rendered output is
//!   byte-identical to what the in-tree
//!   [`aj_tools::bridge::BridgedTool`] currently produces (today's
//!   tools call into `AjUi` via that path; once §2.3a lands, the
//!   bridged tool stops calling `AjUi` and the listener takes over).
//! - Synchronously calls every render method, then resolves the
//!   `Future` immediately. The bus awaits each listener inline, so
//!   rendering is sequenced: the agent never moves on to the next
//!   event before the previous frame has hit stdout.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aj_agent::bus::Listener;
use aj_agent::events::{AgentEvent, AgentId, StreamAction, StreamChannel};
use aj_tools::bridge::render_details_via_ui;
use aj_ui::AjUi;

use crate::cli_sub_agent::SubAgentCli;

/// Bus listener that calls into the legacy [`AjUi`] rendering helpers
/// for every [`AgentEvent`] flowing on the agent's bus.
///
/// Construct with [`EventBridgeListener::new`] (passing the main
/// renderer the binary already built), then convert into a
/// [`Listener`] via [`EventBridgeListener::into_listener`] and pass to
/// `agent.subscribe(...)`. The returned listener captures the bridge
/// state behind an `Arc<Mutex<_>>` so dropping the
/// [`EventBridgeListener`] handle on the binary side does not detach
/// the listener — the [`aj_agent::bus::SubscriptionHandle`] returned
/// from `subscribe` is what controls lifetime.
pub struct EventBridgeListener {
    inner: Arc<Mutex<EventBridgeInner>>,
}

/// Inner state guarded by the listener's mutex. Holds one renderer
/// per agent id; the main one is supplied at construction time, sub-
/// agent ones are created lazily on first event.
struct EventBridgeInner {
    /// Renderer for the top-level agent. Cloned from the binary's
    /// own [`AjCli`] so input-loop and bus-driven output share the
    /// same prompt history and styling.
    main: Box<dyn AjUi>,
    /// Renderers keyed by sub-agent index. Built lazily — the bridge
    /// listener has no way to predict how many sub-agents the
    /// session will spawn, and unused entries would just hold empty
    /// state.
    subs: HashMap<usize, Box<dyn AjUi>>,
}

impl EventBridgeListener {
    /// Build a fresh bridge listener with `main` as the renderer for
    /// [`AgentId::Main`] events.
    pub fn new(main: Box<dyn AjUi>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(EventBridgeInner {
                main,
                subs: HashMap::new(),
            })),
        }
    }

    /// Project the bridge into a [`Listener`] suitable for
    /// [`aj_agent::Agent::subscribe`]. Multiple calls share the same
    /// underlying inner state via `Arc`, but only one subscription
    /// should be registered at a time.
    pub fn into_listener(self) -> Listener {
        let inner = self.inner;
        Arc::new(move |event: &AgentEvent| {
            let inner = Arc::clone(&inner);
            let event = event.clone();
            Box::pin(async move {
                inner
                    .lock()
                    .expect("event bridge mutex poisoned")
                    .handle(&event);
                Ok(())
            })
        })
    }
}

impl EventBridgeInner {
    /// Borrow the renderer responsible for `agent_id`. Lazily
    /// constructs a [`SubAgentCli`] on first reference for any
    /// previously-unseen `Sub(n)`.
    fn renderer_for(&mut self, agent_id: AgentId) -> &mut dyn AjUi {
        match agent_id {
            AgentId::Main => self.main.as_mut(),
            AgentId::Sub(n) => {
                let entry = self
                    .subs
                    .entry(n)
                    .or_insert_with(|| Box::new(SubAgentCli::new(n)));
                entry.as_mut()
            }
        }
    }

    /// Dispatch one event to the appropriate renderer.
    ///
    /// Events that don't carry rendering implications today
    /// (lifecycle markers, persistence-only events, deferred
    /// `MessageStart`/`MessageEnd` shapes) are intentional no-ops:
    /// the bridge is the legacy CLI's window onto the bus, not a
    /// universal sink. Listeners that need those events register
    /// separately.
    fn handle(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::Notice { agent_id, text } => {
                self.renderer_for(*agent_id).display_notice(text);
            }
            AgentEvent::Warning { agent_id, text } => {
                self.renderer_for(*agent_id).display_warning(text);
            }
            AgentEvent::Error { agent_id, text } => {
                self.renderer_for(*agent_id).display_error(text);
            }
            AgentEvent::StreamRetry {
                agent_id,
                attempt: _,
                delay: _,
                error,
            } => {
                // The agent already emits a separate `Error` event
                // with the human-readable retry message before the
                // matching `StreamRetry` fires (see
                // `Agent::execute_turn`'s overload-retry branch).
                // The legacy CLI rendered that error directly; the
                // structured backoff fields are surfaced for future
                // TUI consumers (a "retrying… (attempt N, in Ns)"
                // indicator) and aren't shown here so we don't double-
                // print. Holding the variant in the bridge keeps
                // exhaustiveness checks alive without altering output.
                let _ = error;
                let _ = agent_id;
            }
            AgentEvent::StreamChunk {
                agent_id,
                channel,
                action,
            } => {
                let renderer = self.renderer_for(*agent_id);
                match (channel, action) {
                    (StreamChannel::Text, StreamAction::Start { snapshot }) => {
                        renderer.agent_text_start(snapshot);
                    }
                    (StreamChannel::Text, StreamAction::Update { delta }) => {
                        renderer.agent_text_update(delta);
                    }
                    (StreamChannel::Text, StreamAction::Stop { snapshot }) => {
                        renderer.agent_text_stop(snapshot);
                    }
                    (StreamChannel::Thinking, StreamAction::Start { snapshot }) => {
                        renderer.agent_thinking_start(snapshot);
                    }
                    (StreamChannel::Thinking, StreamAction::Update { delta }) => {
                        renderer.agent_thinking_update(delta);
                    }
                    (StreamChannel::Thinking, StreamAction::Stop { snapshot: _ }) => {
                        // Thinking has no terminal text on the legacy
                        // streaming layer; the renderer just flushes
                        // a newline.
                        renderer.agent_thinking_stop();
                    }
                }
            }
            AgentEvent::TurnUsage { agent_id, usage } => {
                self.renderer_for(*agent_id).display_token_usage(usage);
            }
            AgentEvent::ToolExecutionEnd {
                agent_id,
                call_id: _,
                tool,
                result,
                is_error,
            } => {
                let renderer = self.renderer_for(*agent_id);
                render_details_via_ui(renderer, tool, result, *is_error);
            }
            // Tool-execution start fires before the call runs and
            // historically didn't produce rendered output (the legacy
            // CLI only prints a header alongside the result). Keep
            // it as a structural beat; the TUI listener will render
            // a "running…" placeholder here.
            AgentEvent::ToolExecutionStart { .. } => {}
            // Tool-update events drive incremental rendering for tools
            // that emit progress (today only `bash`); the legacy CLI
            // doesn't render them (it only shows the final result).
            AgentEvent::ToolExecutionUpdate { .. } => {}
            // Sub-agent start/end are correlation markers. The legacy
            // CLI doesn't print anything for them — the sub-agent's
            // own bus events (rendered via `SubAgentCli` above)
            // already carry the visible output.
            AgentEvent::SubAgentStart { .. } | AgentEvent::SubAgentEnd { .. } => {}
            // Lifecycle and message-lifecycle events have no legacy
            // rendering equivalents; future listeners (TUI, JSONL
            // print mode) consume them.
            AgentEvent::AgentStart { .. }
            | AgentEvent::AgentEnd { .. }
            | AgentEvent::TurnStart { .. }
            | AgentEvent::TurnEnd { .. }
            | AgentEvent::MessageStart { .. }
            | AgentEvent::MessageUpdate { .. }
            | AgentEvent::MessageEnd { .. }
            | AgentEvent::QueueUpdate { .. } => {}
        }
    }
}

/// Build a bridge listener for the given main renderer.
///
/// Convenience constructor: returns the [`Listener`] directly so the
/// binary can `agent.subscribe(build_event_bridge_listener(ui))` and
/// stash the resulting [`aj_agent::bus::SubscriptionHandle`].
pub fn build_event_bridge_listener(main: Box<dyn AjUi>) -> Listener {
    EventBridgeListener::new(main).into_listener()
}
