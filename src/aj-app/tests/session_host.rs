//! The session host: lifecycle, fan-out, attach, commands, reads
//! (spec section 5, 6.3-6.9).
//!
//! Every test drives the real host over the scripted provider, so the
//! frames asserted on are the ones a network server would serialize and
//! the client fold ([`SessionClient`]) would receive.

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use aj_agent::events::{AgentEvent, AgentId, AgentSettings};
use aj_agent::tool::{TaskKind, TaskOutputSource, TaskRead, TaskStatus};
use aj_app::chat::ChatState;
use aj_app::client::SessionClient;
use aj_app::host::{
    AttachRequest, Attachment, Command, CommandOutcome, HeadTarget, HostError, HostSetup, QueueOp,
    SessionHost, SettingsAxis, SettingsChange,
};
use aj_app::session_setup::RunConfigSnapshot;
use aj_app::settings::{ConfigLayers, PersistAction};
use aj_app::test_support::{
    CanonicalState, assert_canonical_eq, assert_no_dangling, finalized_text_message,
    finalized_text_message_with_usage, scripted_model_info,
};
use aj_conf::{Config, ConfigLayer, ConfigThinkingDisplay};
use aj_models::auth::AuthStorage;
use aj_models::scripted::{ExhaustedBehavior, ScriptedProvider};
use aj_models::types::{AssistantContent, AssistantMessage, StopReason, ToolCall, UserContent};
use aj_session::{ConversationPersistence, SessionLock, ThreadFilter};
use aj_wire::{Frame, ModelSelection, SessionSettings, SessionSummary};
use tempfile::TempDir;

/// Every wait in this file is bounded by this, so a wedged host fails a
/// test instead of hanging CI.
const DEADLINE: Duration = Duration::from_secs(20);

/// Long enough for the host's `list` debounce to publish whatever it had
/// coalesced, so a test can tell "nothing more is coming" from "not yet".
const LIST_SETTLE: Duration = Duration::from_millis(600);

/// A host over a temp sessions store, plus the store handles a test needs
/// to look behind the host's back (the lock, the on-disk log).
struct Harness {
    _dir: TempDir,
    persistence: ConversationPersistence,
    /// The effective config the host's sessions read, so a test can tune
    /// what a turn does (the compaction budget, say).
    config: Arc<StdMutex<Config>>,
    host: SessionHost,
}

/// The config every harness session reads, with bash's spill files aimed
/// inside `dir`.
///
/// A background task's spill is persisted by contract, so left at the ambient
/// temp directory it would outlive the test that started the task.
fn harness_config(dir: &TempDir) -> Config {
    Config {
        spill_dir: Some(dir.path().join("spill").to_string_lossy().into_owned()),
        ..Config::default()
    }
}

impl Harness {
    /// A host whose sessions run the scripted provider replaying
    /// `messages`. Every session materialized from this host shares that
    /// one script, so a test with two concurrent sessions installs a
    /// per-session provider instead (see [`Harness::install_script`]).
    fn new(messages: Vec<AssistantMessage>) -> Self {
        Self::with_provider(scripted(messages, 0, Duration::ZERO))
    }

    /// A host that releases an idle, unattached session after `grace`, for the
    /// tests about what it holds and for how long.
    fn with_idle_grace(messages: Vec<AssistantMessage>, grace: Duration) -> Self {
        Self::with_run_config(
            snapshot(scripted(messages, 0, Duration::ZERO)),
            Vec::new(),
            Some(grace),
            None,
        )
    }

    /// A host whose clients are evicted after `capacity` undeliverable
    /// frames, so a test can watch the flow-control rule of spec 6.9 without
    /// generating hundreds of them.
    fn with_live_capacity(messages: Vec<AssistantMessage>, capacity: usize) -> Self {
        Self::with_run_config(
            snapshot(scripted(messages, 0, Duration::ZERO)),
            Vec::new(),
            None,
            NonZeroUsize::new(capacity),
        )
    }

    fn with_provider(provider: Arc<ScriptedProvider>) -> Self {
        Self::with_catalog(provider, Vec::new())
    }

    fn with_catalog(
        provider: Arc<ScriptedProvider>,
        catalog: Vec<aj_models::registry::ModelInfo>,
    ) -> Self {
        Self::with_run_config(snapshot(provider), catalog, None, None)
    }

    /// A host whose base run config is `run_config`, for tests about what the
    /// host defaults a session to.
    fn with_run_config(
        run_config: RunConfigSnapshot,
        catalog: Vec<aj_models::registry::ModelInfo>,
        idle_grace: Option<Duration>,
        live_capacity: Option<NonZeroUsize>,
    ) -> Self {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().join("sessions"));
        let config = Arc::new(StdMutex::new(harness_config(&dir)));
        let host = SessionHost::new(HostSetup {
            config: Arc::clone(&config),
            layers: Arc::new(StdMutex::new(ConfigLayers {
                user: Config::default(),
                project: ConfigLayer::default(),
                project_path: None,
            })),
            catalog: Arc::new(catalog),
            run_config,
            restore: None,
            persistence: persistence.clone(),
            auth: AuthStorage::new(dir.path().join("auth.json")),
            working_directory: dir.path().to_path_buf(),
            idle_grace,
            live_capacity,
        })
        .expect("host");
        Self {
            _dir: dir,
            persistence,
            config,
            host,
        }
    }

    /// Point one live session at its own script, so sessions on one host
    /// can run turns that do not consume each other's messages. Goes
    /// through the in-process handles, which is also the assertion that
    /// the run config is per session.
    async fn install_script(&self, session: &str, messages: Vec<AssistantMessage>) {
        let handles = self
            .host
            .local_handles(session)
            .await
            .expect("live session");
        let mut cfg = handles
            .run_config
            .lock()
            .expect("run config mutex poisoned");
        cfg.provider = scripted(messages, 0, Duration::ZERO);
    }

    /// A second host over the same session store, as a restart or a rival
    /// process would see it.
    fn revive(&self, messages: Vec<AssistantMessage>) -> Harness {
        self.revive_with_idle_grace(messages, None)
    }

    /// A second host over the same store, releasing idle sessions after
    /// `idle_grace`.
    fn revive_with_idle_grace(
        &self,
        messages: Vec<AssistantMessage>,
        idle_grace: Option<Duration>,
    ) -> Harness {
        let dir = TempDir::new().expect("tempdir");
        let config = Arc::new(StdMutex::new(harness_config(&dir)));
        let host = SessionHost::new(HostSetup {
            config: Arc::clone(&config),
            layers: Arc::new(StdMutex::new(ConfigLayers {
                user: Config::default(),
                project: ConfigLayer::default(),
                project_path: None,
            })),
            catalog: Arc::new(Vec::new()),
            run_config: snapshot(scripted(messages, 0, Duration::ZERO)),
            restore: None,
            persistence: self.persistence.clone(),
            auth: AuthStorage::new(dir.path().join("auth.json")),
            working_directory: dir.path().to_path_buf(),
            idle_grace,
            live_capacity: None,
        })
        .expect("host");
        Harness {
            _dir: dir,
            persistence: self.persistence.clone(),
            config,
            host,
        }
    }

    async fn create(&self) -> String {
        self.host.create().await.expect("create session")
    }

    async fn prompt(&self, session: &str, text: &str) {
        self.host
            .command(session, prompt(text))
            .await
            .expect("prompt accepted");
    }
}

fn scripted(
    messages: Vec<AssistantMessage>,
    chunk_size: usize,
    chunk_delay: Duration,
) -> Arc<ScriptedProvider> {
    Arc::new(
        ScriptedProvider::from_messages(messages, chunk_size, chunk_delay)
            .on_exhausted(ExhaustedBehavior::Panic),
    )
}

fn snapshot(provider: Arc<ScriptedProvider>) -> RunConfigSnapshot {
    RunConfigSnapshot {
        provider,
        model_info: Arc::new(scripted_model_info()),
        stream_options: aj_models::types::StreamOptions::default(),
        thinking: None,
        thinking_display: None,
        speed: None,
        model_key: ("scripted".to_string(), "scripted".to_string()),
        session_id: None,
    }
}

struct FixedTaskOutput;

impl TaskOutputSource for FixedTaskOutput {
    fn snapshot(&self) -> TaskRead {
        TaskRead {
            stdout_tail: "stdout tail".into(),
            stderr_tail: "stderr tail".into(),
            stdout_total_bytes: 50,
            stderr_total_bytes: 12,
            spill_path: Some("/host/private/spill".into()),
            report: Some("agent report".into()),
        }
    }
}

fn prompt(text: &str) -> Command {
    Command::Prompt {
        agent: AgentId::Main,
        content: vec![UserContent::text(text)],
    }
}

/// A message that calls `tool` and stops for its result.
fn calling(text: &str, call_id: &str, tool: &str, args: serde_json::Value) -> AssistantMessage {
    let mut message = finalized_text_message(text);
    message.content.push(AssistantContent::ToolCall(ToolCall {
        id: call_id.to_string(),
        name: tool.to_string(),
        arguments: args,
    }));
    message.stop_reason = StopReason::ToolUse;
    message
}

/// A turn that spawns a blocking sub-agent, then concludes. The parent and
/// the child share the provider, so the scripts are consumed in run order.
fn sub_agent_turn() -> Vec<AssistantMessage> {
    vec![
        calling(
            "delegating that",
            "call-sub",
            "agent",
            serde_json::json!({"task": "look into it"}),
        ),
        finalized_text_message("the sub found nothing"),
        finalized_text_message("nothing to report"),
    ]
}

/// A turn that spawns a background sub-agent and keeps working, so the
/// child's appends interleave with the parent's.
fn background_sub_turn() -> Vec<AssistantMessage> {
    vec![
        calling(
            "kicking that off",
            "call-bg",
            "agent",
            serde_json::json!({"task": "look into it", "run_in_background": true}),
        ),
        finalized_text_message("meanwhile, here is the answer"),
        finalized_text_message("the background sub is done"),
        // The background task's completion notice wakes the parent, which
        // runs one more inference to acknowledge it.
        finalized_text_message("noted, thanks"),
    ]
}

/// Await `future`, failing the test rather than hanging.
async fn bounded<T>(what: &str, future: impl std::future::Future<Output = T>) -> T {
    match tokio::time::timeout(DEADLINE, future).await {
        Ok(value) => value,
        Err(_) => panic!("timed out waiting for {what}"),
    }
}

/// Collect frames until `done` accepts one, that frame included.
async fn frames_until(
    stream: &mut Attachment,
    what: &str,
    mut done: impl FnMut(&Frame) -> bool,
) -> Vec<Frame> {
    let mut out = Vec::new();
    bounded(what, async {
        while let Some(frame) = stream.recv().await {
            let stop = done(&frame);
            out.push(frame);
            if stop {
                return;
            }
        }
        panic!("the stream closed before {what}");
    })
    .await;
    out
}

/// Whether `frame` is the `state` frame reporting the main agent idle.
fn idle_state(frame: &Frame) -> bool {
    matches!(frame, Frame::State { working: false, .. })
}

/// Frames up to and including the `state` frame that reports the session
/// idle again. A command that starts a turn publishes `working: true`
/// before it returns, so the next idle `state` is that turn's end.
async fn until_idle(stream: &mut Attachment) -> Vec<Frame> {
    frames_until(stream, "the turn to settle", idle_state).await
}

/// The `(steering, follow_up)` counts a `QueueUpdate` frame carries, or
/// `None` for any other frame.
fn queue_counts(frame: &Frame) -> Option<(usize, usize)> {
    let Frame::Event { event, .. } = frame else {
        return None;
    };
    match event.known()? {
        AgentEvent::QueueUpdate {
            steering,
            follow_up,
            ..
        } => Some((steering.len(), follow_up.len())),
        _ => None,
    }
}

