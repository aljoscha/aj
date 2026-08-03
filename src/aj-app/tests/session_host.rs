//! The session host: lifecycle, fan-out, attach, commands, reads
//! (spec section 5, 6.3-6.9).
//!
//! Every test drives the real host over the scripted provider, so the
//! frames asserted on are the ones a network server would serialize and
//! the client fold ([`SessionClient`]) would receive.

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use aj_agent::events::{AgentEvent, AgentId, AgentSettings};
use aj_app::chat::ChatState;
use aj_app::client::SessionClient;
use aj_app::host::{
    AttachRequest, Attachment, Command, CommandOutcome, HostError, HostSetup, QueueOp, SessionHost,
    SettingsAxis, SettingsChange,
};
use aj_app::session_setup::RunConfigSnapshot;
use aj_app::settings::{ConfigLayers, PersistAction};
use aj_app::test_support::{
    CanonicalState, assert_canonical_eq, assert_no_dangling, finalized_text_message,
    scripted_model_info,
};
use aj_conf::{Config, ConfigLayer};
use aj_models::auth::AuthStorage;
use aj_models::scripted::{ExhaustedBehavior, ScriptedProvider};
use aj_models::types::{AssistantContent, AssistantMessage, StopReason, ToolCall, UserContent};
use aj_session::{ConversationPersistence, SessionLock, ThreadFilter};
use aj_wire::Frame;
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

impl Harness {
    /// A host whose sessions run the scripted provider replaying
    /// `messages`. Every session materialized from this host shares that
    /// one script, so a test with two concurrent sessions installs a
    /// per-session provider instead (see [`Harness::install_script`]).
    fn new(messages: Vec<AssistantMessage>) -> Self {
        Self::with_provider(scripted(messages, 0, Duration::ZERO))
    }

