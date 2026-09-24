use super::*;
use crate::gateway::{Gateway, GatewayServer, GatewaySetup, Tuning};
use crate::remote::tests::{HostHandles, addr, bounded, host_setup, scripted, snapshot};
use crate::remote::{IdentityGate, RemoteServer};
use aj_agent::events::{AgentEvent, AgentId, AgentSettings};
use aj_app::chat::ChatState;
use aj_app::client::SessionClient;
use aj_app::footer::{UsageSeverity, context_usage_display};
use aj_app::host::CommandOutcome;
use aj_app::test_support::{finalized_text_message_with_usage, scripted_model_info};
use aj_models::registry::ModelInfo;
use aj_models::types::{AssistantContent, StopReason, ToolCall, UserContent};
use aj_wire::{DecodedAgentEvent, PersistAction};
use std::sync::Arc;

const HOST_WINDOW: u64 = 32_000;
const CHANGED_WINDOW: u64 = 64_000;

// Host tasks may outlive a failed assertion. Keep their scratch under the
// process-lifetime root, as in the other composed Control fixtures.
fn task_directory() -> &'static tempfile::TempDir {
    static ROOT: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    let root = ROOT.get_or_init(|| tempfile::TempDir::with_prefix("aj-task-lifetime-").unwrap());
    Box::leak(Box::new(tempfile::TempDir::new_in(root.path()).unwrap()))
}

struct Fixture {
    host: SessionHost,
    server: RemoteServer,
    gateway: Gateway,
    gateway_server: GatewayServer,
    controls: Vec<(Control, String)>,
    changed_model: ModelInfo,
}

