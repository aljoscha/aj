//! Oracle choices at the host, durable-log, and actual consultation boundaries.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aj_agent::events::{AgentEvent, AgentId};
use aj_app::host::{
    AttachRequest, Command, HeadTarget, HostSetup, SessionHost, SettingsAxis, SettingsChange,
};
use aj_app::session_setup::{ModelConfig, RestoreContext, RunConfigDefaults};
use aj_app::settings::{ConfigLayers, PersistAction};
use aj_app::test_support::{finalized_text_message, scripted_model_info, scripted_run_config};
use aj_conf::{
    Config, ConfigLayer, ConfigSpeed, ConfigThinkingDisplay, ConfigThinkingLevel, ConfigVerbosity,
};
use aj_models::ThinkingConfig;
use aj_models::auth::AuthStorage;
use aj_models::provider::Provider;
use aj_models::registry::{Catalog, ModelInfo, ModelRegistry, OverridesFile, ReasoningOption};
use aj_models::scripted::script_from_message;
use aj_models::streaming::AssistantMessageEventStream;
use aj_models::types::{
    AssistantContent, Context, Message, SimpleStreamOptions, Speed, StopReason, StreamOptions,
    ThinkingLevel, ToolCall, UserContent, Verbosity,
};
use aj_session::{ConversationLog, ConversationPersistence, ThreadFilter};
use aj_wire::{BranchChanges, Frame, ModelSelection, SessionSettings};
use serde_json::json;
use tempfile::TempDir;

#[derive(Debug)]
struct Request {
    child: bool,
    model: String,
    url: String,
    effort: ThinkingLevel,
    speed: Option<Speed>,
    verbosity: Option<Verbosity>,
}

#[derive(Default)]
struct AdvisorFixture(Mutex<Vec<Request>>);

impl Provider for AdvisorFixture {
    fn stream(&self, _: &ModelInfo, _: &Context, _: &StreamOptions) -> AssistantMessageEventStream {
        panic!("thinking must reach stream_simple")
    }

    fn stream_simple(
        &self,
        model: &ModelInfo,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        let child = context
            .system_prompt
            .as_deref()
            .is_some_and(|prompt| prompt.ends_with(aj_tools::tools::oracle::ORACLE_PROMPT));
        self.0.lock().unwrap().push(Request {
            child,
            model: model.id.clone(),
            url: model.base_url.clone(),
            effort: options.reasoning,
            speed: options.base.speed,
            verbosity: options.base.verbosity,
        });
        let mut message = finalized_text_message("oracle evidence received");
        if !child && matches!(context.messages.last(), Some(Message::User(_))) {
            message.content.push(AssistantContent::ToolCall(ToolCall {
                id: "consult-session-oracle".into(),
                name: "oracle".into(),
                arguments: json!({"task": "Inspect the session invariant"}),
            }));
            message.stop_reason = StopReason::ToolUse;
        } else if !child {
            assert!(
                matches!(context.messages.last(), Some(Message::ToolResult(result))
                if result.tool_name == "oracle" && !result.is_error),
                "{context:?}"
            );
        }
        let stream = AssistantMessageEventStream::new();
        let producer = stream.clone();
        tokio::spawn(async move {
            for step in script_from_message(message, 0, Duration::ZERO).steps {
                producer.push(step.event);
            }
        });
        stream
    }
}

struct Store {
    root: TempDir,
    provider: Arc<AdvisorFixture>,
}

impl Store {
    fn new() -> Self {
        Self {
            root: TempDir::new().unwrap(),
            provider: Arc::new(AdvisorFixture::default()),
        }
    }

    fn persistence(&self) -> ConversationPersistence {
        ConversationPersistence::new(self.root.path().join("sessions"))
    }