/// Drain frames until the session reports itself idle with no live
/// background task, so a run whose background sub-agent outlives the
/// parent turn is fully covered.
async fn settle(harness: &Harness, session: &str, stream: &mut Attachment) -> Vec<Frame> {
    let mut out = Vec::new();
    bounded("the session to go quiet", async {
        loop {
            out.extend(drained(stream));
            let list = harness.host.sessions().await.expect("sessions");
            let quiet = list
                .sessions
                .iter()
                .find(|entry| entry.id == session)
                .is_some_and(|entry| !entry.working && entry.tasks == 0);
            if quiet {
                out.extend(drained(stream));
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    out
}

/// Everything already queued on the stream, without waiting.
fn drained(stream: &mut Attachment) -> Vec<Frame> {
    let mut out = Vec::new();
    while let Some(frame) = stream.try_recv() {
        out.push(frame);
    }
    out
}

/// The frames of one session. A stream carries every session's durable and
/// reliable-transient frames, attached or not (spec 6.5), so a test that
/// asserts on one session has to say which.
fn only(frames: Vec<Frame>, session: &str) -> Vec<Frame> {
    frames
        .into_iter()
        .filter(|frame| frame.session() == Some(session))
        .collect()
}

fn events(frames: &[Frame]) -> Vec<&AgentEvent> {
    frames
        .iter()
        .filter_map(|frame| match frame {
            Frame::Event { event, .. } => event.known(),
            _ => None,
        })
        .collect()
}

/// Whether `frames` carry a `Notice` reading exactly `text`.
fn notice(frames: &[Frame], text: &str) -> bool {
    events(frames)
        .into_iter()
        .any(|event| matches!(event, AgentEvent::Notice { text: seen, .. } if seen == text))
}

/// The text of every `Error` event in `frames`.
fn errors(frames: &[Frame]) -> Vec<String> {
    events(frames)
        .into_iter()
        .filter_map(|event| match event {
            AgentEvent::Error { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// The notice the host publishes for a turn that was cancelled. A turn that
/// ran to completion publishes none, which is what makes it evidence.
const CANCELLED: &str = "Turn cancelled.";

/// One event's `type` tag, for assertion messages that name a sequence of
/// events without dumping their payloads.
fn event_kind(event: &AgentEvent) -> String {
    serde_json::to_value(event)
        .ok()
        .and_then(|value| value["type"].as_str().map(str::to_string))
        .unwrap_or_default()
}

/// The `(seq, entry_id)` of every durable frame, in delivery order.
fn durable(frames: &[Frame]) -> Vec<(u64, String)> {
    frames
        .iter()
        .filter_map(|frame| match frame {
            Frame::Event {
                durability: Some(durability),
                ..
            } => Some((durability.seq, durability.entry_id.clone())),
            _ => None,
        })
        .collect()
}

/// The concatenated assistant text of every finalized message.
fn assistant_text(frames: &[Frame]) -> String {
    events(frames)
        .into_iter()
        .filter_map(|event| match event {
            AgentEvent::MessageEnd { message, .. } => message.as_stored_wire(),
            _ => None,
        })
        .filter_map(|message| match message {
            aj_models::types::Message::Assistant(assistant) => Some(assistant),
            _ => None,
        })
        .flat_map(|assistant| {
            assistant
                .content
                .iter()
                .filter_map(|block| match block {
                    AssistantContent::Text(text) => Some(text.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>()
        .join("|")
}

fn epoch_of(frames: &[Frame]) -> String {
    frames
        .iter()
        .find_map(|frame| match frame {
            Frame::State { epoch, .. } => Some(epoch.clone()),
            _ => None,
        })
        .expect("a state frame carries the epoch")
}

/// One attached client: the real fold plus the chat model it folds into.
struct Client {
    client: SessionClient,
    chat: ChatState,
    stream: Attachment,
}

impl Client {
    /// Attach `session` with no cursor and apply the whole attach block.
    async fn attach(host: &SessionHost, session: &str) -> Self {
        let stream = host
            .attach(&[AttachRequest {
                session: session.to_string(),
                cursor: None,
            }])
            .await
            .expect("attach");
        let mut this = Self {
            client: SessionClient::new(session.to_string()),
            chat: ChatState::new(settings(), 200_000, Arc::new(Vec::new())),
            stream,
        };
        // Armed from what the attach reports it served, which is the
        // contract on `expect_attach`: an arm for a block that never
        // arrives freezes the fold's cursor.
        for served in this.stream.attached() {
            assert_eq!(served, session, "one block, for the session asked for");
        }
        this.client.expect_attach();
        this.apply_block().await;
        this
    }

    /// Apply frames up to and including the block's `caught_up`, returning
    /// them so a test can assert on what the block carried.
    async fn apply_block(&mut self) -> Vec<Frame> {
        let frames = frames_until(&mut self.stream, "caught_up", |frame| {
            matches!(frame, Frame::CaughtUp { .. })
        })
        .await;
        for frame in &frames {
            let _ = self.client.apply(&mut self.chat, frame.clone());
        }
        frames
    }

    /// Attach again, offering `cursor`, and apply the block that follows.
    ///
    /// The old stream is dropped only once the new one has been served, so
    /// nothing is lost in between: this is a client re-attaching to reconcile
    /// an older cursor, not one recovering from a disconnect.
    async fn reattach(&mut self, host: &SessionHost, cursor: aj_wire::Cursor) -> Vec<Frame> {
        self.stream = host
            .attach(&[AttachRequest {
                session: self.client.session().to_string(),
                cursor: Some(cursor),
            }])
            .await
            .expect("re-attach");
        self.client.expect_attach();
        self.apply_block().await
    }

    /// Fold everything already queued on the stream, without waiting. The
    /// frame a command earns is queued before the command returns, so this
    /// is enough to see its effect.
    fn drain_into_fold(&mut self) -> Vec<Frame> {
        let frames = drained(&mut self.stream);
        for frame in &frames {
            let _ = self.client.apply(&mut self.chat, frame.clone());
        }
        frames
    }

    /// Fold until the session reports idle, returning what was folded so a
    /// test can assert on the frames as well as on the resulting state.
    async fn pump_until_idle(&mut self) -> Vec<Frame> {
        let frames = until_idle(&mut self.stream).await;
        for frame in &frames {
            let _ = self.client.apply(&mut self.chat, frame.clone());
        }
        frames
    }

    fn canonical(&self) -> CanonicalState {
        CanonicalState::of(&self.chat, &self.client)
    }
}

/// The thinking effort one live session's run config stages.
async fn thinking(host: &SessionHost, session: &str) -> Option<aj_models::ThinkingConfig> {
    let handles = host.local_handles(session).await.expect("live session");
    let cfg = handles
        .run_config
        .lock()
        .expect("run config mutex poisoned");
    cfg.thinking.clone()
}

/// The `(status, finished)` of sub-agent `child`'s box in the main
/// transcript.
fn sub_box(state: &CanonicalState, child: usize) -> (aj_app::chat::SubAgentStatus, bool) {
    state
        .agent(AgentId::Main)
        .expect("main transcript")
        .entries
        .iter()
        .find_map(|entry| match entry {
            aj_app::test_support::CanonicalEntry::SubAgent {
                child: n,
                status,
                finished,
                ..
            } if *n == child => Some((*status, *finished)),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no box for sub-agent {child}"))
}

/// The tools the main transcript's cells name, in order.
fn main_tools(state: &CanonicalState) -> Vec<String> {
    state
        .agent(AgentId::Main)
        .expect("main transcript")
        .entries
        .iter()
        .filter_map(|entry| match entry {
            aj_app::test_support::CanonicalEntry::Tool { tool, .. } => Some(tool.clone()),
            _ => None,
        })
        .collect()
}

/// The report sub-agent `child`'s box renders, which is what a client shows
/// for a run it is not observing.
fn sub_report(state: &CanonicalState, child: usize) -> Option<String> {
    state
        .agent(AgentId::Main)
        .expect("main transcript")
        .entries
        .iter()
        .find_map(|entry| match entry {
            aj_app::test_support::CanonicalEntry::SubAgent {
                child: n, report, ..
            } if *n == child => Some(report.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no box for sub-agent {child}"))
}

fn settings() -> AgentSettings {
    AgentSettings {
        provider: "scripted".into(),
        model_id: "scripted".into(),
        thinking: "off".into(),
        thinking_display: "default".into(),
        speed: "standard".into(),
        verbosity: "default".into(),
    }
}

/// The text of every assistant row in `agent`'s transcript, streaming rows
/// included.
fn assistant_rows(chat: &ChatState, agent: AgentId) -> Vec<String> {
    rows(chat, agent, |kind| match kind {
        aj_app::chat::EntryKind::Assistant(assistant) => Some(
            assistant
                .message
                .content
                .iter()
                .filter_map(|block| match block {
                    AssistantContent::Text(text) => Some(text.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(""),
        ),
        _ => None,
    })
}

/// The tool every tool cell in `agent`'s transcript names.
fn tool_cells(chat: &ChatState, agent: AgentId) -> Vec<String> {
    rows(chat, agent, |kind| match kind {
        aj_app::chat::EntryKind::Tool(tool) => Some(tool.tool.clone()),
        _ => None,
    })
}

fn rows(
    chat: &ChatState,
    agent: AgentId,
    pick: impl Fn(&aj_app::chat::EntryKind) -> Option<String>,
) -> Vec<String> {
    chat.transcript(agent)
        .map(|transcript| {
            transcript
                .entries()
                .iter()
                .filter_map(|entry| pick(&entry.kind))
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// 1. Creation and materialization
// ---------------------------------------------------------------------------

/// A created session runs a turn, and its frames carry its id, its epoch,
/// and the durable positions of the entries the turn appended.
#[tokio::test]
async fn a_created_session_runs_a_turn_and_publishes_its_frames() {
    let harness = Harness::new(vec![finalized_text_message("hello back")]);
    let session = harness.create().await;
    let mut stream = harness
        .host
        .attach(&[AttachRequest {
            session: session.clone(),
            cursor: None,
        }])
        .await
        .expect("attach");
    let block = frames_until(&mut stream, "caught_up", |frame| {
        matches!(frame, Frame::CaughtUp { .. })
    })
    .await;
    assert!(
        matches!(&block[0], Frame::State { session: s, .. } if *s == session),
        "the block opens with this session's state frame",
    );

    harness.prompt(&session, "hi").await;
    let frames = until_idle(&mut stream).await;

    assert_eq!(assistant_text(&frames), "hello back");
    assert!(
        frames.iter().all(|frame| match frame {
            Frame::Event { session: s, .. } | Frame::State { session: s, .. } => *s == session,
            Frame::List { .. } => true,
            other => panic!("unexpected frame kind {other:?}"),
        }),
        "every session-scoped frame names this session",
    );
    let seqs: Vec<u64> = durable(&frames).into_iter().map(|(seq, _)| seq).collect();
    assert!(
        seqs.windows(2).all(|pair| pair[0] < pair[1]),
        "live durable seqs are strictly increasing: {seqs:?}",
    );
    harness.host.shutdown().await;
}

#[tokio::test]
async fn explicit_creation_applies_settings_before_its_first_prompt() {
    let harness = Harness::new(vec![finalized_text_message("created remotely")]);
    let session = harness
        .host
        .create_with(
            Some(SessionSettings {
                model: Some(ModelSelection {
                    api: "scripted".into(),
                    url: None,
                    name: "scripted".into(),
                }),
                thinking: Some("off".into()),
                thinking_display: Some("detailed".into()),
                speed: Some("fast".into()),
                verbosity: Some("high".into()),
            }),
            Some(vec![UserContent::text("begin")]),
        )
        .await
        .expect("create with settings");

    let handles = harness.host.local_handles(&session).await.expect("handles");
    let settings = handles
        .run_config
        .lock()
        .expect("run config mutex poisoned")
        .settings();
    assert_eq!(settings.thinking_display, "detailed");
    assert_eq!(settings.speed, "fast");
    assert_eq!(settings.verbosity, "high");

    let client = Client::attach(&harness.host, &session).await;
    assert!(
        format!("{:?}", client.canonical()).contains("created remotely"),
        "the optional prompt was accepted after creation",
    );
    harness.host.shutdown().await;
}

#[tokio::test]
async fn an_unstated_axis_defaults_against_the_model_the_session_runs() {
    // A host whose configured level its own model has no word for, which is
    // ordinary: the level comes from a config file, the model from a catalog.
    let mut base = snapshot(scripted(Vec::new(), 0, Duration::ZERO));
    base.thinking = Some(aj_models::ThinkingConfig::XHigh);
    let harness = Harness::with_run_config(base, Vec::new(), None, None);

    // Something is stated, but not thinking, so the host defaults that axis
    // against the model it actually runs (spec section 8).
    let session = harness
        .host
        .create_with(
            Some(SessionSettings {
                speed: Some("fast".into()),
                ..SessionSettings::default()
            }),
            None,
        )
        .await
        .expect("an unstated axis is the host's to default");
    assert!(
        thinking(&harness.host, &session).await.is_none(),
        "a level the model cannot serve is not what an unstated axis resolves to",
    );

    // Stated, and therefore strict: no clamping, no substitution.
    let refused = harness
        .host
        .create_with(
            Some(SessionSettings {
                thinking: Some("xhigh".into()),
                ..SessionSettings::default()
            }),
            None,
        )
        .await
        .expect_err("a stated level the model cannot serve is refused");
    assert!(
        format!("{refused}").contains("does not support thinking level"),
        "the refusal names what went wrong: {refused}",
    );
    harness.host.shutdown().await;
}

#[tokio::test]
async fn refused_creation_leaves_no_discoverable_session() {
    let harness = Harness::new(Vec::new());
    for (settings, prompt) in [
        (
            Some(SessionSettings {
                speed: Some("warp".into()),
                ..SessionSettings::default()
            }),
            None,
        ),
        (
            Some(SessionSettings {
                model: Some(ModelSelection {
                    api: "missing".into(),
                    url: None,
                    name: "missing".into(),
                }),
                ..SessionSettings::default()
            }),
            None,
        ),
        (None, Some(Vec::new())),
    ] {
        harness
            .host
            .create_with(settings, prompt)
            .await
            .expect_err("creation is refused");
        assert!(
            harness
                .host
                .sessions()
                .await
                .expect("sessions")
                .sessions
                .is_empty(),
            "validation happens before the log is created",
        );
    }
    harness.host.shutdown().await;
}

#[tokio::test]
async fn creation_resolves_real_models_from_the_host_catalog_with_lazy_auth() {
    let mut real = scripted_model_info();
    real.provider = "openai".into();
    real.api = "openai-responses".into();
    real.id = "gpt-catalog".into();
    real.base_url = "https://catalog.example/v1".into();
    let harness = Harness::with_catalog(scripted(Vec::new(), 0, Duration::ZERO), vec![real]);
    let session = harness
        .host
        .create_with(
            Some(SessionSettings {
                model: Some(ModelSelection {
                    api: "openai".into(),
                    url: Some("https://override.example/v1".into()),
                    name: "gpt-catalog".into(),
                }),
                ..SessionSettings::default()
            }),
            None,
        )
        .await
        .expect("lazy credentials do not prevent session creation");
    let handles = harness.host.local_handles(&session).await.expect("handles");
    let (model_key, base_url) = {
        let config = handles
            .run_config
            .lock()
            .expect("run config mutex poisoned");
        (config.model_key.clone(), config.model_info.base_url.clone())
    };
    assert_eq!(model_key, ("openai".into(), "gpt-catalog".into()));
    assert_eq!(base_url, "https://override.example/v1");
    harness.host.shutdown().await;
}

/// A session that is only on disk materializes when a client attaches it,
/// and its backfill carries the recorded turn.
#[tokio::test]
async fn attaching_a_known_session_materializes_it() {
    let harness = Harness::new(vec![finalized_text_message("recorded answer")]);
    let session = harness.create().await;
    let mut stream = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "hi").await;
    until_idle(&mut stream.stream).await;
    drop(stream);
    harness.host.shutdown().await;

    // A fresh host over the same store knows the session only from disk.
    let revived = harness.revive(Vec::new());
    let listed = revived.host.sessions().await.expect("sessions");
    let summary = listed
        .sessions
        .iter()
        .find(|entry| entry.id == session)
        .expect("the session is listed from disk");
    assert!(!summary.live, "it is not materialized yet");

    let client = Client::attach(&revived.host, &session).await;
    assert!(
        format!("{:?}", client.canonical()).contains("recorded answer"),
        "the backfill carries the recorded turn",
    );
    assert!(
        revived
            .host
            .sessions()
            .await
            .expect("sessions")
            .sessions
            .iter()
            .any(|entry| entry.id == session && entry.live),
        "attaching materialized it",
    );
    drop(client);
    revived.host.shutdown().await;
}

/// A command naming a known-on-disk session materializes it too.
#[tokio::test]
async fn commanding_a_known_session_materializes_it() {
    let harness = Harness::new(vec![finalized_text_message("first")]);
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "hi").await;
    client.pump_until_idle().await;
    drop(client);
    harness.host.shutdown().await;

    let revived = harness.revive(vec![finalized_text_message("second")]);
    revived
        .host
        .command(&session, Command::Queue(QueueOp::Clear))
        .await
        .expect("a queue command materializes the session");
    assert!(
        revived
            .host
            .sessions()
            .await
            .expect("sessions")
            .sessions
            .iter()
            .any(|entry| entry.id == session && entry.live),
    );
    revived.host.shutdown().await;
}

/// An unknown session is a 404-shaped error, not a materialization.
#[tokio::test]
async fn an_unknown_session_is_refused() {
    let harness = Harness::new(Vec::new());
    let err = harness
        .host
        .command("not-a-session", prompt("hi"))
        .await
        .expect_err("unknown sessions are refused");
    assert!(matches!(err, HostError::UnknownSession(_)), "got {err:?}");
    harness.host.shutdown().await;
}

/// An id that could never name a log in this store is refused at every
/// entry point, and refused off its own shape: it does not reach the store
/// at all (spec 6.2).
///
/// One of the ids points at a real, readable log just outside the store, so
/// the refusal cannot be the file simply not being there.
#[tokio::test]
async fn an_id_that_is_not_a_session_id_never_reaches_the_store() {
    let harness = Harness::new(Vec::new());
    // An empty log counts as the current format, so this is a file the store
    // would happily call a session if an id could name it.
    let outside = harness._dir.path().join("elsewhere");
    std::fs::create_dir_all(&outside).expect("a directory beside the store");
    std::fs::write(outside.join("reachable.jsonl"), "").expect("a log outside the store");

    // Taken after construction's enumeration, so the assertions below measure
    // what these calls added.
    let reads = harness.host.store_directory_reads();
    let lookups = harness.host.store_membership_lookups();

    for id in [
        "",
        "..",
        "../elsewhere/reachable",
        "a/b",
        "sneaky.jsonl",
        "hé",
    ] {
        let err = harness
            .host
            .command(id, prompt("hi"))
            .await
            .expect_err("a command names no path");
        assert!(
            matches!(err, HostError::UnknownSession(_)),
            "{id:?}: {err:?}"
        );

        let err = harness
            .host
            .attach(&[AttachRequest {
                session: id.to_string(),
                cursor: None,
            }])
            .await
            .err()
            .expect("an attach names no path");
        assert!(
            matches!(err, HostError::UnknownSession(_)),
            "{id:?}: {err:?}"
        );

        let err = harness
            .host
            .tasks(id)
            .await
            .err()
            .expect("a read names no path");
        assert!(
            matches!(err, HostError::UnknownSession(_)),
            "{id:?}: {err:?}"
        );

        let err = harness
            .host
            .tree(id)
            .await
            .err()
            .expect("the tree read names no path");
        assert!(
            matches!(err, HostError::UnknownSession(_)),
            "{id:?}: {err:?}"
        );
    }

    assert_eq!(
        harness.host.store_membership_lookups(),
        lookups,
        "a refusal off the id's own shape put no question to the store",
    );
    assert_eq!(
        harness.host.store_directory_reads(),
        reads,
        "and read no directory to reach it",
    );

    // The same store answers a well-formed id, so the refusals above are the
    // grammar rather than a host that refuses everything.
    let session = harness.create().await;
    harness
        .host
        .tasks(&session)
        .await
        .expect("a well-formed id is served");
    harness.host.shutdown().await;
}

/// An attach reports the sessions it served, and one named twice is a
/// malformed request: the client contract is one block per named session
/// (spec 6.5), and the second block would open a phase the client is not
/// expecting and quiesce state it just applied.
#[tokio::test]
async fn an_attach_reports_what_it_served_and_refuses_a_duplicate() {
    let harness = Harness::new(Vec::new());
    let first = harness.create().await;
    let second = harness.create().await;

    let stream = harness
        .host
        .attach(&[
            AttachRequest {
                session: first.clone(),
                cursor: None,
            },
            AttachRequest {
                session: second.clone(),
                cursor: None,
            },
        ])
        .await
        .expect("attach");
    assert_eq!(stream.attached(), [first.clone(), second.clone()]);
    drop(stream);

    let err = harness
        .host
        .attach(&[
            AttachRequest {
                session: first.clone(),
                cursor: None,
            },
            AttachRequest {
                session: first.clone(),
                cursor: None,
            },
        ])
        .await
        .err()
        .expect("a session named twice is refused");
    assert!(matches!(err, HostError::Invalid(_)), "got {err:?}");

    // A refused attach hands back no stream, so a client has nothing to arm
    // its fold from.
    let err = harness
        .host
        .attach(&[AttachRequest {
            session: "not-a-session".to_string(),
            cursor: None,
        }])
        .await
        .err()
        .expect("an unknown session is refused");
    assert!(matches!(err, HostError::UnknownSession(_)), "got {err:?}");
    harness.host.shutdown().await;
}

#[tokio::test]
async fn a_large_attach_block_is_not_preloaded_before_attach_returns() {
    let mut calling = finalized_text_message("checking many things");
    calling.stop_reason = StopReason::ToolUse;
    for n in 0..20 {
        calling.content.push(AssistantContent::ToolCall(ToolCall {
            id: format!("call-{n}"),
            name: "todo_read".into(),
            arguments: serde_json::json!({}),
        }));
    }
    let harness = Harness::new(vec![calling, finalized_text_message("done")]);
    let session = harness.create().await;
    harness.prompt(&session, "check").await;
    bounded("the staged turn to settle", async {
        loop {
            let quiet = harness
                .host
                .sessions()
                .await
                .expect("sessions")
                .sessions
                .iter()
                .find(|summary| summary.id == session)
                .is_some_and(|summary| !summary.working);
            if quiet {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;

    let mut attachment = harness
        .host
        .attach(&[AttachRequest {
            session,
            cursor: None,
        }])
        .await
        .expect("attach");
    let mut block = drained(&mut attachment);
    assert!(
        block.len() <= 1,
        "the capacity-one producer cannot preload the backfill: {} frames",
        block.len(),
    );
    block.extend(
        frames_until(&mut attachment, "the producer-paced caught_up", |frame| {
            matches!(frame, Frame::CaughtUp { .. })
        })
        .await,
    );
    assert!(block.len() > 20, "the test built a substantial backfill");
    harness.host.shutdown().await;
}

/// The store's host id is claimed atomically, so two hosts starting in one
/// store cannot both mint one: the store would be advertised under two ids
/// and a gateway would see one working directory as two hosts.
#[tokio::test]
async fn the_host_id_is_claimed_not_written_over() {
    let harness = Harness::new(Vec::new());
    let path = harness.persistence.sessions_dir().join("host-id");
    let minted = harness.host.hello().host_id;
    assert_eq!(
        std::fs::read_to_string(&path)
            .expect("the id was written")
            .trim(),
        minted,
        "the id in the store is the one this host reports",
    );

    // Every later host over the store adopts it rather than minting.
    for _ in 0..2 {
        let revived = harness.revive(Vec::new());
        assert_eq!(revived.host.hello().host_id, minted);
        revived.host.shutdown().await;
    }

    // A blank file is a crashed mint. Overwriting it would reopen the race
    // the claim closes, so it is refused loudly instead.
    std::fs::write(&path, "\n").expect("blank the id");
    let err = SessionHost::new(HostSetup {
        config: Arc::new(StdMutex::new(Config::default())),
        layers: Arc::new(StdMutex::new(ConfigLayers {
            user: Config::default(),
            project: ConfigLayer::default(),
            project_path: None,
        })),
        catalog: Arc::new(Vec::new()),
        run_config: snapshot(scripted(Vec::new(), 0, Duration::ZERO)),
        restore: None,
        persistence: harness.persistence.clone(),
        auth: AuthStorage::new(harness._dir.path().join("auth.json")),
        working_directory: harness._dir.path().to_path_buf(),
        idle_grace: None,
        live_capacity: None,
    })
    .err()
    .expect("a blank host id is refused");
    assert!(err.to_string().contains("empty"), "got {err}");
    harness.host.shutdown().await;
}

// ---------------------------------------------------------------------------
// 2. Two concurrent sessions
// ---------------------------------------------------------------------------

/// Two live sessions on one host run their own turns: their frames carry
/// their own ids, their epochs differ, their seqs are independent, and
/// neither session's model bundle or prompt-cache key reaches the other.
#[tokio::test]
async fn two_sessions_on_one_host_stay_independent() {
    let harness = Harness::new(Vec::new());
    let first = harness.create().await;
    let second = harness.create().await;
    assert_ne!(first, second);
    harness
        .install_script(&first, vec![finalized_text_message("from the first")])
        .await;
    harness
        .install_script(&second, vec![finalized_text_message("from the second")])
        .await;

    let mut one = Client::attach(&harness.host, &first).await;
    let mut two = Client::attach(&harness.host, &second).await;

    harness.prompt(&first, "hi").await;
    harness.prompt(&second, "hi").await;
    let first_frames = only(until_idle(&mut one.stream).await, &first);
    let second_frames = only(until_idle(&mut two.stream).await, &second);

    assert_eq!(assistant_text(&first_frames), "from the first");
    assert_eq!(assistant_text(&second_frames), "from the second");
    assert_ne!(
        epoch_of(&first_frames),
        epoch_of(&second_frames),
        "each materialization mints its own epoch",
    );

    // Per-session seqs: each session's first durable entry is its own
    // position 1..n in its own log, not a host-wide counter.
    for (session, frames) in [(&first, &first_frames), (&second, &second_frames)] {
        let handles = harness
            .host
            .local_handles(session)
            .await
            .expect("live session");
        let log = handles.log.lock().await;
        for (seq, entry_id) in durable(frames) {
            let index = usize::try_from(seq).expect("seq fits usize") - 1;
            let entry = log
                .entries_in_order()
                .get(index)
                .map(|entry| entry.id.clone())
                .unwrap_or_else(|| panic!("no entry at append position {seq}"));
            assert_eq!(entry, entry_id, "seq {seq} names its own log's entry");
        }
    }

    // The prompt-cache key is the session's own id, which is exactly what
    // one shared run-config snapshot used to get wrong.
    for session in [&first, &second] {
        let handles = harness
            .host
            .local_handles(session)
            .await
            .expect("live session");
        let cfg = handles
            .run_config
            .lock()
            .expect("run config mutex poisoned");
        assert_eq!(cfg.session_id.as_deref(), Some(session.as_str()));
        assert_eq!(
            cfg.stream_options.session_id.as_deref(),
            Some(session.as_str())
        );
    }

    // And a settings change in one session leaves the other's alone.
    harness
        .host
        .command(
            &first,
            Command::Settings(SettingsChange {
                agent: AgentId::Main,
                persist: PersistAction::None,
                axis: SettingsAxis::Thinking(Some(aj_models::ThinkingConfig::High)),
            }),
        )
        .await
        .expect("thinking change");
    assert!(
        thinking(&harness.host, &first).await.is_some(),
        "the change landed",
    );
    assert!(
        thinking(&harness.host, &second).await.is_none(),
        "and did not leak into the other session",
    );
    harness.host.shutdown().await;
}

/// A stream sees nothing at all from a session it did not attach, not even
/// that session's durable and reliable-transient frames (spec 6.5). Only its
/// row in the directory travels.
///
/// The rule is what keeps a busy session a client never asked for from
/// filling that client's bounded queue and evicting it.
#[tokio::test]
async fn a_stream_sees_nothing_from_a_session_it_did_not_attach() {
    let harness = Harness::new(Vec::new());
    let watched = harness.create().await;
    let other = harness.create().await;
    harness
        .install_script(&watched, vec![finalized_text_message("watched")])
        .await;
    harness
        .install_script(&other, vec![finalized_text_message("unwatched")])
        .await;

    let mut client = Client::attach(&harness.host, &watched).await;
    // Attached before the prompt, so this client can tell when the turn it
    // is not watching has finished.
    let mut elsewhere = Client::attach(&harness.host, &other).await;

    // A whole turn on the session `client` did not attach, run to completion.
    harness.prompt(&other, "hi").await;
    elsewhere.pump_until_idle().await;
    drop(elsewhere);

    harness.prompt(&watched, "hi").await;
    let frames = client.pump_until_idle().await;
    tokio::time::sleep(LIST_SETTLE).await;
    let frames: Vec<Frame> = frames
        .into_iter()
        .chain(drained(&mut client.stream))
        .collect();

    assert!(
        !frames
            .iter()
            .any(|frame| frame.session() == Some(other.as_str())),
        "the unattached session put a frame on the stream",
    );
    assert_eq!(assistant_text(&only(frames.clone(), &watched)), "watched");
    // Its row is still in the directory, which is where attention for an
    // unattached session comes from.
    let listed = directories(&frames)
        .last()
        .cloned()
        .expect("a directory reached the stream");
    assert!(
        listed.iter().any(|row| row.id == other),
        "the unattached session is missing from the directory",
    );
    harness.host.shutdown().await;
}

// ---------------------------------------------------------------------------
// 3. Seq assignment against the log
// ---------------------------------------------------------------------------

/// Every durable frame's seq and entry id name the log entry at that
/// append position, including while a background sub-agent appends
/// concurrently with the main agent.
#[tokio::test]
async fn durable_frames_name_the_log_entry_at_their_append_position() {
    let harness = Harness::new(background_sub_turn());
    let session = harness.create().await;
    let mut stream = harness
        .host
        .attach(&[AttachRequest {
            session: session.clone(),
            cursor: None,
        }])
        .await
        .expect("attach");
    frames_until(&mut stream, "caught_up", |frame| {
        matches!(frame, Frame::CaughtUp { .. })
    })
    .await;

    harness.prompt(&session, "delegate it").await;
    let mut frames = until_idle(&mut stream).await;
    // The background sub-agent outlives the parent turn: its completion
    // notice wakes the parent for one more turn, so settling twice covers
    // the sub's own appends as well as the parent's.
    frames.extend(settle(&harness, &session, &mut stream).await);

    let tagged = durable(&frames);
    assert!(
        tagged.len() >= 5,
        "the run wrote several entries: {tagged:?}"
    );
    let seqs: Vec<u64> = tagged.iter().map(|(seq, _)| *seq).collect();
    assert!(
        seqs.windows(2).all(|pair| pair[0] < pair[1]),
        "strictly increasing per stream even with interleaved appends: {seqs:?}",
    );

    let handles = harness
        .host
        .local_handles(&session)
        .await
        .expect("live session");
    let log = handles.log.lock().await;
    let entries = log.entries_in_order();
    for (seq, entry_id) in &tagged {
        let index = usize::try_from(*seq).expect("seq fits usize") - 1;
        assert_eq!(
            entries.get(index).map(|entry| entry.id.as_str()),
            Some(entry_id.as_str()),
            "seq {seq} is the append position of {entry_id}",
        );
    }
    // Both agents appended, so the check above spans two threads.
    assert!(
        entries.iter().any(|entry| entry.agent_id == Some(1)),
        "the background sub-agent wrote to the log",
    );
    drop(log);
    harness.host.shutdown().await;
}

// ---------------------------------------------------------------------------
// 4 + 5. Epochs and head switching
// ---------------------------------------------------------------------------

/// Appends leave the epoch alone. A head switch replaces it, clears the
/// queues, and emits `reset`.
#[tokio::test]
async fn a_head_switch_replaces_the_epoch_and_resets_the_stream() {
    let harness = Harness::new(vec![
        finalized_text_message("first answer"),
        finalized_text_message("second answer"),
    ]);
    let session = harness.create().await;
    let mut stream = harness
        .host
        .attach(&[AttachRequest {
            session: session.clone(),
            cursor: None,
        }])
        .await
        .expect("attach");
    let block = frames_until(&mut stream, "caught_up", |frame| {
        matches!(frame, Frame::CaughtUp { .. })
    })
    .await;
    let epoch = epoch_of(&block);

    harness.prompt(&session, "one").await;
    let first = until_idle(&mut stream).await;
    assert_eq!(epoch_of(&first), epoch, "an append keeps the epoch");
    harness.prompt(&session, "two").await;
    let second = until_idle(&mut stream).await;
    assert_eq!(epoch_of(&second), epoch, "still the same materialization");

    // Branch at the first user message's parent: the head after turn one.
    let head = {
        let handles = harness
            .host
            .local_handles(&session)
            .await
            .expect("live session");
        let log = handles.log.lock().await;
        let head = log.head().cloned().expect("a head");
        let conversation = log.linearize(&head, ThreadFilter::USER);
        conversation
            .entries()
            .iter()
            .rev()
            .nth(2)
            .expect("an earlier entry")
            .id
            .clone()
    };

    // Stage queue state the switch has to clear. Enqueued through the
    // in-process handles rather than through a command, because an idle
    // session runs a prompt instead of queueing it and this session has to
    // stay idle for the switch to be allowed at all.
    let handles = harness
        .host
        .local_handles(&session)
        .await
        .expect("live session");
    handles.queues.append_follow_up(AgentId::Main, "leftover");
    let _ = drained(&mut stream);

    harness
        .host
        .command(
            &session,
            Command::Head {
                target: HeadTarget::Entry(head.clone()),
            },
        )
        .await
        .expect("head switch on an idle session");

    let frames = frames_until(&mut stream, "the reset frame", |frame| {
        matches!(frame, Frame::Reset { .. })
    })
    .await;
    assert!(
        frames
            .iter()
            .any(|frame| matches!(frame, Frame::Reset { session: s, .. } if *s == session)),
    );
    let after = frames_until(&mut stream, "a state frame under the new epoch", |frame| {
        matches!(frame, Frame::State { .. })
    })
    .await;
    assert_ne!(epoch_of(&after), epoch, "a head switch mints a fresh epoch",);
    assert_eq!(
        handles.queues.pending_counts(),
        (0, 0),
        "the switch cleared the session's queues",
    );
    assert_eq!(
        handles.log.lock().await.head().cloned(),
        Some(head),
        "the log head moved",
    );
    harness.host.shutdown().await;
}

/// A head switch is refused while a turn runs, and while a background task
/// is live (spec section 11's "head switch refused while busy").
#[tokio::test]
async fn a_head_switch_is_refused_while_work_is_live() {
    // A slow-streaming turn, so the switch lands mid-turn.
    let harness = Harness::with_provider(scripted(
        vec![finalized_text_message("a fairly long answer to stream")],
        1,
        Duration::from_millis(20),
    ));
    let session = harness.create().await;
    let mut stream = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "hi").await;

    let err = harness
        .host
        .command(
            &session,
            Command::Head {
                target: HeadTarget::Entry("whatever".to_string()),
            },
        )
        .await
        .expect_err("a mid-turn head switch is refused");
    assert!(matches!(err, HostError::Conflict { .. }), "got {err:?}");
    stream.pump_until_idle().await;

    // Now with a live background task instead of a turn.
    let harness = Harness::with_provider(scripted(
        vec![
            calling(
                "backgrounding it",
                "call-bash",
                "bash",
                serde_json::json!({"command": "sleep 30", "run_in_background": true,
                                   "description": "sleep"}),
            ),
            finalized_text_message("started it"),
        ],
        0,
        Duration::ZERO,
    ));
    let session = harness.create().await;
    let mut stream = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "background something").await;
    stream.pump_until_idle().await;
    let tasks = harness.host.tasks(&session).await.expect("task table");
    assert!(
        tasks
            .tasks
            .iter()
            .any(|task| task.status == aj_agent::tool::TaskStatus::Running),
        "a background task is live: {tasks:?}",
    );

    let err = harness
        .host
        .command(
            &session,
            Command::Head {
                target: HeadTarget::Entry("whatever".to_string()),
            },
        )
        .await
        .expect_err("a head switch with live background work is refused");
    assert!(matches!(err, HostError::Conflict { .. }), "got {err:?}");
    harness.host.shutdown().await;
}

/// A head switch to an entry that is not in the log is a 404, and one to a
/// known entry whose role cannot be a head is a malformed request. The
/// underlying log refuses both with one error, so the host tells them apart
/// (spec 6.1).
#[tokio::test]
async fn a_head_switch_to_an_unknown_entry_is_refused() {
    let harness = Harness::new(sub_agent_turn());
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;

    let err = harness
        .host
        .command(
            &session,
            Command::Head {
                target: HeadTarget::Entry("no-such-entry".to_string()),
            },
        )
        .await
        .expect_err("unknown entries are refused");
    assert!(matches!(err, HostError::UnknownEntry(_)), "got {err:?}");

    // A sub-agent entry exists but cannot be a head.
    harness.prompt(&session, "delegate it").await;
    client.pump_until_idle().await;
    let sub_entry = {
        let handles = harness
            .host
            .local_handles(&session)
            .await
            .expect("live session");
        let log = handles.log.lock().await;
        log.entries_in_order()
            .into_iter()
            .find(|entry| entry.agent_id == Some(1))
            .expect("the sub-agent run wrote entries")
            .id
            .clone()
    };
    let err = harness
        .host
        .command(
            &session,
            Command::Head {
                target: HeadTarget::Entry(sub_entry),
            },
        )
        .await
        .expect_err("a sub-agent entry cannot be a head");
    assert!(matches!(err, HostError::Invalid(_)), "got {err:?}");
    harness.host.shutdown().await;
}

/// A `before` target moves the head to the named entry's parent, which is
/// what makes branching from a transcript message replace that message
/// rather than continue after it (spec 6.6).
///
/// The resolution is the host's, so an unknown entry is a 404 and an entry
/// with no parent is refused rather than silently branching from nothing.
#[tokio::test]
async fn a_head_switch_before_an_entry_lands_on_its_parent() {
    let harness = Harness::new(vec![
        finalized_text_message("the first answer"),
        finalized_text_message("the second answer"),
    ]);
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "first").await;
    client.pump_until_idle().await;
    harness.prompt(&session, "second").await;
    client.pump_until_idle().await;

    // The second user message, and the entry the head has to land on.
    let (second_user, its_parent, root) = {
        let handles = harness
            .host
            .local_handles(&session)
            .await
            .expect("live session");
        let log = handles.log.lock().await;
        let entries = log.entries_in_order();
        let users: Vec<&aj_session::ConversationEntry> = entries
            .iter()
            .copied()
            .filter(|entry| {
                matches!(
                    &entry.entry,
                    aj_session::ConversationEntryKind::Message { message }
                        if matches!(
                            message.as_stored_wire(),
                            Some(aj_models::types::Message::User(_))
                        )
                )
            })
            .collect();
        let second = users.get(1).expect("two user messages");
        let root = entries.first().expect("a first entry").id.clone();
        (
            second.id.clone(),
            second.parent_id.clone().expect("a parent"),
            root,
        )
    };

    harness
        .host
        .command(
            &session,
            Command::Head {
                target: HeadTarget::Before(second_user.clone()),
            },
        )
        .await
        .expect("branching before a user message is accepted");
    let landed = harness
        .host
        .tree(&session)
        .await
        .expect("tree read")
        .head
        .expect("a head");
    assert_eq!(
        landed, its_parent,
        "the head is the message's parent, so the branch replaces it",
    );

    let err = harness
        .host
        .command(
            &session,
            Command::Head {
                target: HeadTarget::Before("no-such-entry".to_string()),
            },
        )
        .await
        .expect_err("an unknown entry is refused");
    assert!(matches!(err, HostError::UnknownEntry(_)), "got {err:?}");

    let err = harness
        .host
        .command(
            &session,
            Command::Head {
                target: HeadTarget::Before(root),
            },
        )
        .await
        .expect_err("there is nothing before the first entry");
    assert!(matches!(err, HostError::Invalid(_)), "got {err:?}");
    harness.host.shutdown().await;
}

/// Every head refusal quotes the entry the request named. A `before` target
/// moves the head somewhere else, so a refusal that quoted the entry the
/// switch actually rejected would name an id the client never sent, which it
/// cannot act on or even recognize.
#[tokio::test]
async fn a_head_refusal_names_the_entry_the_request_sent() {
    let harness = Harness::new(sub_agent_turn());
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "delegate it").await;
    client.pump_until_idle().await;

    // A sub-agent entry cannot be a head, and its parent is a different
    // entry, so the two ids are distinguishable in the message.
    let (sub_entry, its_parent) = {
        let handles = harness
            .host
            .local_handles(&session)
            .await
            .expect("live session");
        let log = handles.log.lock().await;
        let entries = log.entries_in_order();
        let sub: std::collections::HashSet<&String> = entries
            .iter()
            .filter(|entry| entry.agent_id == Some(1))
            .map(|entry| &entry.id)
            .collect();
        // A sub-agent entry whose parent is also one, so the resolved head
        // is itself off the user thread and the switch refuses. A spawn
        // root's parent is the assistant message that spawned it, which is
        // a legal head, so it would be accepted.
        let entry = entries
            .iter()
            .find(|entry| {
                entry.agent_id == Some(1)
                    && entry.parent_id.as_ref().is_some_and(|id| sub.contains(id))
            })
            .expect("the sub-agent run wrote more than its root");
        (
            entry.id.clone(),
            entry.parent_id.clone().expect("filtered on Some"),
        )
    };
    assert_ne!(sub_entry, its_parent);

    let err = harness
        .host
        .command(
            &session,
            Command::Head {
                target: HeadTarget::Before(sub_entry.clone()),
            },
        )
        .await
        .expect_err("a sub-agent entry's parent is not a legal head");
    let HostError::Invalid(message) = &err else {
        panic!("got {err:?}");
    };
    assert!(
        message.contains(&sub_entry),
        "the refusal names what was asked for: {message}",
    );
    assert!(
        !message.contains(&its_parent),
        "the refusal does not name an entry the client never sent: {message}",
    );
    harness.host.shutdown().await;
}

/// A head switch forgets what belonged to the branch it left: its
/// sub-agents stop being promptable and its background tasks leave the
/// table.
///
/// Without that, a prompt to a sub-agent of the abandoned branch is
/// accepted and grows that branch, published as durable frames under the
/// new epoch, which every attached client folds into the new branch's
/// transcript.
#[tokio::test]
async fn a_head_switch_forgets_the_abandoned_branch() {
    let harness = Harness::new(background_sub_turn());
    let session = harness.create().await;
    let mut stream = harness
        .host
        .attach(&[AttachRequest {
            session: session.clone(),
            cursor: None,
        }])
        .await
        .expect("attach");
    frames_until(&mut stream, "caught_up", |frame| {
        matches!(frame, Frame::CaughtUp { .. })
    })
    .await;
    harness.prompt(&session, "kick it off").await;
    settle(&harness, &session, &mut stream).await;

    let handles = harness
        .host
        .local_handles(&session)
        .await
        .expect("live session");
    assert_eq!(
        handles.registry.ids(),
        vec![1],
        "the background sub-agent is retained and promptable",
    );
    assert!(
        !harness
            .host
            .tasks(&session)
            .await
            .expect("tasks")
            .tasks
            .is_empty(),
        "and its run is in the task table",
    );
    let (head, entries_before) = {
        let log = handles.log.lock().await;
        let current = log.head().cloned().expect("a head");
        let conversation = log.linearize(&current, ThreadFilter::USER);
        let head = conversation
            .entries()
            .iter()
            .rev()
            .nth(2)
            .expect("an earlier entry")
            .id
            .clone();
        (head, log.entries_in_order().len())
    };

    harness
        .host
        .command(
            &session,
            Command::Head {
                target: HeadTarget::Entry(head),
            },
        )
        .await
        .expect("head switch on an idle session");

    assert!(
        handles.registry.ids().is_empty(),
        "the abandoned branch's sub-agents are no longer promptable",
    );
    assert!(
        harness
            .host
            .tasks(&session)
            .await
            .expect("tasks")
            .tasks
            .is_empty(),
        "and its finished tasks left the table",
    );
    let err = harness
        .host
        .command(
            &session,
            Command::Prompt {
                agent: AgentId::Sub(1),
                content: vec![UserContent::text("keep going")],
            },
        )
        .await
        .expect_err("a sub-agent of the abandoned branch cannot be prompted");
    assert!(matches!(err, HostError::Conflict { .. }), "got {err:?}");
    assert_eq!(
        handles.log.lock().await.entries_in_order().len(),
        entries_before,
        "and the refused prompt grew no thread",
    );
    harness.host.shutdown().await;
}

// ---------------------------------------------------------------------------
// 6. Session locks
// ---------------------------------------------------------------------------

/// A live session holds its advisory lock, so a second writer is refused
/// until the host tears the session down.
#[tokio::test]
async fn a_live_session_holds_its_lock_until_teardown() {
    let harness = Harness::new(vec![finalized_text_message("on the record")]);
    let session = harness.create().await;
    // Punctuate the log, so the session is discoverable on disk and a
    // second writer's refusal is the lock rather than a missing session.
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "hi").await;
    client.pump_until_idle().await;
    drop(client);

    assert!(
        SessionLock::try_acquire(&harness.persistence, &session, "a-rival-writer")
            .expect("try_acquire")
            .is_none(),
        "the host holds the session's lock while it is live",
    );

    // A second host over the same store cannot materialize it.
    let rival = harness.revive(Vec::new());
    let err = rival
        .host
        .attach(&[AttachRequest {
            session: session.clone(),
            cursor: None,
        }])
        .await
        .expect_err("a locked session cannot be materialized twice");
    let HostError::Locked { holder, .. } = &err else {
        panic!("got {err:?}")
    };
    let holder = holder.as_ref().expect("the lock file names its holder");
    assert_eq!(holder.pid, std::process::id());
    assert_eq!(
        holder.host_id,
        harness.host.hello().host_id,
        "the refusal names the host that holds it, not the one refused",
    );
    assert!(
        err.to_string()
            .contains(&format!("pid {}", std::process::id())),
        "and says so in the message a user reads: {err}",
    );

    harness.host.shutdown().await;
    let reacquired = SessionLock::try_acquire(&harness.persistence, &session, "a-rival-writer")
        .expect("try_acquire")
        .expect("the lock is free once the host tore the session down");
    drop(reacquired);
    rival.host.shutdown().await;
}

/// A materialization the lock refuses must not have touched the log. The
/// build is not read-only: a resume truncates a torn trailing line and the
/// repair walk appends synthesized tool results, so taking the lock after
/// it would let a refused host rewrite the file the real writer owns.
#[tokio::test]
async fn a_refused_materialization_leaves_the_log_untouched() {
    let harness = Harness::new(vec![finalized_text_message("on the record")]);
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "hi").await;
    client.pump_until_idle().await;
    drop(client);
    harness.host.shutdown().await;

    // A torn trailing line, which is what a resume truncates away.
    let path = harness
        .persistence
        .sessions_dir()
        .join(format!("{session}.jsonl"));
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("reopen the log");
    std::io::Write::write_all(&mut file, b"{\"id\":\"torn\"").expect("append a torn line");
    drop(file);
    let before = std::fs::read(&path).expect("read the log");

    // Another writer holds the session, so every attempt below is refused.
    let held = SessionLock::try_acquire(&harness.persistence, &session, "a-rival-writer")
        .expect("try_acquire")
        .expect("the lock is free once the host tore the session down");

    let rival = harness.revive(Vec::new());
    for what in ["command", "attach"] {
        let err = match what {
            "command" => rival
                .host
                .command(&session, prompt("hi"))
                .await
                .expect_err("a locked session cannot be commanded")
                .to_string(),
            _ => rival
                .host
                .attach(&[AttachRequest {
                    session: session.clone(),
                    cursor: None,
                }])
                .await
                .expect_err("a locked session cannot be attached")
                .to_string(),
        };
        assert!(err.contains("held by another writer"), "{what}: {err}");
        assert_eq!(
            std::fs::read(&path).expect("read the log"),
            before,
            "the refused {what} rewrote the log",
        );
    }

    drop(held);
    rival.host.shutdown().await;
}

/// Every request after shutdown is refused. The session map is drained and
/// the fan-out closed by then, so serving one would rebuild the session
/// behind a driver nobody will ever tell to stop, and re-take its advisory
/// lock. Reachable from a request in flight when SIGTERM lands.
#[tokio::test]
async fn requests_after_shutdown_are_refused() {
    let harness = Harness::new(vec![finalized_text_message("on the record")]);
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "hi").await;
    client.pump_until_idle().await;
    drop(client);

    harness.host.shutdown().await;

    let refusals: Vec<String> = vec![
        harness
            .host
            .command(&session, prompt("hi"))
            .await
            .expect_err("command")
            .to_string(),
        harness
            .host
            .attach(&[AttachRequest {
                session: session.clone(),
                cursor: None,
            }])
            .await
            .err()
            .expect("attach")
            .to_string(),
        harness.host.create().await.expect_err("create").to_string(),
        harness
            .host
            .sessions()
            .await
            .err()
            .expect("sessions")
            .to_string(),
        harness
            .host
            .tasks(&session)
            .await
            .err()
            .expect("tasks")
            .to_string(),
        harness
            .host
            .queue(&session)
            .await
            .err()
            .expect("queue")
            .to_string(),
        harness
            .host
            .tree(&session)
            .await
            .err()
            .expect("tree")
            .to_string(),
        harness
            .host
            .local_handles(&session)
            .await
            .err()
            .expect("local handles")
            .to_string(),
    ];
    for refusal in &refusals {
        assert!(refusal.contains("shut down"), "got {refusal}");
    }

    let free = SessionLock::try_acquire(&harness.persistence, &session, "a-rival-writer")
        .expect("try_acquire")
        .expect("no request re-took the session's lock");
    drop(free);
}

// ---------------------------------------------------------------------------
// 6b. Idle release
// ---------------------------------------------------------------------------

/// Short enough that a test can wait one out, long enough that the sweeper's
/// half-grace tick and a scripted turn fit inside it without racing.
const IDLE_GRACE: Duration = Duration::from_millis(200);

/// The summary the host's directory reports for `session`, if it names it.
///
/// Every row the host produces goes through here or through
/// [`directories`], and both check the invariant the wire type documents but
/// does not enforce: a position is present exactly when the row is live (spec
/// 6.8). Checking it on the way past is what makes it hold for every
/// directory any test in this file ever looks at, rather than for the handful
/// a dedicated test would build.
async fn summary(host: &SessionHost, session: &str) -> Option<aj_wire::SessionSummary> {
    let sessions = host.sessions().await.expect("sessions").sessions;
    assert_rows_well_formed(&sessions);
    sessions.into_iter().find(|entry| entry.id == session)
}

/// Panic unless every row carries a position exactly when it is live.
fn assert_rows_well_formed(sessions: &[aj_wire::SessionSummary]) {
    for row in sessions {
        assert_eq!(
            row.live,
            row.last_seq.is_some(),
            "a position is present iff the row is live (spec 6.8): {row:?}",
        );
    }
}

/// Wait until the host reports `session` as no longer live, and answer the
/// summary it reports for it.
async fn until_released(host: &SessionHost, session: &str) -> aj_wire::SessionSummary {
    bounded("the session to be released", async {
        loop {
            let entry = summary(host, session)
                .await
                .expect("a released session is still in the directory");
            if !entry.live {
                return entry;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
}

/// Assert `session` stays live for `windows` grace periods.
async fn stays_live(host: &SessionHost, session: &str, windows: u32) {
    for _ in 0..windows {
        tokio::time::sleep(IDLE_GRACE).await;
        assert!(
            summary(host, session).await.expect("listed").live,
            "the session must still be live",
        );
    }
}

/// An idle session nobody is attached to is released once the grace is up: its
/// driver is gone, its lock is free for another writer, and the directory
/// reports it cold with the stamp its own work left (spec section 5).
#[tokio::test]
async fn an_idle_unattached_session_is_released() {
    let harness =
        Harness::with_idle_grace(vec![finalized_text_message("on the record")], IDLE_GRACE);
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "hi").await;
    client.pump_until_idle().await;
    let live = summary(&harness.host, &session).await.expect("listed");
    assert!(
        live.live && live.last_seq.is_some_and(|seq| seq > 0),
        "a live row reports the position the host holds (spec 6.8)",
    );

    // Attached, so it stays however long it idles.
    stays_live(&harness.host, &session, 2).await;
    drop(client);

    let released = until_released(&harness.host, &session).await;
    assert_eq!(
        released.last_seq, None,
        "a cold row carries no position: producing one would read the log",
    );
    assert!(
        released.last_activity >= live.last_activity,
        "and the liveness flip does not walk the row's stamp backwards, \
         {} is older than {}",
        released.last_activity,
        live.last_activity,
    );
    assert!(!released.working && released.tasks == 0);
    let lock = SessionLock::try_acquire(&harness.persistence, &session, "a-rival-writer")
        .expect("try_acquire")
        .expect("the release freed the session's lock");
    drop(lock);
    harness.host.shutdown().await;
}

/// A live row's stamp moves on durable events and on nothing else (spec 6.8).
/// Streaming chunks are lossy and carry no log position, so a turn's stamp
/// steps once per durable entry rather than once per frame, which is also
/// what keeps the `list` publisher's suppression from being defeated by a
/// stamp that ticks on every event.
#[tokio::test]
async fn a_lossy_event_does_not_move_a_sessions_stamp() {
    let harness = Harness::with_provider(scripted(
        vec![finalized_text_message("a slowly streamed answer")],
        1,
        Duration::from_millis(20),
    ));
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    let idle = summary(&harness.host, &session)
        .await
        .expect("listed")
        .last_activity;

    harness.prompt(&session, "hi").await;
    // The prompt's own entry is durable, so by the first streamed chunk the
    // stamp has already stepped once. Everything from here to the message's
    // end is lossy.
    let chunk = |frame: &Frame| {
        matches!(frame, Frame::Event { event, .. }
            if matches!(event.known(), Some(AgentEvent::MessageUpdate { .. })))
    };
    frames_until(&mut client.stream, "the first streamed chunk", chunk).await;
    let streaming = summary(&harness.host, &session)
        .await
        .expect("listed")
        .last_activity;
    assert!(streaming > idle, "the prompt's own entry moved the stamp");

    for nth in 0..3 {
        frames_until(&mut client.stream, "another streamed chunk", chunk).await;
        assert_eq!(
            summary(&harness.host, &session)
                .await
                .expect("listed")
                .last_activity,
            streaming,
            "chunk {nth} carries no log position, so it moves no stamp",
        );
    }

    client.pump_until_idle().await;
    assert!(
        summary(&harness.host, &session)
            .await
            .expect("listed")
            .last_activity
            > streaming,
        "and the message's durable end does move it",
    );
    harness.host.shutdown().await;
}

/// Materializing a session is not activity. A resume reports the stamp its
/// log bears, so a session the user merely opens does not claim it just did
/// something, and the row survives a round trip through liveness unchanged.
#[tokio::test]
async fn a_resumed_session_reports_its_logs_stamp_in_both_directions() {
    let harness =
        Harness::with_idle_grace(vec![finalized_text_message("on the record")], IDLE_GRACE);
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "hi").await;
    client.pump_until_idle().await;
    drop(client);
    harness.host.shutdown().await;

    let modified: chrono::DateTime<chrono::Utc> = std::fs::metadata(
        harness
            .persistence
            .sessions_dir()
            .join(format!("{session}.jsonl")),
    )
    .expect("the log")
    .modified()
    .expect("a modification time")
    .into();

    let revived = harness.revive_with_idle_grace(Vec::new(), Some(IDLE_GRACE));
    let client = Client::attach(&revived.host, &session).await;
    let live = summary(&revived.host, &session).await.expect("listed");
    assert!(live.live);
    // Exact, not a bound: a fresh host holds no row for the session, so the
    // file is the only answer there is. It stays exact through the release
    // below because a clean resume buffers nothing, so the teardown flush
    // writes no bytes and the file does not move. A resume that starts
    // buffering (a restore context, a repair of a torn tail) legitimately
    // stamps the session at the repair, and this becomes a `>=`.
    assert_eq!(
        live.last_activity, modified,
        "a resume reports what the log says, not the moment it was opened",
    );

    drop(client);
    let released = until_released(&revived.host, &session).await;
    assert_eq!(
        released.last_activity, modified,
        "and going cold again leaves it exactly where it was",
    );
    revived.host.shutdown().await;
}

/// A session's activity stamp never walks backwards, in either direction over
/// the liveness flip (spec 6.8). The two sides read different clocks that
/// straddle the write: a live row reports when the driver saw the append, a
/// cold one what the file says, and the driver's is the later of the two by
/// however long the event took to reach it. A client stores the stamp it saw
/// at view time, so a row that goes back in time is a glyph that will not
/// fire for output the user has not seen.
#[tokio::test]
async fn a_sessions_stamp_survives_a_round_trip_through_liveness() {
    let harness = Harness::with_idle_grace(
        vec![
            finalized_text_message("on the record"),
            finalized_text_message("and again"),
        ],
        IDLE_GRACE,
    );
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "hi").await;
    client.pump_until_idle().await;
    let live = summary(&harness.host, &session)
        .await
        .expect("listed")
        .last_activity;
    drop(client);

    let released = until_released(&harness.host, &session).await;
    assert!(
        released.last_activity >= live,
        "going cold walked the row back: {} < {live}",
        released.last_activity,
    );

    // And back again, in the same host, so the row the release recorded is
    // still in the cold cache for the materialization to start from.
    let mut client = Client::attach(&harness.host, &session).await;
    let relived = summary(&harness.host, &session).await.expect("listed");
    assert!(relived.live);
    assert!(
        relived.last_seq.is_some_and(|seq| seq > 0),
        "and going live hands the row a position again: {relived:?}",
    );
    assert!(
        relived.last_activity >= released.last_activity,
        "coming back live walked the row back: {} < {}",
        relived.last_activity,
        released.last_activity,
    );

    // A second turn moves it forward, from the stamp it kept rather than from
    // whatever the file happened to say.
    harness.prompt(&session, "more").await;
    client.pump_until_idle().await;
    assert!(
        summary(&harness.host, &session)
            .await
            .expect("listed")
            .last_activity
            > relived.last_activity,
        "and durable work still moves it",
    );
    drop(client);
    harness.host.shutdown().await;
}

/// A release does not stamp the session with its own teardown. Buffered
/// entries (a settings record, say) reach the file only when the release
/// flushes them, so the log's modification time by then is the moment the
/// host tore the session down, a whole idle grace after the work that wrote
/// them. A row carrying that reads to a client as output it has not seen
/// (spec 6.8), on a session that produced nothing.
#[tokio::test]
async fn a_release_does_not_stamp_a_session_with_its_own_flush() {
    let harness = Harness::with_idle_grace(vec![finalized_text_message("recorded")], IDLE_GRACE);
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "hi").await;
    client.pump_until_idle().await;
    // Buffered, not punctuating, so only the teardown flush can land it.
    harness
        .host
        .command(
            &session,
            Command::Settings(SettingsChange {
                agent: AgentId::Main,
                persist: PersistAction::None,
                axis: SettingsAxis::Verbosity(Some(aj_conf::ConfigVerbosity::Low)),
            }),
        )
        .await
        .expect("a settings change");
    client.pump_until_idle().await;
    let live = summary(&harness.host, &session).await.expect("listed");
    drop(client);

    // Idle for several graces, so a stamp taken from the flush is unmistakably
    // later than the last thing the session did.
    tokio::time::sleep(IDLE_GRACE * 6).await;
    let released = until_released(&harness.host, &session).await;
    assert_eq!(
        released.last_activity, live.last_activity,
        "the release reported when the session last did something, not when \
         the host got around to writing it down",
    );

    // And the flush did land, which is what makes the stamp above a choice
    // rather than a coincidence.
    let flushed: chrono::DateTime<chrono::Utc> = std::fs::metadata(
        harness
            .persistence
            .sessions_dir()
            .join(format!("{session}.jsonl")),
    )
    .expect("the log")
    .modified()
    .expect("a modification time")
    .into();
    assert!(
        flushed > released.last_activity,
        "the teardown moved the file past the session's own stamp",
    );
    harness.host.shutdown().await;
}

/// A client that stays attached keeps its session live: attachment is the
/// retention signal (spec section 5), so a sidebar-era client that holds
/// background sessions holds their locks, deliberately.
#[tokio::test]
async fn an_attached_session_is_never_released() {
    let harness = Harness::with_idle_grace(vec![finalized_text_message("hi back")], IDLE_GRACE);
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "hi").await;
    client.pump_until_idle().await;

    stays_live(&harness.host, &session, 5).await;
    assert!(
        SessionLock::try_acquire(&harness.persistence, &session, "a-rival-writer")
            .expect("try_acquire")
            .is_none(),
        "and it still holds the session's lock",
    );
    drop(client);
    until_released(&harness.host, &session).await;
    harness.host.shutdown().await;
}

/// Work outlasting its client holds a session live, and so does a queued
/// message: the queues are memory only, so releasing a session with one
/// pending would discard it. Both let go once they are gone.
#[tokio::test]
async fn live_work_and_queued_messages_hold_a_session_live() {
    let harness = Harness::with_idle_grace(vec![finalized_text_message("noted")], IDLE_GRACE);
    let session = harness.create().await;
    // Punctuate the log first: a session with nothing on disk is never
    // released, so this test would pass for the wrong reason.
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "hi").await;
    client.pump_until_idle().await;
    let handles = harness.host.local_handles(&session).await.expect("handles");
    let (task, _cancel) = handles.task_registry.register(
        AgentId::Main,
        "call-1".into(),
        TaskKind::Bash {
            command: "sleep 100".into(),
        },
        "sleep 100".into(),
        Arc::new(FixedTaskOutput),
    );
    // Nobody is attached from here on: the task is the only thing holding it.
    drop(client);
    stays_live(&harness.host, &session, 2).await;

    // Queued before the task is finished, so no tick can find the session with
    // neither of the two holding it.
    handles.queues.append_follow_up(AgentId::Main, "later");
    handles
        .task_registry
        .set_status(task, TaskStatus::Exited(Some(0)));
    stays_live(&harness.host, &session, 2).await;

    harness
        .host
        .command(&session, Command::Queue(QueueOp::Clear))
        .await
        .expect("clearing the queue");
    let released = until_released(&harness.host, &session).await;
    assert_eq!(released.queued.follow_up, 0);
    harness.host.shutdown().await;
}

/// The grace runs from the last client detaching, not only from the session's
/// own last durable work. A session idle for hours with a client attached must
/// not go the instant that client lets go: the next attach would re-resume the
/// whole log for nothing, which is what the grace exists to prevent.
#[tokio::test]
async fn a_detach_starts_the_grace_over() {
    let harness = Harness::with_idle_grace(vec![finalized_text_message("recorded")], IDLE_GRACE);
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "hi").await;
    client.pump_until_idle().await;
    // Attached and idle for several graces, so the session's own work clock is
    // stale long before the client lets go.
    stays_live(&harness.host, &session, 3).await;
    drop(client);

    // Half a grace after the detach: the release may not have happened yet.
    tokio::time::sleep(IDLE_GRACE / 2).await;
    assert!(
        summary(&harness.host, &session).await.expect("listed").live,
        "the grace must run from the detach, not from the last durable work",
    );
    until_released(&harness.host, &session).await;
    harness.host.shutdown().await;
}

/// A release publishes the directory. The liveness flag flipping is the whole
/// wire surface of a release (spec section 5), and a client watching another
/// session has no other way to learn it: a released session emits no events.
#[tokio::test]
async fn a_release_publishes_the_directory() {
    let harness = Harness::with_idle_grace(
        vec![
            finalized_text_message("on the record"),
            finalized_text_message("watching"),
        ],
        IDLE_GRACE,
    );
    let going = harness.create().await;
    let mut client = Client::attach(&harness.host, &going).await;
    harness.prompt(&going, "hi").await;
    client.pump_until_idle().await;

    // A second session carries the stream that has to learn about the first. It
    // stays attached, so it is never released itself.
    let watching = harness.create().await;
    let mut stream = harness
        .host
        .attach(&[AttachRequest {
            session: watching.clone(),
            cursor: None,
        }])
        .await
        .expect("attach");
    frames_until(&mut stream, "caught_up", |frame| {
        matches!(frame, Frame::CaughtUp { .. })
    })
    .await;
    // Quiesce the directory, so the frame asserted on below can only have come
    // from the release.
    loop {
        tokio::time::sleep(LIST_SETTLE).await;
        if drained(&mut stream)
            .iter()
            .all(|frame| !matches!(frame, Frame::List { .. }))
        {
            break;
        }
    }
    // Nothing holds `going` from here on.
    drop(client);

    frames_until(
        &mut stream,
        "the directory to report the release",
        |frame| {
            matches!(frame, Frame::List { sessions }
            if sessions.iter().any(|entry| entry.id == going && !entry.live))
        },
    )
    .await;
    drop(stream);
    harness.host.shutdown().await;
}

/// The grace starts over from the session's own work, not from whenever the
/// sweeper last happened to look. A sweeper that only sampled its own
/// observations would release a session the instant a tick came due, however
/// recently the session had been working, which is the resume thrash the grace
/// exists to prevent.
#[tokio::test]
async fn work_between_two_ticks_restarts_the_grace() {
    let harness = Harness::with_idle_grace(
        vec![
            finalized_text_message("punctuation"),
            finalized_text_message("and a second answer"),
        ],
        IDLE_GRACE,
    );
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "punctuate").await;
    client.pump_until_idle().await;
    drop(client);

    // Long enough for the sweeper to have stamped this session as idle, short
    // enough that it is not due yet.
    tokio::time::sleep(IDLE_GRACE * 3 / 4).await;
    assert!(
        summary(&harness.host, &session).await.expect("listed").live,
        "the session is not due yet, so the work below lands between two ticks",
    );

    harness.prompt(&session, "one more thing").await;
    bounded("the second turn to finish", async {
        while summary(&harness.host, &session)
            .await
            .is_some_and(|entry| entry.working)
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;

    // Less than a grace after the work, so the session must still be here.
    tokio::time::sleep(IDLE_GRACE * 3 / 4).await;
    assert!(
        summary(&harness.host, &session).await.expect("listed").live,
        "released inside its own grace, measured from the last work",
    );
    until_released(&harness.host, &session).await;
    harness.host.shutdown().await;
}

/// An undelivered task-completion notice holds a session live. The notice queue
/// is memory only, like the message queues, so a release would discard a
/// completion the model never saw.
#[tokio::test]
async fn an_undelivered_task_notice_holds_a_session_live() {
    let harness = Harness::with_idle_grace(
        vec![
            finalized_text_message("punctuation"),
            finalized_text_message("noted, thanks"),
        ],
        IDLE_GRACE,
    );
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "punctuate").await;
    client.pump_until_idle().await;
    let handles = harness.host.local_handles(&session).await.expect("handles");
    let (task, _cancel) = handles.task_registry.register(
        AgentId::Main,
        "call-1".into(),
        TaskKind::Bash {
            command: "true".into(),
        },
        "true".into(),
        Arc::new(FixedTaskOutput),
    );
    // Finished, but its notice is still queued for an agent that has not run
    // since. Nothing is attached from here on.
    handles.task_registry.finish(
        task,
        TaskStatus::Exited(Some(0)),
        aj_agent::tool::TaskNotice {
            owner: AgentId::Main,
            task_id: task,
            kind: TaskKind::Bash {
                command: "true".into(),
            },
            label: "true".into(),
            status: TaskStatus::Exited(Some(0)),
            body: "exit 0".into(),
        },
    );
    drop(client);
    stays_live(&harness.host, &session, 2).await;
    assert_eq!(
        summary(&harness.host, &session)
            .await
            .expect("listed")
            .tasks,
        0,
        "the task itself is finished: the queued notice is what holds it",
    );

    // A turn drains the notice into the model, and the session can go.
    harness.prompt(&session, "anything for me?").await;
    until_released(&harness.host, &session).await;
    harness.host.shutdown().await;
}

/// A release the driver cannot produce a row for does not happen. The host has
/// no row to serve such a session with, so releasing it would drop it out of the
/// directory or leave a row that predates the materialization, and both are
/// worse than holding the lock for another grace.
#[tokio::test]
async fn a_session_the_host_cannot_row_is_not_released() {
    let harness = Harness::with_idle_grace(vec![finalized_text_message("recorded")], IDLE_GRACE);
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "punctuate").await;
    client.pump_until_idle().await;
    drop(client);

    // The one reachable way to make the row unreadable while the session is
    // otherwise perfectly releasable: the log's file is gone, so the driver
    // cannot say what state it left behind.
    std::fs::remove_file(
        harness
            .persistence
            .sessions_dir()
            .join(format!("{session}.jsonl")),
    )
    .expect("remove the log");

    stays_live(&harness.host, &session, 4).await;
    harness.host.shutdown().await;
}

/// A released session's activity stamp never goes backwards. The stamp is
/// what a client derives unseen output from (spec 6.8), so a release
/// publishing a row that predates the session's own work would silently erase
/// it.
#[tokio::test]
async fn a_release_never_lowers_the_stamp_it_publishes() {
    let harness = Harness::with_idle_grace(
        vec![
            finalized_text_message("first"),
            finalized_text_message("second"),
        ],
        IDLE_GRACE,
    );
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "punctuate").await;
    client.pump_until_idle().await;
    // Where the row stood before the work below.
    let before = summary(&harness.host, &session)
        .await
        .expect("listed")
        .last_activity;
    harness.prompt(&session, "more").await;
    client.pump_until_idle().await;
    let live = summary(&harness.host, &session)
        .await
        .expect("listed")
        .last_activity;
    assert!(live > before, "the second turn moved the session's stamp");
    drop(client);

    let released = until_released(&harness.host, &session).await;
    assert!(
        released.last_activity >= live,
        "the release published the stamp the session actually reached, \
         {} is older than {live}",
        released.last_activity,
    );
    harness.host.shutdown().await;
}

/// A client re-attaching after a release is served a fresh epoch and a full
/// backfill, and folds to the same state as one that never lost the session.
/// The epoch dies with the materialization (spec 6.5), so the cursor the
/// client still holds names a history this host no longer has.
#[tokio::test]
async fn attaching_after_a_release_rebuilds_the_same_state() {
    let harness =
        Harness::with_idle_grace(vec![finalized_text_message("recorded answer")], IDLE_GRACE);
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "hi").await;
    client.pump_until_idle().await;
    let before = client.canonical();
    let cursor = client.client.cursor().expect("a committed cursor");
    drop(client);

    until_released(&harness.host, &session).await;

    // Offering the dead epoch's cursor: the host must not serve it as a
    // suffix, since the session it named is gone.
    let mut rejoined = Client::attach(&harness.host, &session).await;
    let block = rejoined.reattach(&harness.host, cursor.clone()).await;
    let epoch = epoch_of(&block);
    assert_ne!(
        epoch, cursor.epoch,
        "a re-materialization mints a fresh epoch",
    );
    assert!(
        block
            .iter()
            .any(|frame| matches!(frame, Frame::Event { .. })),
        "and the block carries a full backfill rather than an empty suffix",
    );
    assert_canonical_eq(
        &rejoined.canonical(),
        &before,
        "a re-attach after a release rebuilds the state the release ended on",
    );
    drop(rejoined);
    harness.host.shutdown().await;
}

/// A command racing the release of its own session is neither lost nor applied
/// twice: it either reaches the driver first, and the driver then declines to
/// go, or it waits out the teardown and re-materializes (spec section 5).
///
/// The commands are spaced across the sweeper's tick, so both branches are
/// actually taken: the test asserts that at least one of them found the session
/// already cold, which is the re-materializing half. A grace of nothing means
/// the sweeper takes every session it finds idle.
#[tokio::test]
async fn a_command_racing_a_release_is_neither_lost_nor_doubled() {
    let harness = Harness::with_idle_grace(
        vec![
            finalized_text_message("on the record"),
            finalized_text_message("the one and only answer"),
        ],
        Duration::ZERO,
    );
    let session = harness.create().await;
    // Punctuate first: a session with no log on disk is never released, so the
    // race below would not happen at all.
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "hi").await;
    client.pump_until_idle().await;
    drop(client);

    let mut cold_when_issued = 0;
    for _ in 0..20 {
        // Long enough for the sweeper's tick to land in between, so the
        // commands straddle releases rather than all fitting inside one tick.
        tokio::time::sleep(Duration::from_millis(5)).await;
        let cold = summary(&harness.host, &session)
            .await
            .is_some_and(|entry| !entry.live);
        match harness
            .host
            .command(&session, Command::Queue(QueueOp::Clear))
            .await
        {
            Ok(_) => cold_when_issued += usize::from(cold),
            Err(err) => panic!("a command racing a release must not be lost: {err}"),
        }
    }
    assert!(
        cold_when_issued > 0,
        "no command found the session released, so the re-materializing half of \
         the race never ran",
    );

    // And a prompt issued into the same race runs exactly once: the script holds
    // one reply and panics if a second inference asks for one, and the turn is
    // what holds the session live while it runs.
    harness.prompt(&session, "hi again").await;
    until_released(&harness.host, &session).await;
    let client = Client::attach(&harness.host, &session).await;
    assert_eq!(
        assistant_rows(&client.chat, AgentId::Main)
            .iter()
            .filter(|text| *text == "the one and only answer")
            .count(),
        1,
        "the prompt ran once, not once per materialization",
    );
    drop(client);
    harness.host.shutdown().await;
}

/// A session with no log on disk is never released. Release hands a session
/// back to the store, and the store does not know a session it has no file
/// for, so releasing one would drop it: a client that created a session and
/// has not prompted or attached yet would find its id gone.
#[tokio::test]
async fn a_session_with_nothing_on_disk_is_never_released() {
    let harness = Harness::with_idle_grace(vec![finalized_text_message("first words")], IDLE_GRACE);
    let session = harness.create().await;
    // Nothing is attached and nothing is queued: only the empty log holds it.
    stays_live(&harness.host, &session, 4).await;
    assert!(
        !harness
            .persistence
            .sessions_dir()
            .join(format!("{session}.jsonl"))
            .exists(),
        "the log is still unwritten, which is the state under test",
    );

    // And it is still the session the creator was handed: the first prompt
    // lands on it rather than on an unknown id.
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "hi").await;
    let frames = client.pump_until_idle().await;
    assert_eq!(assistant_text(&frames), "first words");
    drop(client);
    // Punctuated now, so the grace applies from here on.
    until_released(&harness.host, &session).await;
    harness.host.shutdown().await;
}

/// A turn with nobody watching is not interrupted by the grace: a client that
/// submits a prompt and closes its stream still gets its work done, and the
/// release waits for the turn rather than cancelling it.
#[tokio::test]
async fn a_turn_nobody_is_attached_to_is_not_released_out_from_under() {
    let harness = Harness::with_run_config(
        snapshot(scripted(
            vec![
                finalized_text_message("punctuation"),
                finalized_text_message("a slowly streamed answer nobody is watching"),
            ],
            1,
            Duration::from_millis(10),
        )),
        Vec::new(),
        Some(Duration::from_millis(30)),
        None,
    );
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "punctuate").await;
    client.pump_until_idle().await;
    drop(client);

    // Unattached from here on. The turn streams for far longer than the grace.
    harness.prompt(&session, "and now the long one").await;
    let released = until_released(&harness.host, &session).await;

    // The whole answer is on disk: a release that cancelled the turn would have
    // left the transcript short.
    let mut rejoined = Client::attach(&harness.host, &session).await;
    assert!(
        assistant_rows(&rejoined.chat, AgentId::Main)
            .iter()
            .any(|text| text == "a slowly streamed answer nobody is watching"),
        "the turn ran to completion: {:?}",
        assistant_rows(&rejoined.chat, AgentId::Main),
    );
    assert!(
        !rejoined
            .drain_into_fold()
            .iter()
            .any(|frame| matches!(frame, Frame::Event { event, .. }
                if matches!(event.known(), Some(AgentEvent::Notice { text, .. })
                    if text.contains("cancelled")))),
        "and it was not cancelled on the way out",
    );
    assert_eq!(
        released.last_seq, None,
        "and its cold row carries no position"
    );
    drop(rejoined);
    harness.host.shutdown().await;
}

/// A sub-agent's continuation holds the session live too. Only `driven_subs`
/// records it: main is idle throughout, and a continuation is a turn the host
/// drives rather than a background task, so nothing else in the status names it.
#[tokio::test]
async fn a_sub_agents_continuation_holds_its_session_live() {
    let mut script = sub_agent_turn();
    // A continuation slow enough to outlast several graces.
    script.push(finalized_text_message(
        "the sub-agent taking its time over a continuation",
    ));
    let harness = Harness::with_run_config(
        snapshot(scripted(script, 1, Duration::from_millis(20))),
        Vec::new(),
        Some(Duration::from_millis(30)),
        None,
    );
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "delegate it").await;
    client.pump_until_idle().await;
    drop(client);

    harness
        .host
        .command(
            &session,
            Command::Prompt {
                agent: AgentId::Sub(1),
                content: vec![UserContent::text("keep going")],
            },
        )
        .await
        .expect("the retained sub-agent runs a continuation");
    // Nobody is attached and main is idle: the sub's turn is the only thing
    // holding the session, and the grace is up several times over before the
    // continuation ends.
    assert!(
        summary(&harness.host, &session).await.expect("listed").live,
        "the session is held while its sub-agent works",
    );
    until_released(&harness.host, &session).await;

    // The continuation ran to the end. A release that took the session while
    // its sub-agent was working would have cancelled the turn mid-answer.
    let rejoined = Client::attach(&harness.host, &session).await;
    assert!(
        assistant_rows(&rejoined.chat, AgentId::Sub(1))
            .iter()
            .any(|text| text == "the sub-agent taking its time over a continuation"),
        "the sub's continuation is in its transcript: {:?}",
        assistant_rows(&rejoined.chat, AgentId::Sub(1)),
    );
    drop(rejoined);
    harness.host.shutdown().await;
}

/// The lock a release frees is a lock another writer can take: a second host
/// over the same store materializes the released session and agrees on its
/// mark, which is what makes the teardown flush load-bearing rather than
/// decorative.
#[tokio::test]
async fn a_released_session_can_be_taken_over_by_another_host() {
    let harness = Harness::with_idle_grace(vec![finalized_text_message("recorded")], IDLE_GRACE);
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "hi").await;
    client.pump_until_idle().await;
    // A settings entry buffers without punctuating, so only the teardown flush
    // can put it on disk.
    harness
        .host
        .command(
            &session,
            Command::Settings(SettingsChange {
                agent: AgentId::Main,
                persist: PersistAction::None,
                axis: SettingsAxis::Verbosity(Some(aj_conf::ConfigVerbosity::Low)),
            }),
        )
        .await
        .expect("a settings change");
    client.pump_until_idle().await;
    // What the first host's log holds, settings entry included. It is only
    // buffered at this point, so the teardown flush is the one thing that can
    // put it where a second host would find it.
    let logged = {
        let handles = harness.host.local_handles(&session).await.expect("handles");
        handles.log.lock().await.last_seq()
    };
    drop(client);

    let released = until_released(&harness.host, &session).await;
    assert_eq!(released.last_seq, None, "and a cold one does not");

    let rival = harness.revive_with_idle_grace(Vec::new(), Some(IDLE_GRACE));
    let taken = Client::attach(&rival.host, &session).await;
    let resumed = summary(&rival.host, &session).await.expect("listed");
    assert!(
        resumed.live,
        "the second host materialized the session the first one let go",
    );
    assert_eq!(
        resumed.last_seq,
        Some(logged),
        "and counted every entry the first host flushed on its way out, \
         including the settings record that only the teardown could land",
    );
    assert!(
        assistant_rows(&taken.chat, AgentId::Main)
            .iter()
            .any(|text| text == "recorded"),
        "and its backfill carries the first host's turn",
    );
    drop(taken);
    rival.host.shutdown().await;
    harness.host.shutdown().await;
}

/// A release never drops a session out of the directory, not even for the one
/// refresh that races it. The refresh reads the live set, scans the store with
/// the live logs excluded, then reads the live set again, so a session released
/// in between is in neither half and has to be recovered from the cold cache.
///
/// Opportunistic: it asserts the invariant on every refresh it manages to run
/// across the release, and a run that never lands inside the window still
/// passes. A store with many logs widens the scan, which is what makes landing
/// there likely.
#[tokio::test(flavor = "multi_thread")]
async fn a_release_never_drops_a_session_out_of_the_directory() {
    let harness = Harness::with_idle_grace(vec![finalized_text_message("recorded")], IDLE_GRACE);
    let sessions_dir = harness.persistence.sessions_dir().to_path_buf();
    std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
    let entry = serde_json::json!({
        "id": "00000000",
        "timestamp": "2024-01-01T00:00:00Z",
        "thread": "meta",
        "type": "system_prompt",
        "text": "x",
    });
    for i in 0..200 {
        std::fs::write(
            sessions_dir.join(format!("2020-01-01-00-00-00-{i:03}.jsonl")),
            format!("{entry}\n"),
        )
        .expect("write a cold log");
    }

    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "hi").await;
    client.pump_until_idle().await;
    drop(client);

    bounded("the release to happen", async {
        loop {
            let entry = harness
                .host
                .sessions()
                .await
                .expect("sessions")
                .sessions
                .into_iter()
                .find(|entry| entry.id == session);
            match entry {
                Some(entry) if !entry.live => return,
                Some(_) => {}
                None => panic!("the session vanished from the directory across its release"),
            }
        }
    })
    .await;
    harness.host.shutdown().await;
}

/// A release hands the session's row to the directory from the driver's own
/// state, so a refresh reports a session this host closed without reading the
/// log back (spec 6.8's no-disk-read rule for what the host already knows).
#[tokio::test]
async fn a_released_sessions_row_needs_no_disk_read() {
    let harness = Harness::with_idle_grace(vec![finalized_text_message("recorded")], IDLE_GRACE);
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "hi").await;
    client.pump_until_idle().await;
    // A settings record, which is buffered rather than punctuating, so the
    // release has something to flush and the fingerprint it records is the
    // flushed file's. One recorded before the flush would not match what the
    // next enumeration stats, and the log would be read again to settle it.
    harness
        .host
        .command(
            &session,
            Command::Settings(SettingsChange {
                agent: AgentId::Main,
                persist: PersistAction::None,
                axis: SettingsAxis::Thinking(Some(aj_models::ThinkingConfig::High)),
            }),
        )
        .await
        .expect("thinking change");
    let live = summary(&harness.host, &session)
        .await
        .expect("listed")
        .last_activity;

    // A second session carries the stream the release is watched on. A listing
    // would do, but it is an enumeration point, and one between the release and
    // the unreadable log below would derive afresh what this test is asserting
    // came from the release.
    let watching = harness.create().await;
    let mut stream = harness
        .host
        .attach(&[AttachRequest {
            session: watching.clone(),
            cursor: None,
        }])
        .await
        .expect("attach");
    frames_until(&mut stream, "caught_up", |frame| {
        matches!(frame, Frame::CaughtUp { .. })
    })
    .await;
    drop(client);
    let frames = frames_until(&mut stream, "the release to be published", |frame| {
        matches!(frame, Frame::List { sessions }
            if sessions.iter().any(|entry| entry.id == session && !entry.live))
    })
    .await;
    let released = directories(&frames)
        .pop()
        .expect("the frame that reported the release")
        .into_iter()
        .find(|entry| entry.id == session)
        .expect("the released session's row");
    assert_eq!(released.last_seq, None, "a cold row carries no position");
    assert!(
        released.last_activity >= live,
        "and the stamp it does carry did not go backwards over the release, \
         {} is older than {live}",
        released.last_activity,
    );

    // The log a released session left is unreadable from here on. The row
    // still reports, which it could not if the refresh went back to the file:
    // a log the store cannot open is left out of the directory entirely.
    let path = harness
        .persistence
        .sessions_dir()
        .join(format!("{session}.jsonl"));
    let mode = std::fs::metadata(&path).expect("the log").permissions();
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o000))
        .expect("drop the read bit");
    if std::fs::File::open(&path).is_ok() {
        // Root ignores the permission bits, so there is nothing to prove here.
        std::fs::set_permissions(&path, mode).expect("restore the mode");
        harness.host.shutdown().await;
        return;
    }
    let still = summary(&harness.host, &session)
        .await
        .expect("the session is still listed");
    assert_eq!(
        still.last_activity, released.last_activity,
        "the row came from what the release recorded, not from the log",
    );
    std::fs::set_permissions(&path, mode).expect("restore the mode");
    drop(stream);
    harness.host.shutdown().await;
}

// ---------------------------------------------------------------------------
// 7. Queue mutations emit QueueUpdate
// ---------------------------------------------------------------------------

/// Every queue mutation publishes a `QueueUpdate`, on the enqueue side as
/// well as the drain side, and a second attached subscriber sees them
/// (spec section 11's "queue enqueue visibility on a second client").
#[tokio::test]
async fn queue_mutations_reach_every_subscriber() {
    let harness = Harness::with_provider(scripted(
        vec![finalized_text_message("a slowly streamed answer")],
        1,
        Duration::from_millis(20),
    ));
    let session = harness.create().await;
    let mut one = Client::attach(&harness.host, &session).await;
    let mut two = Client::attach(&harness.host, &session).await;

    harness.prompt(&session, "hi").await;
    // Busy now, so these queue rather than run.
    harness.prompt(&session, "a follow-up").await;
    harness
        .host
        .command(
            &session,
            Command::Steer {
                agent: AgentId::Main,
                text: "steer me".to_string(),
            },
        )
        .await
        .expect("steer while busy queues");
    harness
        .host
        .command(
            &session,
            Command::Steer {
                agent: AgentId::Main,
                text: String::new(),
            },
        )
        .await
        .expect("an empty steer promotes");
    let withdrawn = harness
        .host
        .command(
            &session,
            Command::Queue(QueueOp::Remove {
                agent: AgentId::Main,
            }),
        )
        .await
        .expect("remove");
    assert!(
        matches!(&withdrawn, CommandOutcome::Withdrawn(Some(text)) if text.contains("steer me")),
        "the withdrawn text comes back so a client can restore it: {withdrawn:?}",
    );
    harness.prompt(&session, "and another").await;
    harness
        .host
        .command(&session, Command::Queue(QueueOp::Clear))
        .await
        .expect("clear");

    // The pending counts each subscriber was told about, in order. Ending
    // empty is no evidence on its own: a client the fan-out handed nothing
    // ends empty too, which is exactly what a broken fan-out looks like.
    let mut seen = Vec::new();
    for (label, client) in [("first", &mut one), ("second", &mut two)] {
        let counts: Vec<(usize, usize)> = client
            .pump_until_idle()
            .await
            .iter()
            .filter_map(queue_counts)
            .collect();
        assert!(
            counts.iter().any(|&(steering, _)| steering > 0),
            "the {label} client never saw the steering message queue up: {counts:?}",
        );
        assert!(
            counts.iter().any(|&(_, follow_up)| follow_up > 0),
            "the {label} client never saw a follow-up queue up: {counts:?}",
        );
        assert!(
            client
                .chat
                .queue()
                .queues
                .iter()
                .all(|queue| queue.steering.is_empty() && queue.follow_up.is_empty()),
            "the {label} client's queue view ends empty: {:?}",
            client.chat.queue(),
        );
        seen.push(counts);
    }
    assert_eq!(
        seen[0], seen[1],
        "both subscribers were told the same sequence of queue states",
    );
    harness.host.shutdown().await;
}

/// A steer with text queues as **steering**, an empty steer promotes the
/// pending follow-up into the steering slot, and both are visible on the
/// stream (spec 6.6).
///
/// The two slots decide when the agent delivers a message: steering is
/// injected mid-turn, a follow-up waits for the turn to end. A client reads
/// them off the `QueueUpdate` payloads, so this asserts on the fold of those
/// frames rather than on the queues behind the host.
#[tokio::test]
async fn a_steer_queues_as_steering_and_an_empty_steer_promotes() {
    let harness = Harness::with_provider(scripted(
        vec![finalized_text_message(
            "an answer streamed slowly enough to queue behind",
        )],
        1,
        Duration::from_millis(30),
    ));
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "hi").await;

    // Busy now, so a prompt queues as a follow-up.
    harness.prompt(&session, "afterwards").await;
    client.drain_into_fold();
    assert_eq!(
        queued(&client, AgentId::Main),
        (Vec::new(), vec!["afterwards".to_string()]),
        "a prompt to a busy agent waits for the turn to end",
    );

    harness
        .host
        .command(
            &session,
            Command::Steer {
                agent: AgentId::Main,
                text: "steer me".to_string(),
            },
        )
        .await
        .expect("steer while busy queues");
    client.drain_into_fold();
    assert_eq!(
        queued(&client, AgentId::Main),
        (vec!["afterwards\nsteer me".to_string()], Vec::new()),
        "a steer with text moves the pending message into the steering slot",
    );

    // Back to a follow-up, so the empty steer below has something to
    // promote.
    harness
        .host
        .command(
            &session,
            Command::Queue(QueueOp::Remove {
                agent: AgentId::Main,
            }),
        )
        .await
        .expect("withdraw");
    harness.prompt(&session, "later then").await;
    client.drain_into_fold();
    assert_eq!(
        queued(&client, AgentId::Main),
        (Vec::new(), vec!["later then".to_string()]),
    );

    harness
        .host
        .command(
            &session,
            Command::Steer {
                agent: AgentId::Main,
                text: String::new(),
            },
        )
        .await
        .expect("an empty steer promotes");
    client.drain_into_fold();
    assert_eq!(
        queued(&client, AgentId::Main),
        (vec!["later then".to_string()], Vec::new()),
        "an empty steer promotes the pending follow-up rather than dropping it",
    );

    harness
        .host
        .command(&session, Command::Queue(QueueOp::Clear))
        .await
        .expect("clear");
    client.pump_until_idle().await;
    harness.host.shutdown().await;
}

/// The `(steering, follow_up)` texts `agent`'s queue holds, as the client
/// folded them out of the `QueueUpdate` frames.
fn queued(client: &Client, agent: AgentId) -> (Vec<String>, Vec<String>) {
    let texts = |messages: &[aj_agent::message::AgentMessage]| {
        messages
            .iter()
            .map(|message| match message.to_projected_wire() {
                Some(aj_models::types::Message::User(user)) => user
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        UserContent::Text(text) => Some(text.text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(""),
                other => panic!("a queued message is a user message, got {other:?}"),
            })
            .collect()
    };
    client
        .chat
        .queue()
        .queues
        .iter()
        .find(|queue| queue.agent_id == agent)
        .map(|queue| (texts(&queue.steering), texts(&queue.follow_up)))
        .unwrap_or_default()
}

/// The enqueue side publishes a `QueueUpdate` at every step, which is what
/// a second client's queue view is built from.
#[tokio::test]
async fn every_queue_mutation_publishes_an_update() {
    let harness = Harness::with_provider(scripted(
        vec![finalized_text_message("a slowly streamed answer")],
        1,
        Duration::from_millis(20),
    ));
    let session = harness.create().await;
    let mut stream = harness
        .host
        .attach(&[AttachRequest {
            session: session.clone(),
            cursor: None,
        }])
        .await
        .expect("attach");
    frames_until(&mut stream, "caught_up", |frame| {
        matches!(frame, Frame::CaughtUp { .. })
    })
    .await;

    harness.prompt(&session, "hi").await;
    let mut updates = 0;
    for command in [
        prompt("a follow-up"),
        Command::Steer {
            agent: AgentId::Main,
            text: "steer me".to_string(),
        },
        Command::Steer {
            agent: AgentId::Main,
            text: String::new(),
        },
        Command::Queue(QueueOp::Remove {
            agent: AgentId::Main,
        }),
        prompt("and another"),
        Command::Queue(QueueOp::Clear),
    ] {
        harness
            .host
            .command(&session, command)
            .await
            .expect("queue mutation");
        // The mutation's own update is queued before the command returns.
        updates += events(&drained(&mut stream))
            .into_iter()
            .filter(|event| matches!(event, AgentEvent::QueueUpdate { .. }))
            .count();
    }
    assert_eq!(updates, 6, "one update per mutation");
    harness.host.shutdown().await;
}

/// `clear` is session-wide (spec 6.6), and every agent it emptied gets its
/// own `QueueUpdate`: a client tracks the queues per agent and would keep
/// showing the ones it was not told about.
///
/// Both agents are made busy first, through commands, because that is the
/// only way the command surface can reach a queued message at all: an idle
/// agent runs a prompt instead of queueing it. The queue read (spec 6.7) is
/// asserted here for the same reason, against state the host itself built.
#[tokio::test]
async fn clearing_the_queue_empties_every_agent() {
    let mut script = sub_agent_turn();
    // Two slow turns for the two agents to be busy in, long enough that the
    // prompts below queue behind them rather than starting turns of their own.
    script.push(finalized_text_message(
        "the sub-agent taking its time over a continuation",
    ));
    script.push(finalized_text_message(
        "and the main agent taking its time too",
    ));
    let harness = Harness::with_provider(scripted(script, 1, Duration::from_millis(20)));
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "delegate it").await;
    client.pump_until_idle().await;

    // The sub through a continuation, main through a fresh turn. Both
    // commands return once their turn is spawned, so both agents read busy
    // from here.
    harness
        .host
        .command(
            &session,
            Command::Prompt {
                agent: AgentId::Sub(1),
                content: vec![UserContent::text("keep going")],
            },
        )
        .await
        .expect("the retained sub-agent runs a continuation");
    harness.prompt(&session, "and answer me too").await;

    harness
        .host
        .command(
            &session,
            Command::Prompt {
                agent: AgentId::Sub(1),
                content: vec![UserContent::text("for the sub")],
            },
        )
        .await
        .expect("queues behind the sub's turn");
    harness
        .host
        .command(
            &session,
            Command::Steer {
                agent: AgentId::Main,
                text: "for main".to_string(),
            },
        )
        .await
        .expect("queues as steering behind main's turn");
    client.drain_into_fold();
    assert_eq!(
        queued(&client, AgentId::Sub(1)),
        (Vec::new(), vec!["for the sub".to_string()]),
        "both agents were busy, so both prompts queued",
    );
    assert_eq!(
        queued(&client, AgentId::Main),
        (vec!["for main".to_string()], Vec::new()),
    );

    // The queue read answers the same state, one entry per agent that has
    // something queued, main first.
    let queue = harness.host.queue(&session).await.expect("queue read");
    assert_eq!(
        queue
            .queues
            .iter()
            .map(|entry| entry.agent_id)
            .collect::<Vec<_>>(),
        vec![AgentId::Main, AgentId::Sub(1)],
    );
    assert_eq!(queue.queues[0].steering.len(), 1);
    assert!(queue.queues[0].follow_up.is_empty());
    assert_eq!(queue.queues[1].follow_up.len(), 1);

    harness
        .host
        .command(&session, Command::Queue(QueueOp::Clear))
        .await
        .expect("clear");

    let mut updated: Vec<AgentId> = events(&client.drain_into_fold())
        .into_iter()
        .filter_map(|event| match event {
            AgentEvent::QueueUpdate { agent_id, .. } => Some(*agent_id),
            _ => None,
        })
        .collect();
    // Each update is a full snapshot for its own agent, so the order they
    // are published in carries nothing.
    updated.sort_by_key(|agent| match agent {
        AgentId::Main => (0, 0),
        AgentId::Sub(n) => (1, *n),
    });
    assert_eq!(
        updated,
        vec![AgentId::Main, AgentId::Sub(1)],
        "one update per agent that had something queued",
    );
    assert!(
        harness
            .host
            .queue(&session)
            .await
            .expect("queue read")
            .queues
            .is_empty(),
        "and the session holds nothing pending afterwards",
    );
    for agent in [AgentId::Main, AgentId::Sub(1)] {
        assert_eq!(
            queued(&client, agent),
            (Vec::new(), Vec::new()),
            "the client's own view of {agent:?} is empty too",
        );
    }

    client.pump_until_idle().await;
    harness.host.shutdown().await;
}

// ---------------------------------------------------------------------------
// 8. Synthesized settings frames
// ---------------------------------------------------------------------------

/// A settings change publishes the projected notice tagged with the entry
/// it appended plus a refreshed `state`, and a client attaching afterwards
/// regenerates the same notice from the backfill (spec section 11's
/// "settings visibility for a mid-session joiner").
#[tokio::test]
async fn a_settings_change_publishes_the_projected_notice() {
    let harness = Harness::new(vec![finalized_text_message("hello back")]);
    let session = harness.create().await;
    let mut live = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "hi").await;
    live.pump_until_idle().await;

    harness
        .host
        .command(
            &session,
            Command::Settings(SettingsChange {
                agent: AgentId::Main,
                persist: PersistAction::None,
                axis: SettingsAxis::Thinking(Some(aj_models::ThinkingConfig::High)),
            }),
        )
        .await
        .expect("thinking change");
    let frames = frames_until(&mut live.stream, "the settings state frame", |frame| {
        matches!(frame, Frame::State { .. })
    })
    .await;

    let notice = frames
        .iter()
        .find_map(|frame| match frame {
            Frame::Event {
                durability: Some(durability),
                event,
                ..
            } => match event.known() {
                Some(AgentEvent::Notice { text, .. }) => {
                    Some((durability.seq, durability.entry_id.clone(), text.clone()))
                }
                _ => None,
            },
            _ => None,
        })
        .expect("a durable notice frame");
    assert!(
        notice.2.contains("high"),
        "the projected wording: {notice:?}"
    );
    {
        let handles = harness
            .host
            .local_handles(&session)
            .await
            .expect("live session");
        let log = handles.log.lock().await;
        let entries = log.entries_in_order();
        let index = usize::try_from(notice.0).expect("seq fits usize") - 1;
        assert_eq!(
            entries.get(index).map(|entry| entry.id.as_str()),
            Some(notice.1.as_str()),
            "the notice is tagged with the settings entry's append position",
        );
    }
    assert!(
        frames.iter().any(
            |frame| matches!(frame, Frame::State { settings, .. } if settings.thinking == "high")
        ),
        "the refreshed state frame carries the new settings",
    );

    for frame in frames {
        let _ = live.client.apply(&mut live.chat, frame);
    }
    let joiner = Client::attach(&harness.host, &session).await;
    assert_canonical_eq(
        &joiner.canonical(),
        &live.canonical(),
        "a mid-session joiner regenerates the settings notice from the backfill",
    );
    harness.host.shutdown().await;
}

#[tokio::test]
async fn thinking_display_is_live_only_and_survives_bundle_rebuilds() {
    let harness = Harness::new(Vec::new());
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    let handles = harness.host.local_handles(&session).await.expect("handles");
    let before = handles.log.lock().await.last_seq();

    harness
        .host
        .command(
            &session,
            Command::Settings(SettingsChange {
                agent: AgentId::Main,
                persist: PersistAction::None,
                axis: SettingsAxis::ThinkingDisplay(Some(ConfigThinkingDisplay::Detailed)),
            }),
        )
        .await
        .expect("thinking display change");
    let frames = frames_until(&mut client.stream, "the display state frame", |frame| {
        matches!(frame, Frame::State { settings, .. } if settings.thinking_display == "detailed")
    })
    .await;
    assert!(frames.iter().any(|frame| matches!(
        frame,
        Frame::Event {
            durability: None,
            event,
            ..
        } if matches!(event.known(), Some(AgentEvent::Notice { text, .. }) if text.contains("detailed"))
    )));
    assert_eq!(
        handles.log.lock().await.last_seq(),
        before,
        "thinking display is not written to the log",
    );

    let mut real = scripted_model_info();
    real.provider = "openai".into();
    real.api = "openai-responses".into();
    real.id = "gpt-test".into();
    real.base_url = "https://api.example.test/v1".into();
    harness
        .host
        .command(
            &session,
            Command::Settings(SettingsChange {
                agent: AgentId::Main,
                persist: PersistAction::None,
                axis: SettingsAxis::Model(real),
            }),
        )
        .await
        .expect("model change");
    harness
        .host
        .command(
            &session,
            Command::Settings(SettingsChange {
                agent: AgentId::Main,
                persist: PersistAction::None,
                axis: SettingsAxis::Speed(Some(aj_models::types::Speed::Fast)),
            }),
        )
        .await
        .expect("speed change");

    let (display, reasoning_summary) = {
        let cfg = handles
            .run_config
            .lock()
            .expect("run config mutex poisoned");
        (
            cfg.thinking_display,
            cfg.stream_options.reasoning_summary.clone(),
        )
    };
    assert_eq!(display, Some(ConfigThinkingDisplay::Detailed));
    assert_eq!(
        reasoning_summary,
        Some(aj_models::types::ReasoningSummary::Detailed),
        "model and speed rebuilds preserve the session-tracked display",
    );
    harness.host.shutdown().await;
}

/// A settings change before the thread's first message projects no notice,
/// so the host publishes the confirmation untagged: live clients still see
/// the gesture, and no backfill regenerates a row for it (spec section 5).
#[tokio::test]
async fn a_settings_change_before_the_first_prompt_publishes_an_untagged_notice() {
    let harness = Harness::new(vec![finalized_text_message("hello back")]);
    let session = harness.create().await;
    let mut live = Client::attach(&harness.host, &session).await;

    harness
        .host
        .command(
            &session,
            Command::Settings(SettingsChange {
                agent: AgentId::Main,
                persist: PersistAction::None,
                axis: SettingsAxis::Thinking(Some(aj_models::ThinkingConfig::Low)),
            }),
        )
        .await
        .expect("thinking change");
    let frames = frames_until(&mut live.stream, "the settings state frame", |frame| {
        matches!(frame, Frame::State { .. })
    })
    .await;

    let notices: Vec<&AgentEvent> = events(&frames)
        .into_iter()
        .filter(|event| matches!(event, AgentEvent::Notice { .. }))
        .collect();
    assert_eq!(
        notices.len(),
        1,
        "the confirmation reaches the live client: {frames:?}",
    );
    assert!(
        matches!(notices[0], AgentEvent::Notice { text, .. } if text.contains("low")),
        "it carries the confirmation wording: {:?}",
        notices[0],
    );
    assert!(
        durable(&frames).is_empty(),
        "a seed settings entry projects nothing, so its notice is untagged: {frames:?}",
    );
    assert!(
        frames.iter().any(
            |frame| matches!(frame, Frame::State { settings, .. } if settings.thinking == "low")
        ),
        "the state frame still refreshes",
    );

    // The property the untagged frame buys: a joiner's backfill regenerates
    // no row for it, so an untagged live notice is the only way the gesture
    // is visible at all.
    for frame in frames {
        let _ = live.client.apply(&mut live.chat, frame);
    }
    let joiner = Client::attach(&harness.host, &session).await;
    assert!(
        joiner
            .canonical()
            .agent(AgentId::Main)
            .expect("main")
            .entries
            .is_empty(),
        "the backfill regenerates no notice for a seed settings entry",
    );
    assert_eq!(
        joiner
            .chat
            .footers()
            .settings(AgentId::Main)
            .map(|settings| settings.thinking.as_str()),
        Some("low"),
        "the opening state still carries the authoritative setting",
    );
    harness.host.shutdown().await;
}

/// A settings change the host cannot serve is an error, not an acceptance.
/// It stages nothing and publishes nothing, so a client told "accepted"
/// would render settings this host never adopted.
#[tokio::test]
async fn a_settings_change_that_did_not_apply_is_refused() {
    let harness = Harness::new(Vec::new());
    let session = harness.create().await;
    let mut stream = harness
        .host
        .attach(&[AttachRequest {
            session: session.clone(),
            cursor: None,
        }])
        .await
        .expect("attach");
    frames_until(&mut stream, "caught_up", |frame| {
        matches!(frame, Frame::CaughtUp { .. })
    })
    .await;

    // A model whose api nothing in the auth store can build a provider for.
    let mut unbuildable = scripted_model_info();
    unbuildable.provider = "no-such-api".to_string();
    let err = harness
        .host
        .command(
            &session,
            Command::Settings(SettingsChange {
                agent: AgentId::Main,
                persist: PersistAction::None,
                axis: SettingsAxis::Model(unbuildable),
            }),
        )
        .await
        .expect_err("a model the host cannot build is refused");
    assert!(matches!(err, HostError::Unsupported(_)), "got {err:?}");

    // A sub-agent that does not exist has no live handle to stage into.
    let err = harness
        .host
        .command(
            &session,
            Command::Settings(SettingsChange {
                agent: AgentId::Sub(7),
                persist: PersistAction::None,
                axis: SettingsAxis::Thinking(Some(aj_models::ThinkingConfig::High)),
            }),
        )
        .await
        .expect_err("a settings change for an agent that is not live is refused");
    assert!(matches!(err, HostError::Unsupported(_)), "got {err:?}");

    let handles = harness
        .host
        .local_handles(&session)
        .await
        .expect("live session");
    {
        let cfg = handles
            .run_config
            .lock()
            .expect("run config mutex poisoned");
        assert_eq!(
            cfg.model_key,
            ("scripted".to_string(), "scripted".to_string()),
            "the refused change staged nothing",
        );
    }
    assert!(
        drained(&mut stream)
            .iter()
            .all(|frame| !matches!(frame, Frame::Event { .. })),
        "and published no event frames",
    );
    harness.host.shutdown().await;
}

// ---------------------------------------------------------------------------
// 9. Command semantics
// ---------------------------------------------------------------------------

/// A prompt runs a turn when the agent is idle and queues when it is busy,
/// exactly like the local submit gesture.
#[tokio::test]
async fn a_prompt_runs_when_idle_and_queues_when_busy() {
    let harness = Harness::with_provider(scripted(
        vec![
            finalized_text_message("a slowly streamed answer"),
            finalized_text_message("and the follow-up answer"),
        ],
        1,
        Duration::from_millis(20),
    ));
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;

    harness.prompt(&session, "first").await;
    harness.prompt(&session, "queued").await;
    let handles = harness
        .host
        .local_handles(&session)
        .await
        .expect("live session");
    assert_eq!(
        handles.queues.pending_counts(),
        (0, 1),
        "the second prompt queued as a follow-up",
    );

    // The post-turn wake starts the follow-up turn before `working` ever
    // drops, so one settle covers both turns.
    client.pump_until_idle().await;
    assert_eq!(
        handles.queues.pending_counts(),
        (0, 0),
        "the wake drained the queue",
    );
    let state = format!("{:?}", client.canonical());
    assert!(
        state.contains("a slowly streamed answer") && state.contains("the follow-up answer"),
        "both turns ran: {state}",
    );
    harness.host.shutdown().await;
}

/// Cancelling a running turn stops the work it had left: the tool call the
/// streaming message carries is never executed and the inference that would
/// have answered it never runs, and the host publishes the notice that says
/// so.
///
/// The script is the oracle for "stopped". Its second message is only
/// reachable through the tool call in the first, so a cancel that did
/// nothing would consume it, and `ExhaustedBehavior::Panic` means the script
/// is exactly as long as an uncancelled turn needs.
#[tokio::test]
async fn a_cancel_stops_the_turn_and_publishes_its_notice() {
    let harness = Harness::with_provider(scripted(
        vec![
            calling(
                "let me check the list first",
                "call-1",
                "todo_read",
                serde_json::json!({}),
            ),
            finalized_text_message("the answer after the tool"),
        ],
        1,
        Duration::from_millis(30),
    ));
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "check the todos").await;
    // Cancel while the first message is still streaming, so its tool call
    // has not been dispatched yet.
    frames_until(&mut client.stream, "the turn to start streaming", |frame| {
        matches!(
            frame,
            Frame::Event { event, .. }
                if matches!(event.known(), Some(AgentEvent::MessageUpdate { .. }))
        )
    })
    .await
    .into_iter()
    .for_each(|frame| {
        let _ = client.client.apply(&mut client.chat, frame);
    });

    harness
        .host
        .command(
            &session,
            Command::Cancel {
                agent: AgentId::Main,
            },
        )
        .await
        .expect("cancel");
    let frames = client.pump_until_idle().await;

    assert!(
        notice(&frames, CANCELLED),
        "the cancel is confirmed on the stream: {:?}",
        events(&frames)
            .into_iter()
            .map(event_kind)
            .collect::<Vec<_>>(),
    );
    assert_eq!(
        tool_cells(&client.chat, AgentId::Main),
        Vec::<String>::new(),
        "the cancelled turn never dispatched its tool call",
    );
    let answers = assistant_rows(&client.chat, AgentId::Main);
    assert!(
        !answers
            .iter()
            .any(|text| text.contains("the answer after the tool")),
        "and never ran the inference that would have answered it: {answers:?}",
    );
    harness.host.shutdown().await;
}

/// Cancelling a foreground sub-agent cascades to the main turn that owns
/// it, matching the local gesture.
#[tokio::test]
async fn cancelling_a_foreground_sub_cascades_to_main() {
    let harness = Harness::with_provider(scripted(
        vec![
            calling(
                "delegating",
                "call-sub",
                "agent",
                serde_json::json!({"task": "take your time"}),
            ),
            finalized_text_message("a slowly streamed sub answer"),
            finalized_text_message("done"),
        ],
        1,
        Duration::from_millis(30),
    ));
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "delegate it").await;

    // Wait until the sub-agent's run has started, then cancel the sub.
    frames_until(&mut client.stream, "the sub-agent to start", |frame| {
        matches!(
            frame,
            Frame::Event { event, .. }
                if matches!(event.known(), Some(AgentEvent::AgentStart { agent_id: AgentId::Sub(1) }))
        )
    })
    .await
    .into_iter()
    .for_each(|frame| {
        let _ = client.client.apply(&mut client.chat, frame);
    });
    harness
        .host
        .command(
            &session,
            Command::Cancel {
                agent: AgentId::Sub(1),
            },
        )
        .await
        .expect("cancel the sub");

    let frames = client.pump_until_idle().await;
    assert!(
        !client.client.working(),
        "the cascade cancelled the main turn too",
    );
    // A cancel of a foreground sub fires the main turn's token, because the
    // child's run is owned by that turn. Without the cascade nothing is
    // cancelled at all: the sub reports normally, the parent runs its
    // concluding inference, and neither of the two below holds.
    assert!(
        notice(&frames, CANCELLED),
        "the main turn was cancelled: {:?}",
        events(&frames)
            .into_iter()
            .map(event_kind)
            .collect::<Vec<_>>(),
    );
    let sub_reply = assistant_rows(&client.chat, AgentId::Sub(1));
    assert!(
        !sub_reply
            .iter()
            .any(|text| text.contains("a slowly streamed sub answer")),
        "the child's own run was cut short mid-stream: {sub_reply:?}",
    );
    let answers = assistant_rows(&client.chat, AgentId::Main);
    assert!(
        !answers.iter().any(|text| text == "done"),
        "and the parent never ran its concluding inference: {answers:?}",
    );
    // A sub whose run was cut short emits no `AgentEnd` of its own, so the
    // reap at the parent turn's join sweeps it and the host publishes the
    // conclusion as the event it stands for. Without that frame the box would
    // spin forever, and a client cannot conclude one by reaching into its own
    // model.
    let (status, finished) = sub_box(&client.canonical(), 1);
    assert_ne!(
        status,
        aj_app::chat::SubAgentStatus::Running,
        "the swept sub's box is concluded",
    );
    assert!(finished, "and its runtime clock stopped");
    assert_no_dangling(&client.chat);
    harness.host.shutdown().await;
}

