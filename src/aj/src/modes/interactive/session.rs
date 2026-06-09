//! Per-session state for the interactive TUI.
//!
//! A [`SessionWorld`] owns everything whose lifetime is one
//! conversation session: the agent, its sub-agent registry, the
//! conversation log, the bus subscriptions, and the event pump.
//! Session changes (`/new`, `/resume`) build a fresh world and
//! replace the old one wholesale instead of mutating shared state
//! back into a pristine shape — so per-session state (todo list,
//! usage counters, registered sub-agents) can never leak across
//! session boundaries.

use std::sync::Arc;

use aj_agent::bus::SubscriptionHandle;
use aj_agent::events::{AgentEvent, AgentId};
use aj_agent::types::UsageSummary;
use aj_agent::{Agent, SubAgentRegistry};
use aj_conf::{AgentEnv, Config};
use aj_models::ThinkingConfig;
use aj_models::provider::Provider;
use aj_models::registry::ModelInfo;
use aj_models::types::StreamOptions;
use aj_session::{
    ConversationLog, ConversationPersistence, ThreadFilter, persistence_listener,
    repair_interrupted_tool_uses, replay,
};
use aj_tools::{BuiltinToolOptions, get_builtin_tools};
use aj_tui::tui::Tui;
use anyhow::{Context, Result};
use tokio::sync::Mutex as TokioMutex;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::SYSTEM_PROMPT;
use crate::config::theme::{ThemeHandle, chat_theme};
use crate::modes::interactive::RunConfigSnapshot;
use crate::modes::interactive::apply_editor_agent_marker;
use crate::modes::interactive::components::chat_view::ChatView;
use crate::modes::interactive::components::header::Header;
use crate::modes::interactive::event_pump::EventPump;
use crate::modes::interactive::layout::SlotIndex;
use crate::modes::interactive::render_settings::RenderSettings;
use crate::modes::interactive::shutdown::build_usage_summary;

/// Construct a fresh, not-yet-shared [`Agent`] from the persisted
/// config and a provider bundle cloned out of the live run-config
/// snapshot.
///
/// `thinking` comes from the run-config snapshot rather than from
/// `config.thinking`, so a runtime `/thinking` change carries into
/// agents built for later sessions. The `AgentEnv` is read fresh,
/// so a new session picks up edits to AGENTS.md files and the
/// current date.
fn build_agent(
    config: &Config,
    provider: Arc<dyn Provider>,
    model_info: Arc<ModelInfo>,
    stream_options: StreamOptions,
    thinking: Option<ThinkingConfig>,
) -> Agent {
    let mut tools = get_builtin_tools(&BuiltinToolOptions {
        image_auto_resize: config.image_auto_resize,
    });
    if !config.disabled_tools.is_empty() {
        tools.retain(|tool| !config.disabled_tools.contains(&tool.name));
    }
    let mut agent = Agent::with_provider(
        AgentEnv::new(),
        SYSTEM_PROMPT,
        tools,
        config.disabled_tools.clone(),
        provider,
        model_info,
        stream_options,
        // Placeholder; overridden below with the run-config value so
        // a runtime `/thinking` change survives the session switch.
        None,
    );
    agent.set_block_images(config.image_block);
    agent.set_default_thinking(thinking);
    agent
}

/// How a session world comes into being and what the user sees
/// announced when it is installed.
pub enum SessionSpec {
    /// Mint a fresh conversation log.
    Create { entry: SessionEntry },
    /// Resume the identified log from disk.
    Resume {
        session_id: String,
        entry: SessionEntry,
    },
}

/// Whether the world is the process's first or replaces a previous
/// one; decides the header notice wording.
pub enum SessionEntry {
    Startup,
    Switch,
}