impl Fixture {
    async fn new() -> Self {
        let host_dir = task_directory();
        let gateway_dir = task_directory();
        let mut delegate = finalized_text_message_with_usage("delegate", 100);
        delegate.content.push(AssistantContent::ToolCall(ToolCall {
            id: "call-child".into(),
            name: "agent".into(),
            arguments: serde_json::json!({"task": "inspect the fixture"}),
        }));
        delegate.stop_reason = StopReason::ToolUse;
        // A blocking child consumes the middle script before the parent resumes.
        let provider = scripted(
            vec![
                delegate,
                finalized_text_message_with_usage("child report", 30_000),
                finalized_text_message_with_usage("parent report", 24_000),
            ],
            0,
            Duration::ZERO,
        );
        let mut run = snapshot(provider);
        Arc::make_mut(&mut run.main.model_info).context_window = HOST_WINDOW;
        run.oracle = run.main.clone();
        let mut setup = host_setup(host_dir, run, HostHandles::new(host_dir), None);
        let changed_model = ModelInfo {
            context_window: CHANGED_WINDOW,
            reasoning: true,
            reasoning_options: vec![aj_models::registry::ReasoningOption::Effort {
                values: vec![
                    aj_models::types::ThinkingLevel::Off,
                    aj_models::types::ThinkingLevel::Low,
                ],
            }],
            ..setup.catalog[0].clone()
        };
        // The initial model exists only in the injected bundle, not a catalog.
        assert!(!setup.catalog.iter().any(|m| m.id == "scripted"));
        assert_ne!(scripted_model_info().context_window, HOST_WINDOW);
        setup.catalog = Arc::new(vec![changed_model.clone()]);
        let host = SessionHost::new(setup).unwrap();
        let session = host.create().await.unwrap();
        let server = RemoteServer::bind_with(
            host.clone(),
            addr("127.0.0.1:0"),
            IdentityGate::local(),
            Duration::from_millis(50),
        )
        .await
        .unwrap();
        let gateway = Gateway::new(GatewaySetup {
            state_dir: gateway_dir.path().to_path_buf(),
            static_hosts: vec![server.url().try_into().unwrap()],
            tuning: Tuning {
                upstream_timeout: Duration::from_secs(5),
                ..Tuning::default()
            },
        })
        .unwrap();
        let gateway_server =
            GatewayServer::bind(gateway.clone(), addr("127.0.0.1:0"), IdentityGate::local())
                .await
                .unwrap();
        let remote = RemoteClient::new(&server.url()).unwrap();
        let proxied = RemoteClient::new(&gateway_server.url()).unwrap();
        let routed = bounded("gateway session discovery", async {
            loop {
                if let Some(row) = proxied
                    .sessions()
                    .await
                    .unwrap()
                    .sessions
                    .into_iter()
                    .next()
                {
                    break row.id;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert_ne!(routed, session);
        let controls = vec![
            (Control::local(host.clone()), session.clone()),
            (Control::remote(remote), session),
            (Control::remote(proxied), routed),
        ];
        Self {
            host,
            server,
            gateway,
            gateway_server,
            controls,
            changed_model,
        }
    }

    async fn close(self) {
        self.gateway_server.shutdown().await;
        self.gateway.shutdown().await;
        self.host.shutdown().await;
        self.server.shutdown().await;
    }
}

struct Client {
    stream: Stream,
    fold: SessionClient,
    chat: ChatState,
}

impl Client {
    async fn attach(control: &Control, session: &str, seed_window: u64) -> Self {
        let stream = control
            .attach_all(&[AttachRequest {
                session: session.into(),
                cursor: None,
            }])
            .await
            .unwrap();
        assert!(stream.attached(session));
        let mut client = Self {
            stream,
            fold: SessionClient::new(session.into()),
            chat: ChatState::new(AgentSettings {
                context_window: seed_window,
                provider: "scripted".into(),
                model_id: "scripted".into(),
                thinking: "off".into(),
                thinking_display: "default".into(),
                speed: "standard".into(),
                verbosity: "default".into(),
            }),
        };
        assert_eq!(
            client
                .chat
                .footers()
                .context_usage(AgentId::Main)
                .context_window,
            seed_window
        );
        client.fold.expect_attach();
        client
            .pump_until("attach block", |f| matches!(f, Frame::CaughtUp { .. }))
            .await;
        client
    }

    async fn pump_until(&mut self, what: &str, done: impl Fn(&Frame) -> bool) {
        bounded(what, async {
            loop {
                let frame = match self.stream.recv().await {
                    ControlFrame::Frame(frame) => frame,
                    ControlFrame::Lost(error) => panic!("stream lost before {what}: {error}"),
                    ControlFrame::Closed => panic!("stream closed before {what}"),
                };
                let stop = done(&frame);
                let _ = self.fold.apply(&mut self.chat, frame);
                if stop {
                    break;
                }
            }
        })
        .await;
    }

    async fn settings_changed(&mut self, agent: AgentId, done: impl Fn(&AgentSettings) -> bool) {
        bounded("settings change in observing client", async {
            while !self.chat.footers().settings(agent).is_some_and(&done) {
                self.pump_until("next settings frame", |_| true).await;
            }
        })
        .await;
    }
}

fn assert_usage(
    chat: &ChatState,
    agent: AgentId,
    window: u64,
    tokens: u64,
    percent: &str,
    severity: UsageSeverity,
) {
    // settings() has no fallback, so a missing child footer cannot pass by
    // borrowing the parent's capacity.
    assert_eq!(
        chat.footers().settings(agent).unwrap().context_window,
        window
    );
    let usage = chat.footers().context_usage(agent);
    assert_eq!(usage.context_window, window);
    assert_eq!(usage.tokens, Some(tokens));
    assert!(!usage.incomplete);
    let display = context_usage_display(usage).unwrap();
    assert_eq!(display.percent.as_deref(), Some(percent));
    assert_eq!(display.severity, severity);
}

fn assert_parent_and_child(chat: &ChatState) {
    assert_usage(
        chat,
        AgentId::Main,
        HOST_WINDOW,
        24_000,
        "(75.0%)",
        UsageSeverity::Warning,
    );
    let agents = chat.agents();
    assert_eq!(
        agents.len(),
        2,
        "the scripted turn must really spawn a child"
    );
    let child = agents
        .iter()
        .find(|a| matches!(a.id, AgentId::Sub(_)))
        .unwrap();
    assert_usage(
        chat,
        child.id,
        HOST_WINDOW,
        30_000,
        "(93.8%)",
        UsageSeverity::Critical,
    );
}

#[tokio::test]
async fn host_context_capacity_survives_live_children_replay_and_settings_changes() {
    for adapter in 0..3 {
        let fixture = Fixture::new().await;
        let (control, session) = &fixture.controls[adapter];
        let mut live = Client::attach(control, session, 0).await;
        assert_eq!(
            live.chat
                .footers()
                .context_usage(AgentId::Main)
                .context_window,
            HOST_WINDOW
        );
        let outcome = control
            .command(
                session,
                Command::Prompt {
                    agent: AgentId::Main,
                    content: vec![UserContent::text("delegate once")],
                },
            )
            .await
            .unwrap();
        assert!(matches!(outcome, CommandOutcome::Accepted));
        live.pump_until("parent turn completion", |f| {
            matches!(
                f,
                Frame::Event {
                    event: DecodedAgentEvent::Known(event),
                    ..
                } if matches!(event.value(), AgentEvent::AgentEnd { agent_id: AgentId::Main, .. })
            )
        })
        .await;
        assert_parent_and_child(&live.chat);
        drop(live);

        let mut replay = Client::attach(control, session, 1_000_000).await;
        assert_parent_and_child(&replay.chat);
        // A client supplies only a model identity. Its stale metadata must not
        // replace the host catalog's capacity, even through the local adapter.
        let mut selection = fixture.changed_model.clone();
        selection.context_window = 1_000_000;
        let outcome = control
            .command(
                session,
                Command::Settings(SettingsChange {
                    agent: AgentId::Main,
                    persist: PersistAction::None,
                    axis: SettingsAxis::Model(selection),
                }),
            )
            .await
            .unwrap();
        assert!(matches!(outcome, CommandOutcome::Accepted));
        replay
            .pump_until("main model change", |f| {
                matches!(f,
                    Frame::State { settings, .. } if settings.model_id == fixture.changed_model.id
                )
            })
            .await;
        assert_usage(
            &replay.chat,
            AgentId::Main,
            CHANGED_WINDOW,
            24_000,
            "(37.5%)",
            UsageSeverity::Normal,
        );
        drop(replay);
        // No inference runs against the catalog model. Its provider is resolved
        // lazily with the fixture's isolated auth storage.
        let mut fresh = Client::attach(control, session, 0).await;
        assert_usage(
            &fresh.chat,
            AgentId::Main,
            CHANGED_WINDOW,
            24_000,
            "(37.5%)",
            UsageSeverity::Normal,
        );
        let child = fresh
            .chat
            .agents()
            .into_iter()
            .find(|a| matches!(a.id, AgentId::Sub(_)))
            .unwrap()
            .id;
        let (observer_control, observer_session) = &fixture.controls[(adapter + 1) % 3];
        let mut observer = Client::attach(observer_control, observer_session, 0).await;
        let outcome = control
            .command(
                session,
                Command::Settings(SettingsChange {
                    agent: child,
                    persist: PersistAction::None,
                    axis: SettingsAxis::Model(fixture.changed_model.clone()),
                }),
            )
            .await
            .unwrap();
        assert!(matches!(outcome, CommandOutcome::Accepted));
        for client in [&mut fresh, &mut observer] {
            client
                .settings_changed(child, |settings| {
                    settings.model_id == fixture.changed_model.id
                        && settings.context_window == CHANGED_WINDOW
                })
                .await;
            assert_usage(
                &client.chat,
                child,
                CHANGED_WINDOW,
                30_000,
                "(46.9%)",
                UsageSeverity::Normal,
            );
        }
        let outcome = control
            .command(
                session,
                Command::Settings(SettingsChange {
                    agent: child,
                    persist: PersistAction::None,
                    axis: SettingsAxis::Thinking(Some(aj_models::ThinkingConfig::Low)),
                }),
            )
            .await
            .unwrap();
        assert!(matches!(outcome, CommandOutcome::Accepted));
        for client in [&mut fresh, &mut observer] {
            client
                .settings_changed(child, |settings| settings.thinking == "low")
                .await;
            assert_usage(
                &client.chat,
                child,
                CHANGED_WINDOW,
                30_000,
                "(46.9%)",
                UsageSeverity::Normal,
            );
        }
        drop(observer);
        drop(fresh);
        let reattached = Client::attach(control, session, 0).await;
        assert_eq!(
            reattached.chat.footers().settings(child).unwrap().model_id,
            fixture.changed_model.id
        );
        assert_eq!(
            reattached.chat.footers().settings(child).unwrap().thinking,
            "low"
        );
        assert_usage(
            &reattached.chat,
            child,
            CHANGED_WINDOW,
            30_000,
            "(46.9%)",
            UsageSeverity::Normal,
        );
        drop(reattached);
        fixture.close().await;
    }
}