/// Compaction is refused while the main agent is busy.
#[tokio::test]
async fn compaction_is_refused_while_busy() {
    let harness = Harness::with_provider(scripted(
        vec![finalized_text_message("a slowly streamed answer")],
        1,
        Duration::from_millis(20),
    ));
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "hi").await;

    let err = harness
        .host
        .command(&session, Command::Compact { instructions: None })
        .await
        .expect_err("compaction while busy is refused");
    assert!(matches!(err, HostError::Conflict { .. }), "got {err:?}");
    client.pump_until_idle().await;
    harness.host.shutdown().await;
}

/// A manual compaction's `CompactionEnd` is durable, tagged with the
/// checkpoint entry the compaction appended. Nothing on the bus carries
/// that identity, so it can only come from the append handoff the host
/// shares with its event forwarder.
#[tokio::test]
async fn a_compaction_end_is_tagged_with_its_checkpoint_entry() {
    let harness = Harness::new(vec![
        finalized_text_message("first answer"),
        finalized_text_message("second answer"),
        finalized_text_message("SUMMARY of the earlier work"),
    ]);
    // Keep almost nothing verbatim, so a two-turn session has something to
    // summarize.
    harness
        .config
        .lock()
        .expect("config mutex poisoned")
        .compact_keep_recent = 10;
    let session = harness.create().await;
    let mut stream = harness
        .host
        .attach(&[AttachRequest {
            session: session.clone(),
            cursor: None,
        }])
        .await
        .expect("attach");
    frames_until(&mut stream, "caught_up", |frame| {
        matches!(frame, Frame::CaughtUp { .. })
    })
    .await;
    harness.prompt(&session, "one").await;
    until_idle(&mut stream).await;
    // A large second prompt, so the keep-recent cut lands on it and the
    // first turn is left as the range to summarize.
    harness
        .prompt(&session, &format!("two {}", "X".repeat(2000)))
        .await;
    until_idle(&mut stream).await;

    harness
        .host
        .command(&session, Command::Compact { instructions: None })
        .await
        .expect("compact on an idle session");
    let frames = until_idle(&mut stream).await;

    let end = frames
        .iter()
        .find_map(|frame| match frame {
            Frame::Event {
                durability, event, ..
            } => match event.known() {
                Some(AgentEvent::CompactionEnd { summary, .. }) => {
                    Some((durability.clone(), summary.clone()))
                }
                _ => None,
            },
            _ => None,
        })
        .expect("the compaction ended");
    assert!(
        end.1.is_some(),
        "the compaction wrote a summary, so it appended a checkpoint",
    );
    let durability = end.0.expect("a successful compaction's end is durable");

    let handles = harness
        .host
        .local_handles(&session)
        .await
        .expect("live session");
    let log = handles.log.lock().await;
    let entries = log.entries_in_order();
    let index = usize::try_from(durability.seq).expect("seq fits usize") - 1;
    let entry = entries.get(index).expect("an entry at that position");
    assert_eq!(entry.id, durability.entry_id);
    assert!(
        matches!(
            entry.entry,
            aj_session::ConversationEntryKind::Compaction { .. }
        ),
        "the tagged entry is the compaction checkpoint: {:?}",
        entry.entry,
    );
    drop(log);
    harness.host.shutdown().await;
}

