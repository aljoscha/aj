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
use aj_agent::{Agent, AgentSeed, SubAgentRegistry};
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
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    use aj_agent::bus::EventBus;
    use aj_agent::message::AgentMessage;
    use aj_models::scripted::{ExhaustedBehavior, ScriptedProvider};
    use aj_models::types::{
        AssistantContent, AssistantMessage, Message, StopReason, TextContent, UserMessage,
    };
    use aj_tui::component::Component;
    use aj_tui::terminal::Terminal;
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::config::theme::Theme;
    use crate::modes::interactive::layout::build_layout;

    /// Headless [`Terminal`]: fixed 100×24, writes discarded.
    /// Component output is read via `Component::render`, not the
    /// terminal's write buffer, so a no-op sink is sufficient.
    /// Deliberately duplicates the integration-test stub — unit
    /// tests cannot import from `tests/`.
    struct StubTerminal;

    impl Terminal for StubTerminal {
        fn write(&mut self, _: &str) {}
        fn columns(&self) -> u16 {
            100
        }
        fn rows(&self) -> u16 {
            24
        }
        fn move_by(&mut self, _: i32) {}
        fn hide_cursor(&mut self) {}
        fn show_cursor(&mut self) {}
        fn clear_line(&mut self) {}
        fn clear_from_cursor(&mut self) {}
        fn clear_screen(&mut self) {}
        fn set_title(&mut self, _: &str) {}
        fn flush(&mut self) {}
    }

    /// [`ModelInfo`] consistent with the identity [`ScriptedProvider`]
    /// stamps on every emitted partial, so the agent sees a coherent
    /// provider identity in tests.
    fn scripted_model_info() -> ModelInfo {
        ModelInfo {
            id: "scripted".to_string(),
            name: "scripted".to_string(),
            api: "scripted".to_string(),
            provider: "scripted".to_string(),
            base_url: "scripted://internal".to_string(),
            reasoning: false,
            supports_adaptive_thinking: false,
            input: vec![aj_models::registry::InputModality::Text],
            cost: aj_models::registry::ModelCost::default(),
            context_window: 0,
            max_tokens: 0,
            headers: None,
        }
    }

    /// Finalized text-only assistant reply for scripting one-turn
    /// conversations (no tool calls, `EndTurn`).
    fn finalized_text_message(text: &str) -> AssistantMessage {
        AssistantMessage {
            content: vec![AssistantContent::Text(TextContent {
                text: text.to_string(),
                text_signature: None,
            })],
            api: "scripted".to_string(),
            provider: "scripted".to_string(),
            model: "scripted".to_string(),
            response_id: Some("test-msg".to_string()),
            usage: Default::default(),
            stop_reason: StopReason::Stop,
            error: None,
            timestamp: 0,
        }
    }

    /// Run-config snapshot over a [`ScriptedProvider`] replaying
    /// `messages`. `ExhaustedBehavior::Panic` makes any unscripted
    /// extra inference fail loudly.
    fn scripted_run_config(messages: Vec<AssistantMessage>) -> Arc<StdMutex<RunConfigSnapshot>> {
        Arc::new(StdMutex::new(RunConfigSnapshot {
            provider: Arc::new(
                ScriptedProvider::from_messages(messages, 0, Duration::ZERO)
                    .on_exhausted(ExhaustedBehavior::Panic),
            ),
            model_info: Arc::new(scripted_model_info()),
            stream_options: StreamOptions::default(),
            thinking: None,
            model_key: ("scripted".to_string(), "scripted".to_string()),
        }))
    }

    /// [`SessionWorld::build`] with a default config, bundled theme,
    /// and fixed render settings. The agent's env is read from the
    /// host (cwd, git, context files); tests therefore never assert
    /// on prompt *text*, only on persisted-vs-held equality.
    fn build_test_world(
        persistence: &ConversationPersistence,
        run_config: &Arc<StdMutex<RunConfigSnapshot>>,
        spec: &SessionSpec,
    ) -> Result<SessionWorld> {
        SessionWorld::build(
            &Config::default(),
            run_config,
            &RenderSettings::new(false, false, true),
            &ThemeHandle::new(Theme::bundled_dark()),
            persistence,
            spec,
        )
    }

    /// Drive one prompt turn against the world's agent so the
    /// persistence listener writes real entries into the log.
    async fn drive_turn(world: &SessionWorld, prompt: &str) {
        world
            .agent
            .lock()
            .await
            .prompt(prompt.to_string(), CancellationToken::new())
            .await
            .expect("scripted turn completes");
    }

    fn create_spec() -> SessionSpec {
        SessionSpec::Create {
            entry: SessionEntry::Startup,
        }
    }

    fn resume_spec(session_id: &str) -> SessionSpec {
        SessionSpec::Resume {
            session_id: session_id.to_string(),
            entry: SessionEntry::Switch,
        }
    }

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

    /// Build a `Create` world on `persistence`, drive one scripted
    /// text turn, and return the session id. The world is dropped so
    /// a later resume reads everything from disk.
    async fn one_turn_session(
        persistence: &ConversationPersistence,
        prompt: &str,
        reply: &str,
    ) -> String {
        let run_config = scripted_run_config(vec![finalized_text_message(reply)]);
        let world =
            build_test_world(persistence, &run_config, &create_spec()).expect("create world");
        drive_turn(&world, prompt).await;
        world.session_id.clone()
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
        build_layout(&mut tui, &ThemeHandle::new(Theme::bundled_dark()));
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
            build_layout(&mut tui, &ThemeHandle::new(Theme::bundled_dark()));
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
}