    fn host(&self, oracle: &str) -> (SessionHost, Arc<Mutex<Config>>) {
        let mut run = scripted_run_config(vec![]).lock().unwrap().clone();
        run.main = self.bundle("main-a", Some(ThinkingConfig::Low));
        run.main.stream_options.verbosity = Some(Verbosity::Low);
        run.oracle = self.bundle(oracle, Some(ThinkingConfig::High));
        run.oracle.stream_options.verbosity = Some(Verbosity::High);
        let config = Config {
            model_api: Some("fixture".into()),
            model_name: Some("main-a".into()),
            thinking: Some(ConfigThinkingLevel::Low),
            speed: Some(ConfigSpeed::Standard),
            verbosity: Some(ConfigVerbosity::Low),
            oracle_model_api: Some("fixture".into()),
            oracle_model_name: Some(oracle.into()),
            oracle_thinking: Some(ConfigThinkingLevel::High),
            oracle_speed: Some(ConfigSpeed::Standard),
            oracle_verbosity: Some(ConfigVerbosity::High),
            spill_dir: Some(
                self.root
                    .path()
                    .join("spill")
                    .to_string_lossy()
                    .into_owned(),
            ),
            ..Config::default()
        };
        let layers = ConfigLayers {
            writes: Default::default(),
            user: config.clone(),
            project: ConfigLayer::default(),
            project_path: Some(self.root.path().join("config.toml")),
        };
        let config = Arc::new(Mutex::new(config));
        let catalog: Vec<_> = ["main-a", "main-b", "oracle-a", "oracle-b", "max-only"]
            .into_iter()
            .map(model)
            .collect();
        let auth = AuthStorage::with_providers(self.root.path().join("auth.json"), HashMap::new());
        let restore = RestoreContext {
            registry: Arc::new(ModelRegistry::from_catalog_with_overrides(
                Catalog {
                    schema_version: aj_models::registry::CATALOG_SCHEMA_VERSION,
                    updated_at: 0,
                    source: "oracle-session-test".into(),
                    models: catalog.clone(),
                },
                OverridesFile { overrides: vec![] },
                "oracle-session-test",
            )),
            auth: auth.clone(),
        };
        let host = SessionHost::new(HostSetup {
            config: Arc::clone(&config),
            layers: Arc::new(Mutex::new(layers)),
            catalog: Arc::new(catalog),
            defaults: RunConfigDefaults::fixed(run),
            restore: Some(restore),
            persistence: self.persistence(),
            auth,
            working_directory: self.root.path().to_path_buf(),
            name: None,
            idle_grace: None,
            live_capacity: None,
        })
        .unwrap();
        (host, config)
    }

    fn bundle(&self, id: &str, thinking: Option<ThinkingConfig>) -> ModelConfig {
        ModelConfig {
            provider: Arc::<AdvisorFixture>::clone(&self.provider),
            model_info: Arc::new(model(id)),
            model_key: ("fixture".into(), id.into()),
            thinking,
            thinking_display: None,
            speed: Some(Speed::Standard),
            stream_options: StreamOptions {
                speed: Some(Speed::Standard),
                ..StreamOptions::default()
            },
        }
    }

    async fn consult(
        &self,
        host: &SessionHost,
        session: &str,
        main_effort: ThinkingLevel,
        oracle_effort: ThinkingLevel,
        oracle_verbosity: Verbosity,
        oracle_speed: Speed,
    ) {
        self.provider.0.lock().unwrap().clear();
        let mut attachment = host
            .attach(&[AttachRequest {
                session: session.into(),
                cursor: None,
            }])
            .await
            .unwrap();
        let mut last_frame = None;
        tokio::time::timeout(Duration::from_secs(10), async {
            while !matches!(attachment.recv().await.unwrap(), Frame::CaughtUp { .. }) {}
            host.command(
                session,
                Command::Prompt {
                    agent: AgentId::Main,
                    content: vec![UserContent::text("consult Oracle")],
                },
            )
            .await
            .unwrap();
            let mut started = false;
            loop {
                let frame = attachment.recv().await.unwrap();
                last_frame = Some(format!("{frame:?}"));
                match frame {
                    Frame::Event { event, .. }
                        if matches!(
                            event.known(),
                            Some(AgentEvent::AgentEnd {
                                agent_id: AgentId::Main,
                                ..
                            })
                        ) =>
                    {
                        started = true
                    }
                    Frame::State { working: false, .. } if started => break,
                    // A zero-latency turn need not change the published busy
                    // flag. The list still confirms the host is idle after End.
                    Frame::List { sessions, .. }
                        if started
                            && sessions
                                .iter()
                                .any(|item| item.id == session && !item.working) =>
                    {
                        break;
                    }
                    _ => {}
                }
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "bounded consultation, last frame {last_frame:?}, requests: {:?}",
                self.provider.0.lock().unwrap()
            )
        });
        let requests = self.provider.0.lock().unwrap();
        assert_eq!(
            requests.len(),
            3,
            "parent, real Oracle child, parent: {requests:?}"
        );
        assert!(!requests[0].child && requests[1].child && !requests[2].child);
        assert_eq!(requests[1].model, "oracle-a");
        assert_eq!(requests[1].url, model("oracle-a").base_url);
        assert_eq!(requests[1].effort, oracle_effort);
        assert_eq!(requests[1].speed, Some(oracle_speed));
        assert_eq!(requests[1].verbosity, Some(oracle_verbosity));
        for parent in [&requests[0], &requests[2]] {
            assert_eq!(parent.model, "main-a");
            assert_eq!(parent.effort, main_effort);
            assert_eq!(parent.url, model("main-a").base_url);
            assert_eq!(parent.verbosity, Some(Verbosity::Low));
        }
    }
}

