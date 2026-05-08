//! Bus listener that drives the `AjCliCommon` rendering off of
//! [`AgentEvent`]s.
//!
//! Per `docs/aj-next-plan.md` §2.3, the agent emits typed events
//! through its internal bus and the binary registers a listener that
//! translates those events into [`crate::cli_common::AjCliCommon`]
//! display calls. Once the bus drives rendering end-to-end, the
//! agent no longer touches a UI abstraction from inside
//! `execute_turn`.
//!
//! After §2.6 there is no [`AjUi`](crate::cli::AjCli) trait; the
//! listener owns concrete [`AjCliCommon`] instances directly. The
//! main agent's renderer is supplied at construction time;
//! sub-agent renderers are built lazily on first reference, with a
//! `(sub agent N)` prefix and a more compact rendering profile (no
//! full-output dump, no streaming) so the parent's pane stays
//! readable while the child runs.
//!
//! The listener:
//!
//! - Owns one [`AjCliCommon`] for the main agent and a
//!   `HashMap<usize, AjCliCommon>` for any [`AgentId::Sub(n)`]
//!   sub-agents that have appeared. Sub-agents share the parent's
//!   bus per `docs/aj-next-plan.md` §1.6, so all of their events
//!   flow here too.
//! - Reuses [`render_details_via_common`] for
//!   [`AgentEvent::ToolExecutionEnd`] so the rendered output is
//!   byte-identical to what the legacy CLI produced before the bus
//!   drove rendering — the same `display_tool_*` calls, just keyed
//!   off the structured [`aj_agent::tool::ToolDetails`] payload the
//!   tool returned.
//! - Synchronously calls every render method, then resolves the
//!   `Future` immediately. The bus awaits each listener inline, so
//!   rendering is sequenced: the agent never moves on to the next
//!   event before the previous frame has hit stdout.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aj_agent::bus::Listener;
use aj_agent::events::{AgentEvent, AgentId, StreamAction, StreamChannel};
use aj_agent::tool::ToolDetails;

use crate::cli_common::AjCliCommon;

/// Bus listener that drives [`AjCliCommon`] rendering for every
/// [`AgentEvent`] flowing on the agent's bus.
///
/// Construct with [`EventBridgeListener::new`] (passing the main
/// renderer the binary already built), then convert into a
/// [`Listener`] via [`EventBridgeListener::into_listener`] and pass
/// to `agent.subscribe(...)`. The returned listener captures the
/// bridge state behind an `Arc<Mutex<_>>` so dropping the
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
    /// own [`crate::cli::AjCli`] so input-loop and bus-driven
    /// output share the same rendering profile.
    main: AjCliCommon,
    /// Renderers keyed by sub-agent index. Built lazily — the
    /// bridge listener has no way to predict how many sub-agents
    /// the session will spawn, and unused entries would just hold
    /// empty state.
    subs: HashMap<usize, AjCliCommon>,
}

