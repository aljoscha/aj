//! Session account policy observed at the host command and inference boundaries.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aj_agent::events::{AgentEvent, AgentId};
use aj_app::host::{
    AttachRequest, Attachment, Command, HostSetup, SessionHost, SettingsAxis, SettingsChange,
};
use aj_app::session_setup::{RunConfigDefaults, RunConfigSnapshot};
use aj_app::settings::{ConfigLayers, PersistAction};
use aj_app::test_support::{finalized_text_message, scripted_model_info};
use aj_conf::{Config, ConfigLayer};
use aj_models::auth::{AuthCredential, AuthStorage};
use aj_models::provider::Provider;
use aj_models::registry::ModelInfo;
use aj_models::scripted::script_from_message;
use aj_models::streaming::AssistantMessageEventStream;
use aj_models::types::{
    AssistantContent, AssistantError, AssistantMessage, Context, ErrorCategory, Message,
    SimpleStreamOptions, StopReason, StreamOptions, ToolCall, UserContent,
};
use aj_session::ConversationPersistence;
use aj_wire::{AccountSelection, Frame, SessionSettings};
use tempfile::TempDir;
use tokio::sync::oneshot;

const PROVIDER: &str = "scripted";
const DEADLINE: Duration = Duration::from_secs(20);

struct RequestGate {
    resolved: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}

/// Resolves the host-installed resolver exactly once per request. Only synthetic
/// credentials enter this provider, so echoing the token makes misbilling visible.
#[derive(Default)]
struct CredentialProvider {
    delegate_next: AtomicBool,
    gate: Mutex<Option<RequestGate>>,
}

impl CredentialProvider {
    fn hold_next(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (resolved, ready) = oneshot::channel();
        let (release, wait) = oneshot::channel();
        assert!(
            self.gate
                .lock()
                .unwrap()
                .replace(RequestGate {
                    resolved,
                    release: wait,
                })
                .is_none()
        );
        (ready, release)
    }
}

impl Provider for CredentialProvider {
    fn stream(
        &self,
        model: &ModelInfo,
        _context: &Context,
        options: &StreamOptions,
    ) -> AssistantMessageEventStream {
        let options = options.clone();
        let model = model.clone();
        let delegate = self.delegate_next.swap(false, Ordering::SeqCst);
        let gate = self.gate.lock().unwrap().take();
        let stream = AssistantMessageEventStream::new();
        let producer = stream.clone();
        tokio::spawn(async move {
            let mut message = match options.resolve_api_key().await {
                Ok(resolved) => {
                    let mut message = finalized_text_message(&resolved.key);
                    message.account = resolved.account;
                    if delegate {
                        message.content.push(AssistantContent::ToolCall(ToolCall {
                            id: "delegate-account-check".into(),
                            name: "agent".into(),
                            arguments: serde_json::json!({"task": "report your credential"}),
                        }));
                        message.stop_reason = StopReason::ToolUse;
                    }
                    message
                }
                Err(error) => {
                    let mut message = AssistantMessage::empty();
                    message.stop_reason = StopReason::Error;
                    message.error = Some(AssistantError::new(ErrorCategory::Auth, error));
                    message
                }
            };
            message.api = model.api;
            message.provider = model.provider;
            message.model = model.id;
            if let Some(gate) = gate {
                let _ = gate.resolved.send(());
                bounded(gate.release).await.expect("release held request");
            }
            for step in script_from_message(message, 0, Duration::ZERO).steps {
                producer.push(step.event);
            }
        });
        stream
    }

    fn stream_simple(
        &self,
        model: &ModelInfo,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        self.stream(model, context, &options.base)
    }
}

struct Store {
    dir: TempDir,
    auth: AuthStorage,
    provider: Arc<CredentialProvider>,
}