// Catalog metadata uses a supported API so settings edits and restoration take
// the real resolver path. Only tests that retain the injected bundles prompt.
fn model(id: &str) -> ModelInfo {
    ModelInfo {
        provider: "fixture".into(),
        api: "openai-responses".into(),
        id: id.into(),
        name: id.into(),
        base_url: format!("http://127.0.0.1:1/{id}"),
        reasoning: true,
        reasoning_options: vec![ReasoningOption::Effort {
            values: if id == "max-only" {
                vec![ThinkingLevel::Max]
            } else {
                vec![ThinkingLevel::Off, ThinkingLevel::Low, ThinkingLevel::High]
            },
        }],
        supports_verbosity: true,
        ..scripted_model_info()
    }
}

fn selection(id: &str) -> ModelSelection {
    ModelSelection {
        api: "fixture".into(),
        name: id.into(),
        url: None,
    }
}

async fn select(host: &SessionHost, session: &str, axis: SettingsAxis) {
    edit(host, session, axis, PersistAction::None).await;
}

async fn edit(host: &SessionHost, session: &str, axis: SettingsAxis, persist: PersistAction) {
    host.command(
        session,
        Command::Settings(SettingsChange {
            agent: AgentId::Main,
            persist,
            axis,
        }),
    )
    .await
    .unwrap();
}

fn assert_bundle(bundle: &ModelConfig, id: &str, thinking: &str, speed: &str, verbosity: &str) {
    let settings = bundle.settings();
    assert_eq!(settings.provider, "fixture");
    assert_eq!(settings.model_id, id);
    assert_eq!(bundle.model_info.provider, "fixture");
    assert_eq!(bundle.model_info.id, id);
    assert_eq!(settings.thinking, thinking);
    assert_eq!(settings.speed, speed);
    assert_eq!(aj_models::speed_name(bundle.stream_options.speed), speed);
    assert_eq!(settings.verbosity, verbosity);
}

#[tokio::test]
async fn session_axes_reach_the_next_child_without_following_main_edits() {
    let store = Store::new();
    let (host, _) = store.host("oracle-a");
    let session = host.create().await.unwrap();
    store
        .consult(
            &host,
            &session,
            ThinkingLevel::Low,
            ThinkingLevel::High,
            Verbosity::High,
            Speed::Standard,
        )
        .await;
    select(
        &host,
        &session,
        SettingsAxis::OracleThinking(Some(ThinkingConfig::Low)),
    )
    .await;
    select(
        &host,
        &session,
        SettingsAxis::OracleVerbosity(Some(ConfigVerbosity::Medium)),
    )
    .await;
    select(
        &host,
        &session,
        SettingsAxis::Thinking(Some(ThinkingConfig::High)),
    )
    .await;
    store
        .consult(
            &host,
            &session,
            ThinkingLevel::High,
            ThinkingLevel::Low,
            Verbosity::Medium,
            Speed::Standard,
        )
        .await;
    let handles = host.local_handles(&session).await.unwrap();
    let oracle = handles.run_config.lock().unwrap().oracle.clone();
    for axis in [
        SettingsAxis::Model(model("main-b")),
        SettingsAxis::Speed(Some(Speed::Fast)),
        SettingsAxis::Verbosity(Some(ConfigVerbosity::High)),
        SettingsAxis::ThinkingDisplay(Some(ConfigThinkingDisplay::Omitted)),
    ] {
        select(&host, &session, axis).await;
    }
    let run = handles.run_config.lock().unwrap().clone();
    assert_bundle(&run.main, "main-b", "high", "fast", "high");
    assert_bundle(&run.oracle, "oracle-a", "low", "standard", "medium");
    assert!(Arc::ptr_eq(&run.oracle.provider, &oracle.provider));
    assert_eq!(run.oracle.model_info.base_url, oracle.model_info.base_url);
    assert_eq!(run.oracle.thinking_display, oracle.thinking_display);
    host.shutdown().await;
}