/// A blank prompt is refused rather than sent as an empty user message,
/// matching the local submit gesture's refusal.
#[tokio::test]
async fn a_blank_prompt_is_refused() {
    let harness = Harness::new(Vec::new());
    let session = harness.create().await;
    for content in [Vec::new(), vec![UserContent::text("   \n ")]] {
        let err = harness
            .host
            .command(
                &session,
                Command::Prompt {
                    agent: AgentId::Main,
                    content,
                },
            )
            .await
            .expect_err("a blank prompt is refused");
        assert!(matches!(err, HostError::Invalid(_)), "got {err:?}");
    }
    harness.host.shutdown().await;
}

/// Killing a task the registry does not know is a 404; killing a live one
/// is accepted.
#[tokio::test]
async fn killing_a_task_is_accepted_or_a_miss() {
    let harness = Harness::with_provider(scripted(
        vec![
            calling(
                "backgrounding it",
                "call-bash",
                "bash",
                serde_json::json!({"command": "sleep 30", "run_in_background": true,
                                   "description": "sleep"}),
            ),
            finalized_text_message("started it"),
        ],
        0,
        Duration::ZERO,
    ));
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "background something").await;
    client.pump_until_idle().await;

    let err = harness
        .host
        .command(&session, Command::KillTask { task: 999 })
        .await
        .expect_err("unknown tasks are refused");
    assert!(matches!(err, HostError::UnknownTask(_)), "got {err:?}");

    let table = harness.host.tasks(&session).await.expect("task table");
    let live = table
        .tasks
        .iter()
        .find(|task| task.status == aj_agent::tool::TaskStatus::Running)
        .expect("a live task");
    harness
        .host
        .command(&session, Command::KillTask { task: live.id })
        .await
        .expect("killing a live task is accepted");
    harness.host.shutdown().await;
}