/// The header notice announced when a world is installed. The
/// wording depends on both how the log was obtained (create vs.
/// resume) and whether this is the process's first session.
fn header_notice(spec: &SessionSpec) -> &'static str {
    match spec {
        SessionSpec::Create {
            entry: SessionEntry::Startup,
        } => "Chat with AJ — Ctrl+C to quit",
        SessionSpec::Create {
            entry: SessionEntry::Switch,
        } => "Fresh session",
        SessionSpec::Resume {
            entry: SessionEntry::Startup,
            ..
        } => "Resuming conversation",
        SessionSpec::Resume {
            entry: SessionEntry::Switch,
            ..
        } => "Resumed conversation",
    }
}

/// Everything with session lifetime, built fresh on every session
/// change and never reseeded after construction. Dropping the world
/// drops the agent, its bus subscriptions, and the pump in one go.
pub struct SessionWorld {
    /// The session's agent, freshly constructed for this world.
    /// Shared because the submit handler spawns a task that holds it
    /// across `agent.prompt(...).await`.
    pub agent: Arc<TokioMutex<Agent>>,
    /// Sub-agent registry injected into `agent`; starts empty, so
    /// only sub-agents spawned in this session are promptable.
    pub registry: SubAgentRegistry,
    /// The session's on-disk conversation log, shared with the
    /// persistence listener.
    pub log: Arc<TokioMutex<ConversationLog>>,
    /// Convenience copy of the log's session id, readable without
    /// locking `log`.
    pub session_id: String,
    /// Maps agent events onto TUI component updates.
    pub pump: EventPump,
    /// Receiver side of the bus→channel forwarder feeding `pump`.
    pub event_rx: UnboundedReceiver<AgentEvent>,
    /// Keeps the bus→channel forwarder subscribed; dropped with the
    /// world.
    _event_handle: SubscriptionHandle,
    /// Keeps the persistence listener subscribed; dropped with the
    /// world.
    _persistence_handle: SubscriptionHandle,
}

impl SessionWorld {
    /// Build a world bound to the session `spec` describes. Performs
    /// everything that doesn't touch the TUI: log resolve
    /// (create/resume), interrupted-tool-use repair, agent
    /// construction off the live run-config snapshot, transcript /
    /// system-prompt / sub-agent-counter seeding, bus subscriptions,
    /// and pump construction.
    ///
    /// On error nothing is shared or installed, so a caller swapping
    /// sessions can keep its current world intact.
    pub(crate) fn build(
        config: &Config,
        run_config: &Arc<std::sync::Mutex<RunConfigSnapshot>>,
        render_settings: &RenderSettings,
        theme: &ThemeHandle,
        persistence: &ConversationPersistence,
        spec: &SessionSpec,
    ) -> Result<SessionWorld> {
        // Resolve the log.
        let mut log = match spec {
            SessionSpec::Create { .. } => ConversationLog::create(persistence)
                .context("failed to create a fresh conversation log")?,
            SessionSpec::Resume { session_id, .. } => {
                ConversationLog::resume(persistence, session_id)
                    .with_context(|| format!("failed to resume session {session_id}"))?
            }
        };

        // Repair any interrupted tool uses so the transcript ends
        // cleanly on a user-role message, then re-linearize from the
        // post-repair head to capture any synthesized tool_result
        // message.
        let messages = if let Some(head) = log.latest_leaf(ThreadFilter::USER) {
            let conversation = log.linearize(&head, ThreadFilter::USER);
            repair_interrupted_tool_uses(&mut log, &conversation)?;
            let head = log
                .latest_leaf(ThreadFilter::USER)
                .expect("post-repair head exists when pre-repair head did");
            log.linearize(&head, ThreadFilter::USER).agent_messages()
        } else {
            Vec::new()
        };

        // Build a fresh agent off the live run-config snapshot so
        // runtime `/model` and `/thinking` choices carry into the
        // new session.
        let (provider, model_info, stream_options, thinking) = {
            let cfg = run_config.lock().expect("run config mutex poisoned");
            (
                Arc::clone(&cfg.provider),
                Arc::clone(&cfg.model_info),
                cfg.stream_options.clone(),
                cfg.thinking.clone(),
            )
        };
        let mut agent = build_agent(config, provider, model_info, stream_options, thinking);

        // Resolve the system prompt: reuse a persisted one
        // (cache-warm resume) or assemble fresh from the env. On a
        // fresh log, freeze the assembled prompt as the root entry.
        let system_prompt = if let Some(persisted) = log.system_prompt() {
            persisted.to_string()
        } else {
            let assembled = agent.assemble_system_prompt();
            if log.is_empty() {
                log.set_system_prompt(assembled.clone())?;
            }
            assembled
        };
        agent.set_assembled_system_prompt(system_prompt);

        // Seed the transcript and the sub-agent counter (so freshly
        // minted ids don't collide with persisted sub-agent
        // subtrees). The agent isn't shared yet, so these are
        // construction, not mutation.
        agent.seed_messages(messages);
        if let Some(max_id) = log.max_agent_id() {
            agent.seed_sub_agent_counter(max_id);
        }

        // Fresh, empty registry: only sub-agents spawned in this
        // session become promptable.
        let registry = SubAgentRegistry::default();
        agent.set_sub_agent_registry(registry.clone());

        // Bus subscriptions: the channel forwarder feeds the pump in
        // the main loop; the persistence listener writes events into
        // the log. Seeding never emits bus events, so subscription
        // order relative to it is immaterial.
        let (event_handle, event_rx) = agent.subscribe_channel();
        let session_id = log.session_id().to_string();
        let log = Arc::new(TokioMutex::new(log));
        let persistence_handle = agent.subscribe(persistence_listener(Arc::clone(&log)));

        let context_window = agent.model_info().context_window;
        let pump = EventPump::new(chat_theme(theme), render_settings.clone(), context_window);

        Ok(SessionWorld {
            agent: Arc::new(TokioMutex::new(agent)),
            registry,
            log,
            session_id,
            pump,
            event_rx,
            _event_handle: event_handle,
            _persistence_handle: persistence_handle,
        })
    }