#[tokio::test]
async fn oracle_edits_update_only_the_current_session_and_persist_only_oracle_keys() {
    for persist in [PersistAction::None, PersistAction::ProjectSet] {
        let store = Store::new();
        let path = store.root.path().join("config.toml");
        let untouched =
            "# Main defaults stay independent\nmodel_name = \"main-a\"\nthinking = \"low\"\n";
        std::fs::write(&path, untouched).unwrap();
        let (host, config) = store.host("oracle-a");
        let before = format!("{:?}", config.lock().unwrap());
        let session = host.create().await.unwrap();
        let other = host.create().await.unwrap();
        for axis in [
            SettingsAxis::OracleModel(model("oracle-b")),
            SettingsAxis::OracleThinking(Some(ThinkingConfig::Low)),
            SettingsAxis::OracleSpeed(Some(Speed::Fast)),
            SettingsAxis::OracleVerbosity(Some(ConfigVerbosity::Medium)),
        ] {
            edit(&host, &session, axis, persist).await;
        }
        let handles = host.local_handles(&session).await.unwrap();
        {
            let run = handles.run_config.lock().unwrap();
            assert_bundle(&run.main, "main-a", "low", "standard", "low");
            assert_bundle(&run.oracle, "oracle-b", "low", "fast", "medium");
            assert_eq!(run.oracle.stream_options.speed, Some(Speed::Fast));
        }
        let other = host.local_handles(&other).await.unwrap();
        {
            let run = other.run_config.lock().unwrap();
            assert_bundle(&run.main, "main-a", "low", "standard", "low");
            assert_bundle(&run.oracle, "oracle-a", "high", "standard", "high");
        }
        let log = handles.log.lock().await;
        let recorded = log.settings_at(log.head().unwrap());
        assert_eq!(recorded.model, Some(("fixture".into(), "main-a".into())));
        assert_eq!(
            recorded.oracle_model,
            Some(("fixture".into(), "oracle-b".into()))
        );
        assert_eq!(recorded.oracle_thinking.as_deref(), Some("low"));
        assert_eq!(recorded.oracle_speed.as_deref(), Some("fast"));
        assert_eq!(recorded.oracle_verbosity.as_deref(), Some("medium"));
        drop(log);
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.starts_with(untouched), "{saved}");
        if persist == PersistAction::None {
            assert_eq!(saved, untouched);
            assert_eq!(format!("{:?}", config.lock().unwrap()), before);
        } else {
            let mut lines: Vec<_> = saved
                .lines()
                .filter(|line| line.starts_with("oracle_"))
                .collect();
            lines.sort_unstable();
            assert_eq!(
                lines,
                [
                    "oracle_model_api = \"fixture\"",
                    "oracle_model_name = \"oracle-b\"",
                    "oracle_speed = \"fast\"",
                    "oracle_thinking = \"low\"",
                    "oracle_verbosity = \"medium\"",
                ]
            );
            let config = config.lock().unwrap();
            assert_eq!(config.model_name.as_deref(), Some("main-a"));
            assert_eq!(config.thinking, Some(ConfigThinkingLevel::Low));
            assert_eq!(config.speed, Some(ConfigSpeed::Standard));
            assert_eq!(config.verbosity, Some(ConfigVerbosity::Low));
            assert_eq!(config.oracle_model_name.as_deref(), Some("oracle-b"));
            assert_eq!(config.oracle_thinking, Some(ConfigThinkingLevel::Low));
            assert_eq!(config.oracle_speed, Some(ConfigSpeed::Fast));
            assert_eq!(config.oracle_verbosity, Some(ConfigVerbosity::Medium));
        }
        host.shutdown().await;
    }
}