    fn with_provider(provider: Arc<ScriptedProvider>) -> Self {
        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().join("sessions"));
        let config = Arc::new(StdMutex::new(Config::default()));
        let host = SessionHost::new(HostSetup {
            config: Arc::clone(&config),
            layers: Arc::new(StdMutex::new(ConfigLayers {
                user: Config::default(),
                project: ConfigLayer::default(),
                project_path: None,
            })),
            catalog: Arc::new(Vec::new()),
            run_config: snapshot(provider),
            restore: None,
            persistence: persistence.clone(),
            auth: AuthStorage::new(dir.path().join("auth.json")),
            working_directory: dir.path().to_path_buf(),
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
        let dir = TempDir::new().expect("tempdir");
        let config = Arc::new(StdMutex::new(Config::default()));
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
        speed: None,
        model_key: ("scripted".to_string(), "scripted".to_string()),
        session_id: None,
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

    /// Apply frames up to and including the block's `caught_up`.
    async fn apply_block(&mut self) {
        let frames = frames_until(&mut self.stream, "caught_up", |frame| {
            matches!(frame, Frame::CaughtUp { .. })
        })
        .await;
        for frame in frames {
            let _ = self.client.apply(&mut self.chat, frame);
        }
    }

    /// Fold until the session reports idle.
    async fn pump_until_idle(&mut self) {
        let frames = until_idle(&mut self.stream).await;
        for frame in frames {
            let _ = self.client.apply(&mut self.chat, frame);
        }
    }

    fn canonical(&self) -> CanonicalState {
        CanonicalState::of(&self.chat, self.client.lifecycle())
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

fn settings() -> AgentSettings {
    AgentSettings {
        provider: "scripted".into(),
        model_id: "scripted".into(),
        thinking: "off".into(),
        speed: "standard".into(),
        verbosity: "default".into(),
    }
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
                entry: head.clone(),
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
                entry: "whatever".to_string(),
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
                entry: "whatever".to_string(),
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
                entry: "no-such-entry".to_string(),
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
        .command(&session, Command::Head { entry: sub_entry })
        .await
        .expect_err("a sub-agent entry cannot be a head");
    assert!(matches!(err, HostError::Invalid(_)), "got {err:?}");
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
        .command(&session, Command::Head { entry: head })
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
        SessionLock::try_acquire(&harness.persistence, &session)
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
    assert!(matches!(err, HostError::Locked(_)), "got {err:?}");

    harness.host.shutdown().await;
    let reacquired = SessionLock::try_acquire(&harness.persistence, &session)
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
    let held = SessionLock::try_acquire(&harness.persistence, &session)
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

    let free = SessionLock::try_acquire(&harness.persistence, &session)
        .expect("try_acquire")
        .expect("no request re-took the session's lock");
    drop(free);
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

    for client in [&mut one, &mut two] {
        client.pump_until_idle().await;
    }

    // Count the queue updates each subscriber saw across its whole stream.
    for (label, client) in [("first", &one), ("second", &two)] {
        let seen = client.client.queue();
        assert!(
            seen.queues
                .iter()
                .all(|queue| queue.steering.is_empty() && queue.follow_up.is_empty()),
            "the {label} client's queue view ends empty: {seen:?}",
        );
    }
    harness.host.shutdown().await;
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
#[tokio::test]
async fn clearing_the_queue_empties_every_agent() {
    let harness = Harness::new(sub_agent_turn());
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "delegate it").await;
    client.pump_until_idle().await;

    // Queued through the in-process handles: an idle agent runs a prompt
    // instead of queueing it.
    let handles = harness
        .host
        .local_handles(&session)
        .await
        .expect("live session");
    handles.queues.append_follow_up(AgentId::Main, "for main");
    handles
        .queues
        .append_follow_up(AgentId::Sub(1), "for the sub");
    let _ = drained(&mut client.stream);

    harness
        .host
        .command(&session, Command::Queue(QueueOp::Clear))
        .await
        .expect("clear");

    assert_eq!(handles.queues.pending_counts(), (0, 0));
    let mut updated: Vec<AgentId> = events(&drained(&mut client.stream))
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
    let rendered = format!("{:?}", joiner.canonical());
    assert!(
        !rendered.contains("low"),
        "the backfill regenerates nothing for a seed settings entry: {rendered}",
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
    .await;
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

    client.pump_until_idle().await;
    assert!(
        !client.client.working(),
        "the cascade cancelled the main turn too",
    );
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

    let rest = until_idle(&mut stream).await;
    assert!(
        durable(&rest).iter().all(|(seq, _)| *seq > boundary),
        "no live durable frame at or below the boundary is delivered: {:?}",
        durable(&rest),
    );
    harness.host.shutdown().await;
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
    assert!(
        format!("{:?}", joiner.canonical()).contains("todo_read"),
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
    assert!(summary.last_seq > 0, "it has durable entries");
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

/// The reads answer the task table (with wall-clock timestamps), the queue,
/// the branch tree, and hello with a `host_id` that survives a restart.
#[tokio::test]
async fn the_reads_answer_tasks_queue_tree_and_hello() {
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

    // The queue read: enqueue the way a busy session would (an idle steer
    // runs a turn instead of queueing) and read it back.
    let handles = harness
        .host
        .local_handles(&session)
        .await
        .expect("live session");
    handles.queues.append_steering(AgentId::Main, "urgent");
    handles.queues.append_follow_up(AgentId::Sub(1), "later");
    let queue = harness.host.queue(&session).await.expect("queue read");
    assert_eq!(
        queue
            .queues
            .iter()
            .map(|entry| entry.agent_id)
            .collect::<Vec<_>>(),
        vec![AgentId::Main, AgentId::Sub(1)],
        "one entry per agent with something queued, main first",
    );
    assert_eq!(queue.queues[0].steering.len(), 1);
    assert!(queue.queues[0].follow_up.is_empty());
    assert_eq!(queue.queues[1].follow_up.len(), 1);
    handles.queues.clear(AgentId::Main);
    handles.queues.clear(AgentId::Sub(1));

    let tree = harness.host.tree(&session).await.expect("tree read");
    assert!(
        !tree.segments.is_empty(),
        "the session has at least one branch segment",
    );
    assert!(
        tree.segments.iter().any(|segment| segment.on_active_path),
        "the active path is marked",
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

/// The task and queue reads answer a session that is not live without
/// materializing it (spec 6.7), and the directory reports its durable
/// high-water mark from the store rather than as zero. The tree read is the
/// one exception: it has to parse the log, so it materializes.
#[tokio::test]
async fn reads_do_not_materialize_a_cold_session() {
    let harness = Harness::new(vec![finalized_text_message("on the record")]);
    let session = harness.create().await;
    let mut client = Client::attach(&harness.host, &session).await;
    harness.prompt(&session, "hi").await;
    client.pump_until_idle().await;
    let live_last_seq = {
        let handles = harness
            .host
            .local_handles(&session)
            .await
            .expect("live session");
        handles.log.lock().await.last_seq()
    };
    drop(client);
    harness.host.shutdown().await;

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
        cold.last_seq, live_last_seq,
        "a cold session's high-water mark comes from the store, not from zero",
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
        !is_live().await.live,
        "neither read materialized the session",
    );
    assert!(
        SessionLock::try_acquire(&revived.persistence, &session)
            .expect("try_acquire")
            .is_some(),
        "and neither took its advisory lock",
    );

    // An unknown session is still a 404 rather than an empty answer.
    for err in [
        revived.host.tasks("not-a-session").await.err(),
        revived.host.queue("not-a-session").await.err(),
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

// ---------------------------------------------------------------------------
// 16. Shutdown
// ---------------------------------------------------------------------------

/// Shutdown cancels a running turn through the graceful path, so the
/// transcript keeps its synthetic aborted `MessageEnd`, flushes buffered
/// log writes, and releases the session lock.
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

    let reacquired = SessionLock::try_acquire(&harness.persistence, &session)
        .expect("try_acquire")
        .expect("shutdown released the session lock");
    drop(reacquired);
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
    client.pump_until_idle().await;

    assert!(!client.client.working(), "the session is idle again");
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
        let lock = SessionLock::try_acquire(&harness.persistence, session)
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