impl Store {
    async fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let auth = AuthStorage::with_providers(dir.path().join("auth.json"), HashMap::new());
        for (label, token) in [
            ("personal", "synthetic-personal-token"),
            ("work", "synthetic-work-token"),
            ("", "synthetic-unnamed-token"),
        ] {
            auth.insert_account(
                PROVIDER,
                label,
                AuthCredential::ApiKey { key: token.into() },
            )
            .await
            .unwrap();
        }
        auth.set_default_account(PROVIDER, "personal")
            .await
            .unwrap();
        Self {
            dir,
            auth,
            provider: Arc::new(CredentialProvider::default()),
        }
    }

    fn host(&self) -> SessionHost {
        SessionHost::new(HostSetup {
            config: Arc::new(Mutex::new(Config {
                spill_dir: Some(self.dir.path().join("spill").to_string_lossy().into_owned()),
                compact_keep_recent: 1,
                ..Config::default()
            })),
            layers: Arc::new(Mutex::new(ConfigLayers {
                user: Config::default(),
                project: ConfigLayer::default(),
                project_path: None,
            })),
            catalog: Arc::new(Vec::new()),
            defaults: RunConfigDefaults::fixed(RunConfigSnapshot {
                accounts: Default::default(),
                provider: Arc::<CredentialProvider>::clone(&self.provider),
                model_info: Arc::new(scripted_model_info()),
                stream_options: StreamOptions::default(),
                thinking: None,
                thinking_display: None,
                speed: None,
                model_key: (PROVIDER.into(), "scripted".into()),
                session_id: None,
            }),
            restore: None,
            persistence: ConversationPersistence::new(self.dir.path().join("sessions")),
            auth: self.auth.clone(),
            working_directory: self.dir.path().to_path_buf(),
            name: None,
            idle_grace: None,
            live_capacity: None,
        })
        .unwrap()
    }

    /// A separate auth handle models a login/default edit outside this host.
    fn external_auth(&self) -> AuthStorage {
        AuthStorage::with_providers(self.dir.path().join("auth.json"), HashMap::new())
    }
}

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(DEADLINE, future)
        .await
        .expect("account test timed out")
}

async fn frames_until(stream: &mut Attachment, done: impl Fn(&Frame) -> bool) -> Vec<Frame> {
    bounded(async {
        let mut frames = Vec::new();
        loop {
            let frame = stream.recv().await.expect("host stream closed early");
            let stop = done(&frame);
            frames.push(frame);
            if stop {
                return frames;
            }
        }
    })
    .await
}

async fn attach(host: &SessionHost, session: &str) -> Attachment {
    let mut stream = bounded(host.attach(&[AttachRequest {
        session: session.into(),
        cursor: None,
    }]))
    .await
    .unwrap();
    frames_until(&mut stream, |frame| matches!(frame, Frame::CaughtUp { .. })).await;
    stream
}

async fn command(host: &SessionHost, session: &str, command: Command) {
    bounded(host.command(session, command))
        .await
        .expect("command accepted");
}

async fn select(host: &SessionHost, session: &str, account: Option<&str>) {
    command(
        host,
        session,
        Command::Account {
            provider: PROVIDER.into(),
            account: account.map(str::to_string),
        },
    )
    .await;
}

fn prompt(agent: AgentId) -> Command {
    Command::Prompt {
        agent,
        content: vec![UserContent::text("report your credential")],
    }
}

async fn finish_turn(stream: &mut Attachment, agent: AgentId) -> Vec<Frame> {
    // Attach can synthesize an idle child's AgentEnd after CaughtUp. Only an
    // end following this turn's reply establishes that the inference completed.
    let mut frames = frames_until(stream, |frame| {
        !messages(std::slice::from_ref(frame), agent).is_empty()
    })
    .await;
    frames.extend(frames_until(stream, |frame| matches!(frame,
        Frame::Event { event, .. }
            if matches!(event.known(), Some(AgentEvent::AgentEnd { agent_id, .. }) if *agent_id == agent)
    )).await);
    // State.working describes Main, so a child ending need not publish State.
    if agent == AgentId::Main {
        frames.extend(
            frames_until(stream, |frame| {
                matches!(frame, Frame::State { working: false, .. })
            })
            .await,
        );
    }
    frames
}

async fn turn(host: &SessionHost, session: &str, agent: AgentId) -> Vec<Frame> {
    let mut stream = attach(host, session).await;
    command(host, session, prompt(agent)).await;
    finish_turn(&mut stream, agent).await
}

fn messages(frames: &[Frame], agent: AgentId) -> Vec<&AssistantMessage> {
    frames
        .iter()
        .filter_map(|frame| {
            let Frame::Event { event, .. } = frame else {
                return None;
            };
            let AgentEvent::MessageEnd {
                agent_id, message, ..
            } = event.known()?
            else {
                return None;
            };
            if *agent_id != agent {
                return None;
            }
            match message.as_stored_wire()? {
                Message::Assistant(message) => Some(message),
                _ => None,
            }
        })
        .collect()
}