#[tokio::test]
async fn creation_and_head_changes_resolve_oracle_selections_from_the_catalog() {
    let store = Store::new();
    let (host, _) = store.host("oracle-a");
    let defaulted = host
        .create_with(
            Some(SessionSettings {
                oracle_model: Some(selection("max-only")),
                ..SessionSettings::default()
            }),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    let defaulted = host.local_handles(&defaulted).await.unwrap();
    assert_bundle(
        &defaulted.run_config.lock().unwrap().oracle,
        "max-only",
        "max",
        "standard",
        "high",
    );
    assert!(
        host.create_with(
            Some(SessionSettings {
                oracle_model: Some(selection("max-only")),
                oracle_thinking: Some("high".into()),
                ..SessionSettings::default()
            }),
            None,
            None,
            None,
        )
        .await
        .is_err()
    );
    let settings = SessionSettings {
        oracle_model: Some(ModelSelection {
            url: Some("http://127.0.0.1:1/explicit-oracle".into()),
            ..selection("oracle-b")
        }),
        oracle_thinking: Some("low".into()),
        oracle_speed: Some("fast".into()),
        oracle_verbosity: Some("medium".into()),
        ..SessionSettings::default()
    };
    let session = host
        .create_with(Some(settings), None, None, None)
        .await
        .unwrap();
    let handles = host.local_handles(&session).await.unwrap();
    {
        let run = handles.run_config.lock().unwrap();
        assert_bundle(&run.main, "main-a", "low", "standard", "low");
        assert_bundle(&run.oracle, "oracle-b", "low", "fast", "medium");
        assert_eq!(
            run.oracle.model_info.base_url,
            "http://127.0.0.1:1/explicit-oracle"
        );
    }
    let original = handles.log.lock().await.head().unwrap().clone();
    host.command(
        &session,
        Command::Head {
            target: HeadTarget::Entry(original),
            changes: BranchChanges {
                settings: SessionSettings {
                    oracle_model: Some(selection("oracle-a")),
                    oracle_thinking: Some("high".into()),
                    oracle_speed: Some("standard".into()),
                    oracle_verbosity: Some("high".into()),
                    ..SessionSettings::default()
                },
                ..BranchChanges::default()
            },
        },
    )
    .await
    .unwrap();
    {
        let run = handles.run_config.lock().unwrap();
        assert_bundle(&run.main, "main-a", "low", "standard", "low");
        assert_bundle(&run.oracle, "oracle-a", "high", "standard", "high");
        assert_eq!(run.oracle.model_info.base_url, model("oracle-a").base_url);
    }
    assert!(
        host.create_with(
            Some(SessionSettings {
                oracle_model: Some(selection("missing-model")),
                ..SessionSettings::default()
            }),
            None,
            None,
            None
        )
        .await
        .is_err()
    );
    let head = handles.log.lock().await.head().cloned();
    assert!(
        host.command(
            &session,
            Command::Head {
                target: HeadTarget::Entry(head.clone().unwrap()),
                changes: BranchChanges {
                    settings: SessionSettings {
                        oracle_model: Some(selection("missing-model")),
                        ..SessionSettings::default()
                    },
                    ..BranchChanges::default()
                },
            }
        )
        .await
        .is_err()
    );
    assert_eq!(handles.log.lock().await.head().cloned(), head);
    assert_bundle(
        &handles.run_config.lock().unwrap().oracle,
        "oracle-a",
        "high",
        "standard",
        "high",
    );
    host.shutdown().await;
}

#[tokio::test]
async fn captured_defaults_survive_resume_and_head_switch_restores_each_oracle_axis() {
    let store = Store::new();
    let (host, _) = store.host("oracle-a");
    let session = host.create().await.unwrap();
    store
        .consult(
            &host,
            &session,
            ThinkingLevel::Low,
            ThinkingLevel::High,
            Verbosity::High,
            Speed::Standard,
        )
        .await;
    let handles = host.local_handles(&session).await.unwrap();
    let original = handles.log.lock().await.head().unwrap().clone();
    {
        let log = handles.log.lock().await;
        let recorded = log.settings_at(&original);
        assert_eq!(
            recorded.oracle_model,
            Some(("fixture".into(), "oracle-a".into()))
        );
        assert_eq!(recorded.oracle_thinking.as_deref(), Some("high"));
        assert_eq!(recorded.oracle_speed.as_deref(), Some("standard"));
        assert_eq!(recorded.oracle_verbosity.as_deref(), Some("high"));
    }
    host.shutdown().await;
    let (host, _) = store.host("oracle-b");
    let handles = host.local_handles(&session).await.unwrap();
    assert_bundle(
        &handles.run_config.lock().unwrap().oracle,
        "oracle-a",
        "high",
        "standard",
        "high",
    );
    let fresh = host.create().await.unwrap();
    let fresh = host.local_handles(&fresh).await.unwrap();
    assert_bundle(
        &fresh.run_config.lock().unwrap().oracle,
        "oracle-b",
        "high",
        "standard",
        "high",
    );
    for axis in [
        SettingsAxis::OracleModel(model("oracle-b")),
        SettingsAxis::OracleThinking(None),
        SettingsAxis::OracleSpeed(Some(Speed::Fast)),
        SettingsAxis::OracleVerbosity(None),
    ] {
        select(&host, &session, axis).await;
    }
    let edited = handles.log.lock().await.head().unwrap().clone();
    for (target, id, thinking, speed, verbosity) in [
        (original, "oracle-a", "high", "standard", "high"),
        (edited, "oracle-b", "off", "fast", "default"),
    ] {
        host.command(
            &session,
            Command::Head {
                target: HeadTarget::Entry(target),
                changes: BranchChanges::default(),
            },
        )
        .await
        .unwrap();
        let run = handles.run_config.lock().unwrap();
        assert_bundle(&run.main, "main-a", "low", "standard", "low");
        assert_bundle(&run.oracle, id, thinking, speed, verbosity);
    }
    host.shutdown().await;
    let (host, _) = store.host("oracle-a");
    let handles = host.local_handles(&session).await.unwrap();
    assert_bundle(
        &handles.run_config.lock().unwrap().oracle,
        "oracle-b",
        "off",
        "fast",
        "default",
    );
    host.shutdown().await;
}

#[tokio::test]
async fn legacy_logs_keep_unknown_oracle_history_and_use_independent_defaults() {
    let store = Store::new();
    let mut log = ConversationLog::create(&store.persistence()).unwrap();
    log.set_system_prompt("Legacy session instructions".into())
        .unwrap();
    log.append_model_change(ThreadFilter::USER, "fixture", "main-b")
        .unwrap();
    log.append_thinking_change(ThreadFilter::USER, "low")
        .unwrap();
    log.append(
        log.head().cloned(),
        aj_session::ThreadKind::User,
        None,
        aj_session::ConversationEntryKind::Message {
            message: aj_agent::message::AgentMessage::wire(Message::User(
                aj_models::types::UserMessage::text("legacy prompt"),
            )),
        },
    )
    .unwrap();
    let session = log.session_id().to_string();
    let historical = log.head().unwrap().clone();
    drop(log);
    let (host, _) = store.host("oracle-a");
    let handles = host.local_handles(&session).await.unwrap();
    {
        let run = handles.run_config.lock().unwrap();
        assert_bundle(&run.main, "main-b", "low", "standard", "low");
        assert_bundle(&run.oracle, "oracle-a", "high", "standard", "high");
    }
    let log = handles.log.lock().await;
    let settings = log.settings_at(&historical);
    assert_eq!(settings.oracle_model, None);
    assert_eq!(settings.oracle_thinking, None);
    assert_eq!(settings.oracle_speed, None);
    assert_eq!(settings.oracle_verbosity, None);
    drop(log);
    host.shutdown().await;
}

#[tokio::test]
async fn separate_model_and_thinking_edits_can_repair_invalid_effort_for_main_and_oracle() {
    for oracle in [false, true] {
        let store = Store::new();
        let (host, _) = store.host("oracle-a");
        let session = host.create().await.unwrap();
        let handles = host.local_handles(&session).await.unwrap();
        // A model edit preserves effort, even when the new model cannot use it.
        // Either axis must remain editable so the user can repair that mismatch.
        for (axis, expected_model, expected_thinking) in [
            (
                if oracle {
                    SettingsAxis::OracleModel(model("max-only"))
                } else {
                    SettingsAxis::Model(model("max-only"))
                },
                "max-only",
                if oracle { "high" } else { "low" },
            ),
            (
                if oracle {
                    SettingsAxis::OracleThinking(Some(ThinkingConfig::Max))
                } else {
                    SettingsAxis::Thinking(Some(ThinkingConfig::Max))
                },
                "max-only",
                "max",
            ),
            (
                if oracle {
                    SettingsAxis::OracleModel(model("oracle-b"))
                } else {
                    SettingsAxis::Model(model("oracle-b"))
                },
                "oracle-b",
                "max",
            ),
            (
                if oracle {
                    SettingsAxis::OracleThinking(Some(ThinkingConfig::Low))
                } else {
                    SettingsAxis::Thinking(Some(ThinkingConfig::Low))
                },
                "oracle-b",
                "low",
            ),
            (
                if oracle {
                    SettingsAxis::OracleThinking(Some(ThinkingConfig::Max))
                } else {
                    SettingsAxis::Thinking(Some(ThinkingConfig::Max))
                },
                "oracle-b",
                "max",
            ),
            (
                if oracle {
                    SettingsAxis::OracleModel(model("max-only"))
                } else {
                    SettingsAxis::Model(model("max-only"))
                },
                "max-only",
                "max",
            ),
        ] {
            select(&host, &session, axis).await;
            let run = handles.run_config.lock().unwrap();
            let selected = if oracle { &run.oracle } else { &run.main };
            assert_bundle(
                selected,
                expected_model,
                expected_thinking,
                "standard",
                if oracle { "high" } else { "low" },
            );
            if oracle {
                assert_bundle(&run.main, "main-a", "low", "standard", "low");
            } else {
                assert_bundle(&run.oracle, "oracle-a", "high", "standard", "high");
            }
        }
        host.shutdown().await;
    }
}

#[tokio::test]
async fn unavailable_recorded_oracle_keeps_fallback_and_restores_request_speed() {
    let store = Store::new();
    let mut log = ConversationLog::create(&store.persistence()).unwrap();
    log.set_system_prompt("Shared engineering instructions".into())
        .unwrap();
    log.append_model_change(ThreadFilter::USER, "fixture", "main-a")
        .unwrap();
    log.append_thinking_change(ThreadFilter::USER, "low")
        .unwrap();
    log.append_oracle_model_change("fixture", "unavailable")
        .unwrap();
    log.append_oracle_speed_change("fast").unwrap();
    log.append(
        log.head().cloned(),
        aj_session::ThreadKind::User,
        None,
        aj_session::ConversationEntryKind::Message {
            message: aj_agent::message::AgentMessage::wire(Message::User(
                aj_models::types::UserMessage::text("prior question"),
            )),
        },
    )
    .unwrap();
    let session = log.session_id().to_string();
    drop(log);
    let (host, _) = store.host("oracle-a");
    store
        .consult(
            &host,
            &session,
            ThinkingLevel::Low,
            ThinkingLevel::High,
            Verbosity::High,
            Speed::Fast,
        )
        .await;
    host.shutdown().await;
}

#[tokio::test]
async fn branch_model_restore_preserves_unrecorded_verbosity() {
    let store = Store::new();
    let mut log = ConversationLog::create(&store.persistence()).unwrap();
    log.set_system_prompt("Shared engineering instructions".into())
        .unwrap();
    log.append_model_change(ThreadFilter::USER, "fixture", "main-b")
        .unwrap();
    log.append_oracle_model_change("fixture", "oracle-b")
        .unwrap();
    log.append(
        log.head().cloned(),
        aj_session::ThreadKind::User,
        None,
        aj_session::ConversationEntryKind::Message {
            message: aj_agent::message::AgentMessage::wire(Message::User(
                aj_models::types::UserMessage::text("prior question"),
            )),
        },
    )
    .unwrap();
    let historical = log.head().unwrap().clone();
    let session = log.session_id().to_string();
    drop(log);
    let (host, _) = store.host("oracle-a");
    for axis in [
        SettingsAxis::Model(model("main-a")),
        SettingsAxis::OracleModel(model("oracle-a")),
        SettingsAxis::Verbosity(Some(ConfigVerbosity::High)),
        SettingsAxis::OracleVerbosity(Some(ConfigVerbosity::Medium)),
    ] {
        select(&host, &session, axis).await;
    }
    host.command(
        &session,
        Command::Head {
            target: HeadTarget::Entry(historical),
            changes: Default::default(),
        },
    )
    .await
    .unwrap();
    let handles = host.local_handles(&session).await.unwrap();
    {
        let run = handles.run_config.lock().unwrap();
        assert_eq!(run.main.model_key.1, "main-b");
        assert_eq!(run.oracle.model_key.1, "oracle-b");
        assert_eq!(run.main.stream_options.verbosity, Some(Verbosity::High));
        assert_eq!(run.oracle.stream_options.verbosity, Some(Verbosity::Medium));
    }
    host.shutdown().await;
}