/// A panicked turn task is reaped like any other, so the session goes idle
/// and stays usable. The turn emitted its `AgentStart` and will never emit
/// an `AgentEnd`, so without the reap the agent would read busy forever:
/// every `state` and `list` frame would report working, and the next prompt
/// would queue behind a turn that already died.
#[tokio::test]
async fn a_panicked_turn_leaves_the_session_idle() {
    // An empty script: the scripted provider panics the inference task
    // rather than inventing a reply.
    let harness = Harness::new(Vec::new());
    let session = harness.create().await;
    let mut stream = harness
        .host
        .attach(&[AttachRequest {
            session: session.clone(),
            cursor: None,
        }])
        .await
        .expect("attach");
    frames_until(&mut stream, "caught_up", |frame| {
        matches!(frame, Frame::CaughtUp { .. })
    })
    .await;

    harness.prompt(&session, "hi").await;
    let frames = frames_until(&mut stream, "the panic to surface", |frame| {
        matches!(
            frame,
            Frame::Event { event, .. }
                if matches!(event.known(), Some(AgentEvent::Error { text, .. })
                    if text.contains("panicked"))
        )
    })
    .await;
    assert!(
        !frames.is_empty(),
        "the panic surfaces as an error frame on the session's stream",
    );

    settle(&harness, &session, &mut stream).await;
    let summary = harness
        .host
        .sessions()
        .await
        .expect("sessions")
        .sessions
        .into_iter()
        .find(|entry| entry.id == session)
        .expect("the session is listed");
    assert!(
        !summary.working,
        "the session reports itself idle after the panic",
    );

    // And the next prompt runs a turn instead of queueing behind the dead
    // one.
    harness
        .install_script(&session, vec![finalized_text_message("after the panic")])
        .await;
    harness.prompt(&session, "again").await;
    let frames = until_idle(&mut stream).await;
    assert_eq!(assistant_text(&frames), "after the panic");
    harness.host.shutdown().await;
}

