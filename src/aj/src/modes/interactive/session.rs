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
use aj_agent::queue::MessageQueues;
use aj_agent::types::UsageSummary;
use aj_agent::{Agent, AgentSeed, SubAgentRegistry, TaskRegistry};
use aj_conf::{AgentEnv, Config, ConfigSpeed};
use aj_models::auth::AuthStorage;
use aj_models::provider::Provider;
use aj_models::registry::{ModelInfo, ModelRegistry, validate_thinking_level};
use aj_models::types::{Speed, StreamOptions, ThinkingLevel};
use aj_models::{ThinkingConfig, speed_name, thinking_config_from_name, thinking_config_name};
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
use crate::modes::interactive::apply_editor_agent_marker;
use crate::modes::interactive::components::chat_view::ChatView;
use crate::modes::interactive::components::header::Header;
use crate::modes::interactive::event_pump::EventPump;
use crate::modes::interactive::layout::SlotIndex;
use crate::modes::interactive::render_settings::RenderSettings;
use crate::modes::interactive::shutdown::build_usage_summary;
use crate::modes::interactive::{RunConfigSnapshot, SubAgentOverrides};

/// Construct a fresh, not-yet-shared [`Agent`] from the persisted
/// config and a provider bundle cloned out of the live run-config
/// snapshot.
///
/// `thinking` comes from the run-config snapshot rather than from
/// `config.thinking`, so a runtime `/thinking` change carries into
/// agents built for later sessions. The `AgentEnv` is read fresh,
/// so a new session picks up edits to AGENTS.md files, a system
/// prompt override, and the current date.
fn build_agent(
    config: &Config,
    provider: Arc<dyn Provider>,
    model_info: Arc<ModelInfo>,
    stream_options: StreamOptions,
    thinking: Option<ThinkingConfig>,
    speed: Option<Speed>,
) -> Agent {
    let mut tools = get_builtin_tools(&BuiltinToolOptions {
        image_auto_resize: config.image_auto_resize,
    });
    if !config.disabled_tools.is_empty() {
        tools.retain(|tool| !config.disabled_tools.contains(&tool.name));
    }
    let mut agent = Agent::with_provider(
        AgentEnv::new(SYSTEM_PROMPT, &config.disabled_skills),
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
    agent.set_speed(speed);
    agent
}

/// Dependencies for resume-time settings restoration: the model
/// catalog to resolve recorded `(provider, model_id)` pairs against,
/// and the credential store backing the rebuilt bundle's lazy
/// API-key resolver. `None` at the [`SessionWorld::build`] call site
/// (scripted mode, tests) disables restoration.
pub struct RestoreContext {
    pub registry: Arc<ModelRegistry>,
    pub auth: AuthStorage,
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
fn header_notice(spec: &SessionSpec) -> String {
    match spec {
        SessionSpec::Create {
            entry: SessionEntry::Startup,
        } => format!(
            "Chat with AJ — {} to quit",
            crate::config::keybindings::fixed_keys::CTRL_C
        ),
        SessionSpec::Create {
            entry: SessionEntry::Switch,
        } => "Fresh session".to_string(),
        SessionSpec::Resume {
            entry: SessionEntry::Startup,
            ..
        } => "Resuming conversation".to_string(),
        SessionSpec::Resume {
            entry: SessionEntry::Switch,
            ..
        } => "Resumed conversation".to_string(),
    }
}

/// Project a [`ThinkingConfig`] onto the wire-level
/// [`ThinkingLevel`] for validation against a model's effort
/// vocabulary. One-to-one, mirroring the projection the agent
/// applies before each inference.
pub(crate) fn thinking_level_for(level: &ThinkingConfig) -> ThinkingLevel {
    match level {
        ThinkingConfig::Low => ThinkingLevel::Low,
        ThinkingConfig::Medium => ThinkingLevel::Medium,
        ThinkingConfig::High => ThinkingLevel::High,
        ThinkingConfig::XHigh => ThinkingLevel::XHigh,
        ThinkingConfig::Max => ThinkingLevel::Max,
    }
}

/// Write a resumed session's recorded settings back into the shared
/// run config, per the resume precedence: the log's record wins over
/// the current defaults; an axis the log doesn't record keeps the
/// current value. Returns the user-facing notices describing what
/// was restored or why a recorded value was kept out.
///
/// Speed is restored first because the model bundle rebuilds below
/// stamp speed-derived headers. Auth is deliberately not checked:
/// key resolution is lazy (see `crate::model`), so an uncredentialed
/// restored provider surfaces at the next turn, where the user can
/// `/login`.
fn restore_session_settings(
    config: &Config,
    run_config: &Arc<std::sync::Mutex<RunConfigSnapshot>>,
    settings: &aj_session::SessionSettings,
    restore: &RestoreContext,
) -> Vec<String> {
    let mut notices = Vec::new();
    let mut cfg = run_config.lock().expect("run config mutex poisoned");

    // Speed. `None` and `Some(Standard)` are equivalent on the wire,
    // so changes are tracked by canonical name.
    let prior_speed_name = speed_name(cfg.speed);
    if let Some(s) = settings.speed.as_deref() {
        match s.parse::<ConfigSpeed>() {
            Ok(ConfigSpeed::Standard) => cfg.speed = Some(Speed::Standard),
            Ok(ConfigSpeed::Fast) => cfg.speed = Some(Speed::Fast),
            Err(_) => notices.push(format!(
                "Session recorded unknown speed {s:?}; keeping {prior_speed_name}."
            )),
        }
    }

    // Model. Skipped when the record matches the active bundle —
    // except that a restored speed still needs the bundle's headers
    // rebuilt to match.
    let model_changed = settings.model.as_ref().is_some_and(|(prov, id)| {
        (prov.as_str(), id.as_str()) != (&*cfg.model_key.0, &*cfg.model_key.1)
    });
    if model_changed {
        let (prov, id) = settings.model.as_ref().expect("checked above");
        let resolved = restore
            .registry
            .get(prov, id)
            .cloned()
            .context("not in the model catalog")
            .and_then(|info| crate::model::from_model_info(&restore.auth, info, cfg.speed));
        match resolved {
            Ok(resolved) => {
                let name = resolved.model_info.name.clone();
                cfg.provider = resolved.provider;
                cfg.model_info = resolved.model_info;
                cfg.stream_options = resolved.stream_options;
                crate::model::apply_thinking_display(
                    &mut cfg.stream_options,
                    config.thinking_display,
                );
                cfg.model_key = (prov.clone(), id.clone());
                notices.push(format!("Restored model {name} ({prov}/{id}) from session."));
            }
            Err(err) => {
                tracing::warn!("could not restore session model {prov}/{id}: {err:#}");
                notices.push(format!(
                    "Session used {prov}/{id}, which is not available; continuing with {}/{}.",
                    cfg.model_key.0, cfg.model_key.1
                ));
            }
        }
    } else if speed_name(cfg.speed) != prior_speed_name {
        // Same model, different speed: rebuild the bundle so the
        // stream options carry the restored speed's headers.
        match crate::model::from_model_info(&restore.auth, (*cfg.model_info).clone(), cfg.speed) {
            Ok(resolved) => {
                cfg.provider = resolved.provider;
                cfg.model_info = resolved.model_info;
                cfg.stream_options = resolved.stream_options;
                crate::model::apply_thinking_display(
                    &mut cfg.stream_options,
                    config.thinking_display,
                );
            }
            Err(err) => {
                tracing::warn!("could not rebuild bundle for restored speed: {err:#}");
                notices.push(format!(
                    "Couldn't apply the session's recorded speed: {err:#}"
                ));
            }
        }
    }

    // Thinking, validated against the (possibly just-restored)
    // model. Clamping rules are provider-specific, so a rejected
    // level keeps the current one rather than guessing a substitute.
    if let Some(level_str) = settings.thinking.as_deref() {
        let current = thinking_config_name(cfg.thinking.as_ref());
        match thinking_config_from_name(level_str) {
            None => notices.push(format!(
                "Session recorded unknown thinking level {level_str:?}; keeping {current}."
            )),
            Some(level) => {
                let validation = match &level {
                    None => Ok(()),
                    Some(tc) => validate_thinking_level(&cfg.model_info, &thinking_level_for(tc)),
                };
                match validation {
                    Ok(()) => cfg.thinking = level,
                    Err(msg) => notices.push(format!(
                        "Can't restore thinking level {level_str:?} ({msg}); keeping {current}."
                    )),
                }
            }
        }
    }

    notices
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
    /// Background-task registry injected into `agent`; shared with
    /// the main loop so the wake triggers can poll notices and
    /// shutdown can kill the task tree. Per-world; `run_session`
    /// shuts it down on every exit (quit, fatal error, session
    /// switch), so tasks never outlive their world.
    pub task_registry: TaskRegistry,
    /// Shared steering / follow-up queues injected into `agent` (and
    /// its sub-agents). The main loop's input handlers enqueue onto
    /// them and the wake triggers poll [`MessageQueues::has_pending`].
    /// Per-world, like the agent itself.
    pub message_queues: MessageQueues,
    /// Loop-side staged settings overrides, keyed by sub-agent id.
    /// The `/model` / `/thinking` selectors write entries when the
    /// user changes a sub-agent's settings; the submit handler's
    /// turn task re-applies them at every turn start (see
    /// `apply_turn_config`). Sub ids are per-session, so the map
    /// resets naturally with the world. A sub-agent with no entry
    /// runs with whatever it already holds (spawn-time inheritance).
    pub(crate) sub_overrides:
        Arc<std::sync::Mutex<std::collections::HashMap<usize, SubAgentOverrides>>>,
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
    /// Notices produced by resume-time settings restoration (what
    /// was restored, or why a recorded value was kept out). Pumped
    /// onto the chat scrollback by the caller after `install`.
    pub restore_notices: Vec<String>,
}

impl SessionWorld {
    /// Build a world bound to the session `spec` describes. Performs
    /// everything that doesn't touch the TUI: log resolve
    /// (create/resume), interrupted-tool-use repair, resume-time
    /// settings restoration (when `restore` is supplied, the log's
    /// recorded model/thinking/speed are written back into the
    /// shared run-config snapshot before the agent is built), agent
    /// construction off the run-config snapshot, transcript /
    /// system-prompt / sub-agent-counter seeding, bus subscriptions,
    /// and pump construction. A fresh log additionally gets its
    /// initial settings record so a later resume can restore it.
    ///
    /// On error nothing is shared or installed; the outer session
    /// loop in `InteractiveMode::run` falls back to resuming the
    /// previous session.
    pub(crate) fn build(
        config: &Config,
        run_config: &Arc<std::sync::Mutex<RunConfigSnapshot>>,
        render_settings: &RenderSettings,
        theme: &ThemeHandle,
        persistence: &ConversationPersistence,
        spec: &SessionSpec,
        restore: Option<&RestoreContext>,
        catalog: Arc<Vec<ModelInfo>>,
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
        // message. On a resume, the linearized path also carries the
        // session's recorded settings: restore them into the shared
        // run config (when restoration is enabled) before the agent
        // is built off it.
        let mut restore_notices = Vec::new();
        let messages = if let Some(head) = log.latest_leaf(ThreadFilter::USER) {
            let conversation = log.linearize(&head, ThreadFilter::USER);
            repair_interrupted_tool_uses(&mut log, &conversation)?;
            let head = log
                .latest_leaf(ThreadFilter::USER)
                .expect("post-repair head exists when pre-repair head did");
            let conversation = log.linearize(&head, ThreadFilter::USER);
            if let (SessionSpec::Resume { .. }, Some(restore)) = (spec, restore) {
                restore_notices =
                    restore_session_settings(config, run_config, &conversation.settings(), restore);
            }
            conversation.agent_messages()
        } else {
            Vec::new()
        };

        // Build a fresh agent off the run-config snapshot, which at
        // this point reflects both runtime `/model` / `/thinking`
        // choices and any settings just restored from the resumed
        // log.
        let (provider, model_info, stream_options, thinking, speed, model_key) = {
            let cfg = run_config.lock().expect("run config mutex poisoned");
            (
                Arc::clone(&cfg.provider),
                Arc::clone(&cfg.model_info),
                cfg.stream_options.clone(),
                cfg.thinking.clone(),
                cfg.speed,
                cfg.model_key.clone(),
            )
        };
        let mut agent = build_agent(
            config,
            provider,
            model_info,
            stream_options,
            thinking.clone(),
            speed,
        );

        // Resolve the system prompt: reuse a persisted one
        // (cache-warm resume) or assemble fresh from the env. On a
        // fresh log, freeze the assembled prompt as the root entry
        // and record the active settings so a later resume restores
        // them even if the defaults change in between. Like the
        // prompt itself, the settings entries buffer until the
        // first real message lands.
        let system_prompt = if let Some(persisted) = log.system_prompt() {
            persisted.to_string()
        } else {
            let assembled = agent.assemble_system_prompt();
            if log.is_empty() {
                log.set_system_prompt(assembled.clone())?;
                log.append_model_change(ThreadFilter::USER, &model_key.0, &model_key.1)?;
                log.append_thinking_change(
                    ThreadFilter::USER,
                    thinking_config_name(thinking.as_ref()),
                )?;
                log.append_speed_change(ThreadFilter::USER, speed_name(speed))?;
            }
            assembled
        };

        // One-shot session seed: transcript, frozen system prompt,
        // and the sub-agent counter floor (so freshly minted ids
        // don't collide with persisted sub-agent subtrees; a fresh
        // log has no subtrees and seeds the counter's initial 0).
        agent.seed_session(AgentSeed {
            transcript: messages,
            assembled_system_prompt: Some(system_prompt),
            sub_agent_counter: log.max_agent_id().unwrap_or(0),
        });

        // Fresh, empty registry: only sub-agents spawned in this
        // session become promptable.
        let registry = SubAgentRegistry::default();
        agent.set_sub_agent_registry(registry.clone());

        // Fresh task registry, shared with the main loop's wake
        // triggers; its session-scoped cancellation root is fired
        // when the world winds down.
        let task_registry = TaskRegistry::default();
        agent.set_task_registry(task_registry.clone());

        // Fresh, shared steering / follow-up queues: the TUI input
        // handlers enqueue onto them while a turn runs, the agent
        // drains them, and the pump reads them to paint the
        // pending-message box.
        let message_queues = MessageQueues::default();
        agent.set_message_queues(message_queues.clone());

        // Bus subscriptions: the channel forwarder feeds the pump in
        // the main loop; the persistence listener writes events into
        // the log. Seeding never emits bus events, so subscription
        // order relative to it is immaterial.
        let (event_handle, event_rx) = agent.subscribe_channel();
        let session_id = log.session_id().to_string();
        let log = Arc::new(TokioMutex::new(log));
        let persistence_handle = agent.subscribe(persistence_listener(Arc::clone(&log)));

        let main_settings = aj_agent::events::AgentSettings {
            provider: model_key.0.clone(),
            model_id: model_key.1.clone(),
            thinking: thinking_config_name(thinking.as_ref()).to_string(),
            speed: speed_name(speed).to_string(),
        };
        let context_window = agent.model_info().context_window;
        let pump = EventPump::new(
            chat_theme(theme, config.syntax_highlighting),
            render_settings.clone(),
            main_settings,
            context_window,
            catalog,
            message_queues.clone(),
        );

        Ok(SessionWorld {
            agent: Arc::new(TokioMutex::new(agent)),
            registry,
            task_registry,
            message_queues,
            sub_overrides: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            log,
            session_id,
            pump,
            event_rx,
            _event_handle: event_handle,
            _persistence_handle: persistence_handle,
            restore_notices,
        })
    }

    /// Bind this world to the TUI: clear the chat scrollback, reset
    /// the editor's agent marker to the main agent, seed the footer
    /// (model line and `?/<window>` indicator), replay the log
    /// through the pump, and refresh the header with the session id
    /// and the entry notice.
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
            // Clone the handle so the guard doesn't borrow `self`,
            // which the pump calls below need mutably.
            let log = Arc::clone(&self.log);
            let log = log.lock().await;
            for event in replay(&log) {
                self.pump.handle(tui, &event);
            }
            self.reconcile_sub_agent_settings(tui, &log);
        }
        if let Some(header) = tui.get_mut_as::<Header>(SlotIndex::Header.idx()) {
            header.set_session_id(Some(self.session_id.clone()));
            header.set_notice(Some(header_notice(spec)));
        }
        tui.request_render();
    }

    /// Overwrite each replayed sub-agent's footer entry with the
    /// last-wins settings fold of its log thread. Replay seeds the
    /// entries from the spawn-time snapshot only; later
    /// `ModelChange` / `ThinkingChange` / `SpeedChange` entries on a
    /// sub thread are projected as plain notices, so without this
    /// pass a resumed footer would show stale spawn-time settings.
    ///
    /// Axes the log doesn't record keep the replayed entry's value
    /// (the spawn snapshot, or replay's fallback defaults for
    /// legacy logs). Sub ids with no thread entries are skipped.
    fn reconcile_sub_agent_settings(&mut self, tui: &mut Tui, log: &ConversationLog) {
        let Some(max_id) = log.max_agent_id() else {
            return;
        };
        for n in 1..=max_id {
            let filter = ThreadFilter::subagent(n);
            let Some(head) = log.latest_leaf(filter) else {
                continue;
            };
            let folded = log.linearize(&head, filter).settings();
            let id = AgentId::Sub(n);
            let base = self.pump.agent_settings(id).cloned().unwrap_or_else(|| {
                aj_agent::events::AgentSettings {
                    provider: String::new(),
                    model_id: String::new(),
                    thinking: "off".to_string(),
                    speed: "standard".to_string(),
                }
            });
            let (provider, model_id) = folded.model.unwrap_or((base.provider, base.model_id));
            let settings = aj_agent::events::AgentSettings {
                provider,
                model_id,
                thinking: folded.thinking.unwrap_or(base.thinking),
                speed: folded.speed.unwrap_or(base.speed),
            };
            self.pump.reconcile_agent_settings(tui, id, settings);
        }
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
    use std::time::Duration;

    use aj_agent::bus::EventBus;
    use aj_agent::events::AgentSettings;
    use aj_agent::message::AgentMessage;
    use aj_models::scripted::ScriptedProvider;
    use aj_models::types::{Message, UserMessage};
    use aj_tui::component::Component;
    use tempfile::TempDir;

    use super::*;
    use crate::config::theme::Theme;
    use crate::modes::interactive::layout::build_layout;
    use crate::modes::interactive::test_support::{
        StubTerminal, build_test_world, create_spec, drive_turn, finalized_text_message,
        one_turn_session, resume_spec, scripted_model_info, scripted_run_config,
    };

    /// Strip the `dim` SGR codes the header wraps its banner in so
    /// substring assertions see the visible text contiguously.
    fn strip_dim(line: &str) -> String {
        line.replace("\x1b[2m", "").replace("\x1b[22m", "")
    }

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

    #[tokio::test]
    async fn create_seeds_the_logs_system_prompt() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let run_config = scripted_run_config(Vec::new());

        let world =
            build_test_world(&persistence, &run_config, &create_spec()).expect("create world");

        let log = world.log.lock().await;
        let persisted = log.system_prompt().expect("system prompt frozen on create");
        let agent = world.agent.lock().await;
        let held = agent
            .assembled_system_prompt()
            .expect("agent holds the assembled prompt");
        assert_eq!(persisted, held);
    }

    #[tokio::test]
    async fn resume_round_trips_transcript_prompt_and_rendering() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let session_id = one_turn_session(&persistence, "hello there", "scripted reply").await;

        // Fresh provider: the original script is exhausted, and a
        // resumed world that never prompts needs no scripts anyway.
        let run_config = scripted_run_config(Vec::new());
        let spec = resume_spec(&session_id);
        let mut world = build_test_world(&persistence, &run_config, &spec).expect("resume world");

        {
            let agent = world.agent.lock().await;
            let transcript = format!("{:?}", agent.messages());
            assert!(!agent.messages().is_empty(), "transcript seeded");
            assert!(transcript.contains("hello there"), "user prompt seeded");
            assert!(transcript.contains("scripted reply"), "reply seeded");

            let log = world.log.lock().await;
            assert_eq!(
                log.system_prompt().expect("persisted prompt"),
                agent
                    .assembled_system_prompt()
                    .expect("agent holds the persisted prompt"),
                "resume reuses the persisted prompt byte-for-byte"
            );
        }

        // Install renders the prior conversation into the chat slot.
        let mut tui = Tui::new(Box::new(StubTerminal));
        build_layout(&mut tui, &ThemeHandle::new(Theme::bundled_dark()), true);
        world.install(&mut tui, &spec).await;
        let chat = tui
            .get_mut_as::<ChatView>(SlotIndex::Chat.idx())
            .expect("chat slot present");
        let rendered = chat.render(100).join("\n");
        assert!(rendered.contains("hello there"), "prompt rendered");
        assert!(rendered.contains("scripted reply"), "reply rendered");
    }

    #[tokio::test]
    async fn rebuild_starts_with_an_empty_registry_and_zero_usage() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());

        let run_config = scripted_run_config(vec![finalized_text_message("scripted reply")]);
        let world_a =
            build_test_world(&persistence, &run_config, &create_spec()).expect("create world");
        drive_turn(&world_a, "hello there").await;

        // Pollute world A's registry with a throwaway sub-agent.
        let throwaway = build_agent(
            &Config::default(),
            Arc::new(ScriptedProvider::from_messages(
                Vec::new(),
                0,
                Duration::ZERO,
            )),
            Arc::new(scripted_model_info()),
            StreamOptions::default(),
            None,
            None,
        );
        world_a
            .registry
            .insert(7, Arc::new(TokioMutex::new(throwaway)));
        assert_eq!(world_a.registry.ids(), vec![7]);

        let session_id = world_a.session_id.clone();
        drop(world_a);

        let run_config = scripted_run_config(Vec::new());
        let world_b = build_test_world(&persistence, &run_config, &resume_spec(&session_id))
            .expect("resume world");

        assert!(world_b.registry.ids().is_empty(), "registry starts empty");
        let total = world_b.usage_summary().await.total_usage;
        assert_eq!(total.input_tokens, 0);
        assert_eq!(total.output_tokens, 0);
        assert_eq!(total.cache_write_tokens, 0);
        assert_eq!(total.cache_read_tokens, 0);
    }

    #[tokio::test]
    async fn sub_agent_counter_floor_persists_across_resume() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());

        let run_config = scripted_run_config(vec![finalized_text_message("scripted reply")]);
        let world_a =
            build_test_world(&persistence, &run_config, &create_spec()).expect("create world");
        drive_turn(&world_a, "hello there").await;

        // Synthesize a persisted sub-agent subtree under id 3 by
        // driving the persistence listener directly — the same
        // events a real sub-agent spawn would emit on the bus.
        let bus = EventBus::new();
        let _h = bus.subscribe(persistence_listener(Arc::clone(&world_a.log)));
        bus.emit(AgentEvent::SubAgentStart {
            parent: AgentId::Main,
            child: AgentId::Sub(3),
            task: "synthetic task".to_string(),
            settings: AgentSettings {
                provider: "scripted".into(),
                model_id: "scripted-model".into(),
                thinking: "off".into(),
                speed: "standard".into(),
            },
        })
        .await
        .expect("emit start");
        bus.emit(AgentEvent::MessageEnd {
            agent_id: AgentId::Sub(3),
            message: AgentMessage::wire(Message::User(UserMessage::text("synthetic task"))),
        })
        .await
        .expect("emit sub message");

        let session_id = world_a.session_id.clone();
        drop(world_a);

        let run_config = scripted_run_config(Vec::new());
        let world_b = build_test_world(&persistence, &run_config, &resume_spec(&session_id))
            .expect("resume world");

        // `build` seeds the agent's counter from this value; the
        // resumed agent therefore mints ids strictly greater than 3
        // (mint-side behavior covered by the `SessionState` unit
        // test in `aj-agent`).
        assert_eq!(world_b.log.lock().await.max_agent_id(), Some(3));
    }

    #[tokio::test]
    async fn install_reconciles_sub_agent_footer_settings_from_log() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());

        let run_config = scripted_run_config(vec![finalized_text_message("scripted reply")]);
        let world_a =
            build_test_world(&persistence, &run_config, &create_spec()).expect("create world");
        drive_turn(&world_a, "hello there").await;

        // Persist two sub-agent threads via the same events a live
        // spawn would emit on the bus, both with the spawn-time
        // settings snapshot.
        let bus = EventBus::new();
        let _h = bus.subscribe(persistence_listener(Arc::clone(&world_a.log)));
        for n in [1usize, 2] {
            bus.emit(AgentEvent::SubAgentStart {
                parent: AgentId::Main,
                child: AgentId::Sub(n),
                task: format!("task {n}"),
                settings: AgentSettings {
                    provider: "scripted".into(),
                    model_id: "scripted-model".into(),
                    thinking: "off".into(),
                    speed: "standard".into(),
                },
            })
            .await
            .expect("emit start");
            bus.emit(AgentEvent::MessageEnd {
                agent_id: AgentId::Sub(n),
                message: AgentMessage::wire(Message::User(UserMessage::text(format!("task {n}")))),
            })
            .await
            .expect("emit sub message");
        }
        // Mid-session settings changes recorded on sub 1's thread
        // only; sub 2 keeps its spawn snapshot. Settings entries are
        // non-punctuation and only reach disk with the next
        // punctuation append, so a follow-up message flushes them.
        {
            let mut log = world_a.log.lock().await;
            log.append_thinking_change(ThreadFilter::subagent(1), "high")
                .expect("append thinking change");
            log.append_model_change(ThreadFilter::subagent(1), "anthropic", "claude-x")
                .expect("append model change");
        }
        bus.emit(AgentEvent::MessageEnd {
            agent_id: AgentId::Sub(1),
            message: AgentMessage::wire(Message::User(UserMessage::text("follow-up"))),
        })
        .await
        .expect("emit flush message");
        let session_id = world_a.session_id.clone();
        drop(world_a);

        let run_config = scripted_run_config(Vec::new());
        let spec = resume_spec(&session_id);
        let mut world = build_test_world(&persistence, &run_config, &spec).expect("resume world");
        let mut tui = Tui::new(Box::new(StubTerminal));
        build_layout(&mut tui, &ThemeHandle::new(Theme::bundled_dark()), true);
        world.install(&mut tui, &spec).await;

        let changed = world
            .pump
            .agent_settings(AgentId::Sub(1))
            .expect("sub 1 footer entry");
        assert_eq!(changed.provider, "anthropic");
        assert_eq!(changed.model_id, "claude-x");
        assert_eq!(changed.thinking, "high");
        assert_eq!(changed.speed, "standard");

        let unchanged = world
            .pump
            .agent_settings(AgentId::Sub(2))
            .expect("sub 2 footer entry");
        assert_eq!(unchanged.provider, "scripted");
        assert_eq!(unchanged.model_id, "scripted-model");
        assert_eq!(unchanged.thinking, "off");
        assert_eq!(unchanged.speed, "standard");
    }

    #[tokio::test]
    async fn install_announces_session_id_and_notice_per_spec() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let session_id = one_turn_session(&persistence, "hello there", "scripted reply").await;

        let specs = [
            (create_spec(), "Chat with AJ — Ctrl+C to quit"),
            (
                SessionSpec::Create {
                    entry: SessionEntry::Switch,
                },
                "Fresh session",
            ),
            (
                SessionSpec::Resume {
                    session_id: session_id.clone(),
                    entry: SessionEntry::Startup,
                },
                "Resuming conversation",
            ),
            (resume_spec(&session_id), "Resumed conversation"),
        ];
        for (spec, notice) in specs {
            let run_config = scripted_run_config(Vec::new());
            let mut world =
                build_test_world(&persistence, &run_config, &spec).expect("build world");
            let expected_id = world.session_id.clone();

            let mut tui = Tui::new(Box::new(StubTerminal));
            build_layout(&mut tui, &ThemeHandle::new(Theme::bundled_dark()), true);
            world.install(&mut tui, &spec).await;

            let header = tui
                .get_mut_as::<Header>(SlotIndex::Header.idx())
                .expect("header slot present");
            let banner = header
                .render(200)
                .iter()
                .map(|line| strip_dim(line))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                banner.contains(notice),
                "notice {notice:?} missing from banner: {banner:?}"
            );
            assert!(
                banner.contains(&expected_id),
                "session id {expected_id:?} missing from banner: {banner:?}"
            );
        }
    }

    #[test]
    fn resume_of_missing_session_fails_without_creating_files() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let run_config = scripted_run_config(Vec::new());

        let result = build_test_world(&persistence, &run_config, &resume_spec("does-not-exist"));
        assert!(result.is_err(), "resuming a missing session must fail");

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .map(|entries| entries.flatten().collect())
            .unwrap_or_default();
        assert!(
            leftovers.is_empty(),
            "failed build must not create session files: {leftovers:?}"
        );
    }

    use aj_models::registry::{Catalog, InputModality, ModelCost, OverridesFile};

    /// Catalog row for the restore tests. `anthropic-messages` so
    /// `from_model_info` finds a registered provider; non-reasoning
    /// so thinking validation accepts any level as a no-op.
    fn catalog_model(provider: &str, id: &str) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            name: id.into(),
            api: "anthropic-messages".into(),
            provider: provider.into(),
            base_url: "https://example.invalid".into(),
            reasoning: false,
            supports_adaptive_thinking: false,
            input: vec![InputModality::Text],
            cost: ModelCost::default(),
            context_window: 1_000,
            max_tokens: 100,
            headers: None,
        }
    }

    /// Restore context over an in-memory registry and a tempdir
    /// auth store (never touches `~/.aj`).
    fn restore_ctx(dir: &TempDir, models: Vec<ModelInfo>) -> RestoreContext {
        let catalog = Catalog {
            updated_at: 0,
            source: "test".into(),
            models,
        };
        let overrides = OverridesFile { overrides: vec![] };
        RestoreContext {
            registry: Arc::new(ModelRegistry::from_catalog_with_overrides(
                catalog,
                overrides,
                "test-catalog",
            )),
            auth: AuthStorage::new(dir.path().join("auth.json")),
        }
    }

    fn build_world_with_restore(
        persistence: &ConversationPersistence,
        run_config: &Arc<std::sync::Mutex<RunConfigSnapshot>>,
        spec: &SessionSpec,
        restore: &RestoreContext,
    ) -> Result<SessionWorld> {
        SessionWorld::build(
            &Config::default(),
            run_config,
            &crate::modes::interactive::render_settings::RenderSettings::new(false, false, true),
            &ThemeHandle::new(crate::config::theme::Theme::bundled_dark()),
            persistence,
            spec,
            Some(restore),
            Arc::new(Vec::new()),
        )
    }

    /// Assistant reply carrying a non-scripted model identity, so a
    /// resume sees that identity as the path's last model signal.
    fn reply_from(provider: &str, model: &str) -> aj_models::types::AssistantMessage {
        use crate::modes::interactive::test_support::finalized_text_message;
        let mut msg = finalized_text_message("scripted reply");
        msg.provider = provider.to_string();
        msg.model = model.to_string();
        msg
    }

    #[tokio::test]
    async fn create_seeds_initial_settings_record() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());
        let run_config = scripted_run_config(Vec::new());

        let world =
            build_test_world(&persistence, &run_config, &create_spec()).expect("create world");

        let log = world.log.lock().await;
        let head = log
            .latest_leaf(ThreadFilter::USER)
            .expect("seed entries give the user thread a leaf");
        let settings = log.linearize(&head, ThreadFilter::USER).settings();
        assert_eq!(
            settings.model,
            Some(("scripted".to_string(), "scripted".to_string()))
        );
        assert_eq!(settings.thinking.as_deref(), Some("off"));
        assert_eq!(settings.speed.as_deref(), Some("standard"));
    }

    #[tokio::test]
    async fn resume_restores_recorded_settings_into_run_config() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());

        // Record a thinking + speed change, then drive a turn whose
        // assistant reply carries the anthropic identity: the turn
        // flushes the buffered settings entries, and the assistant
        // message is the path's last model signal.
        let run_config = scripted_run_config(vec![reply_from("anthropic", "claude-x")]);
        let world =
            build_test_world(&persistence, &run_config, &create_spec()).expect("create world");
        {
            let mut log = world.log.lock().await;
            log.append_thinking_change(ThreadFilter::USER, "high")
                .expect("thinking change");
            log.append_speed_change(ThreadFilter::USER, "fast")
                .expect("speed change");
        }
        drive_turn(&world, "hello there").await;
        let session_id = world.session_id.clone();
        drop(world);

        let run_config = scripted_run_config(Vec::new());
        let ctx = restore_ctx(&dir, vec![catalog_model("anthropic", "claude-x")]);
        let world =
            build_world_with_restore(&persistence, &run_config, &resume_spec(&session_id), &ctx)
                .expect("resume world");

        let cfg = run_config.lock().expect("run config mutex poisoned");
        assert_eq!(
            cfg.model_key,
            ("anthropic".to_string(), "claude-x".to_string()),
            "restored model overwrites the run config"
        );
        assert_eq!(cfg.model_info.id, "claude-x");
        assert_eq!(cfg.thinking, Some(ThinkingConfig::High));
        assert_eq!(cfg.speed, Some(Speed::Fast));
        assert!(
            world
                .restore_notices
                .iter()
                .any(|n| n.contains("Restored model claude-x (anthropic/claude-x)")),
            "restore notice missing: {:?}",
            world.restore_notices
        );
    }

    #[tokio::test]
    async fn resume_keeps_current_model_on_catalog_miss() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());

        let run_config = scripted_run_config(vec![reply_from("anthropic", "claude-x")]);
        let world =
            build_test_world(&persistence, &run_config, &create_spec()).expect("create world");
        drive_turn(&world, "hello there").await;
        let session_id = world.session_id.clone();
        drop(world);

        let run_config = scripted_run_config(Vec::new());
        let ctx = restore_ctx(&dir, Vec::new());
        let world =
            build_world_with_restore(&persistence, &run_config, &resume_spec(&session_id), &ctx)
                .expect("resume world");

        let cfg = run_config.lock().expect("run config mutex poisoned");
        assert_eq!(
            cfg.model_key,
            ("scripted".to_string(), "scripted".to_string()),
            "catalog miss keeps the current bundle"
        );
        assert!(
            world
                .restore_notices
                .iter()
                .any(|n| n.contains("anthropic/claude-x, which is not available")),
            "fallback notice missing: {:?}",
            world.restore_notices
        );
    }

    #[tokio::test]
    async fn resume_keeps_current_thinking_on_unknown_level() {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().to_path_buf());

        let run_config = scripted_run_config(vec![finalized_text_message("scripted reply")]);
        let world =
            build_test_world(&persistence, &run_config, &create_spec()).expect("create world");
        {
            let mut log = world.log.lock().await;
            log.append_thinking_change(ThreadFilter::USER, "bogus")
                .expect("thinking change");
        }
        drive_turn(&world, "hello there").await;
        let session_id = world.session_id.clone();
        drop(world);

        let run_config = scripted_run_config(Vec::new());
        let ctx = restore_ctx(&dir, vec![catalog_model("scripted", "scripted")]);
        let world =
            build_world_with_restore(&persistence, &run_config, &resume_spec(&session_id), &ctx)
                .expect("resume world");

        let cfg = run_config.lock().expect("run config mutex poisoned");
        assert_eq!(cfg.thinking, None, "unknown level keeps the current value");
        assert!(
            world
                .restore_notices
                .iter()
                .any(|n| n.contains("unknown thinking level \"bogus\"")),
            "unknown-level notice missing: {:?}",
            world.restore_notices
        );
    }
}