    /// Bind this world to the TUI: clear the chat scrollback, reset
    /// the editor's agent marker to the main agent, seed the footer,
    /// replay the log through the pump, and refresh the header with
    /// the session id and the entry notice.
    ///
    /// Replay never hits the bus, so persistence isn't
    /// double-written.
    pub async fn install(&mut self, tui: &mut Tui, spec: &SessionSpec) {
        if let Some(chat) = tui.get_mut_as::<ChatView>(SlotIndex::Chat.idx()) {
            chat.reset();
        }
        apply_editor_agent_marker(tui, AgentId::Main);
        // Seed the footer's `?/<window>` indicator before replay so
        // it is visible even when replay produces no usage events.
        self.pump.sync_footer(tui);
        {
            let log = self.log.lock().await;
            for event in replay(&log) {
                self.pump.handle(tui, &event);
            }
        }
        if let Some(header) = tui.get_mut_as::<Header>(SlotIndex::Header.idx()) {
            header.set_session_id(Some(self.session_id.clone()));
            header.set_notice(Some(header_notice(spec).to_string()));
        }
        tui.request_render();
    }

    /// Snapshot this world's accumulated token usage for the
    /// shutdown banner. Locks the agent, so call only while no turn
    /// is in flight.
    pub async fn usage_summary(&self) -> UsageSummary {
        let agent = self.agent.lock().await;
        build_usage_summary(&agent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_notice_picks_wording_per_entry_and_kind() {
        assert_eq!(
            header_notice(&SessionSpec::Create {
                entry: SessionEntry::Startup
            }),
            "Chat with AJ — Ctrl+C to quit"
        );
        assert_eq!(
            header_notice(&SessionSpec::Resume {
                session_id: "s".to_string(),
                entry: SessionEntry::Startup
            }),
            "Resuming conversation"
        );
        assert_eq!(
            header_notice(&SessionSpec::Resume {
                session_id: "s".to_string(),
                entry: SessionEntry::Switch
            }),
            "Resumed conversation"
        );
        assert_eq!(
            header_notice(&SessionSpec::Create {
                entry: SessionEntry::Switch
            }),
            "Fresh session"
        );
    }
}