/// A turn's fatal error belongs to its session, not to the host (spec
/// section 5): it surfaces as an error frame on that session's stream, the
/// session stays live and usable, and another session on the same host is
/// untouched by it.
///
/// The fault is a log that cannot be opened. The first punctuating append
/// creates the file with `create_new`, so a path that is already taken makes
/// that append fail, and the append runs inside an inline bus listener whose
/// error is exactly a fatal turn error.
#[tokio::test]
async fn a_fatal_turn_error_stays_inside_its_session() {
    let harness = Harness::new(Vec::new());
    let broken = harness.create().await;
    let healthy = harness.create().await;
    harness
        .install_script(&broken, vec![finalized_text_message("recovered")])
        .await;
    harness
        .install_script(&healthy, vec![finalized_text_message("all fine here")])
        .await;
    let mut broken_client = Client::attach(&harness.host, &broken).await;
    let mut healthy_client = Client::attach(&harness.host, &healthy).await;

    let log_path = harness
        .persistence
        .sessions_dir()
        .join(format!("{broken}.jsonl"));
    std::fs::write(&log_path, "").expect("take the path the log wants");

    harness.prompt(&broken, "hi").await;
    let frames = broken_client.pump_until_idle().await;

    let reported = errors(&frames);
    assert!(
        reported.iter().any(|text| text.starts_with("IO error")),
        "the failed append surfaced as an error frame: {reported:?}",
    );
    assert!(
        !reported.iter().any(|text| text.contains("panicked")),
        "and as the turn's own error rather than a dead task: {reported:?}",
    );

    // The host kept serving: the other session runs its turn.
    harness.prompt(&healthy, "you ok?").await;
    healthy_client.pump_until_idle().await;
    assert_eq!(
        assistant_rows(&healthy_client.chat, AgentId::Main),
        vec!["all fine here".to_string()],
        "a fatal error in one session does not touch another",
    );
    assert!(
        errors(&broken_client.drain_into_fold()).is_empty(),
        "and the other session's turn earned no error of its own",
    );

    // And the failed session is still live: once the fault clears, the next
    // prompt runs a turn rather than finding a session the host tore down.
    std::fs::remove_file(&log_path).expect("clear the fault");
    harness.prompt(&broken, "again").await;
    broken_client.pump_until_idle().await;
    assert!(
        assistant_rows(&broken_client.chat, AgentId::Main)
            .iter()
            .any(|text| text == "recovered"),
        "the session survived its own fatal turn error",
    );
    harness.host.shutdown().await;
}

// ---------------------------------------------------------------------------
// 10 + 11 + 12 + 13. Attach ordering and convergence
// ---------------------------------------------------------------------------

/// The attach block is `state`, backfill, `caught_up`, contiguous on the
/// stream, and no live durable frame at or below the boundary follows it,
/// even when durable events land while the attach is being served.
#[tokio::test]
async fn the_attach_block_is_contiguous_and_filters_the_boundary() {
    let harness = Harness::with_provider(scripted(
        vec![
            finalized_text_message("a slowly streamed first answer"),
            finalized_text_message("a slowly streamed second answer"),
        ],
        1,
        Duration::from_millis(10),
    ));
    let session = harness.create().await;
    let mut warm = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "one").await;
    warm.pump_until_idle().await;

    // Attach while the next turn is streaming, so durable events are in
    // flight in the fan-out as the block is served.
    harness.prompt(&session, "two").await;
    let mut stream = harness
        .host
        .attach(&[AttachRequest {
            session: session.clone(),
            cursor: None,
        }])
        .await
        .expect("attach");
    let block = frames_until(&mut stream, "caught_up", |frame| {
        matches!(frame, Frame::CaughtUp { .. })
    })
    .await;

    // Contiguity: exactly one `state` at the front, one `caught_up` at the
    // back, and nothing but event frames in between.
    assert!(matches!(&block[0], Frame::State { .. }), "opens with state");
    let boundary = match block.last().expect("a block") {
        Frame::CaughtUp { last_seq, .. } => *last_seq,
        other => panic!("the block ends with caught_up, got {other:?}"),
    };
    assert!(
        block[1..block.len() - 1]
            .iter()
            .all(|frame| matches!(frame, Frame::Event { .. })),
        "the block is state, backfill, caught_up with nothing spliced in",
    );
    assert!(
        durable(&block).iter().all(|(seq, _)| *seq <= boundary),
        "the backfill stops at the boundary",
    );

    // End-to-end shape only. Whether a frame at or below the boundary is
    // actually dropped is decided by the fan-out's own filter, and the unit
    // test over it (`a_live_stream_filters_durable_frames_at_or_below_its_
    // boundary`) is the oracle for that: here the assertion holds trivially
    // whenever nothing below the boundary happened to be in flight.
    let rest = until_idle(&mut stream).await;
    assert!(
        durable(&rest).iter().all(|(seq, _)| *seq > boundary),
        "no live durable frame at or below the boundary is delivered: {:?}",
        durable(&rest),
    );
    harness.host.shutdown().await;
}

/// A cursor at the session's high-water mark is served an empty suffix, and one
/// past it a full backfill: it names a history this host does not have, so it
/// counts as an epoch mismatch (spec 6.5). Serving it an empty suffix plus a
/// `caught_up` would silently rewind the client's cursor instead.
#[tokio::test]
async fn a_cursor_past_the_high_water_mark_earns_a_full_backfill() {
    let harness = Harness::new(vec![finalized_text_message("an answer")]);
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "hi").await;
    client.pump_until_idle().await;
    let settled = client.canonical();
    let epoch = client.client.cursor().expect("a committed cursor").epoch;
    // The host's own mark, not the client's cursor: a client holds its last
    // entry back until a trailing event rules one out (spec 6.5), so its
    // cursor sits one short of the boundary this test is about. A client may
    // not turn a position it read in the directory into a cursor either, the
    // test is reading ground truth to build the case.
    let mark = harness
        .host
        .sessions()
        .await
        .expect("sessions")
        .sessions
        .iter()
        .find(|entry| entry.id == session)
        .expect("the session")
        .last_seq
        .expect("a live session's row reports its position");
    assert!(mark > 0, "the turn wrote log entries");

    let at_mark = client
        .reattach(
            &harness.host,
            aj_wire::Cursor {
                epoch: epoch.clone(),
                seq: mark,
            },
        )
        .await;
    assert!(
        durable(&at_mark).is_empty(),
        "everything is at or below the cursor: {:?}",
        durable(&at_mark),
    );

    // Seq 0 reads as "nothing durable yet" (spec 6.4), so it is the cursor
    // whose suffix is the whole log: the oracle for a full backfill.
    let from_the_start = client
        .reattach(
            &harness.host,
            aj_wire::Cursor {
                epoch: epoch.clone(),
                seq: 0,
            },
        )
        .await;
    let beyond = client
        .reattach(
            &harness.host,
            aj_wire::Cursor {
                epoch,
                seq: mark + 1,
            },
        )
        .await;
    assert!(
        !durable(&from_the_start).is_empty(),
        "the turn is in the log, so a full backfill carries it",
    );
    assert_eq!(
        durable(&beyond),
        durable(&from_the_start),
        "a cursor past the mark is served what a cursor at the start is",
    );
    // And re-applying the whole history left the client where it was, which is
    // what makes serving it safe.
    assert_canonical_eq(
        &client.canonical(),
        &settled,
        "a full backfill under the same epoch",
    );
    assert_no_dangling(&client.chat);
    harness.host.shutdown().await;
}

/// An attach whose state moved while it was projecting publishes one more
/// `state` frame behind the block, which is what self-heals the change the
/// block dropped as lossy (spec 6.3).
///
/// `working` and `settings` are read before the projection, and a `state` frame
/// published during it is held and dropped, lossy frames being droppable by
/// definition. Without the refresh, a client whose block was served across the
/// change would keep showing the settings the block opened with until something
/// else moved.
///
/// The change has to land inside the span between an attach's status snapshot
/// and its block delivery, and the client decides how long that span is: the
/// block is producer-paced over a capacity-one channel (see the fan-out's module
/// docs), so a client that takes one frame and stops leaves the producer parked
/// inside the block for as long as the test wants. That is why the flip below
/// needs no timing at all, and why the rest of the block still has to arrive
/// after it.
#[tokio::test]
async fn an_attach_refreshes_state_that_moved_while_it_projected() {
    let harness = Harness::new(vec![finalized_text_message("an answer")]);
    let session = harness.create().await;
    // One turn, so the block has more frames than the channel can buffer and a
    // client that stops reading parks its producer.
    let mut warmup = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "hi").await;
    warmup.pump_until_idle().await;
    drop(warmup);

    let mut stream = harness
        .host
        .attach(&[AttachRequest {
            session: session.clone(),
            cursor: None,
        }])
        .await
        .expect("attach");
    let opened = bounded("the block's opening frame", stream.recv())
        .await
        .expect("the stream carries its block");
    let Frame::State { settings, .. } = &opened else {
        panic!("a block opens with its `state` frame, got {opened:?}");
    };
    let opened_with = settings.thinking.clone();

    // Flip to whatever the block did not open with, so the change moves the
    // settings identity a `state` frame carries.
    let high = aj_models::ThinkingConfig::High;
    let level = if opened_with == aj_models::thinking_config_name(Some(&high)) {
        None
    } else {
        Some(high)
    };
    let expected = aj_models::thinking_config_name(level.as_ref()).to_string();
    assert_ne!(expected, opened_with, "the flip has to move the settings");
    harness
        .host
        .command(
            &session,
            Command::Settings(SettingsChange {
                agent: AgentId::Main,
                persist: PersistAction::None,
                axis: SettingsAxis::Thinking(level),
            }),
        )
        .await
        .expect("thinking change");

    // The producer is still inside the block, so its tail is still to come, and
    // nothing in it can carry the change: those frames were projected before it.
    let block = frames_until(&mut stream, "the rest of the attach block", |frame| {
        matches!(frame, Frame::CaughtUp { .. })
    })
    .await;
    assert_eq!(
        states(&block, &expected),
        0,
        "the block cannot carry a change made after its projection: {block:?}",
    );

    // The refresh is what carries it, behind the block.
    let behind = frames_until(&mut stream, "the refreshed state frame", |frame| {
        matches!(frame, Frame::State { .. })
    })
    .await;
    let refreshed = behind.last().expect("the frame the wait stopped on");
    match refreshed {
        Frame::State { settings, .. } => assert_eq!(
            settings.thinking, expected,
            "the state frame behind the block reports the settings the block \
             opened with",
        ),
        other => panic!("expected a `state` frame, got {other:?}"),
    }
    harness.host.shutdown().await;
}

/// How many `state` frames in `frames` report `thinking`.
fn states(frames: &[Frame], thinking: &str) -> usize {
    frames
        .iter()
        .filter(
            |frame| matches!(frame, Frame::State { settings, .. } if settings.thinking == thinking),
        )
        .count()
}

/// A client that attaches mid-turn converges on the same state as one that
/// was attached from the start. This is the phase-1 shape of the
/// equivalence harness.
#[tokio::test]
async fn attaching_mid_turn_converges_with_a_client_attached_all_along() {
    let harness = Harness::with_provider(scripted(
        vec![
            calling(
                "let me check the list",
                "call-1",
                "todo_read",
                serde_json::json!({}),
            ),
            finalized_text_message("a slowly streamed answer about nothing"),
        ],
        1,
        Duration::from_millis(10),
    ));
    let session = harness.create().await;
    let mut all_along = Client::attach(&harness.host, &session).await;

    harness.prompt(&session, "check the todos").await;
    // Let the turn get going, then join it mid-flight.
    frames_until(&mut all_along.stream, "the tool call to finish", |frame| {
        matches!(
            frame,
            Frame::Event { event, .. }
                if matches!(event.known(), Some(AgentEvent::ToolExecutionEnd { .. }))
        )
    })
    .await
    .into_iter()
    .for_each(|frame| {
        let _ = all_along.client.apply(&mut all_along.chat, frame);
    });
    let mut joiner = Client::attach(&harness.host, &session).await;

    all_along.pump_until_idle().await;
    joiner.pump_until_idle().await;

    assert_canonical_eq(
        &joiner.canonical(),
        &all_along.canonical(),
        "a mid-turn joiner converges",
    );
    assert_no_dangling(&joiner.chat);
    assert_eq!(
        main_tools(&joiner.canonical()),
        vec!["todo_read".to_string()],
        "the compared state is a whole turn, tool cell included",
    );
    harness.host.shutdown().await;
}

/// Attaching while a sub-agent runs leaves its bracket open: no
/// force-closed box, no spurious conclusion. Once the sub finishes both
/// clients converge (spec section 11's "attach mid-sub-run").
#[tokio::test]
async fn attaching_mid_sub_run_leaves_the_bracket_open() {
    let harness = Harness::with_provider(scripted(sub_agent_turn(), 1, Duration::from_millis(20)));
    let session = harness.create().await;
    let mut all_along = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "delegate it").await;

    // Join once the sub-agent's first message is durable, so its spawn
    // root is in the log and its run is still live.
    frames_until(&mut all_along.stream, "the sub-agent to start", |frame| {
        matches!(
            frame,
            Frame::Event { event, .. }
                if matches!(event.known(), Some(AgentEvent::AgentStart { agent_id: AgentId::Sub(1) }))
        )
    })
    .await
    .into_iter()
    .for_each(|frame| {
        let _ = all_along.client.apply(&mut all_along.chat, frame);
    });
    let mut joiner = Client::attach(&harness.host, &session).await;

    let joined = sub_box(&joiner.canonical(), 1);
    assert_eq!(
        joined.0,
        aj_app::chat::SubAgentStatus::Running,
        "the projection left the live sub's bracket open",
    );
    assert!(!joined.1, "and did not freeze its runtime clock");

    all_along.pump_until_idle().await;
    joiner.pump_until_idle().await;
    assert_canonical_eq(
        &joiner.canonical(),
        &all_along.canonical(),
        "the sub-run converges once it finishes",
    );
    harness.host.shutdown().await;
}

/// A sub-agent that concluded while a client was away gets an `AgentEnd`
/// after `caught_up`, including when zero durable entries follow the
/// client's cursor.
#[tokio::test]
async fn the_conclusion_sweep_ends_a_sub_that_finished_in_the_gap() {
    let harness = Harness::new(sub_agent_turn());
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "delegate it").await;
    client.pump_until_idle().await;
    let cursor = client.client.cursor().expect("a committed cursor");

    // Re-attach at the session's high-water mark, so the backfill is
    // empty and can carry no conclusion of its own.
    let last_seq = {
        let handles = harness
            .host
            .local_handles(&session)
            .await
            .expect("live session");
        handles.log.lock().await.last_seq()
    };
    let mut stream = harness
        .host
        .attach(&[AttachRequest {
            session: session.clone(),
            cursor: Some(aj_wire::Cursor {
                epoch: cursor.epoch.clone(),
                seq: last_seq,
            }),
        }])
        .await
        .expect("attach");
    let block = frames_until(&mut stream, "caught_up", |frame| {
        matches!(frame, Frame::CaughtUp { .. })
    })
    .await;
    assert_eq!(
        durable(&block),
        Vec::new(),
        "the suffix is empty at the high-water mark",
    );

    let sweep = frames_until(&mut stream, "the conclusion sweep", |frame| {
        matches!(
            frame,
            Frame::Event { event, .. }
                if matches!(event.known(), Some(AgentEvent::AgentEnd { agent_id: AgentId::Sub(1), .. }))
        )
    })
    .await;
    assert!(
        durable(&sweep).is_empty(),
        "the sweep's frames are synthesized, so they carry no cursor",
    );
    harness.host.shutdown().await;
}

/// A backfill served the instant a sub-agent's spawn root lands must not
/// conclude it.
///
/// The log names a run as soon as its spawn root is appended, which is
/// several bus emits before the host consumes the `AgentStart` that reports
/// it running. A host that tracked the live set directly would therefore
/// lag the log in exactly that window, and the projection would force-close
/// a bracket the sub is still writing into: the client renders a concluded
/// box with a fabricated report for a live sub-agent.
///
/// The window opens once per run and only briefly, so this attaches in a
/// tight loop from the moment of the prompt. The oracle is stream order:
/// the fan-out publishes to every subscriber under one lock, so a run whose
/// real conclusion has not reached the warm client by the time a block has
/// been written was still live when that block was written.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attaching_as_a_sub_agent_spawns_never_concludes_it() {
    let harness =
        Harness::with_provider(scripted(background_sub_turn(), 1, Duration::from_millis(5)));
    let session = harness.create().await;
    let mut warm = harness
        .host
        .attach(&[AttachRequest {
            session: session.clone(),
            cursor: None,
        }])
        .await
        .expect("attach");
    frames_until(&mut warm, "caught_up", |frame| {
        matches!(frame, Frame::CaughtUp { .. })
    })
    .await;

    let concludes_a_sub = |frames: &[Frame]| {
        frames.iter().any(|frame| {
            matches!(frame, Frame::Event { event, .. }
                if matches!(event.known(),
                    Some(AgentEvent::AgentEnd { agent_id: AgentId::Sub(_), .. })
                    | Some(AgentEvent::SubAgentEnd { .. })))
        })
    };

    harness.prompt(&session, "kick it off").await;
    let mut really_ended = false;
    let mut attempts = 0;
    let deadline = tokio::time::Instant::now() + DEADLINE;
    while !really_ended {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the sub-agent run never concluded, after {attempts} attaches",
        );
        attempts += 1;
        let mut joiner = harness
            .host
            .attach(&[AttachRequest {
                session: session.clone(),
                cursor: None,
            }])
            .await
            .expect("attach");
        let block = frames_until(&mut joiner, "caught_up", |frame| {
            matches!(frame, Frame::CaughtUp { .. })
        })
        .await;
        let concluded = concludes_a_sub(&block);
        // Drained after the block, so anything the fan-out published
        // before it is already here.
        really_ended |= concludes_a_sub(&drained(&mut warm));
        assert!(
            !concluded || really_ended,
            "attempt {attempts} concluded a sub-agent whose run was still live, \
             block events: {:?}",
            events(&block)
                .into_iter()
                .map(event_kind)
                .collect::<Vec<_>>(),
        );
    }
    harness.host.shutdown().await;
}

/// A sub-agent continuation re-opens a run the host already saw finish, so a
/// backfill served during it must leave that bracket open again.
///
/// The record of finished runs is monotone (an id is minted once per
/// session), so it cannot say this on its own: what the host is driving a
/// turn for is the other half, recorded before the turn's task exists so no
/// append of the new run can land while the run still reads as finished.
#[tokio::test]
async fn attaching_during_a_sub_agent_continuation_leaves_its_bracket_open() {
    let mut script = sub_agent_turn();
    script.push(finalized_text_message(
        "still here, and taking my time about it",
    ));
    let harness = Harness::with_provider(scripted(script, 1, Duration::from_millis(20)));
    let session = harness.create().await;
    let mut warm = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "delegate it").await;
    warm.pump_until_idle().await;
    assert_ne!(
        sub_box(&warm.canonical(), 1).0,
        aj_app::chat::SubAgentStatus::Running,
        "the first run concluded",
    );

    // The command returns once the driver has accepted it, so the run is
    // recorded as driven by the time the attach below is served.
    harness
        .host
        .command(
            &session,
            Command::Prompt {
                agent: AgentId::Sub(1),
                content: vec![UserContent::text("keep going")],
            },
        )
        .await
        .expect("a retained sub-agent can be prompted again");

    let mut joiner = harness
        .host
        .attach(&[AttachRequest {
            session: session.clone(),
            cursor: None,
        }])
        .await
        .expect("attach");
    let block = frames_until(&mut joiner, "caught_up", |frame| {
        matches!(frame, Frame::CaughtUp { .. })
    })
    .await;
    let concluded = |frames: &[Frame]| {
        events(frames).into_iter().any(|event| {
            matches!(
                event,
                AgentEvent::AgentEnd {
                    agent_id: AgentId::Sub(1),
                    ..
                }
            ) || matches!(
                event,
                AgentEvent::SubAgentEnd {
                    child: AgentId::Sub(1),
                    ..
                }
            )
        })
    };
    assert!(
        !concluded(&block),
        "the continuation's bracket stays open: {:?}",
        events(&block)
            .into_iter()
            .map(event_kind)
            .collect::<Vec<_>>(),
    );

    // The real conclusion arriving after the block is what proves the run
    // was still live while the block was written.
    let after = frames_until(&mut joiner, "the continuation to conclude", |frame| {
        matches!(frame, Frame::Event { event, .. }
            if matches!(event.known(), Some(AgentEvent::AgentEnd { agent_id: AgentId::Sub(1), .. })))
    })
    .await;
    assert!(concluded(&after));
    harness.host.shutdown().await;
}

/// A cursor inside a running sub-agent's run: the block re-synthesizes that
/// run's `SubAgentStart` from its spawn root, leaves the bracket open, and
/// the client converges with one that never re-attached.
///
/// The spawn root sits at the cursor, so the projection cannot serve the real
/// start again (it is at the boundary, and tagging a bracketing frame durable
/// would make the client's cursor invariant drop it). The re-synthesized one
/// is glue, and it has to carry the spawn root's task, background flag and
/// settings: without them a client that had lost the box would re-open it as
/// an unlabelled foreground run.
#[tokio::test]
async fn attaching_with_a_cursor_inside_a_live_sub_run_resynthesizes_its_start() {
    // The parent and the background child share the provider, so which of the
    // two long messages each gets is up to their interleaving. Both are long,
    // so the child's run outlives the attach either way.
    let harness = Harness::with_provider(scripted(
        vec![
            calling(
                "kicking that off",
                "call-bg",
                "agent",
                serde_json::json!({"task": "look into it", "run_in_background": true}),
            ),
            finalized_text_message(
                "meanwhile, here is a long answer streamed one character at a time",
            ),
            finalized_text_message("and the background sub-agent reporting back at similar length"),
            // The task's completion notice wakes the parent once the child is
            // done, which runs one more inference.
            finalized_text_message("noted, thanks"),
        ],
        1,
        Duration::from_millis(20),
    ));
    let session = harness.create().await;
    let mut all_along = Client::attach(&harness.host, &session).await;
    let mut rewinding = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "kick it off").await;

    // The child's first durable entry: its run is under way, so the spawn
    // root is below the cursor and the suffix carries entries of the run.
    let sub_started = |frame: &Frame| {
        matches!(frame, Frame::Event { event, .. }
            if matches!(event.known(),
                Some(AgentEvent::MessageEnd { agent_id: AgentId::Sub(1), .. })))
    };
    let mut spawn_root = None;
    for client in [&mut all_along, &mut rewinding] {
        let frames = frames_until(&mut client.stream, "the child's first entry", sub_started).await;
        spawn_root = Some(spawn_root_seq(&frames));
        for frame in &frames {
            let _ = client.client.apply(&mut client.chat, frame.clone());
        }
    }
    let spawn_root = spawn_root.expect("both clients saw the spawn");
    let epoch = rewinding.client.cursor().expect("a committed cursor").epoch;

    let block = rewinding
        .reattach(
            &harness.host,
            aj_wire::Cursor {
                epoch,
                seq: spawn_root,
            },
        )
        .await;

    let starts: Vec<(AgentId, String, bool, AgentSettings, Option<u64>)> = block
        .iter()
        .filter_map(|frame| match frame {
            Frame::Event {
                durability, event, ..
            } => match event.known() {
                Some(AgentEvent::SubAgentStart {
                    child,
                    task,
                    background,
                    settings,
                    ..
                }) => Some((
                    *child,
                    task.clone(),
                    *background,
                    settings.clone(),
                    durability.as_ref().map(|durability| durability.seq),
                )),
                _ => None,
            },
            _ => None,
        })
        .collect();
    let mut persisted_settings = settings();
    persisted_settings.thinking_display.clear();
    assert_eq!(
        starts,
        vec![(
            AgentId::Sub(1),
            "look into it".to_string(),
            true,
            persisted_settings,
            None,
        )],
        "the run's start is re-synthesized with the spawn root's own fields, untagged",
    );
    assert!(
        !events(&block).into_iter().any(|event| matches!(
            event,
            AgentEvent::AgentEnd {
                agent_id: AgentId::Sub(1),
                ..
            } | AgentEvent::SubAgentEnd {
                child: AgentId::Sub(1),
                ..
            }
        )),
        "and the bracket stays open, the run being live: {:?}",
        events(&block)
            .into_iter()
            .map(event_kind)
            .collect::<Vec<_>>(),
    );
    assert_eq!(
        sub_box(&rewinding.canonical(), 1).0,
        aj_app::chat::SubAgentStatus::Running,
        "so the client keeps rendering a running box",
    );

    for client in [&mut rewinding, &mut all_along] {
        let frames = settle(&harness, &session, &mut client.stream).await;
        for frame in &frames {
            let _ = client.client.apply(&mut client.chat, frame.clone());
        }
    }

    assert_canonical_eq(
        &rewinding.canonical(),
        &all_along.canonical(),
        "a cursor inside a live run converges once the run ends",
    );
    assert_no_dangling(&rewinding.chat);
    harness.host.shutdown().await;
}