impl EventBridgeListener {
    /// Build a fresh bridge listener with `main` as the renderer
    /// for [`AgentId::Main`] events.
    pub fn new(main: AjCliCommon) -> Self {
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
    /// constructs an [`AjCliCommon`] on first reference for any
    /// previously-unseen `Sub(n)`. Sub-agent renderers run with a
    /// `(sub agent N)` prefix and the compact output profile that
    /// the legacy CLI used for nested transcripts.
    fn renderer_for(&mut self, agent_id: AgentId) -> &AjCliCommon {
        match agent_id {
            AgentId::Main => &self.main,
            AgentId::Sub(n) => self.subs.entry(n).or_insert_with(|| {
                AjCliCommon::new(
                    Some(format!("(sub agent {n})")),
                    /* show_full_tool_output = */ false,
                    /* enable_streaming = */ false,
                )
            }),
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
                    (StreamChannel::User, StreamAction::Start { snapshot }) => {
                        renderer.user_text_start(snapshot);
                    }
                    (StreamChannel::User, StreamAction::Update { delta }) => {
                        renderer.user_text_update(delta);
                    }
                    (StreamChannel::User, StreamAction::Stop { snapshot }) => {
                        renderer.user_text_stop(snapshot);
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
                render_details_via_common(renderer, tool, result, *is_error);
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
            // own bus events (rendered via the lazy sub-agent
            // [`AjCliCommon`] above) already carry the visible
            // output.
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
            // The persistence listener owns `MessagePersisted`; the
            // legacy CLI bridge has nothing to do with it (the
            // textual rendering of tool results already flows through
            // `ToolExecutionEnd`, and the assistant message is
            // already rendered live via `StreamChunk`).
            | AgentEvent::MessagePersisted { .. }
            | AgentEvent::QueueUpdate { .. } => {}
        }
    }
}

/// Build a bridge listener for the given main renderer.
///
/// Convenience constructor: returns the [`Listener`] directly so the
/// binary can `agent.subscribe(build_event_bridge_listener(ui))` and
/// stash the resulting [`aj_agent::bus::SubscriptionHandle`].
pub fn build_event_bridge_listener(main: AjCliCommon) -> Listener {
    EventBridgeListener::new(main).into_listener()
}

/// Render a [`ToolDetails`] payload through the supplied
/// [`AjCliCommon`].
///
/// Each variant maps onto the closest matching `display_tool_*`
/// call. `Diff` errors fall back to a textual error display because
/// there is no `display_tool_error_diff`. Variants without a
/// dedicated legacy renderer (`Bash`, `SubAgentReport`, `Todos`,
/// `Json`) come through as a flattened `display_tool_result`.
fn render_details_via_common(
    ui: &AjCliCommon,
    tool_name: &str,
    details: &ToolDetails,
    is_error: bool,
) {
    match details {
        ToolDetails::Text { summary, body } => {
            if is_error {
                ui.display_tool_error(tool_name, summary, body);
            } else {
                ui.display_tool_result(tool_name, summary, body);
            }
        }
        ToolDetails::Diff {
            path,
            before,
            after,
        } => {
            if is_error {
                ui.display_tool_error(tool_name, path, "diff failed");
            } else {
                ui.display_tool_result_diff(tool_name, path, before, after);
            }
        }
        ToolDetails::Bash {
            command,
            stdout,
            stderr,
            exit_code,
            ..
        } => {
            let mut body = String::new();
            if !stdout.is_empty() {
                body.push_str(stdout);
            }
            if !stderr.is_empty() {
                if !body.is_empty() && !body.ends_with('\n') {
                    body.push('\n');
                }
                body.push_str(stderr);
            }
            if let Some(code) = exit_code {
                if !body.is_empty() && !body.ends_with('\n') {
                    body.push('\n');
                }
                body.push_str(&format!("[exit {code}]"));
            }
            if is_error {
                ui.display_tool_error(tool_name, command, &body);
            } else {
                ui.display_tool_result(tool_name, command, &body);
            }
        }
        ToolDetails::SubAgentReport {
            agent_id,
            task,
            report,
        } => {
            let header = format!("sub-agent {agent_id}: {task}");
            if is_error {
                ui.display_tool_error(tool_name, &header, report);
            } else {
                ui.display_tool_result(tool_name, &header, report);
            }
        }
        ToolDetails::Todos { items } => {
            // Reuse the canonical text rendering from the `todo`
            // tool module so the legacy CLI display matches what the
            // pre-migration code produced.
            let body = aj_tools::tools::todo::format_todo_list(items);
            if is_error {
                ui.display_tool_error(tool_name, "", &body);
            } else {
                ui.display_tool_result(tool_name, "", &body);
            }
        }
        ToolDetails::Json(value) => {
            let body = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
            if is_error {
                ui.display_tool_error(tool_name, "", &body);
            } else {
                ui.display_tool_result(tool_name, "", &body);
            }
        }
    }
}