fn assert_credential(message: &AssistantMessage, label: &str, token: &str) {
    assert!(
        message.error.is_none(),
        "unexpected inference failure: {message:?}"
    );
    assert_eq!(message.account.as_deref(), Some(label));
    let text: String = message
        .content
        .iter()
        .filter_map(|block| match block {
            AssistantContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, token);
}

async fn assert_turn(host: &SessionHost, session: &str, label: &str, token: &str) {
    let frames = turn(host, session, AgentId::Main).await;
    let replies = messages(&frames, AgentId::Main);
    assert_eq!(replies.len(), 1, "one credential-resolving inference");
    assert_credential(replies[0], label, token);
}

#[tokio::test]
async fn session_pins_are_isolated_while_provider_defaults_stay_live() {
    let store = Store::new().await;
    let host = store.host();
    let pinned = host.create().await.unwrap();
    let following = host.create().await.unwrap();
    select(&host, &pinned, Some("work")).await;
    assert_turn(&host, &pinned, "work", "synthetic-work-token").await;
    assert_turn(&host, &following, "personal", "synthetic-personal-token").await;

    store
        .external_auth()
        .set_default_account(PROVIDER, "")
        .await
        .unwrap();
    assert_turn(&host, &pinned, "work", "synthetic-work-token").await;
    assert_turn(&host, &following, "", "synthetic-unnamed-token").await;
    let list = host.accounts(&pinned, None).await.unwrap();
    assert_eq!(list.provider, PROVIDER);
    assert_eq!(list.selected.as_deref(), Some("work"));
    assert_eq!(list.default.as_deref(), Some(""));
    assert_eq!(
        host.accounts(&following, None).await.unwrap().selected,
        None
    );

    for (label, token) in [
        ("personal", "synthetic-other-default"),
        ("work", "synthetic-other-token"),
    ] {
        store
            .auth
            .insert_account(
                "other-scripted",
                label,
                AuthCredential::ApiKey { key: token.into() },
            )
            .await
            .unwrap();
    }
    command(
        &host,
        &pinned,
        Command::Account {
            provider: "other-scripted".into(),
            account: Some("work".into()),
        },
    )
    .await;
    select(&host, &pinned, None).await;
    assert_turn(&host, &pinned, "", "synthetic-unnamed-token").await;
    assert_eq!(
        host.accounts(&pinned, Some("other-scripted"))
            .await
            .unwrap()
            .selected
            .as_deref(),
        Some("work")
    );
    assert_eq!(
        host.accounts(&following, Some("other-scripted"))
            .await
            .unwrap()
            .selected,
        None
    );
    select(&host, &pinned, Some("work")).await;
    // Observe the options offered to a real adapter after each model rebuild.
    // No request is sent to the fixture's unused endpoint.
    for (provider, expected) in [
        ("other-scripted", "synthetic-other-token"),
        (PROVIDER, "synthetic-work-token"),
    ] {
        command(
            &host,
            &pinned,
            Command::Settings(SettingsChange {
                agent: AgentId::Main,
                persist: PersistAction::None,
                axis: SettingsAxis::Model(ModelInfo {
                    provider: provider.into(),
                    api: "openai-responses".into(),
                    ..scripted_model_info()
                }),
            }),
        )
        .await;
        let handles = host.local_handles(&pinned).await.unwrap();
        let options = handles.run_config.lock().unwrap().stream_options.clone();
        assert_eq!(options.resolve_api_key().await.unwrap().key, expected);
    }
    bounded(host.shutdown()).await;
}

#[tokio::test]
async fn saved_pins_and_resets_survive_restart_and_missing_accounts_never_fall_back() {
    let store = Store::new().await;
    let host = store.host();
    let session = host.create().await.unwrap();
    select(&host, &session, Some("work")).await;
    assert_turn(&host, &session, "work", "synthetic-work-token").await;
    bounded(host.shutdown()).await;

    let host = store.host();
    assert_turn(&host, &session, "work", "synthetic-work-token").await;
    bounded(host.shutdown()).await;
    store
        .external_auth()
        .remove_account(PROVIDER, "work")
        .await
        .unwrap();

    let host = store.host();
    let list = host.accounts(&session, None).await.unwrap();
    assert_eq!(list.selected.as_deref(), Some("work"));
    assert!(!list.accounts.iter().any(|label| label == "work"));
    assert_eq!(
        list.default.as_deref(),
        Some("personal"),
        "a usable fallback exists"
    );
    let frames = turn(&host, &session, AgentId::Main).await;
    let replies = messages(&frames, AgentId::Main);
    assert_eq!(replies.len(), 1);
    let error = replies[0]
        .error
        .as_ref()
        .expect("missing saved pin must fail inference");
    assert_eq!(error.category, ErrorCategory::Auth);
    assert!(error.message.contains("work"), "{error:?}");
    assert!(
        replies[0].content.is_empty(),
        "no fallback credential was served"
    );

    select(&host, &session, None).await;
    assert_turn(&host, &session, "personal", "synthetic-personal-token").await;
    bounded(host.shutdown()).await;
    store
        .external_auth()
        .set_default_account(PROVIDER, "")
        .await
        .unwrap();
    let host = store.host();
    assert_eq!(host.accounts(&session, None).await.unwrap().selected, None);
    assert_turn(&host, &session, "", "synthetic-unnamed-token").await;
    bounded(host.shutdown()).await;
}

#[tokio::test]
async fn creator_account_is_applied_before_the_optional_first_prompt() {
    let store = Store::new().await;
    let host = store.host();
    let (ready, release) = store.provider.hold_next();
    let session = bounded(host.create_with(
        Some(SessionSettings {
            account: Some(AccountSelection {
                name: Some(String::new()),
            }),
            ..SessionSettings::default()
        }),
        Some(vec![UserContent::text("start immediately")]),
        None,
        None,
    ))
    .await
    .unwrap();
    bounded(ready).await.unwrap();
    let mut stream = attach(&host, &session).await;
    release.send(()).unwrap();
    let frames = finish_turn(&mut stream, AgentId::Main).await;
    let replies = messages(&frames, AgentId::Main);
    assert_eq!(replies.len(), 1);
    assert_credential(replies[0], "", "synthetic-unnamed-token");
    assert_eq!(
        host.accounts(&session, None)
            .await
            .unwrap()
            .selected
            .as_deref(),
        Some("")
    );
    drop(stream);
    bounded(host.shutdown()).await;

    store
        .external_auth()
        .set_default_account(PROVIDER, "work")
        .await
        .unwrap();
    let host = store.host();
    assert_turn(&host, &session, "", "synthetic-unnamed-token").await;
    bounded(host.shutdown()).await;
}

#[tokio::test]
async fn retained_subagents_and_compaction_use_new_picks_without_rebilling_inflight_requests() {
    let store = Store::new().await;
    let host = store.host();
    let session = host.create().await.unwrap();
    store.provider.delegate_next.store(true, Ordering::SeqCst);
    let frames = turn(&host, &session, AgentId::Main).await;
    let child = AgentId::Sub(1);
    let replies = messages(&frames, child);
    assert_eq!(
        replies.len(),
        1,
        "the real agent tool ran a child inference"
    );
    assert_credential(replies[0], "personal", "synthetic-personal-token");
    let handles = host.local_handles(&session).await.unwrap();
    let retained = handles
        .registry
        .get(1)
        .expect("child is retained for continuation");

    let (ready, release) = store.provider.hold_next();
    let mut stream = attach(&host, &session).await;
    command(&host, &session, prompt(child)).await;
    bounded(ready).await.unwrap();
    select(&host, &session, Some("work")).await;
    release.send(()).unwrap();
    let frames = finish_turn(&mut stream, child).await;
    let replies = messages(&frames, child);
    assert_eq!(replies.len(), 1, "held child continuation: {frames:#?}");
    assert_credential(replies[0], "personal", "synthetic-personal-token");
    drop(stream);

    assert!(
        Arc::ptr_eq(&retained, &handles.registry.get(1).unwrap()),
        "selection must not replace the retained child"
    );
    let frames = turn(&host, &session, child).await;
    let replies = messages(&frames, child);
    assert_eq!(replies.len(), 1);
    assert_credential(replies[0], "work", "synthetic-work-token");

    // Compaction needs completed history before the turn it retains.
    assert_turn(&host, &session, "work", "synthetic-work-token").await;
    let mut stream = attach(&host, &session).await;
    command(&host, &session, Command::Compact { instructions: None }).await;
    let frames = frames_until(&mut stream, |frame| {
        matches!(frame, Frame::State { working: false, .. })
    })
    .await;
    let end = frames
        .iter()
        .find_map(|frame| match frame {
            Frame::Event { event, .. } => match event.known()? {
                AgentEvent::CompactionEnd { summary, error, .. } => Some((summary, error)),
                _ => None,
            },
            _ => None,
        })
        .expect("compaction must infer, not report nothing to compact");
    assert!(end.1.is_none(), "compaction failed: {:?}", end.1);
    assert!(
        end.0
            .as_deref()
            .is_some_and(|summary| summary.contains("synthetic-work-token")),
        "compaction must actually infer with the current session credential: {:?}",
        end.0
    );
    drop((stream, retained, handles));
    bounded(host.shutdown()).await;
}