/// The durable position of the `SubAgentStart` frame in `frames`: the append
/// index of the spawn root the host wrote for that run.
fn spawn_root_seq(frames: &[Frame]) -> u64 {
    frames
        .iter()
        .find_map(|frame| match frame {
            Frame::Event {
                durability: Some(durability),
                event,
                ..
            } => matches!(event.known(), Some(AgentEvent::SubAgentStart { .. }))
                .then_some(durability.seq),
            _ => None,
        })
        .expect("a durable sub-agent start")
}

/// A client that re-attaches during a sub-agent continuation converges with
/// one that was attached all along, the box's report included.
///
/// A continuation persists no lifecycle bracket, so the client attached all
/// along learns that the run re-opened from `AgentStart(Sub 1)` while the
/// re-attaching one has only the block's re-synthesized `SubAgentStart` to go
/// on. That start therefore has to mark the run in progress: the box's report
/// is refreshed from the sub's own conclusions only while the box reads
/// `Running`, so one left concluded would render the first run's report for
/// good.
#[tokio::test]
async fn attaching_during_a_sub_agent_continuation_converges_on_its_report() {
    let mut script = sub_agent_turn();
    script.push(finalized_text_message(
        "still here, and taking my time about it",
    ));
    let harness = Harness::with_provider(scripted(script, 1, Duration::from_millis(20)));
    let session = harness.create().await;
    let mut all_along = Client::attach(&harness.host, &session).await;
    let mut rewinding = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "delegate it").await;
    for client in [&mut all_along, &mut rewinding] {
        client.pump_until_idle().await;
        assert_ne!(
            sub_box(&client.canonical(), 1).0,
            aj_app::chat::SubAgentStatus::Running,
            "the first run concluded",
        );
        assert_eq!(
            sub_report(&client.canonical(), 1).as_deref(),
            Some("the sub found nothing"),
            "carrying its report",
        );
    }

    // The command returns once the driver has accepted it, so the run is
    // recorded as driven by the time the attach below is served.
    harness
        .host
        .command(
            &session,
            Command::Prompt {
                agent: AgentId::Sub(1),
                content: vec![UserContent::text("keep going")],
            },
        )
        .await
        .expect("a retained sub-agent can be prompted again");

    // The cursor sits at the end of the first run, so the suffix is the
    // continuation's entries and the spawn root stays below it.
    let cursor = rewinding.client.cursor().expect("a committed cursor");
    let block = rewinding.reattach(&harness.host, cursor).await;

    let starts: Vec<Option<u64>> = block
        .iter()
        .filter_map(|frame| match frame {
            Frame::Event {
                durability, event, ..
            } => matches!(
                event.known(),
                Some(AgentEvent::SubAgentStart {
                    child: AgentId::Sub(1),
                    ..
                })
            )
            .then(|| durability.as_ref().map(|durability| durability.seq)),
            _ => None,
        })
        .collect();
    assert_eq!(
        starts,
        vec![None],
        "the continuation's bracket opens with a re-synthesized, untagged start: {:?}",
        events(&block)
            .into_iter()
            .map(event_kind)
            .collect::<Vec<_>>(),
    );
    assert_eq!(
        sub_box(&rewinding.canonical(), 1).0,
        aj_app::chat::SubAgentStatus::Running,
        "which re-opens the box for the run it brackets",
    );

    // A sub-agent's run is invisible to the session's `working` flag (spec
    // 6.3), so each client is pumped to the continuation's own `AgentEnd`.
    // The reap publishes a second one behind it, which the fold absorbs
    // idempotently, so the two states compare equal either way.
    for client in [&mut rewinding, &mut all_along] {
        let frames = frames_until(&mut client.stream, "the continuation to conclude", |frame| {
            matches!(frame, Frame::Event { event, .. }
                if matches!(event.known(), Some(AgentEvent::AgentEnd { agent_id: AgentId::Sub(1), .. })))
        })
        .await;
        for frame in &frames {
            let _ = client.client.apply(&mut client.chat, frame.clone());
        }
    }

    assert_eq!(
        sub_report(&all_along.canonical(), 1).as_deref(),
        Some("still here, and taking my time about it"),
        "the box tracks the continuation's report",
    );
    assert_canonical_eq(
        &rewinding.canonical(),
        &all_along.canonical(),
        "a re-attach inside a continuation converges once it ends",
    );
    assert_no_dangling(&rewinding.chat);
    harness.host.shutdown().await;
}

/// A resumed session's sub-agent boxes are still concluded: nothing runs at
/// materialization, so every run the log names has finished, which is what
/// the host seeds its record of finished runs with.
#[tokio::test]
async fn a_resumed_session_concludes_the_sub_agents_on_disk() {
    let harness = Harness::new(sub_agent_turn());
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "delegate it").await;
    client.pump_until_idle().await;
    assert_ne!(
        sub_box(&client.canonical(), 1).0,
        aj_app::chat::SubAgentStatus::Running,
        "the live run concluded",
    );
    drop(client);
    harness.host.shutdown().await;

    let revived = harness.revive(Vec::new());
    let joiner = Client::attach(&revived.host, &session).await;
    assert_ne!(
        sub_box(&joiner.canonical(), 1).0,
        aj_app::chat::SubAgentStatus::Running,
        "a run that is only on disk is concluded, not left spinning",
    );
    drop(joiner);
    revived.host.shutdown().await;
}

/// A client that stops draining is evicted rather than buffered without
/// bound, and the ordinary re-attach with its cursor puts it back where a
/// client that never stalled would be (spec 6.9, and spec section 11's
/// "slow-client eviction and recovery").
///
/// The recovery is the interesting half, so the cursor it offers is a real
/// one: the client applied a whole turn first, so the re-attach serves an
/// incremental suffix onto retained state and has to quiesce and re-apply
/// idempotently rather than rebuild.
///
/// The bound is what makes eviction reachable at all: the same rule at 256
/// frames needs 256 frames to hit.
#[tokio::test]
async fn a_slow_client_is_evicted_and_recovers_through_its_cursor() {
    let harness = Harness::with_live_capacity(Vec::new(), 4);
    let session = harness.create().await;
    harness
        .install_script(&session, vec![finalized_text_message("the first answer")])
        .await;

    // The first turn runs with nobody attached, so the client below picks it
    // up through its attach block. An attach block is producer-paced, so
    // reading one cannot trip the bound.
    harness.prompt(&session, "hi").await;
    settled(&harness, &session, 2).await;

    let mut stalled = Client::attach(&harness.host, &session).await;
    let cursor = stalled
        .client
        .cursor()
        .expect("the block committed a cursor");
    assert!(cursor.seq >= 2, "the cursor is a real position: {cursor:?}");

    // From here the client reads nothing.
    harness
        .install_script(&session, vec![finalized_text_message("the second answer")])
        .await;
    harness.prompt(&session, "again").await;
    settled(&harness, &session, cursor.seq + 2).await;

    // The second turn produced more undroppable frames than the bound holds,
    // so the stalled client's stream was closed. It hands back nothing,
    // because an eviction cancels the stream rather than draining it.
    let leftover = bounded("the stalled stream to close", async {
        let mut seen = 0;
        while stalled.stream.recv().await.is_some() {
            seen += 1;
        }
        seen
    })
    .await;
    assert_eq!(leftover, 0, "an evicted stream stops handing frames back");

    // Recovery is the ordinary re-attach, on the client that kept its state:
    // the suffix lands on the first turn it already applied.
    stalled.reattach(&harness.host, cursor).await;
    let fresh = Client::attach(&harness.host, &session).await;

    assert_eq!(
        assistant_rows(&stalled.chat, AgentId::Main),
        vec![
            "the first answer".to_string(),
            "the second answer".to_string()
        ],
        "the suffix carried the turn the eviction cut off, onto the one it had",
    );
    assert_canonical_eq(
        &stalled.canonical(),
        &fresh.canonical(),
        "an evicted client that re-attached with its cursor",
    );
    harness.host.shutdown().await;
}

/// Wait for `session` to be idle with at least `last_seq` durable entries,
/// watched off the host's directory rather than off a stream.
///
/// A second reader would be racing whatever bound the test set.
async fn settled(harness: &Harness, session: &str, last_seq: u64) {
    bounded("the turn to land and settle", async {
        loop {
            let row = summary(&harness.host, session).await.expect("listed");
            if !row.working && row.last_seq.unwrap_or(0) >= last_seq {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
}

// ---------------------------------------------------------------------------
// 14. List frames
// ---------------------------------------------------------------------------

/// `list` frames carry the whole directory with per-session status, and a
/// busy turn does not produce one frame per event.
#[tokio::test]
async fn list_frames_carry_the_directory_and_are_debounced() {
    let harness = Harness::with_provider(scripted(
        vec![finalized_text_message(
            "a long answer streamed one character at a time so the event count is high",
        )],
        1,
        Duration::from_millis(1),
    ));
    let session = harness.create().await;
    let mut stream = harness
        .host
        .attach(&[AttachRequest {
            session: session.clone(),
            cursor: None,
        }])
        .await
        .expect("attach");
    frames_until(&mut stream, "caught_up", |frame| {
        matches!(frame, Frame::CaughtUp { .. })
    })
    .await;

    harness.prompt(&session, "hi").await;
    let frames = until_idle(&mut stream).await;
    // Give the debounce tick room to publish whatever it coalesced.
    tokio::time::sleep(LIST_SETTLE).await;
    let mut frames = frames;
    frames.extend(drained(&mut stream));

    let lists: Vec<&Frame> = frames
        .iter()
        .filter(|frame| matches!(frame, Frame::List { .. }))
        .collect();
    let event_count = events(&frames).len();
    assert!(
        event_count > 20,
        "the turn was chatty: {event_count} events"
    );
    assert!(!lists.is_empty(), "the directory was published");
    assert!(
        lists.len() <= 8,
        "{} list frames for {event_count} events is not debounced",
        lists.len(),
    );

    let last = lists.last().expect("a list frame");
    let Frame::List { sessions } = last else {
        unreachable!("filtered above")
    };
    let summary = sessions
        .iter()
        .find(|entry| entry.id == session)
        .expect("the live session is listed");
    assert!(summary.live);
    assert!(!summary.working, "the turn has settled");
    assert!(
        summary.last_seq.is_some_and(|seq| seq > 0),
        "a live row reports the position the host holds",
    );
    harness.host.shutdown().await;
}

/// Write a current-format log straight into the store, the way a sibling
/// process in the same working directory would. Returns its session id.
fn sibling_log(harness: &Harness, id: &str) -> String {
    let sessions_dir = harness.persistence.sessions_dir().to_path_buf();
    std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
    let entry = serde_json::json!({
        "id": "00000000",
        "timestamp": "2024-01-01T00:00:00Z",
        "thread": "meta",
        "type": "system_prompt",
        "text": "x",
    });
    std::fs::write(
        sessions_dir.join(format!("{id}.jsonl")),
        format!("{entry}\n"),
    )
    .expect("write a sibling's log");
    id.to_string()
}

/// The directories carried by the `list` frames among `frames`, each checked
/// for the present-iff-live invariant (see [`assert_rows_well_formed`]).
fn directories(frames: &[Frame]) -> Vec<Vec<SessionSummary>> {
    frames
        .iter()
        .filter_map(|frame| match frame {
            Frame::List { sessions } => {
                assert_rows_well_formed(sessions);
                Some(sessions.clone())
            }
            _ => None,
        })
        .collect()
}

/// The refresh path performs no filesystem work at all (spec 6.8): every
/// directory read the host does is attributable to an enumeration point, and a
/// streaming turn, which marks the directory dirty on every event, is not one.
#[tokio::test]
async fn a_turns_refreshes_never_read_the_directory() {
    let harness = Harness::with_provider(scripted(
        vec![finalized_text_message(
            "a long answer streamed one character at a time so the event count is high",
        )],
        1,
        Duration::from_millis(1),
    ));
    let session = harness.create().await;
    let mut stream = harness
        .host
        .attach(&[AttachRequest {
            session: session.clone(),
            cursor: None,
        }])
        .await
        .expect("attach");
    frames_until(&mut stream, "caught_up", |frame| {
        matches!(frame, Frame::CaughtUp { .. })
    })
    .await;

    let before = harness.host.store_directory_reads();
    harness.prompt(&session, "hi").await;
    let frames = until_idle(&mut stream).await;
    tokio::time::sleep(LIST_SETTLE).await;
    let mut frames = frames;
    frames.extend(drained(&mut stream));

    let event_count = events(&frames).len();
    assert!(
        event_count > 20,
        "the turn was chatty: {event_count} events"
    );
    assert!(
        !directories(&frames).is_empty(),
        "and the directory was published while it ran",
    );
    assert_eq!(
        harness.host.store_directory_reads(),
        before,
        "{event_count} events' worth of refreshes read the directory",
    );
    harness.host.shutdown().await;
}

/// A session a sibling process leaves in the store appears at the next
/// enumeration point and not before. The host is the single writer of its
/// working directory (spec section 5), so a sibling's session is a conflict to
/// surface when asked, not a workload to poll for.
#[tokio::test]
async fn a_siblings_session_appears_at_the_next_enumeration_point() {
    let harness = Harness::new(vec![finalized_text_message("hi back")]);
    let session = harness.create().await;
    let mut stream = harness
        .host
        .attach(&[AttachRequest {
            session: session.clone(),
            cursor: None,
        }])
        .await
        .expect("attach");
    frames_until(&mut stream, "caught_up", |frame| {
        matches!(frame, Frame::CaughtUp { .. })
    })
    .await;

    let sibling = sibling_log(&harness, "2020-01-01-00-00-00-999");
    // A turn's worth of refreshes, none of which may go looking.
    harness.prompt(&session, "hi").await;
    let frames = until_idle(&mut stream).await;
    tokio::time::sleep(LIST_SETTLE).await;
    let mut frames = frames;
    frames.extend(drained(&mut stream));
    let published = directories(&frames);
    assert!(!published.is_empty(), "the directory was published");
    assert!(
        published
            .iter()
            .all(|directory| !directory.iter().any(|entry| entry.id == sibling)),
        "a refresh went to the store: {published:?}",
    );

    // An explicit listing is an enumeration point, and what it finds reaches
    // the next published frame.
    assert!(
        harness
            .host
            .sessions()
            .await
            .expect("listed")
            .sessions
            .iter()
            .any(|entry| entry.id == sibling),
        "the listing found the sibling's log",
    );
    frames_until(&mut stream, "the sibling to be published", |frame| {
        matches!(frame, Frame::List { sessions }
            if sessions.iter().any(|entry| entry.id == sibling))
    })
    .await;
    drop(stream);
    harness.host.shutdown().await;
}

/// A session is attachable as soon as its log is in the store, whether or not
/// any enumeration has picked it up. Membership is answered off the store, so
/// the rows a refresh serves never gate an attach.
#[tokio::test]
async fn a_siblings_session_is_attachable_before_it_is_listed() {
    let harness = Harness::new(vec![finalized_text_message("resumed")]);
    let sibling = sibling_log(&harness, "2020-01-01-00-00-00-997");
    let mut stream = harness
        .host
        .attach(&[AttachRequest {
            session: sibling.clone(),
            cursor: None,
        }])
        .await
        .expect("a log in the store is attachable");
    frames_until(&mut stream, "caught_up", |frame| {
        matches!(frame, Frame::CaughtUp { .. })
    })
    .await;
    drop(stream);
    harness.host.shutdown().await;
}

/// A fresh stream is an enumeration point too, so a client connecting to a
/// long-running host sees what the store holds now rather than what it held
/// when the host started.
#[tokio::test]
async fn a_fresh_stream_enumerates_the_store() {
    let harness = Harness::new(Vec::new());
    let session = harness.create().await;
    let sibling = sibling_log(&harness, "2020-01-01-00-00-00-998");
    // Past the debounce of the create above, so the frame below can only have
    // come from the attach.
    tokio::time::sleep(LIST_SETTLE).await;

    let mut stream = harness
        .host
        .attach(&[AttachRequest {
            session: session.clone(),
            cursor: None,
        }])
        .await
        .expect("attach");
    frames_until(&mut stream, "the sibling to be published", |frame| {
        matches!(frame, Frame::List { sessions }
            if sessions.iter().any(|entry| entry.id == sibling))
    })
    .await;
    drop(stream);
    harness.host.shutdown().await;
}

/// The host's own structural changes reach the directory without an
/// enumeration: it knows what it just did, and the answer is already in memory.
#[tokio::test]
async fn the_hosts_own_changes_need_no_enumeration() {
    let harness = Harness::with_idle_grace(vec![finalized_text_message("recorded")], IDLE_GRACE);
    let first = harness.create().await;
    let mut stream = harness
        .host
        .attach(&[AttachRequest {
            session: first.clone(),
            cursor: None,
        }])
        .await
        .expect("attach");
    frames_until(&mut stream, "caught_up", |frame| {
        matches!(frame, Frame::CaughtUp { .. })
    })
    .await;

    let before = harness.host.store_directory_reads();
    let created = harness.host.create().await.expect("create");
    frames_until(&mut stream, "the new session to be published", |frame| {
        matches!(frame, Frame::List { sessions }
            if sessions.iter().any(|entry| entry.id == created && entry.live))
    })
    .await;

    // And the same session going the other way. Watched on the stream rather
    // than by polling the host, since a listing is an enumeration point and
    // would be counted below.
    harness.prompt(&created, "hi").await;
    let frames = frames_until(&mut stream, "the release to be published", |frame| {
        matches!(frame, Frame::List { sessions }
            if sessions.iter().any(|entry| entry.id == created && !entry.live))
    })
    .await;
    let released = directories(&frames)
        .pop()
        .expect("the frame that reported the release")
        .into_iter()
        .find(|entry| entry.id == created)
        .expect("the released session's row");
    assert_eq!(
        released.last_seq, None,
        "a cold row carries no position (spec 6.8)",
    );
    assert_eq!(
        harness.host.store_directory_reads(),
        before,
        "a create and a release read the directory",
    );
    drop(stream);
    harness.host.shutdown().await;
}

/// No two `list` frames in a row carry the same directory. Dirty is marked on
/// every session event and most events move nothing a directory row shows, so
/// unsuppressed a streaming turn republishes one payload at the debounce rate
/// for the length of the turn (spec 6.8).
#[tokio::test]
async fn an_unchanged_directory_is_not_published_again() {
    let harness = Harness::with_provider(scripted(
        vec![
            // Long and slow, so the turn spans many coalescing ticks and an
            // unsuppressed publisher has room to repeat itself.
            finalized_text_message(&"a slowly streamed answer ".repeat(40)),
            finalized_text_message("and a second answer"),
        ],
        1,
        Duration::from_millis(2),
    ));
    let session = harness.create().await;
    let mut stream = harness
        .host
        .attach(&[AttachRequest {
            session: session.clone(),
            cursor: None,
        }])
        .await
        .expect("attach");
    frames_until(&mut stream, "caught_up", |frame| {
        matches!(frame, Frame::CaughtUp { .. })
    })
    .await;

    harness.prompt(&session, "hi").await;
    let frames = until_idle(&mut stream).await;
    tokio::time::sleep(LIST_SETTLE).await;
    let mut frames = frames;
    frames.extend(drained(&mut stream));
    let published = directories(&frames);
    assert!(
        published.len() >= 2,
        "the turn published {} directories, too few for the check below to mean \
         anything",
        published.len(),
    );
    for pair in published.windows(2) {
        assert_ne!(pair[0], pair[1], "the same directory was published twice");
    }

    let settled = published.last().expect("a directory").clone();
    let mark = settled
        .iter()
        .find(|entry| entry.id == session)
        .expect("the session's row")
        .last_seq;

    // A real change still gets through, so the suppression is not just a
    // cheaper way of publishing nothing. The mark, because a turn shorter than
    // the coalescing window can be over before `working` is ever sampled.
    harness.prompt(&session, "again").await;
    frames_until(&mut stream, "the second turn to be published", |frame| {
        matches!(frame, Frame::List { sessions }
            if sessions.iter().any(|entry| entry.id == session && entry.last_seq > mark))
    })
    .await;
    drop(stream);
    harness.host.shutdown().await;
}

/// A host that is shutting down publishes no directory. Its session map is
/// drained before its drivers are joined, so a frame composed in between would
/// report every live session from whatever the store last knew about it,
/// dropping its position and walking its stamp backwards on the last
/// directory a client ever sees.
#[tokio::test]
async fn a_shutting_down_host_publishes_no_directory() {
    let harness = Harness::with_idle_grace(
        vec![
            finalized_text_message("punctuation"),
            finalized_text_message("and more of it"),
        ],
        IDLE_GRACE,
    );
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "punctuate").await;
    client.pump_until_idle().await;
    drop(client);
    // Released, so the store has a row for it, and then taken up again and
    // worked past that row: the cold row is now stale by two turns.
    let stale = until_released(&harness.host, &session).await;
    assert_eq!(stale.last_seq, None, "a cold row carries no position");
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "more").await;
    client.pump_until_idle().await;
    let live = summary(&harness.host, &session).await.expect("listed");
    assert!(
        live.last_activity > stale.last_activity,
        "the second turn moved the session past the row it left behind",
    );
    let live_seq = live.last_seq.expect("a live row reports its position");

    // Holding the log stalls this session's teardown, which is what holds the
    // window open: the map is drained by then, and the fan-out is still open.
    let handles = harness.host.local_handles(&session).await.expect("handles");
    let held = handles.log.lock().await;
    // A create marks the directory dirty, so a refresh is due inside the
    // window rather than before it.
    let other = harness.host.create().await.expect("create");
    let host = harness.host.clone();
    let shutting = tokio::spawn(async move { host.shutdown().await });
    tokio::time::sleep(LIST_SETTLE).await;
    drop(held);
    shutting.await.expect("shutdown");

    for directory in directories(&drained(&mut client.stream)) {
        let row = directory
            .iter()
            .find(|entry| entry.id == session)
            .expect("the session is in every directory it publishes");
        assert!(
            row.live
                && row.last_seq.is_some_and(|seq| seq >= live_seq)
                && row.last_activity >= live.last_activity,
            "a directory published during shutdown reported {row:?}, \
             a row may not go backwards",
        );
    }
    drop(other);
}

/// Suppression compares against what subscribers have already been sent, so a
/// client attaching to a host whose directory has not moved in hours is still
/// served a snapshot. Nothing else on the stream carries one.
#[tokio::test]
async fn a_new_subscriber_is_served_a_directory_the_others_already_have() {
    let harness = Harness::new(vec![finalized_text_message("recorded")]);
    let session = harness.create().await;
    let mut settled = harness
        .host
        .attach(&[AttachRequest {
            session: session.clone(),
            cursor: None,
        }])
        .await
        .expect("attach");
    frames_until(&mut settled, "caught_up", |frame| {
        matches!(frame, Frame::CaughtUp { .. })
    })
    .await;
    // Quiesce, so the directory the second client needs is one the first has
    // already been sent and a republish would otherwise be suppressed.
    loop {
        tokio::time::sleep(LIST_SETTLE).await;
        if directories(&drained(&mut settled)).is_empty() {
            break;
        }
    }

    let mut fresh = harness
        .host
        .attach(&[AttachRequest {
            session: session.clone(),
            cursor: None,
        }])
        .await
        .expect("attach");
    frames_until(&mut fresh, "the fresh stream's first directory", |frame| {
        matches!(frame, Frame::List { sessions }
            if sessions.iter().any(|entry| entry.id == session))
    })
    .await;
    drop(fresh);
    drop(settled);
    harness.host.shutdown().await;
}

/// A `list` frame's status fields are what the sidebar's glyphs and the
/// client-side "needs attention" derivation hang on (spec 6.8), so each one
/// is asserted away from its default: a turn in flight, a pending
/// follow-up, a live background task, and a last-activity stamp that moved.
#[tokio::test]
async fn list_frames_report_working_queued_and_live_tasks() {
    let harness = Harness::with_provider(scripted(
        vec![
            calling(
                "backgrounding it",
                "call-bash",
                "bash",
                serde_json::json!({"command": "sleep 30", "run_in_background": true,
                                   "description": "sleep"}),
            ),
            finalized_text_message("started it"),
            finalized_text_message(
                "an answer streamed one character at a time, long enough that the \
                 directory tick fires several times while the turn runs",
            ),
        ],
        1,
        Duration::from_millis(10),
    ));
    let session = harness.create().await;
    let mut stream = harness
        .host
        .attach(&[AttachRequest {
            session: session.clone(),
            cursor: None,
        }])
        .await
        .expect("attach");
    frames_until(&mut stream, "caught_up", |frame| {
        matches!(frame, Frame::CaughtUp { .. })
    })
    .await;

    // The first turn leaves a background task behind, which outlives it.
    harness.prompt(&session, "background something").await;
    until_idle(&mut stream).await;

    let before = chrono::Utc::now();
    harness.prompt(&session, "now answer at length").await;
    // Busy, so this queues instead of running.
    harness.prompt(&session, "and this one later").await;

    // The turn keeps events flowing, so the directory tick keeps publishing
    // while all three conditions hold at once.
    let frames = frames_until(&mut stream, "a list frame for the busy session", |frame| {
        matches!(frame, Frame::List { sessions }
            if sessions.iter().any(|entry| entry.id == session
                && entry.live
                && entry.working
                && entry.queued.follow_up == 1
                && entry.tasks == 1))
    })
    .await;
    let summary = frames
        .iter()
        .rev()
        .find_map(|frame| match frame {
            Frame::List { sessions } => sessions.iter().find(|entry| entry.id == session).cloned(),
            _ => None,
        })
        .expect("filtered above");
    assert_eq!(
        summary.queued.steering, 0,
        "a follow-up is not counted as steering: {summary:?}",
    );
    assert!(
        summary.last_activity >= before,
        "the turn's appends moved the last-activity stamp: {summary:?}",
    );
    assert!(
        summary.last_seq.is_some_and(|seq| seq > 0),
        "and its durable position",
    );

    // Withdraw the follow-up so no wake asks the exhausted script for one
    // more inference.
    harness
        .host
        .command(&session, Command::Queue(QueueOp::Clear))
        .await
        .expect("clear");
    until_idle(&mut stream).await;
    harness.host.shutdown().await;
}

/// Materializing a session publishes a `list` frame saying it is live.
///
/// Without one, every other client's directory keeps reporting the session
/// as on-disk only until it happens to emit an event, and an
/// attached-but-idle session emits none.
#[tokio::test]
async fn a_materialization_publishes_the_directory() {
    let harness = Harness::new(vec![finalized_text_message("on the record")]);
    let dormant = harness.create().await;
    let mut client = Client::attach(&harness.host, &dormant).await;
    harness.prompt(&dormant, "hi").await;
    client.pump_until_idle().await;
    drop(client);
    harness.host.shutdown().await;

    // A fresh host over the same store: `dormant` is on disk only, and
    // `watching` carries the stream that has to learn about it.
    let revived = harness.revive(Vec::new());
    let watching = revived.create().await;
    let mut stream = revived
        .host
        .attach(&[AttachRequest {
            session: watching.clone(),
            cursor: None,
        }])
        .await
        .expect("attach");
    frames_until(&mut stream, "caught_up", |frame| {
        matches!(frame, Frame::CaughtUp { .. })
    })
    .await;
    let listed = |frames: &[Frame], live: bool| {
        frames.iter().any(|frame| match frame {
            Frame::List { sessions } => sessions
                .iter()
                .any(|entry| entry.id == dormant && entry.live == live),
            _ => false,
        })
    };
    // Wait out the directory changes creating `watching` earned, so the
    // frame asserted on below can only have come from materializing
    // `dormant`.
    let settled = frames_until(&mut stream, "the directory to settle", |frame| {
        matches!(frame, Frame::List { sessions }
            if sessions.iter().any(|entry| entry.id == dormant && !entry.live))
    })
    .await;
    assert!(listed(&settled, false), "filtered above");
    tokio::time::sleep(LIST_SETTLE).await;
    assert!(
        drained(&mut stream)
            .iter()
            .all(|frame| !matches!(frame, Frame::List { .. })),
        "no directory change is still pending",
    );

    // Attaching it is what makes it live, and nothing about it will ever
    // reach the stream on its own: it runs no turn.
    let attached = revived
        .host
        .attach(&[AttachRequest {
            session: dormant.clone(),
            cursor: None,
        }])
        .await
        .expect("attach the dormant session");
    let frames = frames_until(&mut stream, "the directory to report it live", |frame| {
        matches!(frame, Frame::List { sessions }
            if sessions.iter().any(|entry| entry.id == dormant && entry.live))
    })
    .await;
    assert!(listed(&frames, true), "filtered above");

    drop(attached);
    revived.host.shutdown().await;
}

// ---------------------------------------------------------------------------
// 15. Reads
// ---------------------------------------------------------------------------

/// The reads answer the task table (with wall-clock timestamps), the
/// session's usage, the branch tree, and hello with a `host_id` that
/// survives a restart.
#[tokio::test]
async fn the_reads_answer_tasks_tree_and_hello() {
    let harness = Harness::with_provider(scripted(
        vec![
            calling(
                "backgrounding it",
                "call-bash",
                "bash",
                serde_json::json!({"command": "sleep 30", "run_in_background": true,
                                   "description": "sleep"}),
            ),
            // Carries usage so the usage read below has something real to
            // report.
            finalized_text_message_with_usage("started it", 1234),
        ],
        0,
        Duration::ZERO,
    ));
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "background something").await;
    client.pump_until_idle().await;

    let before = chrono::Utc::now();
    let tasks = harness.host.tasks(&session).await.expect("task table");
    let task = tasks.tasks.first().expect("a task");
    assert_eq!(task.owner, AgentId::Main);
    assert!(!task.call_id.is_empty(), "the launching call is recorded");
    assert!(
        task.started_at <= before,
        "the wall-clock start is in the past: {:?}",
        task.started_at,
    );

    // An idle session has nothing queued, so the queue read is asserted where
    // queue state can be built through commands, in
    // `clearing_the_queue_empties_every_agent`.
    assert!(
        harness
            .host
            .queue(&session)
            .await
            .expect("queue read")
            .queues
            .is_empty(),
    );

    let usage = harness
        .host
        .usage(&session)
        .await
        .expect("usage read")
        .expect("a live session reports its usage");
    assert_eq!(
        usage.main_agent_usage.input_tokens, 1234,
        "the turn's tokens are accounted for: {usage:?}",
    );
    assert_eq!(usage.total_usage.input_tokens, 1234);

    let tree = harness.host.tree(&session).await.expect("tree read");
    assert!(
        !tree.segments.is_empty(),
        "the session has at least one branch segment",
    );
    assert!(
        tree.segments.iter().any(|segment| segment.on_active_path),
        "the active path is marked",
    );
    // The head travels with the read (spec 6.7). It is not derivable from the
    // segments, and a client that renders the tree needs the exact entry.
    let head = tree.head.clone().expect("a session with a turn has a head");
    assert_eq!(
        head,
        harness
            .host
            .local_handles(&session)
            .await
            .expect("live")
            .log
            .lock()
            .await
            .head()
            .cloned()
            .expect("a persisted head"),
    );

    let hello = harness.host.hello();
    assert_eq!(hello.protocol, aj_wire::PROTOCOL_VERSION);
    assert!(!hello.host_id.is_empty());
    assert_eq!(
        hello.working_directory.as_deref(),
        Some(harness._dir.path()),
        "a host serves the directory it was started in",
    );

    harness.host.shutdown().await;
    // A second host over the same store reads back the persisted id.
    let revived = harness.revive(Vec::new());
    assert_eq!(
        revived.host.hello().host_id,
        hello.host_id,
        "the host id is persisted in the session store",
    );
    revived.host.shutdown().await;
}

#[tokio::test]
async fn a_task_detail_read_omits_host_paths_and_cold_tasks_are_unknown() {
    let harness = Harness::new(vec![finalized_text_message("recorded")]);
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "record this session").await;
    client.pump_until_idle().await;
    drop(client);
    let handles = harness.host.local_handles(&session).await.expect("handles");
    let (task, _cancel) = handles.task_registry.register(
        AgentId::Main,
        "call-1".into(),
        TaskKind::Agent {
            agent_id: 1,
            task: "inspect".into(),
        },
        "inspect".into(),
        Arc::new(FixedTaskOutput),
    );
    handles
        .task_registry
        .set_status(task, TaskStatus::Exited(Some(0)));

    let details = harness.host.task(&session, task).await.expect("task read");
    assert_eq!(details.status, TaskStatus::Exited(Some(0)));
    assert_eq!(details.stdout_tail, "stdout tail");
    assert_eq!(details.stderr_total_bytes, 12);
    assert_eq!(details.report.as_deref(), Some("agent report"));
    let encoded = serde_json::to_value(details).expect("task details serialize");
    assert!(
        encoded.get("spill_path").is_none(),
        "a host path never crosses the transport boundary",
    );
    assert!(matches!(
        harness.host.task(&session, task + 1).await,
        Err(HostError::UnknownTask(_))
    ));

    harness.host.shutdown().await;
    let revived = harness.revive(Vec::new());
    assert!(matches!(
        revived.host.task(&session, task).await,
        Err(HostError::UnknownTask(_))
    ));
    revived.host.shutdown().await;
}

/// The task, queue and usage reads answer a session that is not live
/// without materializing it (spec 6.7), and the directory carries its row
/// with the stamp its log file bears. The tree read is the one exception: it
/// has to parse the log, so it materializes.
#[tokio::test]
async fn reads_do_not_materialize_a_cold_session() {
    let harness = Harness::new(vec![finalized_text_message("on the record")]);
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "hi").await;
    client.pump_until_idle().await;
    drop(client);
    harness.host.shutdown().await;

    let log_modified: chrono::DateTime<chrono::Utc> = std::fs::metadata(
        harness
            .persistence
            .sessions_dir()
            .join(format!("{session}.jsonl")),
    )
    .expect("the log")
    .modified()
    .expect("a modification time")
    .into();

    let revived = harness.revive(Vec::new());
    let is_live = async || {
        revived
            .host
            .sessions()
            .await
            .expect("sessions")
            .sessions
            .into_iter()
            .find(|entry| entry.id == session)
            .expect("the session is listed from disk")
    };
    let cold = is_live().await;
    assert!(!cold.live);
    assert_eq!(
        cold.last_seq, None,
        "a cold row carries no position: producing one would read the log",
    );
    assert_eq!(
        cold.last_activity, log_modified,
        "and its stamp is the log file's modification time (spec 6.8)",
    );

    assert!(
        revived
            .host
            .tasks(&session)
            .await
            .expect("tasks")
            .tasks
            .is_empty(),
    );
    assert!(
        revived
            .host
            .queue(&session)
            .await
            .expect("queue")
            .queues
            .is_empty(),
    );
    assert!(
        revived.host.usage(&session).await.expect("usage").is_none(),
        "usage is per process, so a session this host never held spent nothing",
    );
    assert!(
        !is_live().await.live,
        "neither read materialized the session",
    );
    assert!(
        SessionLock::try_acquire(&revived.persistence, &session, "a-rival-writer")
            .expect("try_acquire")
            .is_some(),
        "and neither took its advisory lock",
    );

    // An unknown session is still a 404 rather than an empty answer.
    for err in [
        revived.host.tasks("not-a-session").await.err(),
        revived.host.queue("not-a-session").await.err(),
        revived.host.usage("not-a-session").await.err(),
    ] {
        let err = err.expect("an unknown session is refused");
        assert!(matches!(err, HostError::UnknownSession(_)), "got {err:?}");
    }

    // The tree read parses the log, so it materializes like a command.
    assert!(
        !revived
            .host
            .tree(&session)
            .await
            .expect("tree")
            .segments
            .is_empty(),
    );
    assert!(is_live().await.live, "the tree read materialized it");
    revived.host.shutdown().await;
}

/// A live session's mark comes from the host's own bookkeeping, and the cold
/// half of the directory tracks the store rather than a snapshot of it: a log
/// that grows behind the host's back reports its new stamp, a session file
/// that appears is listed, one that is deleted goes away, and a pre-refactor
/// log is no session at all.
///
/// This is the correctness half of the list-production contract (spec 6.8).
/// The caches it exercises are what keep a refresh from re-reading the store,
/// and the unit tests over `ColdSessions` are the oracle for the reads they
/// avoid.
#[tokio::test]
async fn the_directory_follows_the_store_it_caches() {
    let harness = Harness::new(vec![finalized_text_message("on the record")]);
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "hi").await;
    client.pump_until_idle().await;
    let logged = {
        let handles = harness
            .host
            .local_handles(&session)
            .await
            .expect("live session");
        handles.log.lock().await.last_seq()
    };
    let live_mark = harness
        .host
        .sessions()
        .await
        .expect("sessions")
        .sessions
        .into_iter()
        .find(|entry| entry.id == session)
        .expect("the live session is listed");
    assert!(live_mark.live && logged > 0);
    assert_eq!(
        live_mark.last_seq,
        Some(logged),
        "a live session's mark is the one the host already holds",
    );
    drop(client);
    harness.host.shutdown().await;

    let revived = harness.revive(Vec::new());
    let sessions_dir = revived.persistence.sessions_dir().to_path_buf();
    // The row's activity stamp, or `None` when the directory does not name
    // the session at all.
    let stamp = async |id: &str| {
        revived
            .host
            .sessions()
            .await
            .expect("sessions")
            .sessions
            .into_iter()
            .find(|entry| entry.id == id)
            .map(|entry| entry.last_activity)
    };
    let modified = |id: &str| -> chrono::DateTime<chrono::Utc> {
        std::fs::metadata(sessions_dir.join(format!("{id}.jsonl")))
            .expect("the log")
            .modified()
            .expect("a modification time")
            .into()
    };
    assert_eq!(
        stamp(&session).await,
        Some(modified(&session)),
        "a cold row's stamp is its log file's modification time",
    );

    // A line appended behind this host's back (a sibling process holding the
    // session, say) moves the file's size and modification time, which is what
    // the next enumeration point picks the row up from.
    let appended = serde_json::json!({
        "id": "ffffffff",
        "timestamp": "2024-01-01T00:00:00Z",
        "thread": "meta",
        "type": "system_prompt",
        "text": "appended behind our back",
    });
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(sessions_dir.join(format!("{session}.jsonl")))
        .expect("reopen the log");
    std::io::Write::write_all(&mut file, format!("{appended}\n").as_bytes())
        .expect("append an entry");
    drop(file);
    assert_eq!(
        stamp(&session).await,
        Some(modified(&session)),
        "a log that grew reports the stamp it grew to",
    );

    // A session file that appears is listed, and one that is deleted goes
    // away: the enumeration is what notices either, so no cached verdict can
    // outlive the file it was taken from.
    let appeared = "2000-01-01-00-00-00-000";
    let appeared_path = sessions_dir.join(format!("{appeared}.jsonl"));
    let entry = serde_json::json!({
        "id": "00000000",
        "timestamp": "2024-01-01T00:00:00Z",
        "thread": "meta",
        "type": "system_prompt",
        "text": "a log written behind the host's back",
    });
    std::fs::write(&appeared_path, format!("{entry}\n")).expect("write a session file");
    assert_eq!(
        stamp(appeared).await,
        Some(modified(appeared)),
        "the new file is listed",
    );
    std::fs::remove_file(&appeared_path).expect("delete it again");
    assert_eq!(stamp(appeared).await, None, "and the deleted one is gone");

    let ancient = "1999-01-01-00-00-00-000";
    std::fs::write(
        sessions_dir.join(format!("{ancient}.jsonl")),
        "not json at all\n",
    )
    .expect("write a pre-refactor file");
    assert_eq!(
        stamp(ancient).await,
        None,
        "a pre-refactor log is not a session",
    );
    revived.host.shutdown().await;
}

// ---------------------------------------------------------------------------
// 16. Shutdown
// ---------------------------------------------------------------------------

/// Shutdown cancels a running turn through the graceful path, so the
/// transcript keeps its synthetic aborted `MessageEnd`, and releases the
/// session lock.
///
/// The buffered records asserted on below reached disk because the turn's
/// own punctuating append drained them, not because teardown flushed: a
/// record that nothing punctuates is covered where teardown is the only
/// thing that can force it out.
#[tokio::test]
async fn shutdown_cancels_gracefully_and_flushes() {
    let harness = Harness::with_provider(scripted(
        vec![finalized_text_message(
            "an answer streamed slowly enough to be interrupted",
        )],
        1,
        Duration::from_millis(40),
    ));
    let session = harness.create().await;
    let mut stream = harness
        .host
        .attach(&[AttachRequest {
            session: session.clone(),
            cursor: None,
        }])
        .await
        .expect("attach");
    frames_until(&mut stream, "caught_up", |frame| {
        matches!(frame, Frame::CaughtUp { .. })
    })
    .await;
    harness.prompt(&session, "hi").await;
    // Wait until the turn is actually streaming before pulling it down.
    frames_until(&mut stream, "the turn to start streaming", |frame| {
        matches!(
            frame,
            Frame::Event { event, .. }
                if matches!(event.known(), Some(AgentEvent::MessageUpdate { .. }))
        )
    })
    .await;
    let log_path = harness
        .persistence
        .sessions_dir()
        .join(format!("{session}.jsonl"));

    harness.host.shutdown().await;

    // Read the log back the way a resume would, so the assertions are
    // about entries rather than about substrings of a file.
    let reopened =
        aj_session::ConversationLog::resume(&harness.persistence, &session).expect("resume");
    let kinds: Vec<&aj_session::ConversationEntryKind> = reopened
        .entries_in_order()
        .into_iter()
        .map(|entry| &entry.entry)
        .collect();
    assert!(
        kinds
            .iter()
            .any(|kind| matches!(kind, aj_session::ConversationEntryKind::SystemPrompt { .. })),
        "the buffered system-prompt root reached disk: {kinds:?}",
    );
    assert!(
        kinds.iter().any(|kind| matches!(
            kind,
            aj_session::ConversationEntryKind::ThinkingChange { .. }
        )),
        "and so did the buffered seed settings records: {kinds:?}",
    );
    // The cancelled turn wrote both its user message and the synthetic
    // aborted assistant message, so the transcript is consistent rather
    // than truncated mid-turn.
    let roles: Vec<&str> = reopened
        .entries_in_order()
        .into_iter()
        .filter_map(|entry| match &entry.entry {
            aj_session::ConversationEntryKind::Message { message } => {
                match message.as_stored_wire()? {
                    aj_models::types::Message::User(_) => Some("user"),
                    aj_models::types::Message::Assistant(_) => Some("assistant"),
                    aj_models::types::Message::ToolResult(_) => Some("tool_result"),
                }
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        roles,
        vec!["user", "assistant"],
        "the aborted turn is bracketed by its own terminal message",
    );
    let _ = log_path;

    let reacquired = SessionLock::try_acquire(&harness.persistence, &session, "a-rival-writer")
        .expect("try_acquire")
        .expect("shutdown released the session lock");
    drop(reacquired);
}

/// Teardown flushes a log entry that nothing else would force out.
///
/// Non-punctuation entries (settings records, spawn roots) buffer in memory
/// until the next punctuating append drains them, so a settings change with
/// no prompt behind it is only on disk because shutdown flushed it. The
/// prompt first is what materializes the file at all: the flush is a no-op
/// while the log has none, which is what keeps an abandoned empty session
/// from leaving one behind.
#[tokio::test]
async fn shutdown_flushes_a_settings_record_nothing_punctuates() {
    let harness = Harness::new(vec![finalized_text_message("on the record")]);
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "hi").await;
    client.pump_until_idle().await;

    harness
        .host
        .command(
            &session,
            Command::Settings(SettingsChange {
                agent: AgentId::Main,
                persist: PersistAction::None,
                axis: SettingsAxis::Thinking(Some(aj_models::ThinkingConfig::High)),
            }),
        )
        .await
        .expect("thinking change");
    // The record is in the log's memory and nowhere else: the change
    // appended no message behind it.
    let levels_before = thinking_levels_on_disk(&harness, &session);
    assert!(
        !levels_before.iter().any(|level| level == "high"),
        "the settings record is still buffered: {levels_before:?}",
    );
    drop(client);

    harness.host.shutdown().await;

    let levels = thinking_levels_on_disk(&harness, &session);
    assert_eq!(
        levels.last().map(String::as_str),
        Some("high"),
        "teardown flushed the buffered settings record: {levels:?}",
    );
}

/// Every thinking level recorded in the session's log **file**, in append
/// order. Read off disk rather than through the live log, which answers from
/// memory whether or not the entry was flushed.
fn thinking_levels_on_disk(harness: &Harness, session: &str) -> Vec<String> {
    let path = harness
        .persistence
        .sessions_dir()
        .join(format!("{session}.jsonl"));
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    raw.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|entry| entry["type"] == "thinking_change")
        .filter_map(|entry| entry["level"].as_str().map(str::to_string))
        .collect()
}

/// The frames a cancelled turn publishes still bracket the transcript: the
/// aborted turn's terminal `MessageEnd` reaches attached clients before the
/// stream closes.
#[tokio::test]
async fn a_cancelled_turn_publishes_its_terminal_message() {
    let harness = Harness::with_provider(scripted(
        vec![finalized_text_message(
            "an answer streamed slowly enough to be interrupted",
        )],
        1,
        Duration::from_millis(40),
    ));
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "hi").await;
    frames_until(&mut client.stream, "the turn to start streaming", |frame| {
        matches!(
            frame,
            Frame::Event { event, .. }
                if matches!(event.known(), Some(AgentEvent::MessageUpdate { .. }))
        )
    })
    .await;

    harness
        .host
        .command(
            &session,
            Command::Cancel {
                agent: AgentId::Main,
            },
        )
        .await
        .expect("cancel");
    let frames = client.pump_until_idle().await;

    assert!(!client.client.working(), "the session is idle again");
    assert!(
        notice(&frames, CANCELLED),
        "the turn ended as a cancellation rather than on its own: {:?}",
        events(&frames)
            .into_iter()
            .map(event_kind)
            .collect::<Vec<_>>(),
    );
    assert_no_dangling(&client.chat);
    let state = format!("{:?}", client.canonical());
    assert!(
        state.contains("finalized: true"),
        "the aborted turn's message was finalized: {state}",
    );
    harness.host.shutdown().await;
}

/// Every session the host holds is torn down, not just the first.
#[tokio::test]
async fn shutdown_releases_every_session() {
    let harness = Harness::new(Vec::new());
    let first = harness.create().await;
    let second = harness.create().await;
    let subscriber = harness
        .host
        .attach(&[AttachRequest {
            session: first.clone(),
            cursor: None,
        }])
        .await
        .expect("attach");

    harness.host.shutdown().await;

    for session in [&first, &second] {
        let lock = SessionLock::try_acquire(&harness.persistence, session, "a-rival-writer")
            .expect("try_acquire")
            .expect("every session's lock is released");
        drop(lock);
    }
    // The stream ends rather than stalling: `recv` yields whatever was
    // queued and then `None`.
    let mut subscriber = subscriber;
    while bounded("the subscriber stream to close", subscriber.recv())
        .await
        .is_some()
    {}
}

// ---------------------------------------------------------------------------
// 17. Wire round-trip
// ---------------------------------------------------------------------------

/// Every frame the host publishes survives the wire codec byte-identically,
/// on the live path and in a full backfill.
///
/// `Frame`'s serializer validates as it writes: a `MessageEnd` whose message
/// id is not its `entry_id`, or that carries no durability at all, is a hard
/// error rather than a frame with a wrong field. Nothing in this phase
/// serializes a frame, so a host that published one of those would look
/// healthy until a stream writer existed, which is why the check lives here
/// rather than waiting for one.
#[tokio::test]
async fn every_published_frame_round_trips_through_the_wire_codec() {
    let harness = Harness::with_provider(scripted(sub_agent_turn(), 1, Duration::from_millis(5)));
    let session = harness.create().await;
    let mut stream = harness
        .host
        .attach(&[AttachRequest {
            session: session.clone(),
            cursor: None,
        }])
        .await
        .expect("attach");
    let mut frames = frames_until(&mut stream, "caught_up", |frame| {
        matches!(frame, Frame::CaughtUp { .. })
    })
    .await;

    harness.prompt(&session, "delegate it").await;
    frames.extend(until_idle(&mut stream).await);
    // A settings change as well, for the notice and `state` frames the host
    // synthesizes itself.
    harness
        .host
        .command(
            &session,
            Command::Settings(SettingsChange {
                agent: AgentId::Main,
                persist: PersistAction::None,
                axis: SettingsAxis::Thinking(Some(aj_models::ThinkingConfig::High)),
            }),
        )
        .await
        .expect("thinking change");
    frames.extend(
        frames_until(&mut stream, "the settings state frame", |frame| {
            matches!(frame, Frame::State { .. })
        })
        .await,
    );
    // Give the directory tick room, so a `list` frame is in the sample too.
    tokio::time::sleep(LIST_SETTLE).await;
    frames.extend(drained(&mut stream));

    // The same history as a full backfill: projected `MessageEnd`s, the
    // sub-agent bracketing, and the conclusion sweep behind `caught_up`.
    let mut second = harness
        .host
        .attach(&[AttachRequest {
            session: session.clone(),
            cursor: None,
        }])
        .await
        .expect("re-attach");
    frames.extend(
        frames_until(&mut second, "the backfill's caught_up", |frame| {
            matches!(frame, Frame::CaughtUp { .. })
        })
        .await,
    );
    frames.extend(drained(&mut second));

    let mut durable_messages = 0;
    for frame in &frames {
        if matches!(frame, Frame::Event { durability: Some(_), event, .. }
            if matches!(event.known(), Some(AgentEvent::MessageEnd { .. })))
        {
            durable_messages += 1;
        }
        let json = serde_json::to_string(frame)
            .unwrap_or_else(|err| panic!("the host published a frame the codec rejects: {err}"));
        let decoded: Frame = serde_json::from_str(&json)
            .unwrap_or_else(|err| panic!("a published frame does not decode: {err}\n{json}"));
        let again = serde_json::to_string(&decoded).expect("a decoded frame re-serializes");
        assert_eq!(json, again, "a published frame does not round-trip");
    }

    // The sample is only worth anything if it covered the frame kinds that
    // carry rules of their own.
    assert!(
        frames.len() > 40,
        "the sample is a whole session: {} frames",
        frames.len(),
    );
    assert!(
        durable_messages >= 8,
        "including the durable message frames the codec validates: {durable_messages}",
    );
    for wanted in ["state", "caught_up", "list"] {
        assert!(
            frames.iter().any(|frame| {
                serde_json::to_value(frame).expect("serializes")["kind"] == wanted
            }),
            "the sample covers the {wanted} frame kind",
        );
    }
    harness.host.shutdown().await;
}
